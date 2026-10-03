#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use retrobat_portable::artwork::{load_bundled_artwork, load_or_fetch, load_snapshot_artwork};
use retrobat_portable::browse::{
    Acquisition, BrowseCatalog, BrowseEntry, BundledArtwork, InstallState,
};
use retrobat_portable::browse_install::{BrowseInstaller, supports_direct_download};
use retrobat_portable::catalog::{Artwork, Catalog, CatalogEntry};
use retrobat_portable::controller_guard::ControllerMouseGuard;
use retrobat_portable::controls::{ControlsCatalog, GameControls};
use retrobat_portable::featured::FeaturedCatalog;
use retrobat_portable::firmware::{import_firmware, install_official_firmware};
use retrobat_portable::import::{
    GameImporter, ImportedManifest, imported_manifests, remove_import,
};
use retrobat_portable::install::{Installer, ReqwestDownloader, is_installed};
use retrobat_portable::launch::{LaunchPlan, process_tree_is_running, terminate_process_tree_id};
use retrobat_portable::paths::PortableLayout;
use retrobat_portable::readiness::{
    BackendState, FirmwareFileStatus, FirmwareInstallAction, FirmwareState, ReadinessReport,
    SystemReadiness,
};

const INPUT_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(7, 9, 13);
const CONTROL_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(36, 43, 58);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(104, 146, 255);

fn main() -> eframe::Result {
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
    let mut import_file = None;
    let mut startup_probe_output = None;
    let mut gameplay_probe_id = None;
    let mut gameplay_probe_output = None;
    let mut gameplay_probe_seconds = 20u64;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--bundle-root" => {
                if let Some(value) = args.next() {
                    bundle_root = PathBuf::from(value);
                }
            }
            "--self-check" => self_check_only = true,
            "--install" => {
                install_id = args.next();
                if install_id.is_none() {
                    eprintln!("--install requires a catalog id");
                    std::process::exit(2);
                }
            }
            "--uninstall" => {
                uninstall_id = args.next();
                if uninstall_id.is_none() {
                    eprintln!("--uninstall requires a catalog id");
                    std::process::exit(2);
                }
            }
            "--download" => {
                download_id = args.next();
                if download_id.is_none() {
                    eprintln!("--download requires a browse catalog id");
                    std::process::exit(2);
                }
            }
            "--import" => {
                import_id = args.next();
                if import_id.is_none() {
                    eprintln!("--import requires a browse catalog id");
                    std::process::exit(2);
                }
            }
            "--file" => {
                import_file = args.next().map(PathBuf::from);
                if import_file.is_none() {
                    eprintln!("--file requires a local game path");
                    std::process::exit(2);
                }
            }
            "--self-check-output" => {
                self_check_output = args.next().map(PathBuf::from);
                if self_check_output.is_none() {
                    eprintln!("--self-check-output requires a path");
                    std::process::exit(2);
                }
            }
            "--startup-probe-output" => {
                startup_probe_output = args.next().map(PathBuf::from);
                if startup_probe_output.is_none() {
                    eprintln!("--startup-probe-output requires a path");
                    std::process::exit(2);
                }
            }
            "--gameplay-probe" => {
                gameplay_probe_id = args.next();
                if gameplay_probe_id.is_none() {
                    eprintln!("--gameplay-probe requires an imported browse catalog id");
                    std::process::exit(2);
                }
            }
            "--gameplay-probe-output" => {
                gameplay_probe_output = args.next().map(PathBuf::from);
                if gameplay_probe_output.is_none() {
                    eprintln!("--gameplay-probe-output requires a path");
                    std::process::exit(2);
                }
            }
            "--gameplay-probe-seconds" => {
                gameplay_probe_seconds = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .filter(|seconds| *seconds >= 10)
                    .unwrap_or_else(|| {
                        eprintln!("--gameplay-probe-seconds requires an integer of at least 10");
                        std::process::exit(2);
                    });
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(2);
            }
        }
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
        "RetroBat Portable",
        options,
        Box::new(move |creation_context| {
            Ok(Box::new(PortableApp::new(
                layout,
                &creation_context.egui_ctx,
                startup_probe_output,
                gameplay_probe,
            )))
        }),
    )
}

struct ArtworkMessage {
    entry_id: String,
    result: Result<DecodedArtwork, String>,
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
}

// Two readers bound storage and image-decoding pressure. Four simultaneous
// reads were enough to starve the desktop event loop on slower storage and
// trigger the compositor's "Application Not Responding" watchdog.
const ARTWORK_WORKERS: usize = 2;
const ARTWORK_QUEUE_CAPACITY: usize = 64;

fn start_artwork_workers(
    layout: PortableLayout,
    completed: mpsc::Sender<ArtworkMessage>,
) -> SyncSender<ArtworkJob> {
    let (jobs, receiver) = mpsc::sync_channel::<ArtworkJob>(ARTWORK_QUEUE_CAPACITY);
    let receiver = Arc::new(Mutex::new(receiver));
    for worker_index in 0..ARTWORK_WORKERS {
        let receiver = Arc::clone(&receiver);
        let completed = completed.clone();
        let layout = layout.clone();
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
                        ArtworkSource::Bundled(artwork) => load_bundled_artwork(&layout, &artwork)
                            .map_err(|error| error.to_string()),
                    };
                    let result = bytes.and_then(|bytes| {
                        decode_artwork_for_texture(&bytes).map_err(|error| error.to_string())
                    });
                    if completed
                        .send(ArtworkMessage {
                            entry_id: job.entry_id,
                            result,
                        })
                        .is_err()
                    {
                        return;
                    }
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
    selected_firmware: usize,
    browser: FileBrowser,
    message: String,
}

struct LoadedLibrary {
    catalog: Catalog,
    browse: BrowseCatalog,
    readiness: Option<ReadinessReport>,
    featured_ids: HashSet<String>,
    search_documents: Vec<String>,
    imported_manifests: BTreeMap<String, ImportedManifest>,
    controls: ControlsCatalog,
    status: String,
}

struct OperationResult {
    success: bool,
    heading: String,
    message: String,
}

struct RunningGame {
    catalog_id: String,
    title: String,
    process_id: u32,
    exit_receiver: Receiver<String>,
    launched_at: Instant,
    termination_requested_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GameButtonIntent {
    Play,
    Terminate,
    Disabled,
}

fn game_button_state(
    active: Option<(&str, Duration, bool)>,
    card_id: &str,
) -> (String, GameButtonIntent) {
    match active {
        Some((id, _, true)) if id == card_id => {
            ("■  TERMINATING…".to_owned(), GameButtonIntent::Disabled)
        }
        Some((id, elapsed, false)) if id == card_id && elapsed < Duration::from_secs(5) => {
            ("⏳  LOADING…".to_owned(), GameButtonIntent::Disabled)
        }
        Some((id, _, false)) if id == card_id => {
            ("■  TERMINATE".to_owned(), GameButtonIntent::Terminate)
        }
        Some(_) => ("GAME RUNNING".to_owned(), GameButtonIntent::Disabled),
        None => ("▶  PLAY".to_owned(), GameButtonIntent::Play),
    }
}

fn active_game_repaint_delay(elapsed: Duration, terminating: bool) -> Option<Duration> {
    (elapsed < Duration::from_secs(5) || terminating).then_some(Duration::from_millis(100))
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

struct GameplayProbe {
    config: GameplayProbeConfig,
    started: bool,
    deadline_receiver: Option<Receiver<()>>,
    terminating: bool,
    complete_recorded: bool,
}

impl GameplayProbe {
    fn new(config: GameplayProbeConfig) -> Self {
        let _ = std::fs::remove_file(&config.output);
        Self {
            config,
            started: false,
            deadline_receiver: None,
            terminating: false,
            complete_recorded: false,
        }
    }

    fn record(&self, event: &str) -> std::io::Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.config.output)?;
        writeln!(output, "{{\"event\":\"{event}\"}}")
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
    fn new(readiness: &SystemReadiness) -> Self {
        let files = readiness.firmware_files.clone();
        let selected_firmware = files.iter().position(|file| !file.present).unwrap_or(0);
        Self {
            system: readiness.catalog_system.clone(),
            files,
            selected_firmware,
            browser: FileBrowser::new(),
            message: "Choose the firmware target, then select the file you obtained. Known hashes are informational, not a gate.".to_owned(),
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
    running_game: Option<RunningGame>,
    artwork_jobs: SyncSender<ArtworkJob>,
    artwork_receiver: Receiver<ArtworkMessage>,
    textures: HashMap<String, egui::TextureHandle>,
    artwork_errors: HashMap<String, String>,
    artwork_pending: usize,
    artwork_inflight: HashSet<String>,
    browse: BrowseCatalog,
    readiness: Option<ReadinessReport>,
    readiness_refresh: Option<Receiver<Result<ReadinessReport, String>>>,
    featured_ids: HashSet<String>,
    search_documents: Vec<String>,
    imported_manifests: BTreeMap<String, ImportedManifest>,
    retrobat_present: bool,
    controls: Option<ControlsCatalog>,
    browse_view_key: Option<BrowseViewKey>,
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
    let readiness = match ReadinessReport::audit(layout, &browse.entries) {
        Ok(report) => {
            status = format!(
                "{status} Backend audit: {} title(s) ready now, {} provisioned on first play, {} unresolved.",
                report.ready_now_entries,
                report.provision_on_first_play_entries,
                report.unresolved_entries
            );
            Some(report)
        }
        Err(error) => {
            status = format!("{status} Backend audit unavailable: {error}");
            None
        }
    };
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
        .collect();
    let featured_ids = FeaturedCatalog::built_in(&browse)
        .map(|featured| featured.entry_ids)
        .unwrap_or_else(|error| {
            eprintln!("Featured snapshot rejected: {error}");
            HashSet::new()
        });
    let imported_manifests = imported_manifests(layout);
    LoadedLibrary {
        catalog,
        browse,
        readiness,
        featured_ids,
        search_documents,
        imported_manifests,
        controls: ControlsCatalog::built_in().expect("built-in controls snapshot must validate"),
        status,
    }
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
        let artwork_jobs = start_artwork_workers(layout.clone(), artwork_sender.clone());
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
            artwork_errors: HashMap::new(),
            artwork_pending: 0,
            artwork_inflight: HashSet::new(),
            browse: BrowseCatalog {
                schema_version: 2,
                generated_at: String::new(),
                sources: Vec::new(),
                entries: Vec::new(),
            },
            readiness: None,
            readiness_refresh: None,
            featured_ids: HashSet::new(),
            search_documents: Vec::new(),
            imported_manifests: BTreeMap::new(),
            retrobat_present: layout.retrobat_executable().is_file(),
            controls: None,
            browse_view_key: None,
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

    fn start_install(&mut self, entry: CatalogEntry) {
        let layout = self.layout.clone();
        let (sender, receiver) = mpsc::channel();
        self.operation = Some(receiver);
        self.status = format!("Downloading and verifying {}…", entry.title);
        thread::spawn(move || {
            let result = ReqwestDownloader::new()
                .and_then(|downloader| Installer::new(&layout, &downloader).install(&entry));
            let result = match result {
                Ok(report) => OperationResult {
                    success: true,
                    heading: "INSTALL COMPLETE".to_owned(),
                    message: format!(
                        "Installed and verified {} bytes at {}.",
                        report.bytes,
                        report.destination.display()
                    ),
                },
                Err(error) => OperationResult {
                    success: false,
                    heading: "INSTALL FAILED".to_owned(),
                    message: format!("Install failed safely: {error}"),
                },
            };
            let _ = sender.send(result);
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
                    FirmwareInstallAction::Rpcs3 => {
                        LaunchPlan::for_current_rpcs3_firmware_install(
                            &layout,
                            &report.destination,
                        )
                        .and_then(|plan| plan.spawn().map(|_| ()))
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
        let entries = self.browse.entries.clone();
        let (sender, receiver) = mpsc::channel();
        self.readiness_refresh = Some(receiver);
        thread::spawn(move || {
            let result =
                ReadinessReport::audit(&layout, &entries).map_err(|error| error.to_string());
            let _ = sender.send(result);
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
            "Downloading {} from its immutable source snapshot…",
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
                        "Downloaded and imported {title}: {} file(s), {} MiB. PLAY is ready.",
                        report.import.imported_files,
                        report.import.imported_bytes / (1024 * 1024)
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
        let mut close = false;
        let Some(dialog) = &mut self.firmware_dialog else {
            return;
        };
        if let Some(path) = dropped
            && path.is_file()
        {
            dialog.browser.path_text = path.display().to_string();
            dialog.browser.selected = Some(path);
            dialog.message = "Dropped file selected. Confirm the destination below.".to_owned();
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
            if root
                .selectable_label(
                    dialog.selected_firmware == index,
                    format!(
                        "{kind} · bios/{}{} · {state}",
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
                .add(egui::Button::new("CANCEL").fill(CONTROL_BACKGROUND))
                .clicked()
            {
                close = true;
            }
        });
        if let Some(firmware) = direct_download {
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
                BackendState::ProvisionOnFirstPlay => {
                    root.label(
                        egui::RichText::new(
                            "EMULATOR SETUP ON FIRST PLAY · RetroBat may need an internet connection once.",
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
                    supports_folder
                        && dialog.browser.directory.is_dir()
                        && self.operation.is_none(),
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
        self.status =
            match LaunchPlan::for_current_host(&layout).and_then(|plan| plan.spawn().map(|_| ())) {
                Ok(()) => "RetroBat launched with the refreshed game library.".to_owned(),
                Err(error) => format!("Launch failed: {error}"),
            };
    }

    fn launch_game(&mut self, catalog_id: &str, title: &str, system: &str, rom: &std::path::Path) {
        if self.running_game.is_some() {
            self.status = "A game is already loading or running. Close it before starting another."
                .to_owned();
            return;
        }
        let layout = self.layout.clone();
        let backend = self
            .readiness
            .as_ref()
            .and_then(|report| report.select_backend(system, rom))
            .cloned();
        let route_label = backend.as_ref().map(|route| route.label());
        self.status = format!("Loading {title}…");
        let plan =
            match LaunchPlan::for_current_game_with_backend(&layout, system, rom, backend.as_ref())
            {
                Ok(plan) => plan,
                Err(error) => {
                    self.status = format!("Could not prepare {title}: {error}");
                    return;
                }
            };
        let controller_guard = match ControllerMouseGuard::acquire_if_available() {
            Ok(guard) => guard,
            Err(error) => {
                self.status = format!(
                    "Could not suspend the desktop controller-to-mouse mapping; {title} was not launched: {error}"
                );
                return;
            }
        };
        match plan.spawn() {
            Ok(mut child) => {
                let process_id = child.id();
                let (exit_sender, exit_receiver) = mpsc::channel();
                let context = self.context.clone();
                let game_title = title.to_owned();
                thread::spawn(move || {
                    let result = child.wait();
                    while process_tree_is_running(process_id) {
                        thread::sleep(Duration::from_millis(100));
                    }
                    let guard_result = controller_guard
                        .map(ControllerMouseGuard::release)
                        .transpose();
                    let mut message = match result {
                        Ok(status) => format!("{game_title} closed ({status})."),
                        Err(error) => format!("Could not monitor {game_title}: {error}"),
                    };
                    if let Err(error) = guard_result {
                        message.push_str(&format!(
                            " The desktop controller-to-mouse mapping could not be restored: {error}"
                        ));
                    }
                    let _ = exit_sender.send(message);
                    context.request_repaint();
                });
                self.running_game = Some(RunningGame {
                    catalog_id: catalog_id.to_owned(),
                    title: title.to_owned(),
                    process_id,
                    exit_receiver,
                    launched_at: Instant::now(),
                    termination_requested_at: None,
                });
                self.status = route_label.map_or_else(
                    || format!("Loading {title}; its configured backend is starting…"),
                    |route| format!("Loading {title} through the installed {route} backend…"),
                );
            }
            Err(error) => {
                drop(controller_guard);
                self.status = format!("Could not launch {title}: {error}");
            }
        }
    }

    fn terminate_running_game(&mut self) {
        let Some(game) = &mut self.running_game else {
            return;
        };
        if game.termination_requested_at.is_some() {
            return;
        }
        match terminate_process_tree_id(game.process_id, false) {
            Ok(()) => {
                game.termination_requested_at = Some(Instant::now());
                self.status = format!("Terminating {} and its emulator process tree…", game.title);
            }
            Err(error) => {
                self.status = format!("Could not terminate {}: {error}", game.title);
            }
        }
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
                    let (sender, receiver) = mpsc::channel();
                    let (duration, repaint) = self
                        .gameplay_probe
                        .as_ref()
                        .map(|probe| (probe.config.duration, self.context.clone()))
                        .expect("gameplay probe exists");
                    thread::spawn(move || {
                        thread::sleep(duration);
                        let _ = sender.send(());
                        repaint.request_repaint();
                    });
                    if let Some(probe) = &mut self.gameplay_probe {
                        probe.deadline_receiver = Some(receiver);
                        let _ = probe.record("game_launched");
                    }
                } else if let Some(probe) = &self.gameplay_probe {
                    let _ = probe.record("launch_failed");
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else if let Some(probe) = &self.gameplay_probe {
                let _ = probe.record("imported_game_not_found");
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
        if let Some(game) = &mut self.running_game
            && game
                .termination_requested_at
                .is_some_and(|started| started.elapsed() >= Duration::from_secs(2))
        {
            let _ = terminate_process_tree_id(game.process_id, true);
        }
        let gameplay_probe_deadline = self
            .gameplay_probe
            .as_ref()
            .and_then(|probe| probe.deadline_receiver.as_ref())
            .is_some_and(|receiver| receiver.try_recv().is_ok());
        if gameplay_probe_deadline {
            if let Some(probe) = &mut self.gameplay_probe {
                probe.deadline_receiver = None;
                probe.terminating = true;
                let _ = probe.record("deadline_reached");
            }
            self.terminate_running_game();
        }
        let game_finished = self
            .running_game
            .as_ref()
            .and_then(|game| game.exit_receiver.try_recv().ok());
        if let Some(status) = game_finished {
            self.running_game = None;
            self.status = status;
        } else if let Some(game) = &self.running_game {
            // Do not continuously render the frontend while a fullscreen game
            // covers it. On native Wayland, presenting an occluded GL surface
            // can wait on compositor/GPU frame availability long enough to
            // prevent winit from answering xdg_wm_base pings. The process
            // watcher requests a repaint on exit; only the short loading and
            // forced-termination transitions need timers here.
            if let Some(delay) = active_game_repaint_delay(
                game.launched_at.elapsed(),
                game.termination_requested_at.is_some(),
            ) {
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
                let _ = probe.record("gameplay_probe_complete");
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
                Ok(decoded) => {
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied(decoded.size, &decoded.rgba);
                    let texture = context.load_texture(
                        format!("artwork/{}", message.entry_id),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.textures.insert(message.entry_id, texture);
                }
                Err(error) => {
                    self.artwork_errors.insert(message.entry_id, error);
                }
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
        const CARD: egui::Color32 = egui::Color32::from_rgb(22, 27, 38);
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
                    egui::RichText::new("// PORTABLE")
                        .size(28.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
            });
            ui.label(
                egui::RichText::new(
                    "One visual library for direct downloads and locally imported classics.",
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
                                "{} TITLES  ·  {} SOURCES  ·  {} VERIFIED INSTALL{}",
                                self.browse.entries.len(),
                                self.browse.sources.len(),
                                self.catalog.entries.len(),
                                if self.catalog.entries.len() == 1 { "" } else { "S" }
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
                    let page_entries: Vec<BrowseEntry> = self
                        .browse_matches
                        .iter()
                        .copied()
                        .skip(page_start)
                        .take(BROWSE_PAGE_SIZE)
                        .map(|index| self.browse.entries[index].clone())
                        .collect();
                    let mut artwork_requests = Vec::new();
                    let mut requested_import = None;
                    let mut requested_install = None;
                    let mut requested_download = None;
                    let mut requested_firmware = None;
                    let mut requested_controls = None;
                    let mut requested_remove_import = None;
                    let mut requested_play: Option<(String, String, String, PathBuf)> = None;
                    let mut requested_terminate = false;
                    let layout = self.layout.clone();

                    let (grid_columns, card_width, grid_spacing) =
                        browse_grid_geometry(library_viewport_width);
                    let artwork_height = (card_width * 2.0 / 3.0).round();
                    egui::Grid::new("browse-card-grid")
                        .num_columns(grid_columns)
                        .spacing([grid_spacing, grid_spacing])
                        .show(ui, |ui| {
                                for (card_index, entry) in page_entries.iter().enumerate() {
                                    egui::Frame::new()
                                        .fill(CARD)
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            egui::Color32::from_rgb(43, 51, 69),
                                        ))
                                        .corner_radius(10)
                                        .inner_margin(10)
                                        .show(ui, |ui| {
                                            ui.vertical(|ui| {
                                                ui.set_min_width(card_width);
                                                ui.set_max_width(card_width);
                                                if let Some(texture) = self.textures.get(&entry.id)
                                                {
                                                    ui.add(
                                                        egui::Image::new((
                                                            texture.id(),
                                                            egui::vec2(card_width, artwork_height),
                                                        ))
                                                        .fit_to_exact_size(egui::vec2(
                                                            card_width,
                                                            artwork_height,
                                                        ))
                                                        .corner_radius(7),
                                                    );
                                                } else {
                                                    let (rect, _) = ui.allocate_exact_size(
                                                        egui::vec2(card_width, artwork_height),
                                                        egui::Sense::hover(),
                                                    );
                                                    let has_sourced_artwork = entry
                                                        .artwork_asset
                                                        .is_some()
                                                        || entry.artwork_url.is_some();
                                                    let failed = self
                                                        .artwork_errors
                                                        .contains_key(&entry.id);
                                                    if has_sourced_artwork && !failed {
                                                        ui.painter().rect_filled(
                                                            rect,
                                                            7,
                                                            egui::Color32::from_rgb(28, 35, 49),
                                                        );
                                                        ui.painter().text(
                                                            rect.center(),
                                                            egui::Align2::CENTER_CENTER,
                                                            "LOADING…",
                                                            egui::FontId::proportional(12.0),
                                                            egui::Color32::from_gray(130),
                                                        );
                                                    } else {
                                                        paint_generated_artwork(
                                                            ui.painter(),
                                                            rect,
                                                            entry,
                                                        );
                                                    }
                                                    if has_sourced_artwork
                                                        && !self
                                                            .artwork_inflight
                                                            .contains(&entry.id)
                                                        && !failed
                                                    {
                                                        let source = self
                                                            .catalog
                                                            .entries
                                                            .iter()
                                                            .find(|trusted| trusted.id == entry.id)
                                                            .and_then(|trusted| {
                                                                trusted.artwork.first().cloned()
                                                            })
                                                            .map(ArtworkSource::Verified)
                                                            .or_else(|| {
                                                                entry
                                                                    .artwork_asset
                                                                    .clone()
                                                                    .map(ArtworkSource::Bundled)
                                                            })
                                                            .or_else(|| {
                                                                entry
                                                                    .artwork_url
                                                                    .clone()
                                                                    .map(ArtworkSource::Snapshot)
                                                            })
                                                            .expect(
                                                                "sourced artwork has a loader",
                                                            );
                                                        artwork_requests.push((
                                                            entry.id.clone(),
                                                            source,
                                                        ));
                                                    }
                                                }
                                                ui.add_space(5.0);
                                                ui.label(
                                                    egui::RichText::new(&entry.title)
                                                        .strong()
                                                        .color(egui::Color32::WHITE),
                                                );
                                                let source_name = self
                                                    .browse
                                                    .sources
                                                    .iter()
                                                    .find(|source| source.id == entry.source_id)
                                                    .map(|source| source.name.as_str())
                                                    .unwrap_or(&entry.source_id);
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
                                                    ui.label(
                                                        egui::RichText::new(license)
                                                            .small()
                                                            .color(egui::Color32::from_gray(125)),
                                                    );
                                                }
                                                let (trust, trust_color) = match (
                                                    entry.acquisition,
                                                    entry.install_state,
                                                ) {
                                                    (
                                                        Acquisition::DirectDownload,
                                                        InstallState::Verified,
                                                    ) => (
                                                        "VERIFIED DOWNLOAD",
                                                        egui::Color32::from_rgb(98, 211, 145),
                                                    ),
                                                    (
                                                        Acquisition::DirectDownload,
                                                        InstallState::AuditRequired,
                                                    ) => (
                                                        "DIRECT DOWNLOAD",
                                                        egui::Color32::from_rgb(238, 177, 89),
                                                    ),
                                                    (Acquisition::DirectDownload, _) => (
                                                        "DIRECT DOWNLOAD",
                                                        egui::Color32::from_gray(145),
                                                    ),
                                                    (Acquisition::LocalImport, _) => (
                                                        "LOCAL COPY REQUIRED",
                                                        egui::Color32::from_gray(145),
                                                    ),
                                                };
                                                ui.label(
                                                    egui::RichText::new(trust)
                                                        .small()
                                                        .strong()
                                                        .color(trust_color),
                                                );
                                                if let Some(readiness) = self
                                                    .readiness
                                                    .as_ref()
                                                    .and_then(|report| {
                                                        report.for_catalog_system(&entry.system)
                                                    })
                                                {
                                                    let (label, color, detail) = match readiness
                                                        .backend
                                                    {
                                                        BackendState::ReadyNow => (
                                                            "BACKEND READY",
                                                            egui::Color32::from_rgb(98, 211, 145),
                                                            readiness
                                                                .ready_route
                                                                .as_ref()
                                                                .map(|route| {
                                                                    format!(
                                                                        "Installed route: {}",
                                                                        route.label()
                                                                    )
                                                                })
                                                                .unwrap_or_else(|| {
                                                                    "An installed emulator route is available."
                                                                        .to_owned()
                                                                }),
                                                        ),
                                                        BackendState::ProvisionOnFirstPlay => (
                                                            "EMULATOR SETUP ON FIRST PLAY",
                                                            egui::Color32::from_rgb(238, 177, 89),
                                                            "RetroBat has a system adapter but no configured backend is installed yet. It may download one on first play."
                                                                .to_owned(),
                                                        ),
                                                        BackendState::Unresolved => (
                                                            "BACKEND NOT YET RESOLVED",
                                                            egui::Color32::from_rgb(235, 113, 113),
                                                            "No RetroBat system adapter is currently mapped for this catalogue system."
                                                                .to_owned(),
                                                        ),
                                                    };
                                                    ui.label(
                                                        egui::RichText::new(label)
                                                            .small()
                                                            .strong()
                                                            .color(color),
                                                    )
                                                    .on_hover_text(detail);
                                                    if readiness.firmware
                                                        == FirmwareState::RequiredMissing
                                                    {
                                                        ui.label(
                                                            egui::RichText::new(
                                                                "FIRMWARE SETUP REQUIRED",
                                                            )
                                                            .small()
                                                            .color(egui::Color32::from_rgb(
                                                                238, 177, 89,
                                                            )),
                                                        )
                                                        .on_hover_text(format!(
                                                            "The selected installed backend declares {} required firmware file(s); none were detected.",
                                                            readiness.firmware_candidates
                                                        ));
                                                    }
                                                    let missing_required = readiness
                                                        .firmware_files
                                                        .iter()
                                                        .any(|file| {
                                                            !file.present && !file.optional
                                                        });
                                                    let missing_optional = readiness
                                                        .firmware_files
                                                        .iter()
                                                        .any(|file| file.optional && !file.present);
                                                    let downloadable_required = readiness
                                                        .firmware_files
                                                        .iter()
                                                        .any(|file| {
                                                            !file.present
                                                                && !file.optional
                                                                && file.download.is_some()
                                                        });
                                                    let downloadable_optional = readiness
                                                        .firmware_files
                                                        .iter()
                                                        .any(|file| {
                                                            !file.present
                                                                && file.optional
                                                                && file.download.is_some()
                                                        });
                                                    if !readiness.firmware_files.is_empty()
                                                        && ui
                                                            .add_enabled(
                                                                self.operation.is_none(),
                                                                egui::Button::new(
                                                                    egui::RichText::new(
                                                                        if missing_required {
                                                                            if downloadable_required {
                                                                                "INSTALL FIRMWARE"
                                                                            } else {
                                                                                "IMPORT FIRMWARE"
                                                                            }
                                                                        } else if missing_optional {
                                                                            if downloadable_optional {
                                                                                "INSTALL OPTIONAL FIRMWARE"
                                                                            } else {
                                                                                "IMPORT OPTIONAL FIRMWARE"
                                                                            }
                                                                        } else {
                                                                            "MANAGE FIRMWARE"
                                                                        },
                                                                    )
                                                                    .small()
                                                                    .strong(),
                                                                )
                                                                .fill(CONTROL_BACKGROUND),
                                                            )
                                                            .clicked()
                                                    {
                                                        requested_firmware =
                                                            Some(readiness.clone());
                                                    }
                                                }
                                                if let Some(url) = &entry.detail_url {
                                                    ui.hyperlink_to(
                                                        egui::RichText::new("SOURCE DETAILS")
                                                            .small()
                                                            .color(ACCENT),
                                                        url,
                                                    );
                                                }
                                                ui.add_space(4.0);
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("⌨  CONTROLS")
                                                                .small()
                                                                .strong(),
                                                        )
                                                        .fill(CONTROL_BACKGROUND)
                                                        .min_size(egui::vec2(122.0, 26.0)),
                                                    )
                                                    .on_hover_text(
                                                        "Available before import: guidance comes from the catalogue's MAME/RetroBat metadata and the installed RetroArch/controller configuration, not from inspecting ROM bytes.",
                                                    )
                                                    .clicked()
                                                {
                                                    requested_controls = Some(entry.clone());
                                                }
                                                ui.add_space(3.0);
                                                match entry.acquisition {
                                                    Acquisition::LocalImport => {
                                                        let imported = self
                                                            .imported_manifests
                                                            .get(&entry.id)
                                                            .cloned();
                                                        let imported_ready = imported.is_some();
                                                        let (label, game_intent) = if imported
                                                            .is_some()
                                                        {
                                                            game_button_state(
                                                                self.running_game.as_ref().map(
                                                                    |game| {
                                                                        (
                                                                            game.catalog_id
                                                                                .as_str(),
                                                                            game.launched_at
                                                                                .elapsed(),
                                                                            game.termination_requested_at
                                                                                .is_some(),
                                                                        )
                                                                    },
                                                                ),
                                                                &entry.id,
                                                            )
                                                        } else {
                                                            (
                                                                "IMPORT GAME".to_owned(),
                                                                GameButtonIntent::Play,
                                                            )
                                                        };
                                                        if ui
                                                            .add_enabled(
                                                                self.operation.is_none()
                                                                    && game_intent
                                                                        != GameButtonIntent::Disabled,
                                                                egui::Button::new(
                                                                    egui::RichText::new(&label)
                                                                        .small()
                                                                        .strong()
                                                                        .color(
                                                                            egui::Color32::WHITE,
                                                                        ),
                                                                )
                                                                .fill(ACCENT)
                                                                .min_size(egui::vec2(122.0, 28.0)),
                                                            )
                                                            .clicked()
                                                        {
                                                            if game_intent
                                                                == GameButtonIntent::Terminate
                                                            {
                                                                requested_terminate = true;
                                                            } else if let Some(manifest) = imported.clone() {
                                                                requested_play = Some((
                                                                    entry.id.clone(),
                                                                    entry.title.clone(),
                                                                    manifest.system,
                                                                    layout.root.join(
                                                                        manifest
                                                                            .launch_relative_path,
                                                                    ),
                                                                ));
                                                            } else {
                                                                requested_import =
                                                                    Some(entry.clone());
                                                            }
                                                        }
                                                        if imported_ready
                                                            && ui
                                                                .add_enabled(
                                                                    remove_import_available(
                                                                        imported_ready,
                                                                        self.operation.is_some(),
                                                                        self.running_game.is_some(),
                                                                    ),
                                                                    egui::Button::new(
                                                                        egui::RichText::new(
                                                                            "REMOVE",
                                                                        )
                                                                        .small()
                                                                        .strong(),
                                                                    )
                                                                    .fill(egui::Color32::from_rgb(
                                                                        113, 47, 55,
                                                                    ))
                                                                    .min_size(egui::vec2(
                                                                        122.0, 26.0,
                                                                    )),
                                                                )
                                                                .on_hover_text(
                                                                    "Remove files owned by this import and return the card to IMPORT GAME. Modified files are preserved.",
                                                                )
                                                                .clicked()
                                                        {
                                                            requested_remove_import = Some((
                                                                entry.id.clone(),
                                                                entry.title.clone(),
                                                            ));
                                                        }
                                                    }
                                                    Acquisition::DirectDownload => {
                                                        let trusted = self
                                                            .catalog
                                                            .entries
                                                            .iter()
                                                            .find(|trusted| {
                                                                trusted.id == entry.id
                                                            });
                                                        let installed = trusted.is_some_and(
                                                            |trusted| {
                                                                is_installed(&layout, trusted)
                                                            },
                                                        );
                                                        let imported = self
                                                            .imported_manifests
                                                            .get(&entry.id)
                                                            .cloned();
                                                        let imported_ready = imported.is_some();
                                                        let game_ready = installed
                                                            || imported.is_some();
                                                        let (label, game_intent) = if game_ready {
                                                            game_button_state(
                                                                self.running_game.as_ref().map(
                                                                    |game| {
                                                                        (
                                                                            game.catalog_id
                                                                                .as_str(),
                                                                            game.launched_at
                                                                                .elapsed(),
                                                                            game.termination_requested_at
                                                                                .is_some(),
                                                                        )
                                                                    },
                                                                ),
                                                                &entry.id,
                                                            )
                                                        } else {
                                                            (
                                                                "DOWNLOAD".to_owned(),
                                                                GameButtonIntent::Play,
                                                            )
                                                        };
                                                        if ui
                                                            .add_enabled(
                                                                self.operation.is_none()
                                                                    && game_intent
                                                                        != GameButtonIntent::Disabled
                                                                    && (installed
                                                                        || imported.is_some()
                                                                        || trusted.is_some()
                                                                        || supports_direct_download(
                                                                            entry,
                                                                        )),
                                                                egui::Button::new(
                                                                    egui::RichText::new(&label)
                                                                        .small()
                                                                        .strong()
                                                                        .color(
                                                                            egui::Color32::WHITE,
                                                                        ),
                                                                )
                                                                .fill(ACCENT)
                                                                .min_size(egui::vec2(150.0, 28.0)),
                                                            )
                                                            .clicked()
                                                        {
                                                            if game_intent
                                                                == GameButtonIntent::Terminate
                                                            {
                                                                requested_terminate = true;
                                                            } else if installed {
                                                                requested_play = Some((
                                                                    entry.id.clone(),
                                                                    entry.title.clone(),
                                                                    trusted
                                                                        .expect(
                                                                            "installed entries are trusted",
                                                                        )
                                                                        .system
                                                                        .clone(),
                                                                    layout.root.join(
                                                                        trusted
                                                                            .expect(
                                                                                "installed entries are trusted",
                                                                            )
                                                                            .install_relative_path(),
                                                                        ),
                                                                ));
                                                            } else if let Some(manifest) = imported.clone() {
                                                                requested_play = Some((
                                                                    entry.id.clone(),
                                                                    entry.title.clone(),
                                                                    manifest.system,
                                                                    layout.root.join(
                                                                        manifest
                                                                            .launch_relative_path,
                                                                    ),
                                                                ));
                                                            } else if let Some(trusted) = trusted {
                                                                requested_install =
                                                                    Some(trusted.clone());
                                                            } else if supports_direct_download(entry)
                                                            {
                                                                requested_download =
                                                                    Some(entry.clone());
                                                            }
                                                        }
                                                        if imported_ready
                                                            && ui
                                                                .add_enabled(
                                                                    remove_import_available(
                                                                        imported_ready,
                                                                        self.operation.is_some(),
                                                                        self.running_game.is_some(),
                                                                    ),
                                                                    egui::Button::new(
                                                                        egui::RichText::new(
                                                                            "REMOVE",
                                                                        )
                                                                        .small()
                                                                        .strong(),
                                                                    )
                                                                    .fill(egui::Color32::from_rgb(
                                                                        113, 47, 55,
                                                                    ))
                                                                    .min_size(egui::vec2(
                                                                        150.0, 26.0,
                                                                    )),
                                                                )
                                                                .on_hover_text(
                                                                    "Remove files owned by this import. Modified files are preserved.",
                                                                )
                                                                .clicked()
                                                        {
                                                            requested_remove_import = Some((
                                                                entry.id.clone(),
                                                                entry.title.clone(),
                                                            ));
                                                        }
                                                    }
                                                }
                                            });
                                        });
                                    if (card_index + 1) % grid_columns == 0 {
                                        ui.end_row();
                                    }
                                }
                        });
                    if let Some(entry) = requested_import {
                        self.import_dialog = Some(ImportDialog::new(entry));
                    }
                    if let Some(entry) = requested_install {
                        self.start_install(entry);
                    }
                    if let Some(entry) = requested_download {
                        self.start_browse_download(entry);
                    }
                    if let Some(readiness) = requested_firmware {
                        self.firmware_dialog = Some(FirmwareDialog::new(&readiness));
                    }
                    if let Some(entry) = requested_controls
                        && let Some(controls) = &self.controls
                    {
                        let imported = self.imported_manifests.get(&entry.id);
                        self.controls_dialog = Some(controls.for_game(&layout, &entry, imported));
                    }
                    if let Some((catalog_id, title)) = requested_remove_import {
                        self.start_remove_import(catalog_id, title);
                    }
                    if requested_terminate {
                        self.terminate_running_game();
                    } else if let Some((catalog_id, title, system, rom)) = requested_play {
                        self.launch_game(&catalog_id, &title, &system, &rom);
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
                                "{} titles have an installed backend · {} can provision a backend on first play · {} remain unresolved",
                                readiness.ready_now_entries,
                                readiness.provision_on_first_play_entries,
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
                                    BackendState::ProvisionOnFirstPlay => {
                                        "emulator setup on first play"
                                    }
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
    let decoded = if decoded.width() > 768 || decoded.height() > 768 {
        decoded.resize(768, 768, image::imageops::FilterType::Triangle)
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
            Some(("mspacman", Duration::from_millis(10), false)),
            "mspacman",
        );
        assert_eq!(label, "⏳  LOADING…");
        assert_eq!(intent, GameButtonIntent::Disabled);

        let (other_label, other_intent) =
            game_button_state(Some(("mspacman", Duration::from_secs(10), false)), "pacman");
        assert_eq!(other_label, "GAME RUNNING");
        assert_eq!(other_intent, GameButtonIntent::Disabled);
    }

    #[test]
    fn running_game_exposes_terminate_instead_of_a_second_play() {
        let (label, intent) = game_button_state(
            Some(("mspacman", Duration::from_secs(10), false)),
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
            active_game_repaint_delay(Duration::from_secs(1), false),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            active_game_repaint_delay(Duration::from_secs(6), false),
            None
        );
        assert_eq!(
            active_game_repaint_delay(Duration::from_secs(6), true),
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
        assert!(decoded.size[0] <= 768);
        assert!(decoded.size[1] <= 768);
        assert_eq!(decoded.rgba.len(), decoded.size[0] * decoded.size[1] * 4);
    }
}
