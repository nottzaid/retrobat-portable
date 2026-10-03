use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use serde::Serialize;
use thiserror::Error;

use crate::browse::BrowseEntry;
use crate::import::canonical_system_alias;
use crate::paths::PortableLayout;

#[derive(Debug, Error)]
pub enum ReadinessError {
    #[error("RetroBat's system configuration is missing: {0}")]
    MissingConfig(String),
    #[error("RetroBat's system configuration is invalid: {0}")]
    Config(String),
    #[error("readiness inventory failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendState {
    ReadyNow,
    /// RetroBat lists an emulator for the system, but none is installed.
    EmulatorMissing,
    Unresolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FirmwareState {
    NotRequired,
    AllRequiredPresent,
    SomeRequiredPresent,
    RequiredMissing,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackendRoute {
    pub emulator: String,
    pub core: Option<String>,
    pub incompatible_extensions: Vec<String>,
}

impl BackendRoute {
    pub fn supports(&self, rom: &Path) -> bool {
        let extension = rom
            .extension()
            .map(|value| format!(".{}", value.to_string_lossy().to_ascii_lowercase()))
            .unwrap_or_default();
        !self
            .incompatible_extensions
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(&extension))
    }

    pub fn label(&self) -> String {
        match &self.core {
            Some(core) => format!("{} / {core}", self.emulator),
            None => self.emulator.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SystemReadiness {
    pub catalog_system: String,
    pub retrobat_systems: Vec<String>,
    pub entry_count: usize,
    pub backend: BackendState,
    pub ready_route: Option<BackendRoute>,
    pub firmware: FirmwareState,
    pub firmware_candidates: usize,
    pub firmware_detected: usize,
    pub optional_firmware_candidates: usize,
    pub optional_firmware_detected: usize,
    pub missing_firmware_examples: Vec<String>,
    pub missing_optional_firmware_examples: Vec<String>,
    pub firmware_files: Vec<FirmwareFileStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FirmwareFileStatus {
    pub relative_path: String,
    pub description: String,
    pub directory: bool,
    pub optional: bool,
    pub present: bool,
    pub guidance_url: String,
    pub guidance: String,
    pub download: Option<FirmwareDownload>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FirmwareDownload {
    pub publisher: String,
    pub source_url: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub install_action: FirmwareInstallAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FirmwareInstallAction {
    PlaceInBios,
    Rpcs3,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReadinessReport {
    pub total_entries: usize,
    pub ready_now_entries: usize,
    pub emulator_missing_entries: usize,
    pub unresolved_entries: usize,
    pub firmware_required_entries: usize,
    pub firmware_required_missing_entries: usize,
    pub systems: Vec<SystemReadiness>,
    pub backend_routes: BTreeMap<String, Vec<BackendRoute>>,
}

impl ReadinessReport {
    pub fn audit(layout: &PortableLayout, entries: &[BrowseEntry]) -> Result<Self, ReadinessError> {
        let configured = load_systems(&layout.systems_config())?;
        let inventory = BackendInventory::load(layout)?;

        let mut backend_routes = BTreeMap::new();
        for (system, configuration) in &configured {
            let mut routes = configuration.available_routes(layout, &inventory);
            prefer_best_route(layout, system, &mut routes);
            backend_routes.insert(system.clone(), routes);
        }
        if layout.retroarch_executable().is_file() && layout.retroarch_core("jaxe").is_file() {
            backend_routes.insert(
                "chip8".to_owned(),
                vec![BackendRoute {
                    emulator: "libretro".to_owned(),
                    core: Some("jaxe".to_owned()),
                    incompatible_extensions: Vec::new(),
                }],
            );
        }

        let mut counts = BTreeMap::<String, usize>::new();
        for entry in entries {
            *counts.entry(entry.system.clone()).or_default() += 1;
        }

        let mut systems = Vec::with_capacity(counts.len());
        for (catalog_system, entry_count) in counts {
            let retrobat_systems = resolve_catalog_systems(&catalog_system, &configured);
            let ready_routes = retrobat_systems
                .iter()
                .filter_map(|system| backend_routes.get(system))
                .collect::<Vec<_>>();
            let all_candidates_ready = !retrobat_systems.is_empty()
                && ready_routes.len() == retrobat_systems.len()
                && ready_routes.iter().all(|routes| !routes.is_empty());
            let any_configured = retrobat_systems
                .iter()
                .any(|system| configured.contains_key(system) || system == "chip8");
            let backend = if all_candidates_ready {
                BackendState::ReadyNow
            } else if any_configured {
                BackendState::EmulatorMissing
            } else {
                BackendState::Unresolved
            };
            let ready_route = retrobat_systems
                .iter()
                .filter_map(|system| backend_routes.get(system))
                .find_map(|routes| routes.first())
                .cloned();
            let firmware_summary =
                summarize_required_firmware(layout, &retrobat_systems, &backend_routes);
            systems.push(SystemReadiness {
                catalog_system,
                retrobat_systems,
                entry_count,
                backend,
                ready_route,
                firmware: firmware_summary.state,
                firmware_candidates: firmware_summary.candidates,
                firmware_detected: firmware_summary.detected,
                optional_firmware_candidates: firmware_summary.optional_candidates,
                optional_firmware_detected: firmware_summary.optional_detected,
                missing_firmware_examples: firmware_summary.missing_examples,
                missing_optional_firmware_examples: firmware_summary.missing_optional_examples,
                firmware_files: firmware_summary.files,
            });
        }

        let ready_now_entries = entries_for_state(&systems, BackendState::ReadyNow);
        let emulator_missing_entries = entries_for_state(&systems, BackendState::EmulatorMissing);
        let unresolved_entries = entries_for_state(&systems, BackendState::Unresolved);
        let firmware_required_entries = systems
            .iter()
            .filter(|system| system.firmware != FirmwareState::NotRequired)
            .map(|system| system.entry_count)
            .sum();
        let firmware_required_missing_entries = systems
            .iter()
            .filter(|system| system.firmware == FirmwareState::RequiredMissing)
            .map(|system| system.entry_count)
            .sum();

        Ok(Self {
            total_entries: entries.len(),
            ready_now_entries,
            emulator_missing_entries,
            unresolved_entries,
            firmware_required_entries,
            firmware_required_missing_entries,
            systems,
            backend_routes,
        })
    }

    pub fn for_catalog_system(&self, system: &str) -> Option<&SystemReadiness> {
        self.systems
            .iter()
            .find(|candidate| candidate.catalog_system.eq_ignore_ascii_case(system))
    }

    pub fn select_backend(&self, retrobat_system: &str, rom: &Path) -> Option<&BackendRoute> {
        self.select_backend_preferring(retrobat_system, rom, None)
    }

    /// Like `select_backend`, but an installed route using `core` wins when
    /// the content needs it (Game & Watch .mgw packs need the gw core, not
    /// the system's default MAME).
    pub fn select_backend_preferring(
        &self,
        retrobat_system: &str,
        rom: &Path,
        core: Option<&str>,
    ) -> Option<&BackendRoute> {
        let routes = self
            .backend_routes
            .iter()
            .find(|(system, _)| system.eq_ignore_ascii_case(retrobat_system))
            .map(|(_, routes)| routes)?;
        core.and_then(|core| {
            routes.iter().find(|route| {
                route
                    .core
                    .as_deref()
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(core))
            })
        })
        .or_else(|| routes.iter().find(|route| route.supports(rom)))
    }
}

/// Puts the best installed route first: the accuracy reference for the
/// system, using the owner's official BIOS whenever it is present. Without
/// that BIOS a built-in-BIOS route comes first, so PLAY works until it is
/// imported. There is no per-game backend picker, so this ordering is what
/// makes importing a BIOS take effect.
fn prefer_best_route(layout: &PortableLayout, system: &str, routes: &mut [BackendRoute]) {
    let bios = layout.retrobat_root().join("bios");
    let has = |name: &str| bios.join(name).is_file();
    let preferred = match system {
        // The accuracy references, installed and full speed on ordinary
        // hardware; neither needs a BIOS.
        "nes" => Some("mesen"),
        "snes" => Some("bsnes"),
        "gb" | "gbc" => Some("sameboy"),
        // PCSX ReARMed has a built-in HLE BIOS; Beetle PSX stops at a
        // "Firmware is missing" screen without one.
        "psx"
            if !["scph5500.bin", "scph5501.bin", "scph5502.bin"]
                .iter()
                .any(|name| has(name)) =>
        {
            Some("pcsx_rearmed")
        }
        // Play! needs no BIOS; PCSX2, the reference, needs one in
        // bios/pcsx2/bios and comes first (RetroBat's order) once it is there.
        "ps2" if !directory_contains_regular_file(&bios.join("pcsx2").join("bios")) => Some("play"),
        // Beetle Saturn (Mednafen), the accuracy reference, reads the
        // regional BIOS files; YabaSanshiro reads a single saturn_bios.bin.
        // Every installed Saturn core refuses to start without a BIOS.
        "saturn" if has("mpr-17933.bin") || has("sega_101.bin") => Some("mednafen_saturn"),
        "saturn" => Some("yabasanshiro"),
        // xemu, the reference, needs the console's boot ROM and flash BIOS;
        // Cxbx-Reloaded needs neither.
        "xbox" if !(has("mcpx_1.0.bin") && has("Complex_4627.bin")) => Some("cxbx"),
        _ => None,
    };
    if let Some(preferred) = preferred
        && let Some(index) = routes.iter().position(|route| {
            route.emulator.eq_ignore_ascii_case(preferred)
                || route
                    .core
                    .as_deref()
                    .is_some_and(|core| core.eq_ignore_ascii_case(preferred))
        })
    {
        routes[..=index].rotate_right(1);
    }
}

fn entries_for_state(systems: &[SystemReadiness], state: BackendState) -> usize {
    systems
        .iter()
        .filter(|system| system.backend == state)
        .map(|system| system.entry_count)
        .sum()
}

#[derive(Clone, Debug)]
struct ConfiguredSystem {
    emulators: Vec<ConfiguredEmulator>,
}

impl ConfiguredSystem {
    fn available_routes(
        &self,
        layout: &PortableLayout,
        inventory: &BackendInventory,
    ) -> Vec<BackendRoute> {
        let mut emulators = self.emulators.iter().enumerate().collect::<Vec<_>>();
        emulators.sort_by_key(|(index, emulator)| (!emulator.default, *index));
        let mut routes = Vec::new();
        for (_, emulator) in emulators {
            let mut cores = emulator.cores.iter().enumerate().collect::<Vec<_>>();
            cores.sort_by_key(|(index, core)| (!core.default, *index));
            if emulator.name.eq_ignore_ascii_case("libretro") {
                for (_, core) in cores {
                    if layout.retroarch_executable().is_file()
                        && inventory.cores.contains(&core.name.to_ascii_lowercase())
                    {
                        routes.push(core.route(&emulator.name));
                    }
                }
            } else if emulator.name.eq_ignore_ascii_case("windows")
                && layout.emulator_launcher_executable().is_file()
            {
                // "windows" is RetroBat's built-in ExeLauncherGenerator, not
                // a separately installed emulator executable.
                routes.push(BackendRoute {
                    emulator: emulator.name.clone(),
                    core: None,
                    incompatible_extensions: Vec::new(),
                });
            } else if inventory.has_emulator(&emulator.name) {
                if cores.is_empty() {
                    routes.push(BackendRoute {
                        emulator: emulator.name.clone(),
                        core: None,
                        incompatible_extensions: Vec::new(),
                    });
                } else {
                    routes.extend(
                        cores
                            .into_iter()
                            .map(|(_, core)| core.route(&emulator.name)),
                    );
                }
            }
        }
        routes
    }
}

#[derive(Clone, Debug)]
struct ConfiguredEmulator {
    name: String,
    default: bool,
    cores: Vec<ConfiguredCore>,
}

#[derive(Clone, Debug)]
struct ConfiguredCore {
    name: String,
    default: bool,
    incompatible_extensions: Vec<String>,
}

impl ConfiguredCore {
    fn route(&self, emulator: &str) -> BackendRoute {
        BackendRoute {
            emulator: emulator.to_owned(),
            core: Some(self.name.clone()),
            incompatible_extensions: self.incompatible_extensions.clone(),
        }
    }
}

fn load_systems(path: &Path) -> Result<BTreeMap<String, ConfiguredSystem>, ReadinessError> {
    if !path.is_file() {
        return Err(ReadinessError::MissingConfig(path.display().to_string()));
    }
    let mut reader =
        Reader::from_file(path).map_err(|error| ReadinessError::Config(error.to_string()))?;
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut systems = BTreeMap::new();
    let mut in_system = false;
    let mut reading_name = false;
    let mut system_name = None;
    let mut emulators = Vec::new();
    let mut current_emulator: Option<ConfiguredEmulator> = None;
    let mut current_core: Option<ConfiguredCore> = None;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) if start.name().as_ref() == b"system" => {
                in_system = true;
                system_name = None;
                emulators.clear();
            }
            Ok(Event::Start(start)) if in_system && start.name().as_ref() == b"name" => {
                reading_name = true;
            }
            Ok(Event::Start(start)) if in_system && start.name().as_ref() == b"emulator" => {
                current_emulator = emulator_from_element(&start);
            }
            Ok(Event::Empty(start)) if in_system && start.name().as_ref() == b"emulator" => {
                if let Some(emulator) = emulator_from_element(&start) {
                    emulators.push(emulator);
                }
            }
            Ok(Event::Start(start))
                if in_system && current_emulator.is_some() && start.name().as_ref() == b"core" =>
            {
                current_core = Some(core_from_element(&start));
            }
            Ok(Event::Text(text)) if in_system && reading_name && current_emulator.is_none() => {
                system_name = Some(
                    text.decode()
                        .map_err(|error| ReadinessError::Config(error.to_string()))?
                        .into_owned(),
                );
            }
            Ok(Event::Text(text)) if current_core.is_some() => {
                if let Some(core) = &mut current_core {
                    core.name = text
                        .decode()
                        .map_err(|error| ReadinessError::Config(error.to_string()))?
                        .into_owned();
                }
            }
            Ok(Event::End(end)) if end.name().as_ref() == b"name" => reading_name = false,
            Ok(Event::End(end)) if end.name().as_ref() == b"core" => {
                if let (Some(emulator), Some(core)) = (&mut current_emulator, current_core.take())
                    && !core.name.is_empty()
                {
                    emulator.cores.push(core);
                }
            }
            Ok(Event::End(end)) if end.name().as_ref() == b"emulator" => {
                if let Some(emulator) = current_emulator.take() {
                    emulators.push(emulator);
                }
            }
            Ok(Event::End(end)) if end.name().as_ref() == b"system" => {
                if let Some(name) = system_name.take() {
                    systems.insert(
                        name.to_ascii_lowercase(),
                        ConfiguredSystem {
                            emulators: std::mem::take(&mut emulators),
                        },
                    );
                }
                in_system = false;
                reading_name = false;
                current_emulator = None;
                current_core = None;
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(ReadinessError::Config(error.to_string())),
        }
        buffer.clear();
    }
    Ok(systems)
}

fn emulator_from_element(element: &BytesStart<'_>) -> Option<ConfiguredEmulator> {
    let name = attribute(element, b"name")?;
    Some(ConfiguredEmulator {
        name,
        default: attribute(element, b"default").is_some_and(|value| value == "true"),
        cores: Vec::new(),
    })
}

fn core_from_element(element: &BytesStart<'_>) -> ConfiguredCore {
    ConfiguredCore {
        name: String::new(),
        default: attribute(element, b"default").is_some_and(|value| value == "true"),
        incompatible_extensions: attribute(element, b"incompatible_extensions")
            .map(|extensions| {
                extensions
                    .split_whitespace()
                    .map(|extension| extension.to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn attribute(element: &BytesStart<'_>, key: &[u8]) -> Option<String> {
    element
        .attributes()
        .with_checks(false)
        .filter_map(Result::ok)
        .find(|attribute| attribute.key.as_ref() == key)
        .map(|attribute| String::from_utf8_lossy(attribute.value.as_ref()).into_owned())
}

fn resolve_catalog_systems(
    catalog_system: &str,
    configured: &BTreeMap<String, ConfiguredSystem>,
) -> Vec<String> {
    if catalog_system.eq_ignore_ascii_case("unknown") {
        return ["gb", "gbc", "gba", "nes"]
            .into_iter()
            .filter(|system| configured.contains_key(*system))
            .map(str::to_owned)
            .collect();
    }
    let direct = catalog_system.to_ascii_lowercase();
    if configured.contains_key(&direct) || direct == "chip8" {
        return vec![direct];
    }
    canonical_system_alias(catalog_system)
        .map(str::to_ascii_lowercase)
        .filter(|system| configured.contains_key(system) || system == "chip8")
        .into_iter()
        .collect()
}

#[derive(Default)]
struct BackendInventory {
    executable_keys: BTreeSet<String>,
    cores: BTreeSet<String>,
}

impl BackendInventory {
    fn load(layout: &PortableLayout) -> Result<Self, io::Error> {
        let mut inventory = Self::default();
        let emulator_root = layout.retrobat_root().join("emulators");
        if emulator_root.is_dir() {
            inventory.walk_executables(&emulator_root, &emulator_root, 0)?;
        }
        let core_root = layout.retroarch_root().join("cores");
        if core_root.is_dir() {
            for entry in fs::read_dir(core_root)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if entry.file_type()?.is_file()
                    && let Some(core) = name.strip_suffix("_libretro.dll")
                {
                    inventory.cores.insert(core.to_owned());
                }
            }
        }
        Ok(inventory)
    }

    fn walk_executables(&mut self, root: &Path, directory: &Path, depth: usize) -> io::Result<()> {
        if depth > 4 {
            return Ok(());
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                self.walk_executables(root, &path, depth + 1)?;
            } else if file_type.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
            {
                if let Ok(relative) = path.strip_prefix(root) {
                    for component in relative.components() {
                        if let Component::Normal(value) = component {
                            self.executable_keys
                                .insert(normalized_key(&value.to_string_lossy()));
                        }
                    }
                }
                if let Some(stem) = path.file_stem() {
                    self.executable_keys
                        .insert(normalized_key(&stem.to_string_lossy()));
                }
            }
        }
        Ok(())
    }

    fn has_emulator(&self, emulator: &str) -> bool {
        let key = normalized_key(emulator);
        if self.executable_keys.contains(&key) {
            return true;
        }
        standalone_aliases(emulator)
            .iter()
            .map(|alias| normalized_key(alias))
            .any(|alias| self.executable_keys.contains(&alias))
    }
}

fn normalized_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn standalone_aliases(emulator: &str) -> &'static [&'static str] {
    match emulator {
        "cxbx" => &["cxbx-reloaded"],
        "dolphin" => &["dolphin-emu"],
        "mame64" => &["mame"],
        _ => &[],
    }
}

struct FirmwareSummary {
    state: FirmwareState,
    candidates: usize,
    detected: usize,
    optional_candidates: usize,
    optional_detected: usize,
    missing_examples: Vec<String>,
    missing_optional_examples: Vec<String>,
    files: Vec<FirmwareFileStatus>,
}

fn summarize_required_firmware(
    layout: &PortableLayout,
    systems: &[String],
    backend_routes: &BTreeMap<String, Vec<BackendRoute>>,
) -> FirmwareSummary {
    let mut discovered = BTreeMap::<String, (CoreFirmwareFile, String, String)>::new();
    for system in systems {
        let Some(route) = backend_routes
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(system))
            .and_then(|(_, routes)| routes.first())
        else {
            continue;
        };
        if !route.emulator.eq_ignore_ascii_case("libretro") {
            continue;
        }
        let Some(core) = &route.core else {
            continue;
        };
        let metadata = load_core_firmware(&layout.retroarch_core_info(core));
        for mut file in metadata.files {
            // A few cores deliberately advertise preferred external ROMs as
            // non-optional in their machine-readable info while their more
            // specific documentation confirms a built-in fallback. Do not
            // turn those compatibility upgrades into a false playability
            // warning.
            if core_has_documented_firmware_fallback(core) {
                file.optional = true;
            }
            discovered
                .entry(file.path.clone())
                .and_modify(|(existing, _, _)| {
                    existing.optional &= file.optional;
                    if existing.description.is_empty() {
                        existing.description.clone_from(&file.description);
                    }
                })
                .or_insert_with(|| (file, system.clone(), core.clone()));
        }
    }
    for system in systems {
        let selected_emulator = backend_routes
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(system))
            .and_then(|(_, routes)| routes.first())
            .map(|route| route.emulator.as_str());
        for file in curated_standalone_firmware(system, selected_emulator) {
            discovered
                .entry(file.path.clone())
                .or_insert_with(|| (file, system.clone(), "standalone".to_owned()));
        }
    }
    let bios_root = layout.retrobat_root().join("bios");
    let files = discovered
        .into_iter()
        .map(|(path, (file, system, core))| {
            let (guidance_url, guidance) = firmware_guidance(&system, &core, file.optional);
            let download = official_firmware_download(&system, &path);
            FirmwareFileStatus {
                relative_path: path.clone(),
                description: file.description,
                directory: file.directory,
                optional: file.optional,
                present: if file.directory {
                    directory_contains_regular_file(&bios_root.join(&path))
                } else {
                    bios_root.join(&path).is_file()
                },
                guidance_url: guidance_url.to_owned(),
                guidance: guidance.to_owned(),
                download,
            }
        })
        .collect::<Vec<_>>();
    let required = files
        .iter()
        .filter(|file| !file.optional)
        .collect::<Vec<_>>();
    let optional = files
        .iter()
        .filter(|file| file.optional)
        .collect::<Vec<_>>();
    let required_detected = required.iter().filter(|file| file.present).count();
    let optional_detected = optional.iter().filter(|file| file.present).count();
    let state = if required.is_empty() {
        FirmwareState::NotRequired
    } else if required_detected == required.len() {
        FirmwareState::AllRequiredPresent
    } else if required_detected > 0 {
        FirmwareState::SomeRequiredPresent
    } else {
        FirmwareState::RequiredMissing
    };
    let missing_examples = required
        .iter()
        .filter(|file| !file.present)
        .take(4)
        .map(|file| format!("bios/{}", file.relative_path))
        .collect();
    let missing_optional_examples = optional
        .iter()
        .filter(|file| !file.present)
        .take(4)
        .map(|file| format!("bios/{}", file.relative_path))
        .collect();
    FirmwareSummary {
        state,
        candidates: required.len(),
        detected: required_detected,
        optional_candidates: optional.len(),
        optional_detected,
        missing_examples,
        missing_optional_examples,
        files,
    }
}

fn official_firmware_download(system: &str, path: &str) -> Option<FirmwareDownload> {
    match (system, path) {
        ("ps3", "PS3UPDAT.PUP") => Some(FirmwareDownload {
            publisher: "Sony Interactive Entertainment".to_owned(),
            source_url:
                "https://www.playstation.com/en-nz/support/hardware/ps3/system-software/"
                    .to_owned(),
            // Sony's page intentionally publishes this Akamai endpoint as
            // HTTP. The immutable size and SHA-256 below were independently
            // recorded only after the URL-embedded MD5 also matched.
            url: "http://dau01.ps3.update.playstation.net/update/ps3/image/au/2026_0318_a2b60b6ac1d2e49e230144345616927c/PS3UPDAT.PUP".to_owned(),
            size: 206_197_916,
            sha256: "158471fd834f8ea8036136b6aab43cd86c7ba73d79ca30e0af3c0fe0001cf365"
                .to_owned(),
            install_action: FirmwareInstallAction::Rpcs3,
        }),
        _ => None,
    }
}

fn curated_standalone_firmware(
    system: &str,
    selected_emulator: Option<&str>,
) -> Vec<CoreFirmwareFile> {
    match (system, selected_emulator) {
        // RetroBat's RPCS3 generator checks this exact BIOS-root filename and
        // invokes RPCS3 --installfw automatically on the next launch.
        ("ps3", Some("rpcs3")) => vec![CoreFirmwareFile {
            path: "PS3UPDAT.PUP".to_owned(),
            description: "Official PlayStation 3 system software update".to_owned(),
            directory: false,
            optional: false,
        }],
        // RetroBat's xemu generator supplies its own blank HDD/eeprom, but
        // reads these two user-provided machine ROMs from the BIOS root.
        ("xbox", Some("xemu")) => vec![
            CoreFirmwareFile {
                path: "mcpx_1.0.bin".to_owned(),
                description: "Xbox MCPX 1.0 boot ROM".to_owned(),
                directory: false,
                optional: false,
            },
            CoreFirmwareFile {
                path: "Complex_4627.bin".to_owned(),
                description: "xemu-compatible Xbox flash ROM (Complex 4627)".to_owned(),
                directory: false,
                optional: false,
            },
        ],
        // Play! and Cxbx-Reloaded run without a BIOS; the owner's official
        // BIOS switches PLAY to the reference emulators, PCSX2 and xemu.
        ("ps2", Some("play")) => vec![CoreFirmwareFile {
            path: "pcsx2/bios".to_owned(),
            description: "PlayStation 2 BIOS from your console — switches PLAY to PCSX2".to_owned(),
            directory: true,
            optional: true,
        }],
        ("xbox", Some("cxbx")) => vec![
            CoreFirmwareFile {
                path: "mcpx_1.0.bin".to_owned(),
                description: "Xbox MCPX 1.0 boot ROM from your console — with the flash BIOS, switches PLAY to xemu".to_owned(),
                directory: false,
                optional: true,
            },
            CoreFirmwareFile {
                path: "Complex_4627.bin".to_owned(),
                description: "Xbox flash BIOS from your console (retail 4627 works), placed under the name RetroBat's xemu setup reads".to_owned(),
                directory: false,
                optional: true,
            },
        ],
        ("switch", Some("eden")) => vec![CoreFirmwareFile {
            path: "eden/keys/prod.keys".to_owned(),
            description: "Nintendo Switch prod.keys dumped from the user's console".to_owned(),
            directory: false,
            optional: false,
        }],
        _ => Vec::new(),
    }
}

fn core_has_documented_firmware_fallback(core: &str) -> bool {
    matches!(
        core.to_ascii_lowercase().as_str(),
        // PUAE's automatic Kickstart selection can use its built-in AROS
        // replacement: https://docs.libretro.com/library/puae/
        "puae"
    )
}

#[derive(Default)]
struct CoreFirmware {
    files: Vec<CoreFirmwareFile>,
}

#[derive(Clone, Default)]
struct CoreFirmwareFile {
    path: String,
    description: String,
    directory: bool,
    optional: bool,
}

fn load_core_firmware(path: &Path) -> CoreFirmware {
    let Ok(input) = fs::read_to_string(path) else {
        return CoreFirmware::default();
    };
    let mut paths = BTreeMap::<usize, String>::new();
    let mut descriptions = BTreeMap::<usize, String>::new();
    let mut optional = BTreeMap::<usize, bool>::new();
    for line in input.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        if let Some(index) = firmware_index(key, "_path") {
            let firmware = Path::new(value);
            if !firmware.as_os_str().is_empty()
                && !firmware.is_absolute()
                && firmware
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
            {
                paths.insert(index, value.replace('\\', "/"));
            }
        } else if let Some(index) = firmware_index(key, "_opt") {
            optional.insert(index, value.eq_ignore_ascii_case("true"));
        } else if let Some(index) = firmware_index(key, "_desc") {
            descriptions.insert(index, value.to_owned());
        }
    }
    CoreFirmware {
        files: paths
            .into_iter()
            .map(|(index, path)| {
                let description = descriptions.remove(&index).unwrap_or_default();
                let directory = {
                    let description = description.to_ascii_lowercase();
                    // Libretro sometimes describes a required tree as a
                    // "folder" while declaring a concrete sentinel file
                    // inside it (Dolphin's Sys/codehandler.bin). Only a
                    // directory-shaped metadata path is an import directory.
                    Path::new(&path).extension().is_none()
                        && (description.contains("folder") || description.contains("directory"))
                };
                CoreFirmwareFile {
                    path,
                    description,
                    directory,
                    optional: optional.get(&index).copied().unwrap_or(false),
                }
            })
            .collect(),
    }
}

fn directory_contains_regular_file(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry
                .file_type()
                .is_ok_and(|file_type| file_type.is_file() && !file_type.is_symlink())
                && entry.metadata().is_ok_and(|metadata| metadata.len() > 0)
        })
    })
}

fn firmware_guidance(system: &str, core: &str, optional: bool) -> (&'static str, &'static str) {
    match system {
        "psx" if optional => (
            "https://docs.libretro.com/library/pcsx_rearmed/",
            "PCSX ReARMed plays without this file through its built-in HLE BIOS. Adding a BIOS dumped from your own console switches PLAY to the more accurate Beetle PSX HW core.",
        ),
        "psx" => (
            "https://docs.libretro.com/library/beetle_psx_hw/",
            "Beetle PSX HW needs a PlayStation BIOS dumped from your own console. Its core page documents every supported filename and regional detail.",
        ),
        "ps2" => (
            "https://pcsx2.net/docs/setup/bios/",
            "Play! runs PS2 games without a BIOS. Add the BIOS dumped from your own PS2 (any region or revision, under any name; RetroPort recognises it) and PLAY switches to PCSX2, the reference PS2 emulator. PCSX2's guide shows how to dump it.",
        ),
        "ps3" => (
            "https://www.playstation.com/en-ph/support/hardware/ps3/system-software/",
            "Download Sony's official PS3 update as PS3UPDAT.PUP. RetroBat detects it in the BIOS root and RPCS3 installs it automatically on the next launch.",
        ),
        "saturn" => (
            "https://docs.libretro.com/library/yabasanshiro/",
            "Saturn games need a BIOS dumped from your own console: every installed Saturn core stops without one. With mpr-17933.bin (USA/Europe) or sega_101.bin (Japan), PLAY uses Beetle Saturn, the accuracy reference; saturn_bios.bin alone starts YabaSanshiro.",
        ),
        "xbox" => (
            "https://xemu.app/docs/required-files/",
            "Cxbx-Reloaded runs Xbox games without a BIOS. Add your console's MCPX boot ROM and flash BIOS and PLAY switches to xemu, the reference Xbox emulator; xemu's guide shows how to dump both.",
        ),
        "switch" => (
            "https://git.eden-emu.dev/eden-emu/eden",
            "Eden requires prod.keys to decrypt retail Switch game copies. Select the prod.keys dumped from your own console; RetroPort places it into both Windows and Linux portable Eden data stores.",
        ),
        "gb" | "gbc" | "gba" if optional => (
            "https://docs.libretro.com/guides/bios/",
            "This boot ROM is optional for play and can be dumped from hardware you own for a more authentic startup path.",
        ),
        _ if core.eq_ignore_ascii_case("puae") => (
            "https://www.amigaforever.com/kb/13-114",
            "PUAE can start through its built-in AROS fallback. For broader game compatibility, Amiga Forever documents where its licensed Kickstart ROM files are stored after download.",
        ),
        _ if core.eq_ignore_ascii_case("mednafen_psx_hw") => (
            "https://docs.libretro.com/library/beetle_psx_hw/",
            "Use the installed core's documentation to identify and dump the exact firmware file.",
        ),
        _ => (
            "https://docs.libretro.com/library/bios/",
            "Use Libretro's BIOS information hub and the installed core documentation to identify the exact owner-supplied file.",
        ),
    }
}

fn firmware_index(key: &str, suffix: &str) -> Option<usize> {
    key.strip_prefix("firmware")?
        .strip_suffix(suffix)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_metadata_folder_requirement_accepts_any_nonempty_file_inside() {
        let root = tempfile::tempdir().unwrap();
        let info = root.path().join("lrps2.info");
        fs::write(
            &info,
            "firmware0_desc = \"'pcsx2/bios' folder\"\nfirmware0_path = \"pcsx2/bios\"\nfirmware0_opt = \"false\"\n",
        )
        .unwrap();
        let parsed = load_core_firmware(&info);
        assert!(parsed.files[0].directory);

        let bios = root.path().join("pcsx2/bios");
        fs::create_dir_all(&bios).unwrap();
        assert!(!directory_contains_regular_file(&bios));
        fs::write(bios.join("arbitrary-name.bin"), b"firmware").unwrap();
        assert!(directory_contains_regular_file(&bios));
    }

    #[test]
    fn core_metadata_folder_description_can_point_to_a_file_sentinel() {
        let root = tempfile::tempdir().unwrap();
        let info = root.path().join("dolphin.info");
        fs::write(
            &info,
            "firmware0_desc = \"Dolphin 'Sys' folder\"\nfirmware0_path = \"dolphin-emu/Sys/codehandler.bin\"\nfirmware0_opt = \"false\"\n",
        )
        .unwrap();

        let parsed = load_core_firmware(&info);
        assert!(!parsed.files[0].directory);
        assert_eq!(parsed.files[0].path, "dolphin-emu/Sys/codehandler.bin");
    }
}
