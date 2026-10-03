#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use retrobat_portable::artwork::{load_bundled_artwork, load_or_fetch, load_snapshot_artwork};
use retrobat_portable::browse::{Acquisition, BrowseCatalog, BrowseEntry, BundledArtwork};
use retrobat_portable::browse_install::{BrowseInstaller, download_route};
use retrobat_portable::catalog::{Artwork, Catalog, CatalogEntry};
use retrobat_portable::controls::{ControlsCatalog, GameControls};
use retrobat_portable::downloads::PinnedDownload;
use retrobat_portable::featured::FeaturedCatalog;
use retrobat_portable::firmware::{
    FirmwareFolderReport, FirmwareRecord, Recognition, firmware_record, import_firmware,
    import_firmware_folder, install_official_firmware,
};
use retrobat_portable::import::{
    GameImporter, ImportedManifest, imported_manifests, remove_import,
};
use retrobat_portable::install::{Installer, ReqwestDownloader, is_installed};
use retrobat_portable::launch::LaunchPlan;
use retrobat_portable::paths::PortableLayout;
use retrobat_portable::readiness::{
    BackendState, FirmwareFileStatus, FirmwareInstallAction, FirmwareState, ReadinessReport,
    SystemReadiness,
};
use retrobat_portable::session::{GameSession, SessionEvent, SessionPhase};

// The catalogues are tens of thousands of small strings; mimalloc parses
// them noticeably faster than the system allocator.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const INPUT_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(7, 9, 13);
const CONTROL_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(36, 43, 58);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(104, 146, 255);

/// The Windows build is a GUI program, which Windows starts without a
/// console. Command-line use (self-check, import, probes) attaches to the
/// console it was started from so its output and errors are visible.
#[cfg(target_os = "windows")]
fn attach_parent_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // SAFETY: AttachConsole takes no pointers; failure (no parent console,
    // or output already redirected) leaves the process unchanged.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

const USAGE: &str = "\
Usage: RetroPort [--bundle-root FOLDER] [ACTION]

Without an action, RetroPort opens its library.

Actions:
  --download ID           Download a card's game from its pinned source, verify it, and install it
  --import ID --file PATH Import your own game file, archive, or folder onto a card
  --remove ID             Remove what DOWNLOAD or IMPORT placed for a card, keeping modified files
  --import-firmware PATH  Recognise every BIOS/firmware file in PATH (a file or a folder, searched
                          recursively) by fingerprint and place each where its emulators look
  --install-firmware SYS  Download a system's firmware from its maker, verify it, and install it
                          (ps3, psvita: Sony's official system software)
  --self-check            Validate the installation and print the report as JSON
                          (--self-check-output FILE also writes it to FILE)
  --gameplay-probe ID     Play an installed card through PLAY, hold it, terminate it, and record
                          every transition (--gameplay-probe-output FILE is required;
                          --gameplay-probe-seconds N, at least 10, defaults to 20)
  --install ID            Install an entry of the trusted catalogue
  --uninstall ID          Remove an entry of the trusted catalogue, keeping modified files

Options:
  --bundle-root FOLDER    The portable folder to use (default: the one holding this program)
  --startup-probe-output FILE
                          Record the library's startup timings to FILE
  -h, --help              Show this help

A card's ID appears when you hover over its title, for example homebrew-hub/dango-dash.
";

fn describe_firmware_folder(report: &FirmwareFolderReport) -> String {
    let mut text = String::new();
    for (source, target, recognition) in &report.placed {
        let how = match recognition {
            Recognition::Fingerprint => "fingerprint",
            Recognition::Structure => "file structure",
            Recognition::Name => "file name",
        };
        text.push_str(&format!(
            "Placed bios/{target} from {} (recognised by {how}).\n",
            source.display()
        ));
    }
    for target in &report.kept_existing {
        text.push_str(&format!(
            "Kept your existing bios/{target}; a different file matched it.\n"
        ));
    }
    if report
        .placed
        .iter()
        .any(|(_, target, _)| target == "PS3UPDAT.PUP")
    {
        text.push_str(
            "The PS3 system software is in place; INSTALL FIRMWARE on a PS3 card (or \
             --install-firmware ps3) installs it into RPCS3.\n",
        );
    }
    text.push_str(&format!(
        "Examined {} file(s): placed {}, already present {}, unrecognised {}.\n",
        report.examined,
        report.placed.len(),
        report.already_present,
        report.unrecognised
    ));
    text
}

/// Installs a system's firmware from its maker's own download.
fn install_firmware_cli(layout: &PortableLayout, system: &str) -> i32 {
    let browse = match BrowseCatalog::built_in() {
        Ok(browse) => browse,
        Err(error) => {
            eprintln!("Browse catalog rejected: {error}");
            return 1;
        }
    };
    let report = match ReadinessReport::audit(layout, &browse.entries) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("Readiness audit failed: {error}");
            return 1;
        }
    };
    let downloadable = report
        .for_catalog_system(system)
        .map(|readiness| {
            readiness
                .firmware_files
                .iter()
                .filter(|file| file.download.is_some())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if downloadable.is_empty() {
        eprintln!(
            "{system} has no firmware its maker publishes for download; its card names the file \
             and how to obtain it, and --import-firmware places it."
        );
        return 2;
    }
    for firmware in downloadable {
        if firmware.present {
            println!("bios/{} is already installed.", firmware.relative_path);
            continue;
        }
        let status = install_one_firmware(layout, &firmware);
        if status != 0 {
            return status;
        }
    }
    0
}

fn install_one_firmware(layout: &PortableLayout, firmware: &FirmwareFileStatus) -> i32 {
    let download = firmware.download.clone().unwrap();
    let installed = ReqwestDownloader::new()
        .map_err(|error| error.to_string())
        .and_then(|downloader| {
            install_official_firmware(layout, firmware, &downloader)
                .map_err(|error| error.to_string())
        });
    let report = match installed {
        Ok(report) => report,
        Err(error) => {
            eprintln!("Firmware installation failed safely: {error}");
            return 1;
        }
    };
    println!(
        "Verified {} bytes from {} at {}.",
        report.bytes,
        download.publisher,
        report.destination.display()
    );
    let (emulator, plan) = match download.install_action {
        FirmwareInstallAction::PlaceInBios => return 0,
        FirmwareInstallAction::Rpcs3 => (
            "RPCS3",
            LaunchPlan::for_current_rpcs3_firmware_install(layout, &report.destination),
        ),
        FirmwareInstallAction::Vita3k => (
            "Vita3K",
            LaunchPlan::for_current_vita3k_firmware_install(layout, &report.destination),
        ),
    };
    println!("Installing it into {emulator}…");
    let status = plan
        .and_then(|plan| plan.spawn(&|phase| println!("{phase}")))
        .and_then(|mut child| child.wait().map_err(Into::into));
    match status {
        Ok(status) if status.success() => {
            println!("{emulator} installed bios/{}.", firmware.relative_path);
            0
        }
        Ok(status) => {
            eprintln!("{emulator}'s firmware installer exited with {status}.");
            1
        }
        Err(error) => {
            eprintln!("Could not run {emulator}'s firmware installer: {error}");
            1
        }
    }
}

fn usage_error(message: &str) -> ! {
    eprintln!("{message}\nRun RetroPort --help for usage.");
    std::process::exit(2);
}

fn flag_path(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
    what: &str,
) -> PathBuf {
    args.next()
        .map(PathBuf::from)
        .unwrap_or_else(|| usage_error(&format!("{flag} requires {what}")))
}

fn flag_id(args: &mut impl Iterator<Item = std::ffi::OsString>, flag: &str) -> String {
    args.next()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| usage_error(&format!("{flag} requires a catalogue id")))
}

fn main() -> eframe::Result {
    #[cfg(target_os = "windows")]
    if std::env::args_os().len() > 1 {
        attach_parent_console();
    }
    let mut bundle_root = std::env::current_exe()
        .ok()
        .map(|path| PortableLayout::discover(&path).root)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut self_check_only = false;
    let mut self_check_output = None;
    let mut install_id = None;
    let mut uninstall_id = None;
    let mut download_id = None;
    let mut import_id = None;
    let mut remove_id = None;
    let mut import_file = None;
    let mut startup_probe_output = None;
    let mut gameplay_probe_id = None;
    let mut gameplay_probe_output = None;
    let mut gameplay_probe_seconds = 20u64;
    let mut firmware_source: Option<PathBuf> = None;
    let mut firmware_system: Option<String> = None;
    // Paths are taken as the OS gives them, so a name that is not valid
    // UTF-8 still reaches the filesystem intact.
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        let Some(flag) = argument.to_str() else {
            usage_error(&format!("Unknown argument: {}", argument.to_string_lossy()));
        };
        match flag {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--bundle-root" => bundle_root = flag_path(&mut args, flag, "a folder"),
            "--self-check" => self_check_only = true,
            "--install" => install_id = Some(flag_id(&mut args, flag)),
            "--uninstall" => uninstall_id = Some(flag_id(&mut args, flag)),
            "--download" => download_id = Some(flag_id(&mut args, flag)),
            "--import" => import_id = Some(flag_id(&mut args, flag)),
            "--remove" => remove_id = Some(flag_id(&mut args, flag)),
            "--import-firmware" => {
                firmware_source = Some(flag_path(&mut args, flag, "a BIOS file or folder"));
            }
            "--install-firmware" => firmware_system = Some(flag_id(&mut args, flag)),
            "--file" => import_file = Some(flag_path(&mut args, flag, "a local game path")),
            "--self-check-output" => {
                self_check_output = Some(flag_path(&mut args, flag, "a path"));
            }
            "--startup-probe-output" => {
                startup_probe_output = Some(flag_path(&mut args, flag, "a path"));
            }
            "--gameplay-probe" => gameplay_probe_id = Some(flag_id(&mut args, flag)),
            "--gameplay-probe-output" => {
                gameplay_probe_output = Some(flag_path(&mut args, flag, "a path"));
            }
            "--gameplay-probe-seconds" => {
                gameplay_probe_seconds = args
                    .next()
                    .and_then(|value| value.to_str()?.parse().ok())
                    .filter(|seconds| *seconds >= 10)
                    .unwrap_or_else(|| {
                        usage_error("--gameplay-probe-seconds requires an integer of at least 10")
                    });
            }
            other => usage_error(&format!("Unknown argument: {other}")),
        }
    }
    let actions = [
        self_check_only,
        install_id.is_some(),
        uninstall_id.is_some(),
        download_id.is_some(),
        import_id.is_some(),
        remove_id.is_some(),
        gameplay_probe_id.is_some(),
        firmware_source.is_some(),
        firmware_system.is_some(),
    ];
    if actions.into_iter().filter(|chosen| *chosen).count() > 1 {
        usage_error(
            "Choose one action: --self-check, --download, --import, --remove, \
             --import-firmware, --install-firmware, --gameplay-probe, --install, or --uninstall.",
        );
    }

    let gameplay_probe = gameplay_probe_id.map(|catalog_id| GameplayProbeConfig {
        catalog_id,
        output: gameplay_probe_output.unwrap_or_else(|| {
            eprintln!("--gameplay-probe requires --gameplay-probe-output");
            std::process::exit(2);
        }),
        duration: Duration::from_secs(gameplay_probe_seconds),
    });

    let layout = PortableLayout::new(bundle_root);
    if self_check_only {
        match retrobat_portable::self_check(&layout) {
            Ok(report) => {
                let json = serde_json::to_string_pretty(&report).unwrap();
                if let Some(path) = self_check_output
                    && let Err(error) = std::fs::write(&path, format!("{json}\n"))
                {
                    eprintln!(
                        "Could not write self-check report to {}: {error}",
                        path.display()
                    );
                    std::process::exit(1);
                }
                println!("{json}");
                return Ok(());
            }
            Err(error) => {
                eprintln!("Self-check failed: {error}");
                std::process::exit(1);
            }
        }
    }
    if install_id.is_some() || uninstall_id.is_some() {
        let catalog = Catalog::built_in().unwrap_or_else(|error| {
            eprintln!("Catalog rejected: {error}");
            std::process::exit(1);
        });
        let requested_id = install_id.as_ref().or(uninstall_id.as_ref()).unwrap();
        let entry = catalog
            .entries
            .iter()
            .find(|entry| &entry.id == requested_id)
            .unwrap_or_else(|| {
                eprintln!("Unknown catalog id: {requested_id}");
                std::process::exit(2);
            });
        let downloader = ReqwestDownloader::new().unwrap_or_else(|error| {
            eprintln!("Could not initialize downloader: {error}");
            std::process::exit(1);
        });
        let installer = Installer::new(&layout, &downloader);
        if install_id.is_some() {
            match installer.install(entry) {
                Ok(report) => println!(
                    "Installed {} bytes at {} (SHA-256 {}).",
                    report.bytes,
                    report.destination.display(),
                    report.sha256
                ),
                Err(error) => {
                    eprintln!("Install failed safely: {error}");
                    std::process::exit(1);
                }
            }
        } else {
            match installer.uninstall(entry) {
                Ok(report) => println!(
                    "Removed {} file(s); preserved {} modified file(s).",
                    report.removed.len(),
                    report.preserved_modified.len()
                ),
                Err(error) => {
                    eprintln!("Uninstall failed safely: {error}");
                    std::process::exit(1);
                }
            }
        }
        return Ok(());
    }
    if let Some(requested_id) = import_id {
        let Some(source) = import_file else {
            eprintln!("--import requires --file <local-game-path>");
            std::process::exit(2);
        };
        let browse = BrowseCatalog::built_in().unwrap_or_else(|error| {
            eprintln!("Browse catalog rejected: {error}");
            std::process::exit(1);
        });
        let entry = browse
            .entries
            .iter()
            .find(|entry| entry.id == requested_id)
            .unwrap_or_else(|| {
                eprintln!("Unknown browse catalog id: {requested_id}");
                std::process::exit(2);
            });
        let result = GameImporter::new(&layout).import(entry, &source);
        match result {
            Ok(report) => println!(
                "Imported {} file(s), {} bytes into {}. Launch file: {}",
                report.imported_files,
                report.imported_bytes,
                report.system,
                report.launch_file.display()
            ),
            Err(error) => {
                eprintln!("Import failed safely: {error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    if let Some(source) = firmware_source {
        match import_firmware_folder(&layout, &source) {
            Ok(report) => {
                print!("{}", describe_firmware_folder(&report));
                return Ok(());
            }
            Err(error) => {
                eprintln!("Firmware import failed safely: {error}");
                std::process::exit(1);
            }
        }
    }
    if let Some(system) = firmware_system {
        std::process::exit(install_firmware_cli(&layout, &system));
    }
    if let Some(requested_id) = remove_id {
        match remove_import(&layout, &requested_id) {
            Ok(report) => println!(
                "Removed {} owned file(s); preserved {} modified file(s); {} already absent.",
                report.removed.len(),
                report.preserved_modified.len(),
                report.already_missing.len()
            ),
            Err(error) => {
                eprintln!("Remove failed safely: {error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    if let Some(requested_id) = download_id {
        let browse = BrowseCatalog::built_in().unwrap_or_else(|error| {
            eprintln!("Browse catalog rejected: {error}");
            std::process::exit(1);
        });
        let entry = browse
            .entries
            .iter()
            .find(|entry| entry.id == requested_id)
            .unwrap_or_else(|| {
                eprintln!("Unknown browse catalog id: {requested_id}");
                std::process::exit(2);
            });
        if ReadinessReport::audit(&layout, std::slice::from_ref(entry))
            .ok()
            .and_then(|report| report.for_catalog_system(&entry.system).cloned())
            .is_some_and(|system| system.backend == BackendState::EmulatorMissing)
        {
            eprintln!(
                "This installation has no emulator for {}, so the game could not be played; nothing was downloaded.",
                entry.system
            );
            std::process::exit(1);
        }
        let downloader = ReqwestDownloader::new().unwrap_or_else(|error| {
            eprintln!("Could not initialize downloader: {error}");
            std::process::exit(1);
        });
        match BrowseInstaller::new(&layout, &downloader).install(entry) {
            Ok(report) => println!(
                "Downloaded {} and imported {} file(s) at {}.",
                report.source_url,
                report.import.imported_files,
                report.import.launch_file.display()
            ),
            Err(error) => {
                eprintln!("Download failed safely: {error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 600.0])
            .with_min_inner_size([680.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RetroPort",
        options,
        Box::new(move |creation_context| {
            Ok(Box::new(PortableApp::new(
                layout,
                &creation_context.egui_ctx,
                startup_probe_output,
                gameplay_probe,
            )))
        }),
    )?;
    if GAMEPLAY_PROBE_FAILED.load(Ordering::SeqCst) {
        std::process::exit(1);
    }
    Ok(())
}

/// What a backend's own log says about a launch.
#[derive(serde::Serialize)]
struct BackendLogEvidence {
    path: String,
    core_loaded: bool,
    content_loaded: bool,
    errors: Vec<String>,
}

fn backend_log_evidence(path: &std::path::Path) -> BackendLogEvidence {
    let text = std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let errors = text
        .lines()
        .filter(|line| line.contains("[ERROR]"))
        .take(8)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    BackendLogEvidence {
        path: path.display().to_string(),
        core_loaded: text.contains("Loading dynamic libretro core from"),
        // RetroArch reports core geometry only after retro_load_game accepted
        // the content, including cores that load their content themselves.
        content_loaded: text.contains("[Core] Geometry:")
            && !text.contains("Failed to load content"),
        errors,
    }
}

struct ArtworkMessage {
    entry_id: String,
    result: ArtworkResult,
}

enum ArtworkResult {
    Decoded(DecodedArtwork),
    Failed(String),
    /// The page that asked for it is no longer shown.
    Skipped,
}

struct DecodedArtwork {
    size: [usize; 2],
    rgba: Vec<u8>,
}

#[derive(Clone)]
enum ArtworkSource {
    Snapshot(String),
    Verified(Artwork),
    Bundled(BundledArtwork),
}

struct ArtworkJob {
    entry_id: String,
    source: ArtworkSource,
    generation: u64,
}

// Network fetches mostly wait, so four workers keep remote covers flowing;
// disk reads and decoding are limited to two at a time because more
// simultaneous reads starved the desktop event loop on slow storage and
// tripped the compositor's "Application Not Responding" watchdog.
const ARTWORK_WORKERS: usize = 4;
const ARTWORK_LOCAL_PERMITS: usize = 2;
const ARTWORK_QUEUE_CAPACITY: usize = 64;

/// A counting semaphore for the local read/decode stage.
struct Permits {
    available: Mutex<usize>,
    released: std::sync::Condvar,
}

impl Permits {
    fn acquire(&self) -> PermitGuard<'_> {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while *available == 0 {
            available = self
                .released
                .wait(available)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        *available -= 1;
        PermitGuard(self)
    }
}

struct PermitGuard<'a>(&'a Permits);

impl Drop for PermitGuard<'_> {
    fn drop(&mut self) {
        *self
            .0
            .available
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) += 1;
        self.0.released.notify_one();
    }
}

fn start_artwork_workers(
    layout: PortableLayout,
    completed: mpsc::Sender<ArtworkMessage>,
    generation: Arc<AtomicU64>,
    notify: egui::Context,
) -> SyncSender<ArtworkJob> {
    let (jobs, receiver) = mpsc::sync_channel::<ArtworkJob>(ARTWORK_QUEUE_CAPACITY);
    let receiver = Arc::new(Mutex::new(receiver));
    let permits = Arc::new(Permits {
        available: Mutex::new(ARTWORK_LOCAL_PERMITS),
        released: std::sync::Condvar::new(),
    });
    for worker_index in 0..ARTWORK_WORKERS {
        let receiver = Arc::clone(&receiver);
        let completed = completed.clone();
        let layout = layout.clone();
        let generation = Arc::clone(&generation);
        let permits = Arc::clone(&permits);
        let notify = notify.clone();
        thread::Builder::new()
            .name(format!("artwork-{worker_index}"))
            .spawn(move || {
                let downloader = ReqwestDownloader::new().map_err(|error| error.to_string());
                loop {
                    let job = {
                        let Ok(receiver) = receiver.lock() else {
                            return;
                        };
                        receiver.recv()
                    };
                    let Ok(job) = job else {
                        return;
                    };
                    let result = if job.generation != generation.load(Ordering::SeqCst) {
                        ArtworkResult::Skipped
                    } else {
                        let bytes = match job.source {
                            ArtworkSource::Snapshot(url) => match &downloader {
                                Ok(downloader) => load_snapshot_artwork(&layout, &url, downloader)
                                    .map_err(|error| error.to_string()),
                                Err(error) => Err(error.clone()),
                            },
                            ArtworkSource::Verified(artwork) => match &downloader {
                                Ok(downloader) => load_or_fetch(&layout, &artwork, downloader)
                                    .map_err(|error| error.to_string()),
                                Err(error) => Err(error.clone()),
                            },
                            ArtworkSource::Bundled(artwork) => {
                                let _permit = permits.acquire();
                                load_bundled_artwork(&layout, &artwork)
                                    .map_err(|error| error.to_string())
                            }
                        };
                        match bytes.and_then(|bytes| {
                            let _permit = permits.acquire();
                            decode_artwork_for_texture(&bytes).map_err(|error| error.to_string())
                        }) {
                            Ok(decoded) => ArtworkResult::Decoded(decoded),
                            Err(error) => ArtworkResult::Failed(error),
                        }
                    };
                    if completed
                        .send(ArtworkMessage {
                            entry_id: job.entry_id,
                            result,
                        })
                        .is_err()
                    {
                        return;
                    }
                    notify.request_repaint();
                }
            })
            .expect("artwork worker thread must start");
    }
    jobs
}

/// The folder browser shared by the import and firmware dialogs. Listing a
/// folder touches the disk, so it happens once per folder, not per frame.
struct FileBrowser {
    directory: PathBuf,
    path_text: String,
    selected: Option<PathBuf>,
    listing: Vec<(bool, String, PathBuf)>,
    listed: Option<PathBuf>,
}

impl FileBrowser {
    fn new() -> Self {
        let directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            path_text: directory.display().to_string(),
            directory,
            selected: None,
            listing: Vec::new(),
            listed: None,
        }
    }

    fn open(&mut self, directory: PathBuf) {
        self.path_text = directory.display().to_string();
        self.directory = directory;
        self.selected = None;
    }

    fn entries(&mut self) -> Vec<(bool, String, PathBuf)> {
        if self.listed.as_ref() != Some(&self.directory) {
            self.listing = std::fs::read_dir(&self.directory)
                .map(|entries| {
                    let mut listing = entries
                        .filter_map(Result::ok)
                        .map(|entry| {
                            let path = entry.path();
                            let is_directory = entry.file_type().is_ok_and(|kind| {
                                kind.is_dir() || (kind.is_symlink() && path.is_dir())
                            });
                            (
                                is_directory,
                                entry.file_name().to_string_lossy().into_owned(),
                                path,
                            )
                        })
                        .collect::<Vec<_>>();
                    listing.sort_by_cached_key(|(is_directory, name, _)| {
                        (!*is_directory, name.to_lowercase())
                    });
                    listing
                })
                .unwrap_or_default();
            self.listed = Some(self.directory.clone());
        }
        self.listing.clone()
    }
}

struct ImportDialog {
    entry: BrowseEntry,
    browser: FileBrowser,
    message: String,
}

struct FirmwareDialog {
    system: String,
    files: Vec<FirmwareFileStatus>,
    records: Vec<Option<FirmwareRecord>>,
    selected_firmware: usize,
    browser: FileBrowser,
    message: String,
}

struct LoadedLibrary {
    catalog: Catalog,
    browse: Arc<BrowseCatalog>,
    readiness: Option<ReadinessReport>,
    featured_ids: HashSet<String>,
    search_documents: Vec<String>,
    imported_manifests: BTreeMap<String, ImportedManifest>,
    installed_ids: HashSet<String>,
    controls: ControlsCatalog,
    status: String,
}

/// Something a card asked for; performed after the page is drawn.
enum CardAction {
    Import(BrowseEntry),
    Download(BrowseEntry),
    Firmware(SystemReadiness),
    Controls(BrowseEntry),
    Remove {
        catalog_id: String,
        title: String,
    },
    Uninstall(CatalogEntry),
    Terminate,
    Play {
        catalog_id: String,
        title: String,
        system: String,
        rom: PathBuf,
    },
}

struct OperationResult {
    success: bool,
    heading: String,
    message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GameButtonIntent {
    Play,
    Terminate,
    Disabled,
}

/// The card button while a game session exists: LOADING for the first
/// seconds (a second PLAY is impossible), then TERMINATE, which also cancels
/// a launch still being prepared.
fn game_button_state(
    active: Option<(&str, SessionPhase, Duration)>,
    card_id: &str,
) -> (String, GameButtonIntent) {
    match active {
        Some((id, SessionPhase::Terminating, _)) if id == card_id => {
            ("■  TERMINATING…".to_owned(), GameButtonIntent::Disabled)
        }
        Some((id, _, age)) if id == card_id && age < Duration::from_secs(5) => {
            ("⏳  LOADING…".to_owned(), GameButtonIntent::Disabled)
        }
        Some((id, _, _)) if id == card_id => {
            ("■  TERMINATE".to_owned(), GameButtonIntent::Terminate)
        }
        Some(_) => ("GAME RUNNING".to_owned(), GameButtonIntent::Disabled),
        None => ("▶  PLAY".to_owned(), GameButtonIntent::Play),
    }
}

fn active_game_repaint_delay(age: Duration, phase: SessionPhase) -> Option<Duration> {
    (age < Duration::from_secs(5)
        || matches!(phase, SessionPhase::Preparing | SessionPhase::Terminating))
    .then_some(Duration::from_millis(100))
}

fn format_import_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    match bytes {
        0..KIB => format!("{bytes} B"),
        KIB..MIB if bytes.is_multiple_of(KIB) => format!("{} KiB", bytes / KIB),
        KIB..MIB => format!("{:.1} KiB", bytes as f64 / KIB as f64),
        MIB..GIB => format!("{:.2} MiB", bytes as f64 / MIB as f64),
        _ => format!("{:.2} GiB", bytes as f64 / GIB as f64),
    }
}

fn remove_import_available(imported: bool, operation_running: bool, game_running: bool) -> bool {
    imported && !operation_running && !game_running
}

struct StartupProbe {
    output: PathBuf,
    started: Instant,
    first_frame_recorded: bool,
    library_ready_at: Option<Instant>,
    library_rendered_recorded: bool,
    post_load_responsive_recorded: bool,
}

struct GameplayProbeConfig {
    catalog_id: String,
    output: PathBuf,
    duration: Duration,
}

/// Set when a gameplay probe observes anything other than a launch that runs
/// until its deadline and then leaves no process behind; the process exits
/// non-zero so scripted runs cannot mistake a crash for sustained execution.
static GAMEPLAY_PROBE_FAILED: AtomicBool = AtomicBool::new(false);

const PROBE_RETROARCH_COMMAND_PORT: u16 = 55355;

struct GameplayProbe {
    config: GameplayProbeConfig,
    created: Instant,
    started: bool,
    deadline_receiver: Option<Receiver<()>>,
    screenshot_receiver: Option<Receiver<serde_json::Value>>,
    terminating: bool,
    complete_recorded: bool,
}

/// Asks the running RetroArch for screenshots and describes what the game
/// was displaying. A game may be between screens (a fade to black or white)
/// at any one moment, so up to five frames about 1.5 s apart are sampled;
/// the frame counts as blank only if every sample is.
fn capture_retroarch_screenshot(screenshots: PathBuf) -> serde_json::Value {
    const ATTEMPTS: u32 = 5;
    let mut attempt = 1;
    loop {
        let mut frame = capture_one_retroarch_screenshot(&screenshots);
        let blank = frame.get("blank") == Some(&serde_json::Value::Bool(true));
        if !blank || attempt == ATTEMPTS {
            frame["samples"] = attempt.into();
            return frame;
        }
        attempt += 1;
        thread::sleep(Duration::from_millis(1500));
    }
}

fn capture_one_retroarch_screenshot(screenshots: &Path) -> serde_json::Value {
    let started = std::time::SystemTime::now();
    let sent = std::net::UdpSocket::bind("127.0.0.1:0").and_then(|socket| {
        socket.send_to(b"SCREENSHOT", ("127.0.0.1", PROBE_RETROARCH_COMMAND_PORT))
    });
    if let Err(error) = sent {
        return serde_json::json!({ "error": format!("command port: {error}") });
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let newest = std::fs::read_dir(screenshots).ok().and_then(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
                })
                .filter_map(|entry| {
                    let modified = entry.metadata().ok()?.modified().ok()?;
                    (modified >= started).then(|| (modified, entry.path()))
                })
                .max()
        });
        if let Some((_, path)) = newest {
            // RetroArch writes the PNG incrementally and, under load, in
            // bursts; retry decoding until it is complete.
            let mut last_error = String::new();
            while Instant::now() < deadline {
                thread::sleep(Duration::from_millis(250));
                match image::open(&path) {
                    Ok(frame) => {
                        let rgba = frame.to_rgba8();
                        let mut colors = HashSet::new();
                        for pixel in rgba.pixels().step_by(7) {
                            colors.insert(pixel.0);
                            if colors.len() > 4096 {
                                break;
                            }
                        }
                        return serde_json::json!({
                            "path": path.display().to_string(),
                            "width": rgba.width(),
                            "height": rgba.height(),
                            "sampled_colors": colors.len(),
                            "blank": colors.len() < 2,
                        });
                    }
                    Err(error) => last_error = error.to_string(),
                }
            }
            return serde_json::json!({
                "path": path.display().to_string(),
                "error": last_error,
            });
        }
        thread::sleep(Duration::from_millis(200));
    }
    serde_json::json!({ "error": "RetroArch wrote no screenshot within 15 seconds" })
}

impl GameplayProbe {
    fn new(config: GameplayProbeConfig) -> Self {
        let _ = std::fs::remove_file(&config.output);
        Self {
            config,
            created: Instant::now(),
            started: false,
            deadline_receiver: None,
            screenshot_receiver: None,
            terminating: false,
            complete_recorded: false,
        }
    }

    fn record(&self, event: &str, detail: serde_json::Value) -> std::io::Result<()> {
        let mut record = serde_json::json!({
            "event": event,
            "elapsed_ms": self.created.elapsed().as_millis() as u64,
        });
        if let (Some(record), serde_json::Value::Object(detail)) = (record.as_object_mut(), detail)
        {
            record.extend(detail);
        }
        let mut output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.config.output)?;
        writeln!(output, "{record}")?;
        output.sync_data()
    }

    fn fail(&self, event: &str, detail: serde_json::Value) {
        GAMEPLAY_PROBE_FAILED.store(true, Ordering::SeqCst);
        let _ = self.record(event, detail);
    }
}

impl StartupProbe {
    fn new(output: PathBuf) -> Self {
        let _ = std::fs::remove_file(&output);
        Self {
            output,
            started: Instant::now(),
            first_frame_recorded: false,
            library_ready_at: None,
            library_rendered_recorded: false,
            post_load_responsive_recorded: false,
        }
    }

    fn record(&self, event: &str) -> std::io::Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.output)?;
        writeln!(
            output,
            "{{\"event\":\"{event}\",\"elapsed_ms\":{}}}",
            self.started.elapsed().as_millis()
        )?;
        output.sync_data()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BrowseViewKey {
    source: String,
    system: String,
    query: String,
}

impl FirmwareDialog {
    fn new(readiness: &SystemReadiness, layout: &PortableLayout) -> Self {
        let files = readiness.firmware_files.clone();
        let selected_firmware = files.iter().position(|file| !file.present).unwrap_or(0);
        let records = files
            .iter()
            .map(|file| firmware_record(layout, &file.relative_path))
            .collect();
        Self {
            system: readiness.catalog_system.clone(),
            files,
            records,
            selected_firmware,
            browser: FileBrowser::new(),
            message: "Select the file for a target below, or open the folder holding your BIOS files and let RetroPort recognise every one by fingerprint (dropping a folder here does the same). RetroPort records each file's SHA-256; an unfamiliar dump is never rejected.".to_owned(),
        }
    }
}

impl ImportDialog {
    fn new(entry: BrowseEntry) -> Self {
        let message = if entry.system.eq_ignore_ascii_case("mame") {
            "Select the intact MAME ROM-set ZIP (for example, mspacman.zip). If it came inside a RAR archive, select the RAR and RetroPort will unpack it."
        } else {
            "Choose the local game file, disc descriptor, or RAR archive. RAR files are unpacked automatically. Double-click a file to import it immediately."
        };
        Self {
            entry,
            browser: FileBrowser::new(),
            message: message.to_owned(),
        }
    }
}

struct PortableApp {
    context: egui::Context,
    layout: PortableLayout,
    catalog: Catalog,
    status: String,
    operation: Option<Receiver<OperationResult>>,
    operation_notice: Option<OperationResult>,
    running_game: Option<GameSession>,
    artwork_jobs: SyncSender<ArtworkJob>,
    artwork_receiver: Receiver<ArtworkMessage>,
    /// Uploaded covers with the frame that last showed them.
    textures: HashMap<String, (egui::TextureHandle, u64)>,
    frame_number: u64,
    artwork_generation: Arc<AtomicU64>,
    artwork_errors: HashMap<String, String>,
    artwork_pending: usize,
    artwork_inflight: HashSet<String>,
    browse: Arc<BrowseCatalog>,
    readiness: Option<ReadinessReport>,
    readiness_refresh: Option<Receiver<Result<ReadinessReport, String>>>,
    featured_ids: HashSet<String>,
    search_documents: Vec<String>,
    imported_manifests: BTreeMap<String, ImportedManifest>,
    /// Trusted-catalogue installs made by earlier versions (PLAY + REMOVE).
    installed_ids: HashSet<String>,
    retrobat_present: bool,
    controls: Option<ControlsCatalog>,
    browse_view_key: Option<BrowseViewKey>,
    browse_page_key: Option<(Option<BrowseViewKey>, usize)>,
    browse_systems: Vec<String>,
    browse_matches: Vec<usize>,
    browse_page: usize,
    source_filter: String,
    system_filter: String,
    search: String,
    import_dialog: Option<ImportDialog>,
    firmware_dialog: Option<FirmwareDialog>,
    controls_dialog: Option<GameControls>,
    loading: Option<Receiver<LoadedLibrary>>,
    startup_probe: Option<StartupProbe>,
    gameplay_probe: Option<GameplayProbe>,
}

fn load_library(layout: &PortableLayout) -> LoadedLibrary {
    let (catalog, mut status) = match Catalog::built_in() {
        Ok(catalog) => {
            let count = catalog.entries.len();
            (
                catalog,
                format!("Verified catalog loaded: {count} item(s)."),
            )
        }
        Err(error) => (
            Catalog {
                schema_version: 1,
                generated_at: String::new(),
                entries: Vec::new(),
            },
            format!("Catalog rejected: {error}"),
        ),
    };
    let browse = BrowseCatalog::built_in().unwrap_or_else(|error| {
        eprintln!("Browse snapshot rejected: {error}");
        BrowseCatalog {
            schema_version: 2,
            generated_at: String::new(),
            sources: Vec::new(),
            entries: Vec::new(),
        }
    });
    // Everything below depends only on the parsed catalogue; run it
    // side by side so the library is ready as soon as the slowest part is.
    let (readiness, search_documents, featured_ids, imported, controls) =
        std::thread::scope(|scope| {
            let readiness = scope.spawn(|| ReadinessReport::audit(layout, &browse.entries));
            let featured = scope.spawn(|| FeaturedCatalog::built_in(&browse));
            let imported = scope.spawn(|| imported_manifests(layout));
            let controls = scope.spawn(ControlsCatalog::built_in);
            let search_documents = browse
                .entries
                .iter()
                .map(|entry| {
                    format!(
                        "{} {} {} {} {} {} {} {} {}",
                        entry.title,
                        entry.developer,
                        entry.system,
                        entry.source_id,
                        entry.description,
                        entry.license.as_deref().unwrap_or_default(),
                        entry
                            .release_year
                            .map(|year| year.to_string())
                            .unwrap_or_default(),
                        entry.tags.join(" "),
                        entry.kind,
                    )
                    .to_lowercase()
                })
                .collect::<Vec<_>>();
            (
                readiness.join().expect("readiness audit thread"),
                search_documents,
                featured.join().expect("featured thread"),
                imported.join().expect("import record thread"),
                controls.join().expect("controls thread"),
            )
        });
    let readiness = match readiness {
        Ok(report) => {
            status = format!(
                "{status} Backend audit: {} title(s) ready now, {} without an installed emulator, {} unresolved.",
                report.ready_now_entries,
                report.emulator_missing_entries,
                report.unresolved_entries
            );
            Some(report)
        }
        Err(error) => {
            status = format!("{status} Backend audit unavailable: {error}");
            None
        }
    };
    let featured_ids = featured_ids
        .map(|featured| featured.entry_ids)
        .unwrap_or_else(|error| {
            eprintln!("Featured snapshot rejected: {error}");
            HashSet::new()
        });
    let installed_ids = installed_trusted_ids(layout, &catalog);
    LoadedLibrary {
        catalog,
        browse: Arc::new(browse),
        readiness,
        featured_ids,
        search_documents,
        imported_manifests: imported,
        installed_ids,
        controls: controls.expect("built-in controls snapshot must validate"),
        status,
    }
}

fn installed_trusted_ids(layout: &PortableLayout, catalog: &Catalog) -> HashSet<String> {
    catalog
        .entries
        .iter()
        .filter(|entry| is_installed(layout, entry))
        .map(|entry| entry.id.clone())
        .collect()
}

impl PortableApp {
    fn new(
        layout: PortableLayout,
        context: &egui::Context,
        startup_probe_output: Option<PathBuf>,
        gameplay_probe: Option<GameplayProbeConfig>,
    ) -> Self {
        #[cfg(target_os = "windows")]
        if context.native_pixels_per_point().unwrap_or(1.0) < 1.15 {
            // Wine and some 100%-scaled Windows desktops report 96 DPI even on
            // dense displays. Keep the library comfortably readable while
            // leaving Windows' 125%+ accessibility scaling untouched.
            context.set_zoom_factor(1.2);
        }

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(13, 16, 23);
        visuals.window_fill = egui::Color32::from_rgb(18, 22, 31);
        visuals.extreme_bg_color = egui::Color32::from_rgb(7, 9, 13);
        visuals.selection.bg_fill = egui::Color32::from_rgb(58, 113, 255);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(18, 22, 31);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(48, 54, 66);
        visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(30, 36, 49);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(62, 72, 92);
        visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(42, 50, 67);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(78, 91, 118);
        visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(58, 68, 91);
        visuals.widgets.open.bg_fill = egui::Color32::from_rgb(42, 50, 67);
        visuals.widgets.open.weak_bg_fill = egui::Color32::from_rgb(36, 43, 58);
        context.set_visuals(visuals);

        let (artwork_sender, artwork_receiver) = mpsc::channel();
        let artwork_generation = Arc::new(AtomicU64::new(0));
        let artwork_jobs = start_artwork_workers(
            layout.clone(),
            artwork_sender,
            Arc::clone(&artwork_generation),
            context.clone(),
        );
        let (loading_sender, loading_receiver) = mpsc::channel();
        let loading_layout = layout.clone();
        thread::spawn(move || {
            let _ = loading_sender.send(load_library(&loading_layout));
        });
        Self {
            context: context.clone(),
            layout: layout.clone(),
            catalog: Catalog {
                schema_version: 1,
                generated_at: String::new(),
                entries: Vec::new(),
            },
            status: "Loading catalogues and auditing installed backends…".to_owned(),
            operation: None,
            operation_notice: None,
            running_game: None,
            artwork_jobs,
            artwork_receiver,
            textures: HashMap::new(),
            frame_number: 0,
            artwork_generation,
            artwork_errors: HashMap::new(),
            artwork_pending: 0,
            artwork_inflight: HashSet::new(),
            browse: Arc::new(BrowseCatalog {
                schema_version: 2,
                generated_at: String::new(),
                sources: Vec::new(),
                entries: Vec::new(),
            }),
            readiness: None,
            readiness_refresh: None,
            featured_ids: HashSet::new(),
            search_documents: Vec::new(),
            imported_manifests: BTreeMap::new(),
            installed_ids: HashSet::new(),
            retrobat_present: layout.retrobat_executable().is_file(),
            controls: None,
            browse_view_key: None,
            browse_page_key: None,
            browse_systems: Vec::new(),
            browse_matches: Vec::new(),
            browse_page: 0,
            source_filter: "featured".into(),
            system_filter: "all".into(),
            search: String::new(),
            import_dialog: None,
            firmware_dialog: None,
            controls_dialog: None,
            loading: Some(loading_receiver),
            startup_probe: startup_probe_output.map(StartupProbe::new),
            gameplay_probe: gameplay_probe.map(GameplayProbe::new),
        }
    }

    fn start_browse_artwork(&mut self, requests: Vec<(String, ArtworkSource)>) {
        for (entry_id, source) in requests {
            if !self.artwork_inflight.insert(entry_id.clone()) {
                continue;
            }
            match self.artwork_jobs.try_send(ArtworkJob {
                entry_id: entry_id.clone(),
                source,
                generation: self.artwork_generation.load(Ordering::SeqCst),
            }) {
                Ok(()) => self.artwork_pending += 1,
                Err(TrySendError::Full(_)) => {
                    self.artwork_inflight.remove(&entry_id);
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.artwork_inflight.remove(&entry_id);
                    self.artwork_errors.insert(
                        entry_id,
                        "Artwork worker pool stopped unexpectedly.".to_owned(),
                    );
                }
            }
        }
    }

    fn refresh_browse_view(&mut self) {
        let key = BrowseViewKey {
            source: self.source_filter.clone(),
            system: self.system_filter.clone(),
            query: self.search.trim().to_lowercase(),
        };
        if self.browse_view_key.as_ref() == Some(&key) {
            return;
        }
        let in_collection = |entry: &BrowseEntry| {
            key.source == "all"
                || (key.source == "featured" && self.featured_ids.contains(&entry.id))
                || entry.source_id == key.source
        };
        self.browse_systems = self
            .browse
            .entries
            .iter()
            .filter(|entry| in_collection(entry))
            .map(|entry| entry.system.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        if key.query.is_empty() {
            self.browse_matches = self
                .browse
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    in_collection(entry) && (key.system == "all" || entry.system == key.system)
                })
                .map(|(index, _)| index)
                .collect();
        } else {
            // Search is intentionally global and ignores collection/system
            // filters. Every word must appear somewhere in the record.
            let terms = key.query.split_whitespace().collect::<Vec<_>>();
            let mut ranked = self
                .search_documents
                .iter()
                .enumerate()
                .filter(|(_, document)| terms.iter().all(|term| document.contains(term)))
                .map(|(index, _)| {
                    let entry = &self.browse.entries[index];
                    let title = entry.title.to_lowercase();
                    let tier = if title == key.query {
                        0
                    } else if title.starts_with(&key.query) {
                        1
                    } else if terms.iter().all(|term| title.contains(term)) {
                        2
                    } else {
                        3
                    };
                    (
                        tier,
                        !self.imported_manifests.contains_key(&entry.id),
                        title,
                        index,
                    )
                })
                .collect::<Vec<_>>();
            ranked.sort_unstable();
            self.browse_matches = ranked.into_iter().map(|(_, _, _, index)| index).collect();
        }
        self.browse_view_key = Some(key);
    }

    /// Draws one catalogue card, collecting what the user asked for into
    /// `actions`. Returns the artwork request when its cover is not loaded.
    fn card(
        &self,
        ui: &mut egui::Ui,
        entry: &BrowseEntry,
        card_width: f32,
        artwork_height: f32,
        actions: &mut Vec<CardAction>,
    ) -> Option<(String, ArtworkSource)> {
        const CARD: egui::Color32 = egui::Color32::from_rgb(22, 27, 38);
        const GOOD: egui::Color32 = egui::Color32::from_rgb(98, 211, 145);
        const WARN: egui::Color32 = egui::Color32::from_rgb(238, 177, 89);
        const BAD: egui::Color32 = egui::Color32::from_rgb(235, 113, 113);
        let mut artwork_request = None;
        egui::Frame::new()
            .fill(CARD)
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(43, 51, 69)))
            .corner_radius(10)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_min_width(card_width);
                    ui.set_max_width(card_width);
                    let size = egui::vec2(card_width, artwork_height);
                    if let Some((texture, _)) = self.textures.get(&entry.id) {
                        ui.add(
                            egui::Image::new((texture.id(), size))
                                .fit_to_exact_size(size)
                                .corner_radius(7),
                        );
                    } else {
                        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        let sourced = entry.artwork_asset.is_some() || entry.artwork_url.is_some();
                        let failed = self.artwork_errors.contains_key(&entry.id);
                        if sourced && !failed {
                            ui.painter().rect_filled(rect, 7, egui::Color32::from_rgb(28, 35, 49));
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "LOADING…",
                                egui::FontId::proportional(12.0),
                                egui::Color32::from_gray(130),
                            );
                            if !self.artwork_inflight.contains(&entry.id) {
                                let source = self
                                    .catalog
                                    .entries
                                    .iter()
                                    .find(|trusted| trusted.id == entry.id)
                                    .and_then(|trusted| trusted.artwork.first().cloned())
                                    .map(ArtworkSource::Verified)
                                    .or_else(|| entry.artwork_asset.clone().map(ArtworkSource::Bundled))
                                    .or_else(|| entry.artwork_url.clone().map(ArtworkSource::Snapshot));
                                artwork_request = source.map(|source| (entry.id.clone(), source));
                            }
                        } else {
                            paint_generated_artwork(ui.painter(), rect, entry);
                        }
                    }
                    ui.add_space(5.0);
                    ui.label(egui::RichText::new(&entry.title).strong().color(egui::Color32::WHITE))
                        .on_hover_text(format!("Card ID: {}", entry.id));
                    let source_name = self
                        .browse
                        .sources
                        .iter()
                        .find(|source| source.id == entry.source_id)
                        .map_or(entry.source_id.as_str(), |source| source.name.as_str());
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  ·  {}{}",
                            entry.system.to_ascii_uppercase(),
                            source_name,
                            entry
                                .release_year
                                .map(|year| format!("  ·  {year}"))
                                .unwrap_or_default()
                        ))
                        .small()
                        .color(egui::Color32::from_gray(145)),
                    );
                    ui.label(
                        egui::RichText::new(&entry.developer)
                            .small()
                            .color(egui::Color32::from_gray(165)),
                    );
                    if let Some(license) = &entry.license {
                        ui.label(egui::RichText::new(license).small().color(egui::Color32::from_gray(125)));
                    }
                    let download = (entry.acquisition == Acquisition::DirectDownload)
                        .then(|| download_route(entry));
                    let (trust, trust_color, trust_detail) = match &download {
                        Some(Ok(pinned)) => (
                            "VERIFIED DOWNLOAD",
                            GOOD,
                            format!(
                                "Fetched from {} and checked against its pinned SHA-256 ({}) before anything is installed.",
                                pinned.url,
                                format_import_size(pinned.size)
                            ),
                        ),
                        Some(Err(reason)) => (
                            "SOURCE UNAVAILABLE",
                            egui::Color32::from_gray(145),
                            format!("No verified download exists for this game: {reason}."),
                        ),
                        None => (
                            "LOCAL COPY REQUIRED",
                            egui::Color32::from_gray(145),
                            "Import your own copy of this game.".to_owned(),
                        ),
                    };
                    ui.label(egui::RichText::new(trust).small().strong().color(trust_color))
                        .on_hover_text(trust_detail);
                    if let Some(readiness) = self
                        .readiness
                        .as_ref()
                        .and_then(|report| report.for_catalog_system(&entry.system))
                    {
                        let (label, color, detail) = match readiness.backend {
                            BackendState::ReadyNow => (
                                "BACKEND READY",
                                GOOD,
                                readiness.ready_route.as_ref().map_or_else(
                                    || "An installed emulator route is available.".to_owned(),
                                    |route| format!("Installed route: {}", route.label()),
                                ),
                            ),
                            BackendState::EmulatorMissing => (
                                "EMULATOR NOT INSTALLED",
                                BAD,
                                "RetroBat lists an emulator for this system, but this installation does not include it, so PLAY cannot start these games.".to_owned(),
                            ),
                            BackendState::Unresolved => (
                                "BACKEND NOT YET RESOLVED",
                                BAD,
                                "No RetroBat system adapter is currently mapped for this catalogue system.".to_owned(),
                            ),
                        };
                        ui.label(egui::RichText::new(label).small().strong().color(color))
                            .on_hover_text(detail);
                        if readiness.firmware == FirmwareState::RequiredMissing {
                            ui.label(egui::RichText::new("FIRMWARE SETUP REQUIRED").small().color(WARN))
                                .on_hover_text(format!(
                                    "The selected installed backend declares {} required firmware file(s); none were detected.",
                                    readiness.firmware_candidates
                                ));
                        }
                        if !readiness.firmware_files.is_empty() {
                            let missing = |optional: bool| {
                                readiness
                                    .firmware_files
                                    .iter()
                                    .filter(|file| !file.present && file.optional == optional)
                                    .collect::<Vec<_>>()
                            };
                            let (required, optional) = (missing(false), missing(true));
                            let label = match (required.is_empty(), optional.is_empty()) {
                                (false, _) if required.iter().any(|file| file.download.is_some()) => "INSTALL FIRMWARE",
                                (false, _) => "IMPORT FIRMWARE",
                                (true, false) if optional.iter().any(|file| file.download.is_some()) => {
                                    "INSTALL OPTIONAL FIRMWARE"
                                }
                                (true, false) => "IMPORT OPTIONAL FIRMWARE",
                                (true, true) => "MANAGE FIRMWARE",
                            };
                            if ui
                                .add_enabled(
                                    self.operation.is_none(),
                                    egui::Button::new(egui::RichText::new(label).small().strong())
                                        .fill(CONTROL_BACKGROUND),
                                )
                                .clicked()
                            {
                                actions.push(CardAction::Firmware(readiness.clone()));
                            }
                        }
                    }
                    if let Some(url) = &entry.detail_url {
                        ui.hyperlink_to(egui::RichText::new("SOURCE DETAILS").small().color(ACCENT), url);
                    }
                    ui.add_space(4.0);
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("⌨  CONTROLS").small().strong())
                                .fill(CONTROL_BACKGROUND)
                                .min_size(egui::vec2(122.0, 26.0)),
                        )
                        .on_hover_text(
                            "Available before import: guidance comes from the catalogue's MAME/RetroBat metadata and the installed RetroArch/controller configuration, not from inspecting ROM bytes.",
                        )
                        .clicked()
                    {
                        actions.push(CardAction::Controls(entry.clone()));
                    }
                    ui.add_space(3.0);
                    self.card_game_buttons(ui, entry, download.as_ref(), actions);
                });
            });
        artwork_request
    }

    /// PLAY/TERMINATE, IMPORT GAME or DOWNLOAD, and REMOVE for one card.
    fn card_game_buttons(
        &self,
        ui: &mut egui::Ui,
        entry: &BrowseEntry,
        download: Option<&Result<&'static PinnedDownload, String>>,
        actions: &mut Vec<CardAction>,
    ) {
        let imported = self.imported_manifests.get(&entry.id);
        let trusted = self
            .catalog
            .entries
            .iter()
            .find(|trusted| trusted.id == entry.id)
            .filter(|trusted| self.installed_ids.contains(&trusted.id));
        let playable = imported
            .map(|manifest| {
                (
                    manifest.system.clone(),
                    self.layout.root.join(&manifest.launch_relative_path),
                )
            })
            .or_else(|| {
                trusted.map(|trusted| {
                    (
                        trusted.system.clone(),
                        self.layout.root.join(trusted.install_relative_path()),
                    )
                })
            });
        let busy = self.operation.is_some();
        // A game whose system has no installed emulator could be fetched but
        // never started.
        let emulator_missing = self
            .readiness
            .as_ref()
            .and_then(|report| report.for_catalog_system(&entry.system))
            .is_some_and(|system| system.backend == BackendState::EmulatorMissing);
        let (label, intent) = match &playable {
            Some(_) if emulator_missing => ("NO EMULATOR".to_owned(), GameButtonIntent::Disabled),
            Some(_) => game_button_state(
                self.running_game
                    .as_ref()
                    .map(|game| (game.catalog_id.as_str(), game.phase(), game.age())),
                &entry.id,
            ),
            None => match download {
                Some(Ok(_)) if emulator_missing => {
                    ("NO EMULATOR".to_owned(), GameButtonIntent::Disabled)
                }
                Some(Ok(_)) => ("DOWNLOAD".to_owned(), GameButtonIntent::Play),
                Some(Err(_)) => ("UNAVAILABLE".to_owned(), GameButtonIntent::Disabled),
                None => ("IMPORT GAME".to_owned(), GameButtonIntent::Play),
            },
        };
        let wide = entry.acquisition == Acquisition::DirectDownload;
        let width = if wide { 150.0 } else { 122.0 };
        if ui
            .add_enabled(
                !busy && intent != GameButtonIntent::Disabled,
                egui::Button::new(
                    egui::RichText::new(&label)
                        .small()
                        .strong()
                        .color(egui::Color32::WHITE),
                )
                .fill(ACCENT)
                .min_size(egui::vec2(width, 28.0)),
            )
            .clicked()
        {
            actions.push(match (intent, playable) {
                (GameButtonIntent::Terminate, _) => CardAction::Terminate,
                (_, Some((system, rom))) => CardAction::Play {
                    catalog_id: entry.id.clone(),
                    title: entry.title.clone(),
                    system,
                    rom,
                },
                (_, None) if download.is_some() => CardAction::Download(entry.clone()),
                (_, None) => CardAction::Import(entry.clone()),
            });
        }
        if imported.is_some() || trusted.is_some() {
            let removable = remove_import_available(true, busy, self.running_game.is_some());
            if ui
                .add_enabled(
                    removable,
                    egui::Button::new(egui::RichText::new("REMOVE").small().strong())
                        .fill(egui::Color32::from_rgb(113, 47, 55))
                        .min_size(egui::vec2(width, 26.0)),
                )
                .on_hover_text(
                    "Remove the files this card installed and return it to its first action. Files you changed since are kept.",
                )
                .clicked()
            {
                actions.push(match trusted {
                    Some(trusted) if imported.is_none() => CardAction::Uninstall(trusted.clone()),
                    _ => CardAction::Remove {
                        catalog_id: entry.id.clone(),
                        title: entry.title.clone(),
                    },
                });
            }
        }
    }

    fn perform(&mut self, action: CardAction) {
        match action {
            CardAction::Import(entry) => self.import_dialog = Some(ImportDialog::new(entry)),
            CardAction::Download(entry) => self.start_browse_download(entry),
            CardAction::Firmware(readiness) => {
                self.firmware_dialog = Some(FirmwareDialog::new(&readiness, &self.layout));
            }
            CardAction::Controls(entry) => {
                if let Some(controls) = &self.controls {
                    let imported = self.imported_manifests.get(&entry.id);
                    self.controls_dialog = Some(controls.for_game(
                        &self.layout,
                        &entry,
                        imported,
                        self.readiness.as_ref(),
                    ));
                }
            }
            CardAction::Remove { catalog_id, title } => self.start_remove_import(catalog_id, title),
            CardAction::Uninstall(entry) => self.start_uninstall(entry),
            CardAction::Terminate => self.terminate_running_game(),
            CardAction::Play {
                catalog_id,
                title,
                system,
                rom,
            } => self.launch_game(&catalog_id, &title, &system, &rom),
        }
    }

    fn evict_textures(&mut self) {
        // Keep the current page and recent ones; a cover is at most 1 MiB of
        // GPU memory, so 150 bounds the cache near 150 MiB however far the
        // user browses.
        const KEEP: usize = 150;
        if self.textures.len() <= KEEP {
            return;
        }
        let mut by_age = self
            .textures
            .iter()
            .map(|(id, (_, frame))| (*frame, id.clone()))
            .collect::<Vec<_>>();
        by_age.sort_unstable();
        for (_, id) in by_age.into_iter().take(self.textures.len() - KEEP) {
            self.textures.remove(&id);
        }
    }

    /// Removes a game an earlier RetroPort version installed through the
    /// trusted catalogue (its record lives in .retrobat-portable/installed).
    fn start_uninstall(&mut self, entry: CatalogEntry) {
        let layout = self.layout.clone();
        let (sender, receiver) = mpsc::channel();
        self.operation = Some(receiver);
        self.status = format!("Removing {}…", entry.title);
        let context = self.context.clone();
        thread::spawn(move || {
            let result = ReqwestDownloader::new()
                .and_then(|downloader| Installer::new(&layout, &downloader).uninstall(&entry));
            let _ = sender.send(match result {
                Ok(report) => OperationResult {
                    success: true,
                    heading: "GAME REMOVED".to_owned(),
                    message: format!(
                        "Removed {}: {} owned file(s) deleted, {} modified file(s) preserved.",
                        entry.title,
                        report.removed.len(),
                        report.preserved_modified.len()
                    ),
                },
                Err(error) => OperationResult {
                    success: false,
                    heading: "REMOVE FAILED".to_owned(),
                    message: format!("Could not remove {}: {error}", entry.title),
                },
            });
            context.request_repaint();
        });
    }

    fn start_import_path(&mut self, entry: BrowseEntry, source: PathBuf) {
        let layout = self.layout.clone();
        let (sender, receiver) = mpsc::channel();
        self.operation = Some(receiver);
        self.status = format!("Copying and preparing {}…", entry.title);
        thread::spawn(move || {
            let title = entry.title.clone();
            let result = GameImporter::new(&layout).import(&entry, &source);
            let result = match result {
                Ok(report) => OperationResult {
                    success: true,
                    heading: "IMPORT COMPLETE — PLAY IS READY".to_owned(),
                    message: format!(
                        "Imported {title}: {} file(s), {}. The card now shows PLAY.",
                        report.imported_files,
                        format_import_size(report.imported_bytes)
                    ),
                },
                Err(error) => OperationResult {
                    success: false,
                    heading: "IMPORT FAILED".to_owned(),
                    message: format!("Could not import {title}: {error}"),
                },
            };
            let _ = sender.send(result);
        });
    }

    fn start_remove_import(&mut self, catalog_id: String, title: String) {
        let layout = self.layout.clone();
        let (sender, receiver) = mpsc::channel();
        self.operation = Some(receiver);
        self.status = format!("Removing imported {title}…");
        thread::spawn(move || {
            let result = match remove_import(&layout, &catalog_id) {
                Ok(report) => OperationResult {
                    success: true,
                    heading: "IMPORTED GAME REMOVED".to_owned(),
                    message: format!(
                        "Removed {title}: {} owned file(s) deleted, {} modified file(s) preserved, {} already absent. The card now shows IMPORT GAME.",
                        report.removed.len(),
                        report.preserved_modified.len(),
                        report.already_missing.len(),
                    ),
                },
                Err(error) => OperationResult {
                    success: false,
                    heading: "REMOVE FAILED".to_owned(),
                    message: format!("Could not remove imported {title}: {error}"),
                },
            };
            let _ = sender.send(result);
        });
    }

    fn start_firmware_import(&mut self, firmware: FirmwareFileStatus, source: PathBuf) {
        let layout = self.layout.clone();
        let (sender, receiver) = mpsc::channel();
        self.operation = Some(receiver);
        self.status = format!("Adding firmware at bios/{}…", firmware.relative_path);
        thread::spawn(move || {
            let destination = firmware.relative_path.clone();
            let result = match import_firmware(&layout, &firmware, &source) {
                Ok(report) => {
                    let action = if report.replaced_existing {
                        "Replaced"
                    } else {
                        "Added"
                    };
                    OperationResult {
                        success: true,
                        heading: "FIRMWARE READY".to_owned(),
                        message: format!(
                            "{action} bios/{destination}: {} bytes (SHA-256 recorded as {}). Readiness refreshed.",
                            report.bytes, report.sha256,
                        ),
                    }
                }
                Err(error) => OperationResult {
                    success: false,
                    heading: "FIRMWARE IMPORT FAILED".to_owned(),
                    message: format!("Firmware import failed safely: {error}"),
                },
            };
            let _ = sender.send(result);
        });
    }

    fn start_firmware_folder_import(&mut self, folder: PathBuf) {
        let layout = self.layout.clone();
        let (sender, receiver) = mpsc::channel();
        self.operation = Some(receiver);
        self.status = format!("Recognising BIOS files in {}…", folder.display());
        thread::spawn(move || {
            let result = match import_firmware_folder(&layout, &folder) {
                Ok(report) => OperationResult {
                    success: true,
                    heading: if report.placed.is_empty() {
                        "NO NEW FIRMWARE FOUND".to_owned()
                    } else {
                        "FIRMWARE READY".to_owned()
                    },
                    message: describe_firmware_folder(&report),
                },
                Err(error) => OperationResult {
                    success: false,
                    heading: "FIRMWARE IMPORT FAILED".to_owned(),
                    message: format!("Firmware import failed safely: {error}"),
                },
            };
            let _ = sender.send(result);
        });
    }

    fn start_firmware_download(&mut self, firmware: FirmwareFileStatus) {
        let layout = self.layout.clone();
        let Some(download) = firmware.download.clone() else {
            self.status = "This firmware has no publisher download configured.".to_owned();
            return;
        };
        let (sender, receiver) = mpsc::channel();
        self.operation = Some(receiver);
        self.status = format!("Downloading firmware directly from {}…", download.publisher);
        thread::spawn(move || {
            let result = ReqwestDownloader::new()
                .map_err(|error| error.to_string())
                .and_then(|downloader| {
                    install_official_firmware(&layout, &firmware, &downloader)
                        .map_err(|error| error.to_string())
                })
                .and_then(|report| match download.install_action {
                    FirmwareInstallAction::PlaceInBios => Ok(format!(
                        "Installed and verified {} firmware: {} bytes at {}.",
                        download.publisher,
                        report.bytes,
                        report.destination.display()
                    )),
                    FirmwareInstallAction::Vita3k => {
                        let status = LaunchPlan::for_current_vita3k_firmware_install(
                            &layout,
                            &report.destination,
                        )
                        .and_then(|plan| plan.spawn(&|_| {}))
                        .and_then(|mut child| child.wait().map_err(Into::into))
                        .map_err(|error| error.to_string())?;
                        if !status.success() {
                            return Err(format!("Vita3K's firmware installer exited with {status}"));
                        }
                        Ok(format!(
                            "Downloaded and verified {} bytes from {}, and Vita3K installed it.",
                            report.bytes, download.publisher
                        ))
                    }
                    FirmwareInstallAction::Rpcs3 => {
                        LaunchPlan::for_current_rpcs3_firmware_install(
                            &layout,
                            &report.destination,
                        )
                        .and_then(|plan| plan.spawn(&|_| {}))
                        .map(|mut child| {
                            // Reap the installer when the user closes it.
                            thread::spawn(move || {
                                let _ = child.wait();
                            });
                        })
                        .map_err(|error| error.to_string())?;
                        Ok(format!(
                            "Downloaded and verified {} bytes from {}. RPCS3's firmware installer is open for confirmation.",
                            report.bytes, download.publisher
                        ))
                    }
                });
            let result = match result {
                Ok(message) => OperationResult {
                    success: true,
                    heading: "FIRMWARE DOWNLOAD COMPLETE".to_owned(),
                    message,
                },
                Err(error) => OperationResult {
                    success: false,
                    heading: "FIRMWARE INSTALLATION FAILED".to_owned(),
                    message: format!("Firmware installation failed safely: {error}"),
                },
            };
            let _ = sender.send(result);
        });
    }

    fn refresh_readiness(&mut self) {
        if self.readiness_refresh.is_some() {
            return;
        }
        let layout = self.layout.clone();
        let browse = Arc::clone(&self.browse);
        let context = self.context.clone();
        let (sender, receiver) = mpsc::channel();
        self.readiness_refresh = Some(receiver);
        thread::spawn(move || {
            let result =
                ReadinessReport::audit(&layout, &browse.entries).map_err(|error| error.to_string());
            let _ = sender.send(result);
            context.request_repaint();
        });
    }

    fn show_controls_dialog(&mut self, root: &mut egui::Ui) {
        let Some(profile) = &self.controls_dialog else {
            return;
        };
        let mut close = false;
        root.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.heading(format!("Controls · {}", profile.title));
                ui.label(egui::RichText::new(&profile.scope).strong().color(ACCENT));
                ui.label(
                    egui::RichText::new(&profile.confidence)
                        .small()
                        .color(egui::Color32::from_gray(155)),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                if ui.button("CLOSE").clicked() {
                    close = true;
                }
            });
        });
        root.separator();
        egui::ScrollArea::vertical().show(root, |ui| {
            ui.heading("Required input hardware");
            for line in &profile.device_summary {
                ui.label(format!("• {line}"));
            }
            ui.add_space(10.0);
            ui.columns(2, |columns| {
                columns[0].heading("Keyboard");
                if profile.keyboard.is_empty() {
                    columns[0]
                        .label("The installed backend does not declare keyboard-to-game bindings.");
                } else {
                    for binding in &profile.keyboard {
                        columns[0].horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&binding.input)
                                    .monospace()
                                    .strong()
                                    .color(ACCENT),
                            );
                            ui.label(format!("— {}", binding.function));
                        });
                    }
                }
                columns[1].heading("Controller");
                for binding in &profile.controller {
                    columns[1].horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&binding.input)
                                .monospace()
                                .strong()
                                .color(ACCENT),
                        );
                        ui.label(format!("— {}", binding.function));
                    });
                }
            });
            ui.add_space(12.0);
            ui.heading("Notes");
            for note in &profile.notes {
                ui.label(format!("• {note}"));
            }
            ui.add_space(12.0);
            ui.heading("Evidence and provenance");
            for source in &profile.sources {
                ui.horizontal_wrapped(|ui| {
                    ui.hyperlink_to(&source.name, &source.url);
                    ui.label(
                        egui::RichText::new(format!("· {}", source.version))
                            .small()
                            .color(egui::Color32::from_gray(135)),
                    );
                });
            }
        });
        if close {
            self.controls_dialog = None;
        }
    }

    fn start_browse_download(&mut self, entry: BrowseEntry) {
        let layout = self.layout.clone();
        let (sender, receiver) = mpsc::channel();
        self.operation = Some(receiver);
        self.status = format!(
            "Downloading {} from its publisher and verifying it…",
            entry.title
        );
        thread::spawn(move || {
            let title = entry.title.clone();
            let result = ReqwestDownloader::new()
                .map_err(|error| error.to_string())
                .and_then(|downloader| {
                    BrowseInstaller::new(&layout, &downloader)
                        .install(&entry)
                        .map_err(|error| error.to_string())
                });
            let result = match result {
                Ok(report) => OperationResult {
                    success: true,
                    heading: "DOWNLOAD COMPLETE — PLAY IS READY".to_owned(),
                    message: format!(
                        "Downloaded, verified, and installed {title}: {} file(s), {}. PLAY is ready.",
                        report.import.imported_files,
                        format_import_size(report.import.imported_bytes)
                    ),
                },
                Err(error) => OperationResult {
                    success: false,
                    heading: "DOWNLOAD FAILED".to_owned(),
                    message: format!("Download failed safely: {error}"),
                },
            };
            let _ = sender.send(result);
        });
    }

    fn show_firmware_dialog(&mut self, root: &mut egui::Ui) {
        let dropped = root.ctx().input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .find_map(|file| file.path.clone())
        });
        let mut import = None;
        let mut direct_download = None;
        let mut folder_import = None;
        let mut close = false;
        let Some(dialog) = &mut self.firmware_dialog else {
            return;
        };
        if let Some(path) = dropped {
            if path.is_file() {
                dialog.browser.path_text = path.display().to_string();
                dialog.browser.selected = Some(path);
                dialog.message = "Dropped file selected. Confirm the destination below.".to_owned();
            } else if path.is_dir() {
                folder_import = Some(path);
            }
        }
        let directory_entries = dialog.browser.entries();

        root.heading(format!("Firmware · {}", dialog.system.to_ascii_uppercase()));
        root.label(
            egui::RichText::new(&dialog.message)
                .small()
                .color(egui::Color32::from_gray(160)),
        );
        root.add_space(5.0);
        root.label(
            egui::RichText::new("CHOOSE DESTINATION")
                .small()
                .strong()
                .color(egui::Color32::from_gray(145)),
        );
        for (index, firmware) in dialog.files.iter().enumerate() {
            let kind = if firmware.optional {
                "OPTIONAL"
            } else {
                "REQUIRED"
            };
            let state = if firmware.present { "READY" } else { "MISSING" };
            let alternatives = if firmware.alternatives.is_empty() {
                String::new()
            } else {
                format!(" (or {})", firmware.alternatives.join(", "))
            };
            if root
                .selectable_label(
                    dialog.selected_firmware == index,
                    format!(
                        "{kind} · bios/{}{}{alternatives} · {state}",
                        firmware.relative_path,
                        if firmware.directory { "/" } else { "" }
                    ),
                )
                .clicked()
            {
                dialog.selected_firmware = index;
            }
        }
        let Some(target) = dialog.files.get(dialog.selected_firmware).cloned() else {
            root.label("This installed backend does not publish a firmware file list.");
            if root.button("CLOSE").clicked() {
                close = true;
            }
            if close {
                self.firmware_dialog = None;
            }
            return;
        };
        root.add_space(5.0);
        root.label(
            egui::RichText::new(if target.description.is_empty() {
                format!("Expected file: {}", target.relative_path)
            } else {
                target.description.clone()
            })
            .strong(),
        );
        root.label(
            egui::RichText::new(&target.guidance)
                .small()
                .color(egui::Color32::from_gray(150)),
        );
        root.horizontal_wrapped(|ui| {
            ui.hyperlink_to(
                egui::RichText::new("OPEN FIRMWARE GUIDANCE")
                    .strong()
                    .color(egui::Color32::from_rgb(104, 146, 255)),
                &target.guidance_url,
            );
            ui.label(
                egui::RichText::new(&target.guidance_url)
                    .small()
                    .color(egui::Color32::from_gray(130)),
            );
        });
        if let Some(record) = dialog
            .records
            .get(dialog.selected_firmware)
            .cloned()
            .flatten()
        {
            root.label(
                egui::RichText::new(format!(
                    "Recorded: {} · {} · SHA-256 {}",
                    record.origin,
                    format_import_size(record.size),
                    record.sha256
                ))
                .small()
                .monospace()
                .color(egui::Color32::from_gray(150)),
            );
        }
        root.add_space(6.0);

        if let Some(download) = &target.download {
            if root
                .add_enabled(
                    self.operation.is_none(),
                    egui::Button::new(
                        egui::RichText::new(format!(
                            "INSTALL FIRMWARE FROM {}",
                            download.publisher.to_ascii_uppercase()
                        ))
                        .strong()
                        .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(104, 146, 255))
                    .min_size(egui::vec2(260.0, 34.0)),
                )
                .clicked()
            {
                direct_download = Some(target.clone());
            }
            root.label(
                egui::RichText::new(format!(
                    "Direct publisher download · {} MiB · SHA-256 verified before installation",
                    download.size / (1024 * 1024)
                ))
                .small()
                .color(egui::Color32::from_gray(145)),
            );
            root.add_space(8.0);
            root.label(
                egui::RichText::new("OR CHOOSE A LOCAL FIRMWARE FILE")
                    .small()
                    .strong()
                    .color(egui::Color32::from_gray(145)),
            );
        }

        root.horizontal(|ui| {
            if ui
                .add(egui::Button::new("HOME").fill(CONTROL_BACKGROUND))
                .clicked()
                && let Some(home) = dirs::home_dir()
            {
                dialog.browser.open(home);
            }
            if ui
                .add(egui::Button::new("DOWNLOADS").fill(CONTROL_BACKGROUND))
                .clicked()
                && let Some(downloads) = dirs::download_dir()
            {
                dialog.browser.open(downloads);
            }
            if ui
                .add(egui::Button::new("UP").fill(CONTROL_BACKGROUND))
                .clicked()
                && let Some(parent) = dialog.browser.directory.parent()
            {
                dialog.browser.open(parent.to_owned());
            }
            #[cfg(target_os = "linux")]
            if ui
                .add(egui::Button::new("FILESYSTEM").fill(CONTROL_BACKGROUND))
                .clicked()
            {
                dialog.browser.open(PathBuf::from("/"));
            }
        });
        root.horizontal(|ui| {
            let submitted = ui
                .add_sized(
                    [(ui.available_width() - 52.0).max(180.0), 24.0],
                    egui::TextEdit::singleline(&mut dialog.browser.path_text)
                        .background_color(INPUT_BACKGROUND),
                )
                .lost_focus()
                && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if (ui
                .add_sized(
                    [48.0, 24.0],
                    egui::Button::new("GO").fill(CONTROL_BACKGROUND),
                )
                .clicked()
                || submitted)
                && !dialog.browser.path_text.trim().is_empty()
            {
                let path = PathBuf::from(dialog.browser.path_text.trim());
                if path.is_dir() {
                    dialog.browser.directory = path;
                    dialog.browser.selected = None;
                } else if path.is_file() {
                    dialog.browser.selected = Some(path);
                } else {
                    dialog.message = "That path does not exist.".to_owned();
                }
            }
        });
        root.separator();
        egui::ScrollArea::vertical()
            .id_salt("firmware-file-list")
            .auto_shrink([false, false])
            .max_height((root.available_height() - 52.0).max(100.0))
            .show(root, |ui| {
                for (is_directory, name, path) in &directory_entries {
                    let label = if *is_directory {
                        format!("📁  {name}")
                    } else {
                        format!("      {name}")
                    };
                    let selected = dialog.browser.selected.as_ref() == Some(path);
                    if ui.selectable_label(selected, label).clicked() {
                        if *is_directory {
                            dialog.browser.open(path.clone());
                        } else {
                            dialog.browser.path_text = path.display().to_string();
                            dialog.browser.selected = Some(path.clone());
                        }
                    }
                }
            });
        root.separator();
        root.horizontal(|ui| {
            if ui
                .add_enabled(
                    dialog
                        .browser
                        .selected
                        .as_ref()
                        .is_some_and(|path| path.is_file())
                        && self.operation.is_none(),
                    egui::Button::new(format!(
                        "{} {} bios/{}{}",
                        if target.present { "REPLACE" } else { "ADD" },
                        if target.directory { "TO" } else { "AS" },
                        target.relative_path,
                        if target.directory { "/" } else { "" },
                    ))
                    .fill(CONTROL_BACKGROUND),
                )
                .clicked()
                && let Some(source) = dialog.browser.selected.clone()
            {
                import = Some((target.clone(), source));
            }
            if ui
                .add_enabled(
                    self.operation.is_none(),
                    egui::Button::new("RECOGNISE EVERY BIOS IN THIS FOLDER").fill(CONTROL_BACKGROUND),
                )
                .on_hover_text(
                    "Searches this folder and its subfolders, identifies each BIOS or firmware file \
                     by fingerprint for every system, and places it where its emulators look. \
                     A different file already in place is kept.",
                )
                .clicked()
            {
                folder_import = Some(dialog.browser.directory.clone());
            }
            if ui
                .add(egui::Button::new("CANCEL").fill(CONTROL_BACKGROUND))
                .clicked()
            {
                close = true;
            }
        });
        if let Some(folder) = folder_import {
            self.firmware_dialog = None;
            self.start_firmware_folder_import(folder);
        } else if let Some(firmware) = direct_download {
            self.firmware_dialog = None;
            self.start_firmware_download(firmware);
        } else if let Some((firmware, source)) = import {
            self.firmware_dialog = None;
            self.start_firmware_import(firmware, source);
        } else if close {
            self.firmware_dialog = None;
            self.status = "Firmware setup cancelled; no files were changed.".to_owned();
        }
    }

    fn show_import_dialog(&mut self, root: &mut egui::Ui) {
        let readiness = self
            .import_dialog
            .as_ref()
            .and_then(|dialog| {
                self.readiness
                    .as_ref()
                    .and_then(|report| report.for_catalog_system(&dialog.entry.system))
            })
            .cloned();
        let dropped = root.ctx().input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .find_map(|file| file.path.clone())
        });
        let mut import = None;
        let mut close = false;
        let Some(dialog) = &mut self.import_dialog else {
            return;
        };
        if let Some(path) = dropped
            && (path.is_file() || path.is_dir())
        {
            dialog.browser.path_text = path.display().to_string();
            if path.is_dir() {
                dialog.browser.directory = path;
                dialog.browser.selected = None;
                dialog.message =
                    "Dropped folder selected. Importing it preserves every required game file."
                        .to_owned();
            } else {
                dialog.browser.selected = Some(path);
                dialog.message = "Dropped file selected. Confirm the import below.".to_owned();
            }
        }

        let directory_entries = dialog.browser.entries();
        root.heading(format!("Import {}", dialog.entry.title));
        root.label(
            egui::RichText::new(format!(
                "{} · {}",
                dialog.entry.system.to_ascii_uppercase(),
                dialog.entry.title
            ))
            .strong(),
        );
        root.label(
            egui::RichText::new(&dialog.message)
                .small()
                .color(egui::Color32::from_gray(160)),
        );
        root.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!("Developer: {}", dialog.entry.developer))
                    .small()
                    .color(egui::Color32::from_gray(150)),
            );
            if let Some(year) = dialog.entry.release_year {
                ui.label(
                    egui::RichText::new(format!("Year: {year}"))
                        .small()
                        .color(egui::Color32::from_gray(150)),
                );
            }
            ui.label(
                egui::RichText::new(format!("Catalogue: {}", dialog.entry.source_id))
                    .small()
                    .color(egui::Color32::from_gray(150)),
            );
        });
        if !dialog.entry.description.is_empty() {
            root.label(
                egui::RichText::new(&dialog.entry.description)
                    .small()
                    .color(egui::Color32::from_gray(145)),
            );
        }
        if let Some(readiness) = &readiness {
            match readiness.backend {
                BackendState::ReadyNow => {
                    let route = readiness
                        .ready_route
                        .as_ref()
                        .map(|route| format!(" through {}", route.label()))
                        .unwrap_or_default();
                    root.label(
                        egui::RichText::new(format!("BACKEND READY{route}"))
                            .small()
                            .strong()
                            .color(egui::Color32::from_rgb(98, 211, 145)),
                    );
                }
                BackendState::EmulatorMissing => {
                    root.label(
                        egui::RichText::new(
                            "EMULATOR NOT INSTALLED · This installation has no emulator for this system yet.",
                        )
                        .small()
                        .strong()
                        .color(egui::Color32::from_rgb(238, 177, 89)),
                    );
                }
                BackendState::Unresolved => {
                    root.label(
                        egui::RichText::new(
                            "BACKEND NOT YET RESOLVED · This system still needs a launch adapter.",
                        )
                        .small()
                        .strong()
                        .color(egui::Color32::from_rgb(235, 113, 113)),
                    );
                }
            }
            match readiness.firmware {
                FirmwareState::RequiredMissing => {
                    root.label(
                        egui::RichText::new(format!(
                            "FIRMWARE SETUP REQUIRED · The selected backend declares {} required firmware file(s), and none are detected.",
                            readiness.firmware_candidates
                        ))
                        .small()
                        .color(egui::Color32::from_rgb(238, 177, 89)),
                    );
                    if !readiness.missing_firmware_examples.is_empty() {
                        root.label(
                            egui::RichText::new(format!(
                                "Examples: {}",
                                readiness.missing_firmware_examples.join(", ")
                            ))
                            .small()
                            .color(egui::Color32::from_gray(140)),
                        );
                    }
                }
                FirmwareState::SomeRequiredPresent => {
                    root.label(
                        egui::RichText::new(format!(
                            "FIRMWARE SETUP INCOMPLETE · {} of {} required file(s) are present.",
                            readiness.firmware_detected, readiness.firmware_candidates
                        ))
                        .small()
                        .color(egui::Color32::from_rgb(238, 177, 89)),
                    );
                }
                FirmwareState::AllRequiredPresent => {
                    root.label(
                        egui::RichText::new(format!(
                            "REQUIRED FIRMWARE READY · All {} required file(s) are present.",
                            readiness.firmware_candidates
                        ))
                        .small()
                        .color(egui::Color32::from_rgb(98, 211, 145)),
                    );
                }
                FirmwareState::NotRequired => {}
            }
        }
        if let Some(url) = &dialog.entry.detail_url {
            root.horizontal_wrapped(|ui| {
                ui.hyperlink_to(
                    egui::RichText::new("OPEN SOURCE PAGE")
                        .strong()
                        .color(egui::Color32::from_rgb(104, 146, 255)),
                    url,
                );
                ui.label(
                    egui::RichText::new(url)
                        .small()
                        .color(egui::Color32::from_gray(135)),
                );
            });
            root.label(
                egui::RichText::new(
                    "Use this catalogue record to identify the game, then select or drop your compatible local copy here.",
                )
                .small()
                .color(egui::Color32::from_gray(145)),
            );
        }
        if !dialog.entry.known_sha1.is_empty() {
            root.label(
                egui::RichText::new(format!(
                    "{} known dump identit{} available for matching. Alternate revisions are accepted.",
                    dialog.entry.known_sha1.len(),
                    if dialog.entry.known_sha1.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                ))
                .small()
                .color(egui::Color32::from_gray(145)),
            );
            root.horizontal_wrapped(|ui| {
                for sha1 in dialog.entry.known_sha1.iter().take(3) {
                    ui.code(sha1);
                }
            });
        }
        root.add_space(6.0);

        // Any game that is a folder of files (PS3/PS4/Wii U dumps, PC games,
        // PSP homebrew with its assets) imports as a whole. MAME is the
        // exception: it plays the intact ROM-set ZIP.
        let supports_folder = !dialog.entry.system.eq_ignore_ascii_case("mame");
        if dialog.entry.system.eq_ignore_ascii_case("mame") {
            root.label(
                egui::RichText::new(
                    "MAME uses the intact ZIP. Folder import is disabled for this system.",
                )
                .small()
                .color(egui::Color32::from_rgb(238, 177, 89)),
            );
        }
        root.horizontal(|ui| {
            if ui
                .add_enabled(
                    supports_folder && dialog.browser.directory.is_dir() && self.operation.is_none(),
                    egui::Button::new("IMPORT THIS FOLDER").fill(CONTROL_BACKGROUND),
                )
                .on_hover_text(
                    "Imports the whole folder shown below: extracted PS3/PS4/Wii U games, PC games with their DLLs and data, homebrew with its assets.",
                )
                .clicked()
            {
                import = Some((dialog.entry.clone(), dialog.browser.directory.clone()));
            }
            if ui
                .add(egui::Button::new("HOME").fill(CONTROL_BACKGROUND))
                .clicked()
                && let Some(home) = dirs::home_dir()
            {
                dialog.browser.open(home);
            }
            if ui
                .add(egui::Button::new("DOWNLOADS").fill(CONTROL_BACKGROUND))
                .clicked()
                && let Some(downloads) = dirs::download_dir()
            {
                dialog.browser.open(downloads);
            }
            if ui
                .add(egui::Button::new("UP").fill(CONTROL_BACKGROUND))
                .clicked()
                && let Some(parent) = dialog.browser.directory.parent()
            {
                dialog.browser.open(parent.to_owned());
            }
            #[cfg(target_os = "linux")]
            if ui
                .add(egui::Button::new("FILESYSTEM").fill(CONTROL_BACKGROUND))
                .clicked()
            {
                dialog.browser.open(PathBuf::from("/"));
            }
        });

        #[cfg(target_os = "windows")]
        root.horizontal_wrapped(|ui| {
            ui.label("DRIVES");
            for letter in b'C'..=b'Z' {
                let drive = PathBuf::from(format!("{}:\\", letter as char));
                if drive.is_dir()
                    && ui
                        .add(
                            egui::Button::new(format!("{}:", letter as char))
                                .fill(CONTROL_BACKGROUND),
                        )
                        .clicked()
                {
                    dialog.browser.open(drive);
                }
            }
        });

        root.horizontal(|ui| {
            let go_button_width = 48.0;
            let path_width =
                (ui.available_width() - go_button_width - ui.spacing().item_spacing.x).max(180.0);
            let submitted = ui
                .add_sized(
                    [path_width, 24.0],
                    egui::TextEdit::singleline(&mut dialog.browser.path_text)
                        .background_color(INPUT_BACKGROUND),
                )
                .lost_focus()
                && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if (ui
                .add_sized(
                    [go_button_width, 24.0],
                    egui::Button::new("GO").fill(CONTROL_BACKGROUND),
                )
                .clicked()
                || submitted)
                && !dialog.browser.path_text.trim().is_empty()
            {
                let path = PathBuf::from(dialog.browser.path_text.trim());
                if path.is_dir() {
                    dialog.browser.directory = path;
                    dialog.browser.selected = None;
                } else if path.is_file() {
                    dialog.browser.selected = Some(path);
                } else {
                    dialog.message = "That path does not exist.".to_owned();
                }
            }
        });

        root.separator();
        let file_list_height = (root.available_height() - 52.0).max(120.0);
        egui::ScrollArea::vertical()
            .id_salt("import-file-list")
            .auto_shrink([false, false])
            .max_height(file_list_height)
            .show(root, |ui| {
                for (is_directory, name, path) in &directory_entries {
                    let label = if *is_directory {
                        format!("📁  {name}")
                    } else {
                        format!("      {name}")
                    };
                    let selected = dialog.browser.selected.as_ref() == Some(path);
                    let response = ui.selectable_label(selected, label);
                    if response.double_clicked() && !*is_directory && self.operation.is_none() {
                        import = Some((dialog.entry.clone(), path.clone()));
                    } else if response.clicked() {
                        if *is_directory {
                            dialog.browser.open(path.clone());
                        } else {
                            dialog.browser.path_text = path.display().to_string();
                            dialog.browser.selected = Some(path.clone());
                        }
                    }
                }
            });
        root.separator();
        root.horizontal(|ui| {
            if ui
                .add_enabled(
                    dialog
                        .browser
                        .selected
                        .as_ref()
                        .is_some_and(|path| path.is_file())
                        && self.operation.is_none(),
                    egui::Button::new("IMPORT AND PREPARE GAME").fill(ACCENT),
                )
                .clicked()
                && let Some(path) = dialog.browser.selected.clone()
            {
                import = Some((dialog.entry.clone(), path));
            }
            if ui
                .add(egui::Button::new("CANCEL").fill(CONTROL_BACKGROUND))
                .clicked()
            {
                close = true;
            }
        });

        if let Some((entry, path)) = import {
            self.import_dialog = None;
            self.start_import_path(entry, path);
        } else if close {
            self.import_dialog = None;
            self.status = "Import cancelled; no files were changed.".to_owned();
        }
    }

    fn launch_library(&mut self) {
        let layout = self.layout.clone();
        match LaunchPlan::for_current_host(&layout) {
            Ok(plan) => {
                self.status = "Opening RetroBat's library…".to_owned();
                // RetroBat is a separate application; its process is reaped
                // in the background so it never lingers as a zombie.
                let context = self.context.clone();
                let (sender, receiver) = mpsc::channel();
                self.operation = Some(receiver);
                thread::spawn(move || {
                    let result = plan.spawn(&|_| {}).map(|mut child| {
                        thread::spawn(move || {
                            let _ = child.wait();
                        });
                    });
                    let _ = sender.send(match result {
                        Ok(()) => OperationResult {
                            success: true,
                            heading: "RETROBAT LIBRARY OPEN".to_owned(),
                            message: "RetroBat launched with the refreshed game library."
                                .to_owned(),
                        },
                        Err(error) => OperationResult {
                            success: false,
                            heading: "RETROBAT DID NOT START".to_owned(),
                            message: format!("RetroBat could not be launched: {error}"),
                        },
                    });
                    context.request_repaint();
                });
            }
            Err(error) => self.status = format!("Launch failed: {error}"),
        }
    }

    fn launch_game(&mut self, catalog_id: &str, title: &str, system: &str, rom: &std::path::Path) {
        if self.running_game.is_some() {
            self.status = "A game is already loading or running. Close it before starting another."
                .to_owned();
            return;
        }
        let layout = self.layout.clone();
        let preferred_core = self
            .imported_manifests
            .get(catalog_id)
            .and_then(|manifest| manifest.core.clone());
        let backend = self
            .readiness
            .as_ref()
            .and_then(|report| {
                report.select_backend_preferring(system, rom, preferred_core.as_deref())
            })
            .cloned();
        let route_label = backend.as_ref().map(|route| route.label());
        let mut plan =
            match LaunchPlan::for_current_game_with_backend(&layout, system, rom, backend.as_ref())
            {
                Ok(plan) => plan,
                Err(error) => {
                    self.status = format!("Could not prepare {title}: {error}");
                    return;
                }
            };
        if self.gameplay_probe.is_some() {
            plan.enable_retroarch_commands(PROBE_RETROARCH_COMMAND_PORT);
        }
        let context = self.context.clone();
        self.running_game = Some(GameSession::start(plan, catalog_id, title, move || {
            context.request_repaint()
        }));
        self.status = route_label.map_or_else(
            || format!("Loading {title}; its configured backend is starting…"),
            |route| format!("Loading {title} through the installed {route} backend…"),
        );
    }

    fn terminate_running_game(&mut self) {
        let Some(game) = &mut self.running_game else {
            return;
        };
        if game.phase() == SessionPhase::Terminating {
            return;
        }
        game.terminate();
        self.status = format!("Terminating {} and its emulator process tree…", game.title);
    }
}

impl eframe::App for PortableApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(probe) = &mut self.startup_probe
            && !probe.first_frame_recorded
        {
            if let Err(error) = probe.record("first_frame") {
                self.status = format!("Startup probe could not record its first frame: {error}");
            }
            probe.first_frame_recorded = true;
        }
        let loaded = self
            .loading
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some(loaded) = loaded {
            self.catalog = loaded.catalog;
            self.browse = loaded.browse;
            self.readiness = loaded.readiness;
            self.featured_ids = loaded.featured_ids;
            self.search_documents = loaded.search_documents;
            self.imported_manifests = loaded.imported_manifests;
            self.installed_ids = loaded.installed_ids;
            self.controls = Some(loaded.controls);
            self.browse_view_key = None;
            self.browse_systems.clear();
            self.browse_matches.clear();
            self.status = loaded.status;
            self.loading = None;
            if let Some(probe) = &mut self.startup_probe {
                if let Err(error) = probe.record("library_ready") {
                    self.status = format!("Startup probe could not record readiness: {error}");
                }
                probe.library_ready_at = Some(Instant::now());
            }
            context.request_repaint();
        } else if self.loading.is_some() {
            context.request_repaint_after(std::time::Duration::from_millis(50));
        }
        let gameplay_probe_request = self
            .gameplay_probe
            .as_ref()
            .filter(|probe| !probe.started && self.loading.is_none())
            .map(|probe| probe.config.catalog_id.clone());
        if let Some(catalog_id) = gameplay_probe_request {
            let launch = self
                .browse
                .entries
                .iter()
                .find(|entry| entry.id == catalog_id)
                .cloned()
                .and_then(|entry| {
                    self.imported_manifests
                        .get(&catalog_id)
                        .cloned()
                        .map(|manifest| (entry, manifest))
                });
            if let Some(probe) = &mut self.gameplay_probe {
                probe.started = true;
            }
            if let Some((entry, manifest)) = launch {
                let rom = self
                    .layout
                    .clone()
                    .root
                    .join(&manifest.launch_relative_path);
                self.launch_game(&entry.id, &entry.title, &manifest.system, &rom);
                if self.running_game.is_some() {
                    if let Some(probe) = &self.gameplay_probe {
                        let _ = probe.record(
                            "launch_requested",
                            serde_json::json!({
                                "system": manifest.system,
                                "launch_file": rom.display().to_string(),
                                "status": self.status,
                            }),
                        );
                    }
                } else if let Some(probe) = &self.gameplay_probe {
                    probe.fail(
                        "launch_failed",
                        serde_json::json!({ "status": self.status }),
                    );
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else if let Some(probe) = &self.gameplay_probe {
                probe.fail("imported_game_not_found", serde_json::json!({}));
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        let completed_operation = self
            .operation
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some(result) = completed_operation {
            self.status = result.message.clone();
            self.operation_notice = Some(result);
            self.operation = None;
            self.imported_manifests = imported_manifests(&self.layout);
            self.installed_ids = installed_trusted_ids(&self.layout, &self.catalog);
            self.retrobat_present = self.layout.retrobat_executable().is_file();
            self.browse_view_key = None;
            self.refresh_readiness();
        }
        if self.operation.is_some() {
            context.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let refreshed_readiness = self
            .readiness_refresh
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some(result) = refreshed_readiness {
            self.readiness_refresh = None;
            match result {
                Ok(report) => self.readiness = Some(report),
                Err(error) => {
                    self.status = format!("{} Readiness refresh failed: {error}", self.status)
                }
            }
        } else if self.readiness_refresh.is_some() {
            context.request_repaint_after(Duration::from_millis(100));
        }
        let session_events = self
            .running_game
            .as_mut()
            .map(GameSession::poll)
            .unwrap_or_default();
        for event in session_events {
            match event {
                SessionEvent::Phase(phase) => {
                    if let Some(probe) = &self.gameplay_probe {
                        let _ = probe.record("phase", serde_json::json!({ "phase": phase }));
                    }
                    self.status = phase;
                }
                SessionEvent::Started { process_id } => {
                    if let Some(probe) = &mut self.gameplay_probe {
                        // The measured window starts with the backend itself,
                        // not with first-time preparation such as Wine setup.
                        let (sender, receiver) = mpsc::channel();
                        let duration = probe.config.duration;
                        let repaint = self.context.clone();
                        thread::spawn(move || {
                            thread::sleep(duration);
                            let _ = sender.send(());
                            repaint.request_repaint();
                        });
                        probe.deadline_receiver = Some(receiver);
                        let _ = probe.record(
                            "game_started",
                            serde_json::json!({ "process_id": process_id }),
                        );
                    }
                }
                SessionEvent::Exited { message, failed } => {
                    let finished = self.running_game.take();
                    if let (Some(probe), Some(game)) = (&mut self.gameplay_probe, finished)
                        && probe.started
                        && !probe.complete_recorded
                    {
                        let evidence = game.log_file.as_deref().map(backend_log_evidence);
                        let tree_running = game.tree_is_running();
                        let detail = serde_json::json!({
                            "status": message,
                            "failed": failed,
                            "process_tree_running": tree_running,
                            "backend_log": evidence,
                        });
                        if !probe.terminating {
                            probe.fail("exited_before_deadline", detail);
                        } else if tree_running {
                            probe.fail("process_tree_survived_termination", detail);
                        } else if evidence
                            .as_ref()
                            .is_some_and(|evidence| !evidence.content_loaded)
                        {
                            probe.fail("backend_did_not_load_content", detail);
                        } else {
                            let _ = probe.record("terminated", detail);
                        }
                        probe.terminating = true;
                    }
                    self.status = message;
                }
            }
        }
        let gameplay_probe_deadline = self
            .gameplay_probe
            .as_ref()
            .and_then(|probe| probe.deadline_receiver.as_ref())
            .is_some_and(|receiver| receiver.try_recv().is_ok());
        if gameplay_probe_deadline {
            let alive = self.running_game.as_ref().map(GameSession::tree_is_running);
            let retroarch = self
                .running_game
                .as_ref()
                .is_some_and(|game| game.log_file.is_some());
            let screenshots = self.layout.clone().retrobat_root().join("screenshots");
            let mut terminate_now = true;
            if let Some(probe) = &mut self.gameplay_probe {
                probe.deadline_receiver = None;
                match alive {
                    Some(true) => {
                        let _ = probe.record("alive_at_deadline", serde_json::json!({}));
                        if retroarch {
                            let (sender, receiver) = mpsc::channel();
                            let repaint = self.context.clone();
                            thread::spawn(move || {
                                let _ = sender.send(capture_retroarch_screenshot(screenshots));
                                repaint.request_repaint();
                            });
                            probe.screenshot_receiver = Some(receiver);
                            terminate_now = false;
                        }
                    }
                    _ => probe.fail(
                        "not_running_at_deadline",
                        serde_json::json!({ "status": self.status }),
                    ),
                }
                probe.terminating = terminate_now;
            }
            if terminate_now {
                self.terminate_running_game();
            }
        }
        let screenshot = self
            .gameplay_probe
            .as_ref()
            .and_then(|probe| probe.screenshot_receiver.as_ref())
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some(screenshot) = screenshot {
            if let Some(probe) = &mut self.gameplay_probe {
                probe.screenshot_receiver = None;
                probe.terminating = true;
                if screenshot.get("path").is_some() && screenshot.get("error").is_none() {
                    let _ = probe.record("screenshot", screenshot);
                } else {
                    probe.fail("screenshot_failed", screenshot);
                }
            }
            self.terminate_running_game();
        } else if self
            .gameplay_probe
            .as_ref()
            .is_some_and(|probe| probe.screenshot_receiver.is_some())
        {
            context.request_repaint_after(Duration::from_millis(100));
        }
        if let Some(game) = &self.running_game {
            // Do not continuously render the frontend while a fullscreen game
            // covers it. On native Wayland, presenting an occluded GL surface
            // can wait on compositor/GPU frame availability long enough to
            // prevent winit from answering xdg_wm_base pings. The session
            // requests a repaint for every event; only the loading,
            // preparation, and forced-termination transitions need timers.
            if let Some(delay) = active_game_repaint_delay(game.age(), game.phase()) {
                context.request_repaint_after(delay);
            }
        }
        if self
            .gameplay_probe
            .as_ref()
            .is_some_and(|probe| probe.terminating && !probe.complete_recorded)
            && self.running_game.is_none()
        {
            if let Some(probe) = &mut self.gameplay_probe {
                let outcome = if GAMEPLAY_PROBE_FAILED.load(Ordering::SeqCst) {
                    "gameplay_probe_failed"
                } else {
                    "gameplay_probe_complete"
                };
                let _ = probe.record(outcome, serde_json::json!({}));
                probe.complete_recorded = true;
            }
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Bound texture uploads per frame. Decoding already happened on a worker;
        // uploading an unbounded completion burst here would still freeze input.
        for _ in 0..2 {
            let Ok(message) = self.artwork_receiver.try_recv() else {
                break;
            };
            self.artwork_pending = self.artwork_pending.saturating_sub(1);
            self.artwork_inflight.remove(&message.entry_id);
            match message.result {
                ArtworkResult::Decoded(decoded) => {
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied(decoded.size, &decoded.rgba);
                    let texture = context.load_texture(
                        format!("artwork/{}", message.entry_id),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.textures
                        .insert(message.entry_id, (texture, self.frame_number));
                }
                ArtworkResult::Failed(error) => {
                    self.artwork_errors.insert(message.entry_id, error);
                }
                ArtworkResult::Skipped => {}
            }
            context.request_repaint();
        }
        if self.artwork_pending > 0 {
            context.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if let Some(probe) = &mut self.startup_probe
            && let Some(ready_at) = probe.library_ready_at
            && probe.library_rendered_recorded
        {
            if ready_at.elapsed() >= std::time::Duration::from_secs(2) {
                if !probe.post_load_responsive_recorded {
                    if let Err(error) = probe.record("post_load_responsive") {
                        self.status =
                            format!("Startup probe could not record responsiveness: {error}");
                    }
                    probe.post_load_responsive_recorded = true;
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else {
                context.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let panel = egui::Frame::new()
            .fill(egui::Color32::from_rgb(13, 16, 23))
            .inner_margin(24);
        egui::CentralPanel::default().frame(panel).show(root, |ui| {
            if self.loading.is_some() {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.28).max(40.0));
                    ui.spinner();
                    ui.add_space(12.0);
                    ui.heading("Preparing the portable library");
                    ui.label(
                        egui::RichText::new(
                            "Loading 80,734 catalogue records and auditing installed emulator routes in the background…",
                        )
                        .color(egui::Color32::from_gray(155)),
                    );
                    ui.label(
                        egui::RichText::new("The window remains responsive while this completes.")
                            .small()
                            .color(egui::Color32::from_gray(125)),
                    );
                });
                return;
            }
            if self.controls_dialog.is_some() {
                self.show_controls_dialog(ui);
                return;
            }
            if self.firmware_dialog.is_some() {
                self.show_firmware_dialog(ui);
                return;
            }
            if self.import_dialog.is_some() {
                self.show_import_dialog(ui);
                return;
            }
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("RETRO")
                        .size(28.0)
                        .strong()
                        .color(ACCENT),
                );
                ui.label(
                    egui::RichText::new("PORT")
                        .size(28.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
            });
            ui.label(
                egui::RichText::new(
                    "One visual library for verified downloads and your own imported games.",
                )
                    .size(15.0)
                    .color(egui::Color32::from_gray(165)),
            );
            ui.horizontal(|ui| {
                let can_launch = self.retrobat_present;
                if ui
                    .add_enabled(
                        can_launch && self.operation.is_none(),
                        egui::Button::new(
                            egui::RichText::new("▶  PLAY LIBRARY")
                                .strong()
                                .color(egui::Color32::WHITE),
                        )
                        .fill(ACCENT)
                        .min_size(egui::vec2(150.0, 34.0)),
                    )
                    .clicked()
                {
                    self.launch_library();
                }
                if !can_launch {
                    ui.label(
                        egui::RichText::new("RETROBAT SETUP NEEDED")
                            .small()
                            .color(egui::Color32::from_rgb(238, 177, 89)),
                    );
                }
            });
            let library_viewport_width = ui.available_width();
            egui::ScrollArea::vertical()
                .id_salt("library-body")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(20.0);

                    ui.horizontal_wrapped(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.search)
                                .hint_text("Search games, systems, or developers…")
                                .desired_width(420.0)
                                .background_color(INPUT_BACKGROUND),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{} TITLES  ·  {} SOURCES  ·  {} VERIFIED DOWNLOADS",
                                self.browse.entries.len(),
                                self.browse.sources.len(),
                                retrobat_portable::browse_install::ledger()
                                    .map_or(0, |ledger| ledger.entries.len())
                            ))
                            .small()
                            .color(egui::Color32::from_gray(145)),
                        );
                    });
                    ui.add_space(14.0);

                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("DISCOVER")
                                .size(18.0)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.label(
                            egui::RichText::new("ALL CATALOGUES")
                                .small()
                                .color(egui::Color32::from_gray(135)),
                        );
                    });
                    ui.label(
                        egui::RichText::new(
                            "Search spans every source, system, developer, year, genre, and license.",
                        )
                        .small()
                        .color(egui::Color32::from_gray(140)),
                    );
                    ui.add_space(8.0);

                    ui.scope(|ui| {
                        ui.spacing_mut().scroll = filter_scroll_style();
                        egui::ScrollArea::horizontal()
                            .id_salt("source-filters")
                            .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(
                                        self.source_filter == "featured",
                                        format!("FEATURED  {}", self.featured_ids.len()),
                                    )
                                    .clicked()
                                {
                                    self.source_filter = "featured".to_owned();
                                    self.system_filter = "all".to_owned();
                                    self.browse_page = 0;
                                }
                                if ui
                                    .selectable_label(self.source_filter == "all", "ALL SOURCES")
                                    .clicked()
                                {
                                    self.source_filter = "all".to_owned();
                                    self.system_filter = "all".to_owned();
                                    self.browse_page = 0;
                                }
                                for source in &self.browse.sources {
                                    let selected = self.source_filter == source.id;
                                    let label =
                                        format!("{}  {}", source.name, source.entry_count);
                                    if ui.selectable_label(selected, label).clicked() {
                                        self.source_filter = source.id.clone();
                                        self.system_filter = "all".to_owned();
                                        self.browse_page = 0;
                                    }
                                }
                            });
                        });
                    });

                    self.refresh_browse_view();
                    let systems = self.browse_systems.clone();
                    ui.scope(|ui| {
                        ui.spacing_mut().scroll = filter_scroll_style();
                        egui::ScrollArea::horizontal()
                            .id_salt("system-filters")
                            .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("SYSTEM")
                                        .small()
                                        .strong()
                                        .color(egui::Color32::from_gray(125)),
                                );
                                if ui
                                    .selectable_label(self.system_filter == "all", "ALL")
                                    .clicked()
                                {
                                    self.system_filter = "all".to_owned();
                                    self.browse_page = 0;
                                }
                                for system in systems {
                                    let selected = self.system_filter == system;
                                    if ui
                                        .selectable_label(selected, system.to_ascii_uppercase())
                                        .clicked()
                                    {
                                        self.system_filter = system;
                                        self.browse_page = 0;
                                    }
                                }
                            });
                        });
                    });
                    ui.add_space(8.0);

                    const BROWSE_PAGE_SIZE: usize = 30;
                    self.refresh_browse_view();
                    let page_count = self
                        .browse_matches
                        .len()
                        .div_ceil(BROWSE_PAGE_SIZE)
                        .max(1);
                    let match_count = self.browse_matches.len();
                    self.browse_page = self.browse_page.min(page_count - 1);
                    let page_start = self.browse_page * BROWSE_PAGE_SIZE;
                    let page: Vec<usize> = self
                        .browse_matches
                        .iter()
                        .copied()
                        .skip(page_start)
                        .take(BROWSE_PAGE_SIZE)
                        .collect();
                    if self.browse_page_key != Some((self.browse_view_key.clone(), self.browse_page)) {
                        // A new page makes queued artwork for the old one stale.
                        self.browse_page_key = Some((self.browse_view_key.clone(), self.browse_page));
                        self.artwork_generation.fetch_add(1, Ordering::SeqCst);
                    }
                    let (grid_columns, card_width, grid_spacing) =
                        browse_grid_geometry(library_viewport_width);
                    let artwork_height = (card_width * 2.0 / 3.0).round();
                    let mut actions = Vec::new();
                    let mut artwork_requests = Vec::new();
                    self.frame_number += 1;
                    egui::Grid::new("browse-card-grid")
                        .num_columns(grid_columns)
                        .spacing([grid_spacing, grid_spacing])
                        .show(ui, |ui| {
                            for (card_index, &entry_index) in page.iter().enumerate() {
                                let entry = &self.browse.entries[entry_index];
                                if let Some(request) = self.card(ui, entry, card_width, artwork_height, &mut actions) {
                                    artwork_requests.push(request);
                                }
                                if (card_index + 1) % grid_columns == 0 {
                                    ui.end_row();
                                }
                            }
                        });
                    for &entry_index in &page {
                        if let Some(texture) = self.textures.get_mut(&self.browse.entries[entry_index].id) {
                            texture.1 = self.frame_number;
                        }
                    }
                    self.evict_textures();
                    for action in actions {
                        self.perform(action);
                    }
                    self.start_browse_artwork(artwork_requests);

                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(self.browse_page > 0, egui::Button::new("PREVIOUS"))
                            .clicked()
                        {
                            self.browse_page -= 1;
                        }
                        ui.label(format!(
                            "Page {} of {} / {} matching titles",
                            self.browse_page + 1,
                            page_count,
                            match_count
                        ));
                        if ui
                            .add_enabled(
                                self.browse_page + 1 < page_count,
                                egui::Button::new("NEXT"),
                            )
                            .clicked()
                        {
                            self.browse_page += 1;
                        }
                    });
                    ui.add_space(12.0);
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(17, 21, 29))
                        .corner_radius(8)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(&self.status)
                                    .small()
                                    .color(egui::Color32::from_gray(170)),
                            );
                        });

                    ui.add_space(8.0);
                    if let Some(readiness) = &self.readiness {
                        ui.collapsing("SYSTEM READINESS", |ui| {
                            ui.label(format!(
                                "{} titles have an installed backend · {} have no installed emulator · {} remain unresolved",
                                readiness.ready_now_entries,
                                readiness.emulator_missing_entries,
                                readiness.unresolved_entries
                            ));
                            ui.label(
                                egui::RichText::new(
                                    "Firmware status comes from the selected installed core's own metadata. Optional firmware is inventoried separately and never creates a false playability warning.",
                                )
                                .small()
                                .color(egui::Color32::from_gray(145)),
                            );
                            let attention = readiness
                                .systems
                                .iter()
                                .filter(|system| {
                                    system.backend != BackendState::ReadyNow
                                        || matches!(
                                            system.firmware,
                                            FirmwareState::RequiredMissing
                                                | FirmwareState::SomeRequiredPresent
                                        )
                                })
                                .take(18)
                                .collect::<Vec<_>>();
                            for system in attention {
                                let backend = match system.backend {
                                    BackendState::ReadyNow => "backend ready",
                                    BackendState::EmulatorMissing => "emulator not installed",
                                    BackendState::Unresolved => "backend unresolved",
                                };
                                let firmware = match system.firmware {
                                    FirmwareState::NotRequired => String::new(),
                                    FirmwareState::AllRequiredPresent => {
                                        " · required firmware ready".to_owned()
                                    }
                                    FirmwareState::SomeRequiredPresent => format!(
                                        " · required firmware {}/{} present",
                                        system.firmware_detected, system.firmware_candidates
                                    ),
                                    FirmwareState::RequiredMissing => format!(
                                        " · 0/{} required firmware files present",
                                        system.firmware_candidates
                                    ),
                                };
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} · {} title(s) · {backend}{firmware}",
                                        system.catalog_system.to_ascii_uppercase(),
                                        system.entry_count
                                    ))
                                    .small()
                                    .color(egui::Color32::from_gray(155)),
                                );
                            }
                            if readiness.systems.len() > 18 {
                                ui.label(
                                    egui::RichText::new(
                                        "Game cards and Import screens show the status for every remaining system.",
                                    )
                                    .small()
                                    .color(egui::Color32::from_gray(130)),
                                );
                            }
                        });
                    }
                    ui.add_space(8.0);
                    ui.collapsing("Installation", |ui| {
                        // The installation is the folder this launcher lives
                        // in; RetroPort never switches to another one.
                        ui.horizontal_wrapped(|ui| {
                            ui.label("This installation:");
                            ui.label(
                                egui::RichText::new(self.layout.root.display().to_string())
                                    .monospace(),
                            );
                        });
                        if !self.retrobat_present {
                            ui.label("RetroBat/RetroBat.exe is missing from this installation; run its bootstrap or verifier.");
                        }
                    });
                });
        });
        if let Some(notice) = &self.operation_notice {
            let mut dismiss = false;
            let color = if notice.success {
                egui::Color32::from_rgb(98, 211, 145)
            } else {
                egui::Color32::from_rgb(235, 113, 113)
            };
            egui::Window::new(&notice.heading)
                .id(egui::Id::new("operation-result"))
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .collapsible(false)
                .resizable(false)
                .show(root.ctx(), |ui| {
                    ui.set_max_width(520.0);
                    ui.label(egui::RichText::new(&notice.message).color(color));
                    ui.add_space(10.0);
                    if ui
                        .add_sized(
                            [140.0, 32.0],
                            egui::Button::new("OK").fill(CONTROL_BACKGROUND),
                        )
                        .clicked()
                    {
                        dismiss = true;
                    }
                });
            if dismiss {
                self.operation_notice = None;
            }
        }
        if self.loading.is_none()
            && let Some(probe) = &mut self.startup_probe
            && !probe.library_rendered_recorded
        {
            if let Err(error) = probe.record("library_rendered") {
                self.status = format!("Startup probe could not record rendered library: {error}");
            }
            probe.library_rendered_recorded = true;
            root.ctx().request_repaint();
        }
    }
}

fn decode_artwork_for_texture(bytes: &[u8]) -> image::ImageResult<DecodedArtwork> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4_096);
    limits.max_image_height = Some(4_096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode()?;
    // Cards are at most a few hundred points wide; 512 px stays sharp at 2x
    // while keeping each texture under 1 MiB.
    let decoded = if decoded.width() > 512 || decoded.height() > 512 {
        decoded.resize(512, 512, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    let rgba = decoded.to_rgba8();
    Ok(DecodedArtwork {
        size: [rgba.width() as usize, rgba.height() as usize],
        rgba: rgba.into_raw(),
    })
}

fn browse_grid_geometry(viewport_width: f32) -> (usize, f32, f32) {
    const MIN_CARD_WIDTH: f32 = 180.0;
    const CARD_MARGIN: f32 = 20.0;
    const GRID_SPACING: f32 = 12.0;
    let columns = ((viewport_width + GRID_SPACING) / (MIN_CARD_WIDTH + CARD_MARGIN + GRID_SPACING))
        .floor()
        .max(1.0) as usize;
    let card_width = ((viewport_width - GRID_SPACING * columns.saturating_sub(1) as f32 - 2.0)
        / columns as f32
        - CARD_MARGIN)
        .max(MIN_CARD_WIDTH)
        .floor();
    (columns, card_width, GRID_SPACING)
}

fn filter_scroll_style() -> egui::style::ScrollStyle {
    let mut scroll = egui::style::ScrollStyle::solid();
    scroll.bar_width = 3.0;
    scroll.bar_inner_margin = 3.0;
    scroll.bar_outer_margin = 1.0;
    scroll
}

fn generated_artwork_title_layout(
    painter: &egui::Painter,
    rect: egui::Rect,
    title: &str,
    font_id: egui::FontId,
    color: egui::Color32,
) -> (Arc<egui::Galley>, egui::Rect) {
    let title_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(12.0, 42.0),
        rect.right_bottom() - egui::vec2(12.0, 24.0),
    );
    let mut job =
        egui::text::LayoutJob::simple(title.to_owned(), font_id, color, title_rect.width());
    job.halign = egui::Align::Center;
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    (painter.layout_job(job), title_rect)
}

fn generated_artwork_title_position(title: &egui::Galley, title_rect: egui::Rect) -> egui::Pos2 {
    egui::pos2(
        title_rect.center().x,
        title_rect.center().y - title.rect.height() / 2.0,
    )
}

fn paint_generated_artwork(painter: &egui::Painter, rect: egui::Rect, entry: &BrowseEntry) {
    const PALETTES: [(egui::Color32, egui::Color32, egui::Color32); 6] = [
        (
            egui::Color32::from_rgb(35, 55, 91),
            egui::Color32::from_rgb(83, 126, 201),
            egui::Color32::from_rgb(189, 216, 255),
        ),
        (
            egui::Color32::from_rgb(65, 39, 82),
            egui::Color32::from_rgb(151, 87, 186),
            egui::Color32::from_rgb(239, 205, 255),
        ),
        (
            egui::Color32::from_rgb(31, 68, 61),
            egui::Color32::from_rgb(65, 157, 126),
            egui::Color32::from_rgb(196, 247, 225),
        ),
        (
            egui::Color32::from_rgb(83, 48, 31),
            egui::Color32::from_rgb(197, 112, 55),
            egui::Color32::from_rgb(255, 220, 184),
        ),
        (
            egui::Color32::from_rgb(72, 34, 43),
            egui::Color32::from_rgb(190, 72, 96),
            egui::Color32::from_rgb(255, 205, 214),
        ),
        (
            egui::Color32::from_rgb(43, 51, 61),
            egui::Color32::from_rgb(106, 124, 146),
            egui::Color32::from_rgb(226, 235, 245),
        ),
    ];
    let hash = entry.id.bytes().fold(0_u64, |value, byte| {
        value.wrapping_mul(109).wrapping_add(u64::from(byte))
    });
    let (base, accent, ink) = PALETTES[hash as usize % PALETTES.len()];
    painter.rect_filled(rect, 7, base);
    let stripe_width = (rect.width() / 8.0).max(8.0);
    for index in 0..4 {
        let left = rect.left() + (index as f32 * 2.0 + 1.0) * stripe_width;
        let stripe = egui::Rect::from_min_max(
            egui::pos2(left, rect.top()),
            egui::pos2((left + stripe_width).min(rect.right()), rect.bottom()),
        );
        painter.rect_filled(stripe, 0, accent.gamma_multiply(0.22));
    }
    let badge = egui::Rect::from_min_size(
        rect.min + egui::vec2(10.0, 10.0),
        egui::vec2((rect.width() - 20.0).min(96.0), 22.0),
    );
    painter.rect_filled(badge, 4, accent);
    painter.text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        entry.system.to_ascii_uppercase(),
        egui::FontId::monospace(10.0),
        egui::Color32::WHITE,
    );
    let (title, title_rect) = generated_artwork_title_layout(
        painter,
        rect,
        &entry.title,
        egui::FontId::proportional((rect.width() / 13.0).clamp(13.0, 20.0)),
        ink,
    );
    let title_position = generated_artwork_title_position(&title, title_rect);
    painter
        .with_clip_rect(title_rect)
        .galley(title_position, title, ink);
    painter.text(
        rect.left_bottom() + egui::vec2(10.0, -9.0),
        egui::Align2::LEFT_BOTTOM,
        "CATALOGUE ART",
        egui::FontId::monospace(8.0),
        ink.gamma_multiply(0.65),
    );
}

#[cfg(test)]
mod ui_tests {
    use super::*;

    #[test]
    fn card_grid_fills_windowed_and_wide_viewports_without_overflow() {
        for (width, expected_columns) in [(588.0, 2), (1_228.0, 5), (1_860.0, 8)] {
            let (columns, card_width, spacing) = browse_grid_geometry(width);
            assert_eq!(columns, expected_columns);
            let used =
                columns as f32 * (card_width + 20.0) + columns.saturating_sub(1) as f32 * spacing;
            assert!(used <= width);
            assert!(width - used <= columns as f32 + 2.0);
        }
    }

    #[test]
    fn filter_scrollbars_reserve_space_and_never_expand_over_labels() {
        let scroll = filter_scroll_style();
        assert!(!scroll.floating);
        assert_eq!(scroll.bar_width, 3.0);
        assert_eq!(scroll.allocated_width(), 7.0);
    }

    #[test]
    fn long_generated_artwork_titles_wrap_inside_the_artwork() {
        let context = egui::Context::default();
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(180.0, 120.0));
            for name in [
                "Adventure (USA) (Atari Flashback)",
                "Advance Wars (Europe) (GBA) (Virtual Console) (eShop)",
                "Alone in the Dark (USA, Europe)",
            ] {
                let (title, title_rect) = generated_artwork_title_layout(
                    ui.painter(),
                    rect,
                    name,
                    egui::FontId::proportional(14.0),
                    egui::Color32::WHITE,
                );

                assert!(title.rows.len() > 1, "{name}");
                assert!(title.rows.len() <= 2, "{name}");
                assert!(title.size().x <= title_rect.width() + 0.5, "{name}");
                assert!(title.size().y <= title_rect.height() + 0.5, "{name}");
                assert!(rect.contains_rect(title_rect), "{name}");
                let painted_title = title
                    .rect
                    .translate(generated_artwork_title_position(&title, title_rect).to_vec2());
                assert!(
                    title_rect.expand(0.5).contains_rect(painted_title),
                    "{name} paints at {painted_title:?}, outside {title_rect:?}"
                );
            }
        });
    }

    #[test]
    fn play_is_disabled_as_soon_as_a_game_starts_loading() {
        let (label, intent) = game_button_state(
            Some((
                "mspacman",
                SessionPhase::Preparing,
                Duration::from_millis(10),
            )),
            "mspacman",
        );
        assert_eq!(label, "⏳  LOADING…");
        assert_eq!(intent, GameButtonIntent::Disabled);

        let (other_label, other_intent) = game_button_state(
            Some(("mspacman", SessionPhase::Running, Duration::from_secs(10))),
            "pacman",
        );
        assert_eq!(other_label, "GAME RUNNING");
        assert_eq!(other_intent, GameButtonIntent::Disabled);
    }

    #[test]
    fn running_game_exposes_terminate_instead_of_a_second_play() {
        let (label, intent) = game_button_state(
            Some(("mspacman", SessionPhase::Running, Duration::from_secs(10))),
            "mspacman",
        );
        assert_eq!(label, "■  TERMINATE");
        assert_eq!(intent, GameButtonIntent::Terminate);
    }

    #[test]
    fn play_becomes_available_again_after_the_child_exits() {
        let (label, intent) = game_button_state(None, "mspacman");
        assert_eq!(label, "▶  PLAY");
        assert_eq!(intent, GameButtonIntent::Play);
    }

    #[test]
    fn small_imports_never_round_down_to_zero_megabytes() {
        assert_eq!(format_import_size(0), "0 B");
        assert_eq!(format_import_size(4_096), "4 KiB");
        assert_eq!(format_import_size(1_572_864), "1.50 MiB");
    }

    #[test]
    fn remove_exists_only_for_idle_imported_cards() {
        assert!(!remove_import_available(false, false, false));
        assert!(remove_import_available(true, false, false));
        assert!(!remove_import_available(true, true, false));
        assert!(!remove_import_available(true, false, true));
    }

    #[test]
    fn fullscreen_game_does_not_keep_redrawing_the_covered_frontend() {
        assert_eq!(
            active_game_repaint_delay(Duration::from_secs(1), SessionPhase::Running),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            active_game_repaint_delay(Duration::from_secs(6), SessionPhase::Running),
            None
        );
        assert_eq!(
            active_game_repaint_delay(Duration::from_secs(6), SessionPhase::Terminating),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn app_construction_starts_with_background_loading_instead_of_parsing_inline() {
        let root = tempfile::tempdir().unwrap();
        let context = egui::Context::default();
        let app = PortableApp::new(PortableLayout::new(root.path()), &context, None, None);
        assert!(app.loading.is_some());
        assert!(app.browse.entries.is_empty());
    }

    #[test]
    fn artwork_is_decoded_and_downscaled_before_reaching_the_ui_thread() {
        let source = image::DynamicImage::new_rgba8(1_536, 1_024);
        let mut encoded = Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();

        let decoded = decode_artwork_for_texture(encoded.get_ref()).unwrap();
        assert!(decoded.size[0] <= 512);
        assert!(decoded.size[1] <= 512);
        assert_eq!(decoded.rgba.len(), decoded.size[0] * decoded.size[1] * 4);
    }
}
