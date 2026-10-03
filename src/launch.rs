use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::Serialize;
use thiserror::Error;

use crate::paths::PortableLayout;
use crate::readiness::BackendRoute;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum HostPlatform {
    Windows,
    Linux,
    Unsupported,
}

impl HostPlatform {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Unsupported
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::Unsupported => "unsupported",
        }
    }
}

/// A setting a backend needs in its own state before it can start a game
/// unattended. Each one leaves everything else in that state untouched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeSetting {
    /// Create the file with these contents only if it does not exist yet.
    SeedFile { path: PathBuf, contents: String },
    /// Ensure `key=value` inside `[section]` of an INI file.
    IniValue {
        path: PathBuf,
        section: String,
        key: String,
        value: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchPlan {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub current_dir: PathBuf,
    pub env: BTreeMap<String, OsString>,
    /// Files rewritten on every launch (RetroArch's appended configuration).
    pub generated_files: Vec<(PathBuf, String)>,
    /// Directories the backend is configured to use, created before launch.
    pub generated_directories: Vec<PathBuf>,
    pub settings: Vec<RuntimeSetting>,
    /// Backend diagnostic log written by this launch, when the backend
    /// accepts an explicit log destination. It is replaced on every launch.
    pub log_file: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("this host platform is not supported")]
    Unsupported,
    #[error("cannot determine a local data directory for the Wine prefix")]
    NoDataDirectory,
    #[error("game system name is invalid: {0}")]
    InvalidSystem(String),
    #[error("Wine cannot address a relative game path: {0}")]
    RelativeWinePath(PathBuf),
    #[error("RetroArch cannot quote a path containing a double quote: {0}")]
    UnquotablePath(PathBuf),
    #[error("Wine cannot address a non-Unicode game path: {0}")]
    NonUnicodeWinePath(PathBuf),
    #[error("failed to start the backend: {0}")]
    Io(#[from] io::Error),
}

impl LaunchPlan {
    fn new(program: impl Into<OsString>, current_dir: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: current_dir.into(),
            env: BTreeMap::new(),
            generated_files: Vec::new(),
            generated_directories: Vec::new(),
            settings: Vec::new(),
            log_file: None,
        }
    }

    fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.args.push(argument.into());
        self
    }

    /// A Windows program started through Wine in RetroPort's prefix.
    fn under_wine(
        linux_data_dir: Option<&Path>,
        program: &Path,
        current_dir: impl Into<PathBuf>,
    ) -> Result<Self, LaunchError> {
        let prefix = wine_prefix(linux_data_dir)?;
        let mut plan = Self::new("wine", current_dir).arg(program);
        plan.env
            .insert("WINEPREFIX".to_owned(), prefix.into_os_string());
        Ok(plan)
    }

    pub fn for_host(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
    ) -> Result<Self, LaunchError> {
        match host {
            HostPlatform::Windows => Ok(Self::new(
                layout.retrobat_executable(),
                layout.retrobat_root(),
            )),
            HostPlatform::Linux => Self::under_wine(
                linux_data_dir,
                &layout.retrobat_executable(),
                layout.retrobat_root(),
            ),
            HostPlatform::Unsupported => Err(LaunchError::Unsupported),
        }
    }

    pub fn for_current_host(layout: &PortableLayout) -> Result<Self, LaunchError> {
        let data = dirs::data_local_dir();
        Self::for_host(layout, HostPlatform::current(), data.as_deref())
    }

    pub fn for_game_host(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
        system: &str,
        rom: &Path,
    ) -> Result<Self, LaunchError> {
        Self::for_game_host_with_backend(layout, host, linux_data_dir, system, rom, None)
    }

    pub fn for_game_host_with_backend(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
        system: &str,
        rom: &Path,
        backend: Option<&BackendRoute>,
    ) -> Result<Self, LaunchError> {
        if system.is_empty()
            || !system.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
            })
        {
            return Err(LaunchError::InvalidSystem(system.to_owned()));
        }
        if system == "chip8" {
            return Self::for_retroarch_core(layout, host, linux_data_dir, system, "jaxe", rom);
        }
        if host == HostPlatform::Linux
            && let Some(BackendRoute {
                emulator,
                core: Some(core),
                ..
            }) = backend
            && emulator.eq_ignore_ascii_case("libretro")
        {
            // EmulatorLauncher is a .NET Framework/WinForms program. Several
            // current Wine-Mono combinations abort before spawning RetroArch
            // (gmisc-win32 filename assertion). Libretro has a complete,
            // stable direct command line, so Linux card launches bypass that
            // unnecessary process without bypassing the selected core.
            return Self::for_retroarch_core(layout, host, linux_data_dir, system, core, rom);
        }
        if host == HostPlatform::Linux
            && backend.is_some_and(|route| route.emulator.eq_ignore_ascii_case("windows"))
            && rom
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            return Self::under_wine(
                linux_data_dir,
                rom,
                rom.parent().unwrap_or_else(|| Path::new(".")),
            );
        }
        if host == HostPlatform::Linux
            && let Some(backend) = backend
            && let Some(plan) = native_linux_game_plan(layout, backend, rom)
        {
            return Ok(plan);
        }

        let launcher = layout.emulator_launcher_executable();
        let rom_argument = match host {
            HostPlatform::Windows => rom.to_owned(),
            HostPlatform::Linux => wine_path(rom)?,
            HostPlatform::Unsupported => return Err(LaunchError::Unsupported),
        };
        let mut plan = match host {
            HostPlatform::Windows => Self::new(launcher, layout.emulationstation_root()),
            _ => Self::under_wine(linux_data_dir, &launcher, layout.emulationstation_root())?,
        };
        plan = plan.arg("-system").arg(system);
        if let Some(backend) = backend {
            plan = plan.arg("-emulator").arg(&backend.emulator);
            if let Some(core) = &backend.core {
                plan = plan.arg("-core").arg(core);
            }
        }
        Ok(plan.arg("-rom").arg(rom_argument))
    }

    fn for_retroarch_core(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
        system: &str,
        core_name: &str,
        rom: &Path,
    ) -> Result<Self, LaunchError> {
        let retroarch = layout.retroarch_executable();
        let core = layout.retroarch_core(core_name);
        let retrobat = layout.retrobat_root();
        let save = retrobat.join("saves").join(system);
        let state = save.join("states");
        let runtime = layout.metadata_root().join("runtime").join("retroarch");
        let append_config = runtime.join(format!("{system}.cfg"));
        let log_file = retroarch_log_path(layout, system);
        // RetroArch's shared retroarch.cfg is rewritten by EmulatorLauncher
        // and by RetroArch itself, so it can hold absolute paths from another
        // machine, drive letter, or installation. Every location-dependent
        // setting a direct launch relies on is therefore pinned here, to this
        // installation. RetroArch ignores a missing directory, so each one is
        // also created before launch.
        let directories = [
            ("system_directory", retrobat.join("bios")),
            ("savefile_directory", save.clone()),
            ("savestate_directory", state.clone()),
            ("screenshot_directory", retrobat.join("screenshots")),
            (
                "cheat_database_path",
                retrobat.join("cheats").join("retroarch"),
            ),
            (
                "recording_output_directory",
                retrobat.join("records").join("output"),
            ),
            (
                "recording_config_directory",
                retrobat.join("records").join("config"),
            ),
            ("rgui_browser_directory", retrobat.join("roms")),
            (
                "cache_directory",
                layout.metadata_root().join("cache").join("retroarch"),
            ),
        ];
        let host_path = |path: &Path| -> Result<PathBuf, LaunchError> {
            match host {
                HostPlatform::Windows => Ok(path.to_owned()),
                HostPlatform::Linux => wine_path(path),
                HostPlatform::Unsupported => Err(LaunchError::Unsupported),
            }
        };
        let config_value = |path: &Path| -> Result<String, LaunchError> {
            let value = retroarch_config_path(&host_path(path)?);
            if value.contains('"') {
                return Err(LaunchError::UnquotablePath(path.to_owned()));
            }
            Ok(value)
        };
        let mut config = String::new();
        for (key, directory) in &directories {
            config.push_str(&format!("{key} = \"{}\"\n", config_value(directory)?));
        }
        let mut generated_files = Vec::new();
        config.push_str(concat!(
            // The overlay in the shared configuration belongs to whichever
            // system EmulatorLauncher prepared last and names a file on that
            // machine; a direct launch never inherits it.
            "input_overlay_enable = \"false\"\n",
            "input_overlay = \"\"\n",
            // Likewise the shared display geometry is that machine's: a custom
            // viewport sized for its bezel (aspect_ratio_index 23) and a fixed
            // fullscreen mode crop the game on any other display. Use the
            // core's aspect ratio, centred, at this desktop's resolution.
            "aspect_ratio_index = \"22\"\n",
            "video_aspect_ratio_auto = \"true\"\n",
            "video_viewport_bias_x = \"0.500000\"\n",
            "video_viewport_bias_y = \"0.500000\"\n",
            "video_fullscreen = \"true\"\n",
            "video_windowed_fullscreen = \"true\"\n",
            "video_fullscreen_x = \"0\"\n",
            "video_fullscreen_y = \"0\"\n",
            // Rewind snapshots every frame; inherited from another system's
            // session it costs performance and makes cores such as DOSBox Pure
            // report save-state errors over the game.
            "rewind_enable = \"false\"\n",
            "config_save_on_exit = \"false\"\n",
            "audio_enable = \"true\"\n",
            "audio_driver = \"xaudio\"\n",
            "audio_mute_enable = \"false\"\n",
            "audio_mixer_mute_enable = \"false\"\n",
            "audio_volume = \"0.000000\"\n",
            "input_autodetect_enable = \"true\"\n",
            "input_joypad_driver = \"sdl2\"\n",
        ));
        for (key, value) in retroarch_input_overrides(system) {
            config.push_str(&format!("{key} = \"{value}\"\n"));
        }
        generated_files.insert(0, (append_config.clone(), config));

        let mut plan = match host {
            HostPlatform::Windows => Self::new(retroarch, layout.retroarch_root()),
            HostPlatform::Linux => {
                Self::under_wine(linux_data_dir, &retroarch, layout.retroarch_root())?
            }
            HostPlatform::Unsupported => return Err(LaunchError::Unsupported),
        };
        plan = plan
            .arg("--verbose")
            .arg("--log-file")
            .arg(host_path(&log_file)?)
            .arg("--appendconfig")
            .arg(host_path(&append_config)?)
            .arg("-L")
            .arg(host_path(&core)?)
            .arg(host_path(rom)?);
        plan.generated_files = generated_files;
        plan.generated_directories = directories
            .into_iter()
            .map(|(_, directory)| directory)
            .collect();
        plan.log_file = Some(log_file);
        Ok(plan)
    }

    pub fn for_current_game(
        layout: &PortableLayout,
        system: &str,
        rom: &Path,
    ) -> Result<Self, LaunchError> {
        let data = dirs::data_local_dir();
        Self::for_game_host(
            layout,
            HostPlatform::current(),
            data.as_deref(),
            system,
            rom,
        )
    }

    pub fn for_current_game_with_backend(
        layout: &PortableLayout,
        system: &str,
        rom: &Path,
        backend: Option<&BackendRoute>,
    ) -> Result<Self, LaunchError> {
        let data = dirs::data_local_dir();
        Self::for_game_host_with_backend(
            layout,
            HostPlatform::current(),
            data.as_deref(),
            system,
            rom,
            backend,
        )
    }

    pub fn for_rpcs3_firmware_install_host(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
        firmware: &Path,
    ) -> Result<Self, LaunchError> {
        let rpcs3 = layout.rpcs3_executable();
        let native = layout.linux_runtime_root().join("RPCS3.AppImage");
        match host {
            HostPlatform::Windows => Ok(Self::new(rpcs3, layout.emulator_root("rpcs3"))
                .arg("--installfw")
                .arg(firmware)),
            HostPlatform::Linux if native.is_file() => Ok(native_linux_plan(
                layout,
                native,
                vec!["--installfw".into(), firmware.into()],
                "rpcs3",
            )),
            HostPlatform::Linux => {
                Ok(
                    Self::under_wine(linux_data_dir, &rpcs3, layout.emulator_root("rpcs3"))?
                        .arg("--installfw")
                        .arg(wine_path(firmware)?),
                )
            }
            HostPlatform::Unsupported => Err(LaunchError::Unsupported),
        }
    }

    pub fn for_current_rpcs3_firmware_install(
        layout: &PortableLayout,
        firmware: &Path,
    ) -> Result<Self, LaunchError> {
        let data = dirs::data_local_dir();
        Self::for_rpcs3_firmware_install_host(
            layout,
            HostPlatform::current(),
            data.as_deref(),
            firmware,
        )
    }

    pub fn spawn(&self) -> Result<Child, LaunchError> {
        self.prepare_runtime()?;

        let mut command = Command::new(&self.program);
        command.args(&self.args).current_dir(&self.current_dir);
        #[cfg(unix)]
        command.process_group(0);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        Ok(command.spawn()?)
    }

    fn prepare_runtime(&self) -> Result<(), LaunchError> {
        if let Some(prefix) = self.env.get("WINEPREFIX") {
            fs::create_dir_all(prefix)?;
        }
        for key in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"] {
            if let Some(directory) = self.env.get(key) {
                fs::create_dir_all(directory)?;
            }
        }
        for directory in &self.generated_directories {
            fs::create_dir_all(directory)?;
        }
        for (path, contents) in &self.generated_files {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, contents)?;
        }
        for setting in &self.settings {
            apply_runtime_setting(setting)?;
        }
        if let Some(log) = &self.log_file {
            if let Some(parent) = log.parent() {
                fs::create_dir_all(parent)?;
            }
            // A previous launch's log must never be mistaken for evidence
            // about this one.
            match fs::remove_file(log) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

/// Player-one input RetroPort pins on every direct RetroArch launch of
/// `system` (a RetroBat system name). CONTROLS reads the same list, so what
/// it shows is what PLAY applies.
pub fn retroarch_input_overrides(system: &str) -> Vec<(&'static str, &'static str)> {
    // SDL2 GameController indices: A/B/X/Y are 0/1/2/3 by position, Back 4,
    // Start 6, D-pad 11-14. RetroPad B is the bottom face button.
    let mut overrides = vec![
        ("input_player1_joypad_index", "0"),
        ("input_player1_analog_dpad_mode", "1"),
        ("input_player1_b_btn", "0"),
        ("input_player1_a_btn", "1"),
        ("input_player1_y_btn", "2"),
        ("input_player1_x_btn", "3"),
        ("input_player1_select_btn", "4"),
        ("input_player1_start_btn", "6"),
        ("input_player1_up_btn", "11"),
        ("input_player1_down_btn", "12"),
        ("input_player1_left_btn", "13"),
        ("input_player1_right_btn", "14"),
    ];
    if system == "mame" {
        // MAME's own coin and start keys, and arrow keys for the joystick.
        overrides.extend([
            ("input_player1_select", "num5"),
            ("input_player1_start", "num1"),
            ("input_player1_up", "up"),
            ("input_player1_down", "down"),
            ("input_player1_left", "left"),
            ("input_player1_right", "right"),
        ]);
    }
    overrides
}

/// Whether PLAY starts this system's games through RetroArch directly on
/// this host (Linux libretro routes, and CHIP-8 everywhere).
pub fn uses_direct_retroarch(host: HostPlatform, system: &str, emulator: Option<&str>) -> bool {
    system == "chip8"
        || (host == HostPlatform::Linux
            && emulator.is_some_and(|emulator| emulator.eq_ignore_ascii_case("libretro")))
}

/// Where a direct RetroArch launch for `system` writes its verbose log.
pub fn retroarch_log_path(layout: &PortableLayout, system: &str) -> PathBuf {
    layout
        .metadata_root()
        .join("logs")
        .join(format!("retroarch-{system}.log"))
}

/// Linux keeps only Wine's symlink-heavy prefix outside the installation.
pub fn wine_prefix(linux_data_dir: Option<&Path>) -> Result<PathBuf, LaunchError> {
    Ok(linux_data_dir
        .ok_or(LaunchError::NoDataDirectory)?
        .join("retrobat-portable")
        .join("wine-prefix"))
}

fn apply_runtime_setting(setting: &RuntimeSetting) -> io::Result<()> {
    match setting {
        RuntimeSetting::SeedFile { path, contents } => {
            if path.exists() {
                return Ok(());
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, contents)
        }
        RuntimeSetting::IniValue {
            path,
            section,
            key,
            value,
        } => {
            let existing = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error),
            };
            let updated = set_ini_value(&existing, section, key, value);
            if updated != existing {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, updated)?;
            }
            Ok(())
        }
    }
}

fn set_ini_value(contents: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let mut lines = contents.lines().map(str::to_owned).collect::<Vec<_>>();
    let start = lines.iter().position(|line| line.trim() == header);
    let Some(start) = start else {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(header);
        lines.push(format!("{key}={value}"));
        return lines.join("\n") + "\n";
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |offset| start + 1 + offset);
    match lines[start + 1..end].iter().position(|line| {
        line.split_once('=')
            .is_some_and(|(name, _)| name.trim() == key)
    }) {
        Some(offset) => lines[start + 1 + offset] = format!("{key}={value}"),
        None => {
            let mut insert = end;
            while insert > start + 1 && lines[insert - 1].trim().is_empty() {
                insert -= 1;
            }
            lines.insert(insert, format!("{key}={value}"));
        }
    }
    lines.join("\n") + "\n"
}

/// A helper process that must never open a console window on Windows.
pub(crate) fn background_command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

pub fn terminate_process_tree(child: &mut Child, force: bool) -> Result<(), LaunchError> {
    terminate_process_tree_id(child.id(), force)
}

pub fn terminate_process_tree_id(process_id: u32, force: bool) -> Result<(), LaunchError> {
    #[cfg(unix)]
    {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        let process_group = -(process_id as i32);
        // SAFETY: kill is called with a process-group id created by
        // CommandExt::process_group(0). No pointers cross the FFI boundary.
        let result = unsafe { libc::kill(process_group, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(LaunchError::Io(error))
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = background_command("taskkill");
        command.args(["/PID", &process_id.to_string(), "/T"]);
        if force {
            command.arg("/F");
        }
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(LaunchError::Io(io::Error::other(format!(
                "taskkill exited with {status}"
            ))))
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = force;
        Err(LaunchError::Unsupported)
    }
}

pub fn process_tree_is_running(process_id: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 performs existence/permission checking only and
        // receives no pointer arguments.
        (unsafe { libc::kill(-(process_id as i32), 0) } == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = process_id;
        false
    }
}

fn retroarch_config_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn native_linux_game_plan(
    layout: &PortableLayout,
    backend: &BackendRoute,
    rom: &Path,
) -> Option<LaunchPlan> {
    let runtime = layout.linux_runtime_root();
    let emulator = backend.emulator.to_ascii_lowercase();
    let (program, args, key): (PathBuf, Vec<OsString>, &str) = match emulator.as_str() {
        "eden" => (
            runtime.join("Eden.AppImage"),
            vec!["-f".into(), "-g".into(), rom.into()],
            "eden",
        ),
        "cemu" => (
            runtime.join("Cemu.AppImage"),
            vec!["-g".into(), rom.into(), "-f".into()],
            "cemu",
        ),
        "rpcs3" => (
            runtime.join("RPCS3.AppImage"),
            vec!["--no-gui".into(), "--fullscreen".into(), rom.into()],
            "rpcs3",
        ),
        "shadps4" => (
            runtime.join("shadPS4/Shadps4-sdl.AppImage"),
            vec![rom.into()],
            "shadps4",
        ),
        "xenia-canary" => (
            runtime.join("XeniaCanary.AppImage"),
            vec![rom.into()],
            "xenia-canary",
        ),
        _ => return None,
    };
    if !program.is_file() {
        return None;
    }
    Some(native_linux_plan(layout, program, args, key))
}

fn native_linux_plan(
    layout: &PortableLayout,
    program: PathBuf,
    args: Vec<OsString>,
    emulator: &str,
) -> LaunchPlan {
    let root = layout
        .metadata_root()
        .join("runtime")
        .join("linux")
        .join(emulator);
    let config = root.join("config");
    let mut plan = LaunchPlan::new(program, layout.linux_runtime_root());
    plan.args = args;
    plan.env = BTreeMap::from([
        (
            "XDG_CONFIG_HOME".to_owned(),
            config.clone().into_os_string(),
        ),
        (
            "XDG_DATA_HOME".to_owned(),
            root.join("data").into_os_string(),
        ),
        (
            "XDG_CACHE_HOME".to_owned(),
            root.join("cache").into_os_string(),
        ),
        // Some bundled AppImages carry a self-updater that offers to replace
        // the AppImage with an unpinned download. These runtimes are pinned
        // and verified by SHA256SUMS, so updates belong to the bootstrap.
        ("DISABLE_AUTO_UPDATES".to_owned(), "1".into()),
    ]);
    plan.settings = match emulator {
        // Without a settings file Cemu opens its first-start assistant
        // instead of the requested game.
        "cemu" => vec![RuntimeSetting::SeedFile {
            path: config.join("Cemu").join("settings.xml"),
            contents: concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
                "<content>\n",
                "    <check_update>false</check_update>\n",
                "</content>\n"
            )
            .to_owned(),
        }],
        // RPCS3's welcome dialog and its update check both interrupt an
        // unattended start (the firmware installer runs its GUI).
        "rpcs3" => {
            let gui = config
                .join("rpcs3")
                .join("GuiConfigs")
                .join("CurrentSettings.ini");
            vec![
                RuntimeSetting::IniValue {
                    path: gui.clone(),
                    section: "main_window".to_owned(),
                    key: "infoBoxEnabledWelcome".to_owned(),
                    value: "false".to_owned(),
                },
                RuntimeSetting::IniValue {
                    path: gui,
                    section: "Meta".to_owned(),
                    key: "checkUpdateStart".to_owned(),
                    value: "false".to_owned(),
                },
            ]
        }
        _ => Vec::new(),
    };
    plan
}

fn wine_path(path: &Path) -> Result<PathBuf, LaunchError> {
    if !path.is_absolute() {
        return Err(LaunchError::RelativeWinePath(path.to_owned()));
    }
    let Some(path_text) = path.to_str() else {
        return Err(LaunchError::NonUnicodeWinePath(path.to_owned()));
    };
    Ok(PathBuf::from(format!("Z:{}", path_text.replace('/', "\\"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wine_addresses_host_paths_through_the_z_drive() {
        assert_eq!(
            wine_path(Path::new("/opt/retroport/RetroBat/roms/gb/2048.gb")).unwrap(),
            PathBuf::from(r"Z:\opt\retroport\RetroBat\roms\gb\2048.gb")
        );
        assert!(matches!(
            wine_path(Path::new("relative/game.gb")),
            Err(LaunchError::RelativeWinePath(_))
        ));
    }

    #[test]
    fn game_launch_rejects_a_system_that_could_be_parsed_as_arguments() {
        let result = LaunchPlan::for_game_host(
            &PortableLayout::new("/opt/retroport"),
            HostPlatform::Linux,
            Some(Path::new("/home/user/.local/share")),
            "../gb",
            Path::new("/tmp/game.gb"),
        );
        assert!(matches!(result, Err(LaunchError::InvalidSystem(_))));
    }

    #[test]
    fn ini_values_are_set_without_disturbing_other_settings() {
        let original =
            "[Meta]\nattachCommandLine=false\n\n[main_window]\nlastExplorePathPUP=/media\n";
        let updated = set_ini_value(original, "main_window", "infoBoxEnabledWelcome", "false");
        let updated = set_ini_value(&updated, "Meta", "checkUpdateStart", "false");
        let updated = set_ini_value(&updated, "Meta", "attachCommandLine", "true");
        assert_eq!(
            updated,
            "[Meta]\nattachCommandLine=true\ncheckUpdateStart=false\n\n[main_window]\nlastExplorePathPUP=/media\ninfoBoxEnabledWelcome=false\n"
        );
        assert_eq!(
            set_ini_value("", "Meta", "checkUpdateStart", "false"),
            "[Meta]\ncheckUpdateStart=false\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminate_kills_the_entire_spawned_process_group() {
        let mut child = LaunchPlan::new("sh", "/tmp")
            .arg("-c")
            .arg("sleep 30 & wait")
            .spawn()
            .unwrap();
        let process_id = child.id();
        assert!(process_tree_is_running(process_id));

        terminate_process_tree(&mut child, false).unwrap();
        for _ in 0..100 {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(child.try_wait().unwrap().is_some());
        assert!(!process_tree_is_running(process_id));
    }
}
