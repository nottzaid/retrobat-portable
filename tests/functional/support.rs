//! Drives the real RetroPort binary against disposable copies of a real,
//! assembled installation. Nothing here substitutes a component: downloads
//! go to the publishers, games come from their authors, emulators run.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

pub const BINARY: &str = env!("CARGO_BIN_EXE_retrobat-portable");

fn project() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn work() -> PathBuf {
    project().join("target").join("functional")
}

/// The assembled installation under test (RETROPORT_INSTALLATION, else this
/// checkout after `tools/bootstrap_bundle.sh`).
pub fn installation() -> PathBuf {
    let root = std::env::var_os("RETROPORT_INSTALLATION")
        .map(PathBuf::from)
        .unwrap_or_else(project);
    assert!(
        root.join("RetroBat/RetroBat.exe").is_file()
            && root
                .join("RetroBat/emulationstation/.emulationstation/es_systems.cfg")
                .is_file(),
        "functional tests need an assembled installation at {}; run tools/bootstrap_bundle.sh \
         or set RETROPORT_INSTALLATION",
        root.display()
    );
    root
}

/// Wine's per-user prefix for these tests, kept apart from the user's own.
pub fn xdg_data() -> PathBuf {
    work().join("xdg-data")
}

pub struct Bundle {
    pub root: PathBuf,
}

impl Bundle {
    /// A private copy of the installation. On btrfs/XFS the copy is a
    /// reflink and costs no space; elsewhere it is a real copy.
    pub fn fresh(name: &str) -> Self {
        let root = work().join("runs").join(name);
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        let source = installation();
        for directory in ["RetroBat", "Runtime", "Artwork"] {
            let status = Command::new("cp")
                .args(["-a", "--reflink=auto"])
                .arg(source.join(directory))
                .arg(&root)
                .status()
                .unwrap();
            assert!(status.success(), "copying {directory} failed");
        }
        // Start without anyone's imports, downloads, or emulator state.
        let _ = fs::remove_dir_all(root.join(".retrobat-portable"));
        Self { root }
    }

    pub fn retroport(&self, arguments: &[&str]) -> Output {
        Command::new(BINARY)
            .arg("--bundle-root")
            .arg(&self.root)
            .args(arguments)
            .env("XDG_DATA_HOME", xdg_data())
            .output()
            .unwrap()
    }

    /// Runs RetroPort with a private X display (for emulators that open a
    /// window even for maintenance tasks) and asserts success.
    pub fn ok_with_display(&self, arguments: &[&str]) -> String {
        let mut command = Command::new(BINARY);
        command.arg("--bundle-root").arg(&self.root);
        self.ok_on_display(command, arguments)
    }

    /// Like `ok_with_display`, through the Windows build under Wine.
    pub fn ok_with_wine_display(&self, arguments: &[&str]) -> String {
        let mut command = Command::new("wine");
        command.arg(self.root.join("RetroPort.exe")).env(
            "WINEPREFIX",
            xdg_data().join("retrobat-portable/wine-prefix"),
        );
        self.ok_on_display(command, arguments)
    }

    fn ok_on_display(&self, mut command: Command, arguments: &[&str]) -> String {
        let number = 140 + DISPLAY.fetch_add(1, Ordering::SeqCst) + std::process::id() % 20;
        let display = format!(":{number}");
        let mut server = Command::new(xvfb())
            .args([
                display.as_str(),
                "-screen",
                "0",
                "1280x720x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_secs(1));
        let output = command
            .args(arguments)
            .env("DISPLAY", &display)
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_DATA_HOME", xdg_data())
            .output()
            .unwrap();
        let _ = server.kill();
        let _ = server.wait();
        assert!(
            output.status.success(),
            "retroport {arguments:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Runs RetroPort and asserts success, returning its standard output.
    pub fn ok(&self, arguments: &[&str]) -> String {
        let output = self.retroport(arguments);
        assert!(
            output.status.success(),
            "retroport {arguments:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Runs RetroPort, asserts failure, and returns its error output.
    pub fn refused(&self, arguments: &[&str]) -> String {
        let output = self.retroport(arguments);
        assert!(
            !output.status.success(),
            "retroport {arguments:?} unexpectedly succeeded:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    pub fn manifest(&self, id: &str) -> Option<Value> {
        let path = self
            .root
            .join(".retrobat-portable/imported")
            .join(format!("{}.json", id.replace('/', "--")));
        fs::read(path)
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
    }

    /// Absolute paths of every file an import record owns.
    pub fn owned_files(&self, id: &str) -> Vec<PathBuf> {
        let manifest = self.manifest(id).expect("import record");
        manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| self.root.join(file["relative_path"].as_str().unwrap()))
            .collect()
    }

    pub fn launch_file(&self, id: &str) -> PathBuf {
        let manifest = self.manifest(id).expect("import record");
        self.root
            .join(manifest["launch_relative_path"].as_str().unwrap())
    }

    pub fn self_check(&self) -> Value {
        serde_json::from_str(&self.ok(&["--self-check"])).unwrap()
    }
}

/// A real game from its author, pinned by SHA-256 and cached.
pub struct Game {
    pub url: &'static str,
    pub sha256: &'static str,
}

pub fn fetch(game: &Game) -> PathBuf {
    fetch_with(game, &[])
}

/// Like `fetch`, with extra curl arguments for hosts that require them
/// (GameBrew serves files only to a browser coming from its own page).
pub fn fetch_with(game: &Game, curl_arguments: &[&str]) -> PathBuf {
    let cache = work().join("cache");
    fs::create_dir_all(&cache).unwrap();
    let name = game.url.rsplit('/').next().unwrap();
    let path = cache.join(format!("{}-{name}", &game.sha256[..12]));
    if !path.is_file() {
        let temporary = path.with_extension("part");
        let status = Command::new("curl")
            .args(["--fail", "--location", "--silent", "--show-error"])
            .args(curl_arguments)
            .arg("--output")
            .arg(&temporary)
            .arg(game.url)
            .status()
            .unwrap();
        assert!(status.success(), "downloading {} failed", game.url);
        fs::rename(&temporary, &path).unwrap();
    }
    let digest = hex::encode(Sha256::digest(fs::read(&path).unwrap()));
    assert_eq!(digest, game.sha256, "{} changed upstream", game.url);
    path
}

/// Extracts an archive with the system's 7-Zip into a fresh directory.
pub fn extract(archive: &Path, name: &str) -> PathBuf {
    let target = work().join("extracted").join(name);
    if target.exists() {
        fs::remove_dir_all(&target).unwrap();
    }
    fs::create_dir_all(&target).unwrap();
    let mut output_flag = std::ffi::OsString::from("-o");
    output_flag.push(&target);
    let status = Command::new("7z")
        .args(["x", "-y", "-bb0"])
        .arg(output_flag)
        .arg(archive)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "extracting {} failed", archive.display());
    target
}

fn xvfb() -> PathBuf {
    let local = work().join("tools/usr/bin/Xvfb");
    if local.is_file() {
        return local;
    }
    PathBuf::from("Xvfb")
}

static DISPLAY: AtomicU32 = AtomicU32::new(0);

/// What a gameplay probe observed.
pub struct Played {
    pub events: Vec<Value>,
    /// The whole virtual screen shortly before the deadline.
    pub frame: PathBuf,
}

impl Played {
    pub fn event(&self, name: &str) -> Option<&Value> {
        self.events.iter().find(|event| event["event"] == name)
    }

    /// Asserts the game started, ran until the deadline, and its whole
    /// process tree was gone after TERMINATE.
    pub fn assert_ran(&self, label: &str) {
        assert!(
            self.event("gameplay_probe_complete").is_some()
                && self.event("alive_at_deadline").is_some()
                && self.event("terminated").is_some(),
            "{label} did not run to its deadline: {:#?}",
            self.events
        );
        let terminated = self.event("terminated").unwrap();
        assert_eq!(terminated["process_tree_running"], false, "{label}");
    }

    /// Asserts RetroArch's own screenshot shows a picture, not one colour.
    pub fn assert_retroarch_frame(&self, label: &str) {
        let screenshot = self
            .event("screenshot")
            .unwrap_or_else(|| panic!("{label} has no RetroArch screenshot: {:#?}", self.events));
        assert_eq!(screenshot["blank"], false, "{label} showed a blank frame");
        assert_eq!(
            self.event("terminated").unwrap()["backend_log"]["content_loaded"],
            true,
            "{label}: RetroArch did not accept the content"
        );
    }

    /// Distinct colours in the captured screen (sampled).
    pub fn frame_colours(&self) -> usize {
        let image = image::open(&self.frame)
            .unwrap_or_else(|error| panic!("{}: {error}", self.frame.display()))
            .to_rgb8();
        let mut colours = std::collections::HashSet::new();
        for pixel in image.pixels().step_by(5) {
            colours.insert(pixel.0);
        }
        colours.len()
    }
}

/// Starts a card's game through RetroPort's real PLAY path on a private X
/// display, holds it for `seconds`, then lets the probe terminate it.
pub fn play(bundle: &Bundle, id: &str, seconds: u64) -> Played {
    let mut game = Command::new(BINARY);
    game.arg("--bundle-root").arg(&bundle.root);
    play_with(game, id, seconds)
}

/// Like `play`, through the Windows build (`RetroPort.exe` beside the
/// bundle's RetroBat folder) under Wine, so PLAY takes its Windows routes.
pub fn play_windows(bundle: &Bundle, id: &str, seconds: u64) -> Played {
    let mut game = Command::new("wine");
    game.arg(bundle.root.join("RetroPort.exe")).env(
        "WINEPREFIX",
        xdg_data().join("retrobat-portable/wine-prefix"),
    );
    play_with(game, id, seconds)
}

fn play_with(mut game: Command, id: &str, seconds: u64) -> Played {
    let number = 160 + DISPLAY.fetch_add(1, Ordering::SeqCst) + std::process::id() % 40;
    let display = format!(":{number}");
    let evidence = work().join("evidence");
    fs::create_dir_all(&evidence).unwrap();
    let stem = id.replace('/', "--");
    let output = evidence.join(format!("{stem}.jsonl"));
    let frame = evidence.join(format!("{stem}.png"));
    let _ = fs::remove_file(&output);
    let _ = fs::remove_file(&frame);
    let mut server = Command::new(xvfb())
        .args([
            display.as_str(),
            "-screen",
            "0",
            "1280x720x24",
            "-nolisten",
            "tcp",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Xvfb must be installed (Debian: xvfb, Arch: xorg-server-xvfb)");
    thread::sleep(Duration::from_secs(1));
    game.args(["--gameplay-probe", id, "--gameplay-probe-output"])
        .arg(&output)
        .args(["--gameplay-probe-seconds", &seconds.to_string()])
        .env("DISPLAY", &display)
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_DATA_HOME", xdg_data())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        game.process_group(0);
    }
    let mut child = game.spawn().unwrap();
    let mut started = None;
    let mut grabbed = false;
    let deadline = Instant::now() + Duration::from_secs(seconds + 600);
    while Instant::now() < deadline {
        let text = fs::read_to_string(&output).unwrap_or_default();
        if started.is_none() && text.contains("\"game_started\"") {
            started = Some(Instant::now());
        }
        if !grabbed
            && started.is_some_and(|at: Instant| {
                at.elapsed() >= Duration::from_secs(seconds.saturating_sub(3))
            })
        {
            let status = Command::new("ffmpeg")
                .args([
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "x11grab",
                    "-video_size",
                    "1280x720",
                    "-i",
                ])
                .arg(&display)
                .args(["-frames:v", "1"])
                .arg(&frame)
                .status()
                .unwrap();
            assert!(status.success(), "ffmpeg could not capture {display}");
            grabbed = true;
        }
        if text.contains("gameplay_probe_complete")
            || text.contains("gameplay_probe_failed")
            || child.try_wait().unwrap().is_some()
        {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    thread::sleep(Duration::from_secs(1));
    // The probe closes RetroPort itself; this only clears leftovers.
    // SAFETY: signals the test's own process group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    let _ = Command::new("wineserver")
        .arg("-k")
        .env(
            "WINEPREFIX",
            xdg_data().join("retrobat-portable/wine-prefix"),
        )
        .status();
    let _ = server.kill();
    let _ = server.wait();
    let events = fs::read_to_string(&output)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    Played { events, frame }
}
