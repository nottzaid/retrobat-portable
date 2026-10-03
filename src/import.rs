use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use sha1::{Digest as Sha1Digest, Sha1};
use thiserror::Error;

use crate::browse::BrowseEntry;
use crate::install::{InstallError, digest_file, ensure_safe_parent};
use crate::paths::PortableLayout;

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("RetroBat's system configuration is missing: {0}")]
    MissingConfig(PathBuf),
    #[error("RetroBat's system configuration could not be read: {0}")]
    Config(String),
    #[error("catalogue system “{0}” is not mapped to a RetroBat system")]
    UnknownSystem(String),
    #[error("the selected file has no filename")]
    MissingFilename,
    #[error("the selected path is not a regular file: {0}")]
    NotAFile(PathBuf),
    #[error("the selected path is not a safe directory: {0}")]
    NotADirectory(PathBuf),
    #[error("no launchable game file was found inside {0}")]
    NoDirectoryLaunch(PathBuf),
    #[error("the selected file type {extension} is not accepted for {system}")]
    UnsupportedExtension { system: String, extension: String },
    #[error("RAR import needs 7-Zip, but no usable extractor was found")]
    ArchiveToolMissing,
    #[error("could not {action} the RAR archive: {message}")]
    ArchiveCommand {
        action: &'static str,
        message: String,
    },
    #[error("the RAR archive contains no files")]
    EmptyArchive,
    #[error("disc playlist or descriptor references an unsafe path: {0}")]
    UnsafeReference(PathBuf),
    #[error("disc playlist or descriptor references a missing file: {0}")]
    MissingReferencedFile(PathBuf),
    #[error("could not parse disc descriptor {path}: {message}")]
    Descriptor { path: PathBuf, message: String },
    #[error("an imported copy already exists at {0}")]
    DestinationExists(PathBuf),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("destination safety check failed: {0}")]
    Safety(#[from] InstallError),
    #[error("import record could not be written: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("import record is invalid: {0}")]
    InvalidManifest(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportReport {
    pub system: String,
    pub launch_file: PathBuf,
    pub imported_files: usize,
    pub imported_bytes: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RemoveImportReport {
    pub removed: Vec<PathBuf>,
    pub preserved_modified: Vec<PathBuf>,
    pub already_missing: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImportCoverage {
    pub total_entries: usize,
    pub covered_entries: usize,
    pub uncovered_entry_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedManifest {
    pub schema_version: u32,
    pub catalog_id: String,
    pub title: String,
    pub system: String,
    pub launch_relative_path: PathBuf,
    #[serde(default)]
    pub source_sha1: Option<String>,
    #[serde(default)]
    pub matched_catalog_sha1: Option<bool>,
    pub files: Vec<ImportedFile>,
    pub imported_at_unix: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedFile {
    pub relative_path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
struct SystemProfile {
    extensions: BTreeSet<String>,
    rom_folder: String,
}

pub struct GameImporter<'a> {
    layout: &'a PortableLayout,
}

impl<'a> GameImporter<'a> {
    pub fn new(layout: &'a PortableLayout) -> Self {
        Self { layout }
    }

    pub fn import(&self, entry: &BrowseEntry, source: &Path) -> Result<ImportReport, ImportError> {
        if normalized_extension(source) == ".rar" {
            return self.import_rar(entry, source);
        }
        let profiles = load_system_profiles(&self.layout.systems_config())?;
        reject_non_file_or_symlink(source)?;
        let extension = normalized_extension(source);
        let profile_name = resolve_system_for_import(entry, &extension, &profiles)
            .ok_or_else(|| ImportError::UnknownSystem(entry.system.clone()))?;
        let profile = &profiles[&profile_name];
        let system = profile.rom_folder.clone();

        if !profile.extensions.contains(&extension) {
            return Err(ImportError::UnsupportedExtension { system, extension });
        }
        let source_sha1 = should_verify_sha1(&system, &extension)
            .then(|| digest_sha1(source))
            .transpose()?;
        let matched_catalog_sha1 = (!entry.known_sha1.is_empty()).then(|| {
            source_sha1
                .as_ref()
                .is_some_and(|actual| entry.known_sha1.contains(actual))
        });

        let source_root = source.parent().ok_or(ImportError::MissingFilename)?;
        let source_root = source_root.canonicalize()?;
        let source = source.canonicalize()?;
        if !source.starts_with(&source_root) {
            return Err(ImportError::UnsafeReference(source));
        }

        let mut sources = BTreeMap::new();
        collect_related_files(&source_root, &source, &mut sources, &mut BTreeSet::new())?;
        let launch_source_relative = source
            .strip_prefix(&source_root)
            .map_err(|_| ImportError::UnsafeReference(source.clone()))?
            .to_owned();

        let operation_id = format!(
            "import-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let stage_dir = self.layout.staging_root().join(operation_id);
        let payload = stage_dir.join("payload");
        fs::create_dir_all(&payload)?;

        let result = (|| {
            let multi_file = sources.len() > 1;
            let destination_relative_root = if multi_file {
                PathBuf::from("RetroBat")
                    .join("roms")
                    .join(&system)
                    .join(safe_component(&entry.title, &entry.id))
            } else {
                PathBuf::from("RetroBat").join("roms").join(&system)
            };
            let destination_root = self.layout.root.join(&destination_relative_root);
            let launch_relative_in_destination = if multi_file {
                launch_source_relative.clone()
            } else {
                PathBuf::from(source.file_name().ok_or(ImportError::MissingFilename)?)
            };
            let final_launch = destination_root.join(&launch_relative_in_destination);

            if multi_file {
                ensure_safe_parent(&self.layout.root, &destination_relative_root)?;
                if fs::symlink_metadata(&destination_root).is_ok() {
                    return Err(ImportError::DestinationExists(destination_root));
                }
            } else {
                let final_relative =
                    destination_relative_root.join(&launch_relative_in_destination);
                ensure_safe_parent(&self.layout.root, &final_relative)?;
                if fs::symlink_metadata(&final_launch).is_ok() {
                    return Err(ImportError::DestinationExists(final_launch));
                }
            }

            let mut imported = Vec::with_capacity(sources.len());
            for (relative, source_file) in &sources {
                let staged = payload.join(relative);
                if let Some(parent) = staged.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(source_file, &staged)?;
                let (size, sha256) = digest_file(&staged)?;
                imported.push(ImportedFile {
                    relative_path: if multi_file {
                        destination_relative_root.join(relative)
                    } else {
                        destination_relative_root.join(
                            source_file
                                .file_name()
                                .ok_or(ImportError::MissingFilename)?,
                        )
                    },
                    sha256,
                    size,
                });
            }

            if multi_file {
                fs::rename(&payload, &destination_root)?;
            } else {
                fs::rename(payload.join(&launch_source_relative), &final_launch)?;
            }

            let launch_relative_path =
                destination_relative_root.join(&launch_relative_in_destination);
            let manifest = ImportedManifest {
                schema_version: 1,
                catalog_id: entry.id.clone(),
                title: entry.title.clone(),
                system: system.clone(),
                launch_relative_path: launch_relative_path.clone(),
                source_sha1,
                matched_catalog_sha1,
                files: imported,
                imported_at_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            };
            if let Err(error) = write_manifest(self.layout, &manifest) {
                if multi_file {
                    let _ = fs::remove_dir_all(&destination_root);
                } else {
                    let _ = fs::remove_file(&final_launch);
                }
                return Err(error);
            }

            Ok(ImportReport {
                system,
                launch_file: self.layout.root.join(launch_relative_path),
                imported_files: manifest.files.len(),
                imported_bytes: manifest.files.iter().map(|file| file.size).sum(),
            })
        })();

        let _ = fs::remove_dir_all(&stage_dir);
        result
    }

    fn import_rar(&self, entry: &BrowseEntry, source: &Path) -> Result<ImportReport, ImportError> {
        reject_non_file_or_symlink(source)?;
        let source = source.canonicalize()?;
        let operation_id = format!(
            "import-rar-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let stage_dir = self.layout.staging_root().join(operation_id);
        let payload = stage_dir.join("payload");
        fs::create_dir_all(&payload)?;

        let result = (|| {
            let listing = run_7zip(
                self.layout,
                ["l", "-slt", "-ba"]
                    .into_iter()
                    .map(Into::into)
                    .chain(std::iter::once(source.as_os_str().to_owned())),
            )?;
            require_archive_success("inspect", &listing)?;
            validate_rar_listing(&listing.stdout)?;

            let output_directory = format!("-o{}", payload.display());
            let extraction = run_7zip(
                self.layout,
                ["x", "-y", "-bb0"]
                    .into_iter()
                    .map(Into::into)
                    .chain(std::iter::once(output_directory.into()))
                    .chain(std::iter::once(source.as_os_str().to_owned())),
            )?;
            require_archive_success("extract", &extraction)?;
            self.import_directory(entry, &payload)
        })();

        let _ = fs::remove_dir_all(&stage_dir);
        result
    }

    /// Imports a complete extracted game/application directory. This is the
    /// normal shape for PS3, PS4, Wii U and many native Windows games; copying
    /// only the selected executable would silently omit required assets and
    /// DLLs.
    pub fn import_directory(
        &self,
        entry: &BrowseEntry,
        source: &Path,
    ) -> Result<ImportReport, ImportError> {
        let profiles = load_system_profiles(&self.layout.systems_config())?;
        reject_non_directory_or_symlink(source)?;
        let profile_name = resolve_system(&entry.system, &profiles)
            .ok_or_else(|| ImportError::UnknownSystem(entry.system.clone()))?;
        let profile = &profiles[&profile_name];
        let system = profile.rom_folder.clone();
        let source = source.canonicalize()?;
        let mut sources = BTreeMap::new();
        collect_directory_files(&source, &source, &mut sources)?;
        if sources.is_empty() {
            return Err(ImportError::NoDirectoryLaunch(source));
        }

        let directory_marker = match system.to_ascii_lowercase().as_str() {
            "ps3" => Some("ps3"),
            "ps4" => Some("ps4"),
            _ => None,
        };
        let launch_source = select_directory_launch(&system, profile, &sources)
            .ok_or_else(|| ImportError::NoDirectoryLaunch(source.clone()))?;
        let destination_name = safe_component(&entry.title, &entry.id);
        let destination_name = directory_marker
            .map(|extension| format!("{destination_name}.{extension}"))
            .unwrap_or(destination_name);
        let destination_relative_root = PathBuf::from("RetroBat")
            .join("roms")
            .join(&system)
            .join(destination_name);
        ensure_safe_parent(&self.layout.root, &destination_relative_root)?;
        let destination_root = self.layout.root.join(&destination_relative_root);
        if fs::symlink_metadata(&destination_root).is_ok() {
            return Err(ImportError::DestinationExists(destination_root));
        }

        let operation_id = format!(
            "import-dir-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let stage_dir = self.layout.staging_root().join(operation_id);
        let payload = stage_dir.join("payload");
        fs::create_dir_all(&payload)?;

        let result = (|| {
            let mut imported = Vec::with_capacity(sources.len());
            for (relative, source_file) in &sources {
                let staged = payload.join(relative);
                if let Some(parent) = staged.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(source_file, &staged)?;
                let (size, sha256) = digest_file(&staged)?;
                imported.push(ImportedFile {
                    relative_path: destination_relative_root.join(relative),
                    sha256,
                    size,
                });
            }
            fs::rename(&payload, &destination_root)?;
            let launch_relative_path = if directory_marker.is_some() {
                destination_relative_root.clone()
            } else {
                destination_relative_root.join(&launch_source)
            };
            let manifest = ImportedManifest {
                schema_version: 1,
                catalog_id: entry.id.clone(),
                title: entry.title.clone(),
                system: system.clone(),
                launch_relative_path: launch_relative_path.clone(),
                source_sha1: None,
                matched_catalog_sha1: None,
                files: imported,
                imported_at_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            };
            if let Err(error) = write_manifest(self.layout, &manifest) {
                let _ = fs::remove_dir_all(&destination_root);
                return Err(error);
            }
            Ok(ImportReport {
                system,
                launch_file: self.layout.root.join(launch_relative_path),
                imported_files: manifest.files.len(),
                imported_bytes: manifest.files.iter().map(|file| file.size).sum(),
            })
        })();
        let _ = fs::remove_dir_all(&stage_dir);
        result
    }

    pub fn audit_coverage(&self, entries: &[BrowseEntry]) -> Result<ImportCoverage, ImportError> {
        let profiles = load_system_profiles(&self.layout.systems_config())?;
        let uncovered_entry_ids = entries
            .iter()
            .filter(|entry| import_route_systems(entry, &profiles).is_empty())
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        Ok(ImportCoverage {
            total_entries: entries.len(),
            covered_entries: entries.len() - uncovered_entry_ids.len(),
            uncovered_entry_ids,
        })
    }

    pub fn register_existing(
        &self,
        entry: &BrowseEntry,
        catalogue_system: &str,
        launch_file: &Path,
    ) -> Result<ImportReport, ImportError> {
        let profiles = load_system_profiles(&self.layout.systems_config())?;
        let profile_name = resolve_system(catalogue_system, &profiles)
            .ok_or_else(|| ImportError::UnknownSystem(catalogue_system.to_owned()))?;
        let profile = &profiles[&profile_name];
        reject_non_file_or_symlink(launch_file)?;
        let extension = normalized_extension(launch_file);
        if !profile.extensions.contains(&extension) {
            return Err(ImportError::UnsupportedExtension {
                system: profile.rom_folder.clone(),
                extension,
            });
        }
        let launch_file = launch_file.canonicalize()?;
        let system_root = self
            .layout
            .retrobat_root()
            .join("roms")
            .join(&profile.rom_folder)
            .canonicalize()?;
        if !launch_file.starts_with(&system_root) {
            return Err(ImportError::UnsafeReference(launch_file));
        }
        let launch_relative_path = launch_file
            .strip_prefix(&self.layout.root)
            .map_err(|_| ImportError::UnsafeReference(launch_file.clone()))?
            .to_owned();
        validate_relative(&launch_relative_path)?;
        let (size, sha256) = digest_file(&launch_file)?;
        let source_sha1 =
            should_verify_sha1(&profile.rom_folder, &normalized_extension(&launch_file))
                .then(|| digest_sha1(&launch_file))
                .transpose()?;
        let matched_catalog_sha1 = (!entry.known_sha1.is_empty()).then(|| {
            source_sha1
                .as_ref()
                .is_some_and(|actual| entry.known_sha1.contains(actual))
        });
        let manifest = ImportedManifest {
            schema_version: 1,
            catalog_id: entry.id.clone(),
            title: entry.title.clone(),
            system: profile.rom_folder.clone(),
            launch_relative_path: launch_relative_path.clone(),
            source_sha1,
            matched_catalog_sha1,
            files: vec![ImportedFile {
                relative_path: launch_relative_path,
                sha256,
                size,
            }],
            imported_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        write_manifest(self.layout, &manifest)?;
        Ok(ImportReport {
            system: manifest.system,
            launch_file,
            imported_files: 1,
            imported_bytes: size,
        })
    }
}

pub fn is_imported(layout: &PortableLayout, catalog_id: &str) -> bool {
    imported_manifest(layout, catalog_id).is_ok_and(|manifest| manifest.is_some())
}

pub fn remove_import(
    layout: &PortableLayout,
    catalog_id: &str,
) -> Result<RemoveImportReport, ImportError> {
    let record_path = manifest_path(layout, catalog_id);
    let manifest: ImportedManifest = serde_json::from_reader(File::open(&record_path)?)?;
    if manifest.schema_version != 1
        || manifest.catalog_id != catalog_id
        || manifest.files.is_empty()
    {
        return Err(ImportError::InvalidManifest(
            record_path.display().to_string(),
        ));
    }

    let system_root_relative = Path::new("RetroBat").join("roms").join(&manifest.system);
    validate_relative(&system_root_relative)?;
    let system_root = layout.root.join(&system_root_relative);
    let mut seen = BTreeSet::new();
    let mut report = RemoveImportReport::default();
    let mut parents = BTreeSet::new();

    for imported in &manifest.files {
        validate_relative(&imported.relative_path)?;
        if !imported.relative_path.starts_with(&system_root_relative)
            || !seen.insert(imported.relative_path.clone())
        {
            return Err(ImportError::InvalidManifest(
                imported.relative_path.display().to_string(),
            ));
        }
        validate_existing_parent_chain(&layout.root, &imported.relative_path)?;
        let destination = layout.root.join(&imported.relative_path);
        if let Some(parent) = destination.parent() {
            parents.insert(parent.to_owned());
        }
        match fs::symlink_metadata(&destination) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                report.already_missing.push(destination);
            }
            Err(error) => return Err(error.into()),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                report.preserved_modified.push(destination);
            }
            Ok(_) => {
                let (size, sha256) = digest_file(&destination)?;
                if size == imported.size && sha256 == imported.sha256 {
                    fs::remove_file(&destination)?;
                    report.removed.push(destination);
                } else {
                    report.preserved_modified.push(destination);
                }
            }
        }
    }

    fs::remove_file(record_path)?;
    for parent in parents.into_iter().rev() {
        prune_empty_import_directories(&parent, &system_root)?;
    }
    Ok(report)
}

pub fn imported_manifest(
    layout: &PortableLayout,
    catalog_id: &str,
) -> Result<Option<ImportedManifest>, ImportError> {
    let path = manifest_path(layout, catalog_id);
    let input = match File::open(&path) {
        Ok(input) => input,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let manifest: ImportedManifest = serde_json::from_reader(input)?;
    if manifest.schema_version != 1 || manifest.catalog_id != catalog_id {
        return Err(ImportError::InvalidManifest(path.display().to_string()));
    }
    validate_relative(&manifest.launch_relative_path)?;
    let expected_prefix = Path::new("RetroBat").join("roms");
    if !manifest.launch_relative_path.starts_with(&expected_prefix) {
        return Err(ImportError::InvalidManifest(
            manifest.launch_relative_path.display().to_string(),
        ));
    }
    let launch = layout.root.join(&manifest.launch_relative_path);
    if launch.is_dir() {
        reject_non_directory_or_symlink(&launch)?;
        let extension = normalized_extension(&launch);
        if !matches!(extension.as_str(), ".ps3" | ".ps4") {
            return Err(ImportError::InvalidManifest(launch.display().to_string()));
        }
    } else {
        reject_non_file_or_symlink(&launch)?;
    }
    Ok(Some(manifest))
}

fn load_system_profiles(path: &Path) -> Result<BTreeMap<String, SystemProfile>, ImportError> {
    if !path.is_file() {
        return Err(ImportError::MissingConfig(path.to_owned()));
    }
    let mut reader =
        Reader::from_file(path).map_err(|error| ImportError::Config(error.to_string()))?;
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut in_system = false;
    let mut field = None;
    let mut name = None;
    let mut extensions = None;
    let mut rom_path = None;
    let mut profiles = BTreeMap::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) if start.name().as_ref() == b"system" => {
                in_system = true;
                name = None;
                extensions = None;
                rom_path = None;
            }
            Ok(Event::Start(start)) if in_system && start.name().as_ref() == b"name" => {
                field = Some("name");
            }
            Ok(Event::Start(start)) if in_system && start.name().as_ref() == b"extension" => {
                field = Some("extension");
            }
            Ok(Event::Start(start)) if in_system && start.name().as_ref() == b"path" => {
                field = Some("path");
            }
            Ok(Event::Text(text)) if in_system => {
                let value = text
                    .decode()
                    .map_err(|error| ImportError::Config(error.to_string()))?
                    .into_owned();
                match field {
                    Some("name") => name = Some(value),
                    Some("extension") => extensions = Some(value),
                    Some("path") => rom_path = Some(value),
                    _ => {}
                }
            }
            Ok(Event::End(end)) if end.name().as_ref() == b"name" => field = None,
            Ok(Event::End(end)) if end.name().as_ref() == b"extension" => field = None,
            Ok(Event::End(end)) if end.name().as_ref() == b"path" => field = None,
            Ok(Event::End(end)) if end.name().as_ref() == b"system" => {
                if let (Some(name), Some(raw_extensions)) = (name.take(), extensions.take()) {
                    let extensions = raw_extensions
                        .split_whitespace()
                        .map(|extension| extension.to_ascii_lowercase())
                        .collect();
                    let rom_folder = rom_path
                        .take()
                        .and_then(|path| {
                            path.replace('\\', "/")
                                .trim_end_matches('/')
                                .rsplit('/')
                                .next()
                                .map(str::to_owned)
                        })
                        .filter(|folder| !folder.is_empty())
                        .unwrap_or_else(|| name.to_ascii_lowercase());
                    profiles.insert(
                        name,
                        SystemProfile {
                            extensions,
                            rom_folder,
                        },
                    );
                }
                in_system = false;
                field = None;
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(ImportError::Config(error.to_string())),
        }
        buffer.clear();
    }
    profiles
        .entry("chip8".into())
        .or_insert_with(|| SystemProfile {
            extensions: [".ch8", ".sc8", ".xo8"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            rom_folder: "chip8".into(),
        });
    Ok(profiles)
}

fn resolve_system(
    catalogue_system: &str,
    profiles: &BTreeMap<String, SystemProfile>,
) -> Option<String> {
    if let Some(name) = profiles
        .keys()
        .find(|name| name.eq_ignore_ascii_case(catalogue_system))
    {
        return Some(name.clone());
    }
    let alias = canonical_system_alias(catalogue_system)?;
    profiles
        .keys()
        .find(|name| name.eq_ignore_ascii_case(alias))
        .cloned()
}

pub(crate) fn canonical_system_alias(catalogue_system: &str) -> Option<&str> {
    Some(match catalogue_system {
        "chip-8" => "chip8",
        "doom" => "gzdoom",
        "handheld-electronic-game" => "lcdgames",
        "jump-n-bump" => "ports",
        "mattel-intellivision" => "intellivision",
        "nec-pc-engine-supergrafx" => "supergrafx",
        "nec-pc-engine-turbografx-16" => "pcengine",
        "nintendo-gamecube-wii" => "gamecube",
        "nintendo-nintendo-64" => "n64",
        "nintendo-pokemon-mini" => "pokemini",
        "pb" => "powerbomberman",
        "pocketcdg" => "karaoke",
        "quake-ii" => "quake2",
        "rick-dangerous" => "ports",
        "sega-saturn" => "saturn",
        "snk-neo-geo-pocket" => "ngp",
        "sony-playstation-portable" => "psp",
        "super-bros-war" => "superbroswar",
        "tic-80" => "tic80",
        "tomb-raider" => "openlara",
        "wasm-4" => "wasm4",
        "wolfenstein-3d" => "ecwolf",
        _ => return None,
    })
}

fn resolve_system_for_import(
    entry: &BrowseEntry,
    extension: &str,
    profiles: &BTreeMap<String, SystemProfile>,
) -> Option<String> {
    if entry.source_id == "homebrew-hub" && entry.system == "unknown" {
        let inferred = match extension {
            ".gb" => "gb",
            ".gbc" => "gbc",
            ".gba" => "gba",
            ".nes" => "nes",
            _ => return None,
        };
        return profiles
            .keys()
            .find(|name| name.eq_ignore_ascii_case(inferred))
            .cloned();
    }
    resolve_system(&entry.system, profiles)
}

fn import_route_systems(
    entry: &BrowseEntry,
    profiles: &BTreeMap<String, SystemProfile>,
) -> Vec<String> {
    if entry.source_id == "homebrew-hub" && entry.system == "unknown" {
        return ["gb", "gbc", "gba", "nes"]
            .iter()
            .filter_map(|candidate| {
                profiles
                    .keys()
                    .find(|name| name.eq_ignore_ascii_case(candidate))
                    .cloned()
            })
            .collect();
    }
    resolve_system(&entry.system, profiles)
        .into_iter()
        .collect()
}

fn normalized_extension(path: &Path) -> String {
    path.extension()
        .map(|extension| format!(".{}", extension.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default()
}

fn should_verify_sha1(system: &str, extension: &str) -> bool {
    system == "mame"
        || !matches!(
            extension,
            ".zip" | ".7z" | ".cue" | ".gdi" | ".m3u" | ".chd" | ".iso" | ".cso" | ".rvz" | ".wbfs"
        )
}

fn run_7zip(
    layout: &PortableLayout,
    arguments: impl IntoIterator<Item = std::ffi::OsString> + Clone,
) -> Result<Output, ImportError> {
    for program in seven_zip_candidates(layout) {
        match Command::new(&program).args(arguments.clone()).output() {
            Ok(output) => return Ok(output),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ImportError::ArchiveCommand {
                    action: "start 7-Zip for",
                    message: format!("{}: {error}", program.display()),
                });
            }
        }
    }
    Err(ImportError::ArchiveToolMissing)
}

fn seven_zip_candidates(layout: &PortableLayout) -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        vec![
            layout
                .retrobat_root()
                .join("emulationstation")
                .join("7z.exe"),
            layout
                .retrobat_root()
                .join("emulationstation")
                .join("7za.exe"),
            PathBuf::from("7z.exe"),
        ]
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = layout;
        vec![PathBuf::from("7z"), PathBuf::from("7zz")]
    }
}

fn require_archive_success(action: &'static str, output: &Output) -> Result<(), ImportError> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let message = if !stderr.is_empty() { stderr } else { stdout };
    Err(ImportError::ArchiveCommand {
        action,
        message: if message.is_empty() {
            format!("7-Zip exited with {}", output.status)
        } else {
            message
        },
    })
}

fn validate_rar_listing(listing: &[u8]) -> Result<(), ImportError> {
    let listing = String::from_utf8_lossy(listing);
    let mut entries = BTreeSet::new();
    let mut current = None;

    for line in listing.lines() {
        if let Some(value) = line.strip_prefix("Path = ") {
            let normalized = value.replace('\\', "/");
            let path = PathBuf::from(&normalized);
            validate_relative(&path)?;
            if path.components().any(|component| match component {
                Component::Normal(value) => {
                    let value = value.to_string_lossy();
                    value.contains(':') || value.chars().any(char::is_control)
                }
                _ => true,
            }) {
                return Err(ImportError::UnsafeReference(path));
            }
            if !entries.insert(path.clone()) {
                return Err(ImportError::UnsafeReference(path));
            }
            current = Some(path);
        } else if ["Symbolic Link = ", "Hard Link = ", "Copy Link = "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .is_some_and(|value| !value.trim().is_empty() && value.trim() != "-")
            || line
                .strip_prefix("Alternate Stream = ")
                .is_some_and(|value| value.trim() != "-")
        {
            return Err(ImportError::UnsafeReference(
                current
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("archive-entry")),
            ));
        }
    }

    if entries.is_empty() {
        Err(ImportError::EmptyArchive)
    } else {
        Ok(())
    }
}

fn digest_sha1(path: &Path) -> Result<String, io::Error> {
    let mut file = File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn reject_non_file_or_symlink(path: &Path) -> Result<(), ImportError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ImportError::NotAFile(path.to_owned()));
    }
    Ok(())
}

fn reject_non_directory_or_symlink(path: &Path) -> Result<(), ImportError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ImportError::NotADirectory(path.to_owned()));
    }
    Ok(())
}

fn collect_directory_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<PathBuf, PathBuf>,
) -> Result<(), ImportError> {
    reject_non_directory_or_symlink(directory)?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(ImportError::UnsafeReference(path));
        }
        if metadata.is_dir() {
            collect_directory_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| ImportError::UnsafeReference(path.clone()))?
                .to_owned();
            validate_relative(&relative)?;
            files.insert(relative, path);
        }
    }
    Ok(())
}

fn select_directory_launch(
    system: &str,
    profile: &SystemProfile,
    files: &BTreeMap<PathBuf, PathBuf>,
) -> Option<PathBuf> {
    let system = system.to_ascii_lowercase();
    let mut candidates = files
        .keys()
        .filter(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            match system.as_str() {
                "ps3" | "ps4" => name == "eboot.bin",
                "wiiu" => normalized_extension(path) == ".rpx",
                "windows" => {
                    normalized_extension(path) == ".exe"
                        && !matches!(
                            name.as_str(),
                            "setup.exe" | "uninstall.exe" | "unins000.exe"
                        )
                }
                _ => profile.extensions.contains(&normalized_extension(path)),
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| {
        let depth = path.components().count();
        let name = path
            .file_stem()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let utility =
            name.contains("launcher") || name.contains("config") || name.contains("crash");
        (utility, depth, path.to_string_lossy().len())
    });
    candidates.into_iter().next()
}

fn collect_related_files(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<PathBuf, PathBuf>,
    visited_descriptors: &mut BTreeSet<PathBuf>,
) -> Result<(), ImportError> {
    reject_non_file_or_symlink(path)?;
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(root) {
        return Err(ImportError::UnsafeReference(path.to_owned()));
    }
    let relative = canonical
        .strip_prefix(root)
        .map_err(|_| ImportError::UnsafeReference(path.to_owned()))?
        .to_owned();
    validate_relative(&relative)?;
    files.insert(relative, canonical.clone());

    let extension = normalized_extension(&canonical);
    if !matches!(extension.as_str(), ".cue" | ".gdi" | ".m3u")
        || !visited_descriptors.insert(canonical.clone())
    {
        return Ok(());
    }

    let references = match extension.as_str() {
        ".cue" => parse_cue(&canonical)?,
        ".gdi" => parse_gdi(&canonical)?,
        ".m3u" => parse_m3u(&canonical)?,
        _ => Vec::new(),
    };
    let descriptor_dir = canonical.parent().ok_or(ImportError::MissingFilename)?;
    for reference in references {
        validate_relative(&reference)?;
        let referenced = descriptor_dir.join(reference);
        if !referenced.is_file() {
            return Err(ImportError::MissingReferencedFile(referenced));
        }
        collect_related_files(root, &referenced, files, visited_descriptors)?;
    }
    Ok(())
}

fn parse_cue(path: &Path) -> Result<Vec<PathBuf>, ImportError> {
    let mut references = Vec::new();
    for line in lines(path)? {
        let trimmed = line.trim();
        if !trimmed.to_ascii_uppercase().starts_with("FILE ") {
            continue;
        }
        let remainder = trimmed[5..].trim();
        let name = if let Some(remainder) = remainder.strip_prefix('"') {
            remainder
                .split_once('"')
                .map(|(name, _)| name)
                .ok_or_else(|| descriptor_error(path, "unterminated quoted FILE name"))?
        } else {
            remainder
                .rsplit_once(char::is_whitespace)
                .map(|(name, _)| name)
                .ok_or_else(|| descriptor_error(path, "FILE line has no type"))?
        };
        references.push(PathBuf::from(name));
    }
    Ok(references)
}

fn parse_gdi(path: &Path) -> Result<Vec<PathBuf>, ImportError> {
    let all_lines = lines(path)?;
    let mut references = Vec::new();
    for (index, line) in all_lines.into_iter().enumerate() {
        if index == 0 || line.trim().is_empty() {
            continue;
        }
        let tokens = split_quoted(&line);
        if tokens.len() < 6 {
            return Err(descriptor_error(
                path,
                "track line has fewer than six fields",
            ));
        }
        references.push(PathBuf::from(&tokens[4]));
    }
    Ok(references)
}

fn parse_m3u(path: &Path) -> Result<Vec<PathBuf>, ImportError> {
    Ok(lines(path)?
        .into_iter()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(PathBuf::from)
        .collect())
}

fn lines(path: &Path) -> Result<Vec<String>, ImportError> {
    BufReader::new(File::open(path)?)
        .lines()
        .collect::<Result<Vec<_>, _>>()
        .map_err(ImportError::Io)
}

fn split_quoted(line: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in line.chars() {
        match character {
            '"' => quoted = !quoted,
            character if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    output.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        output.push(current);
    }
    output
}

fn validate_relative(path: &Path) -> Result<(), ImportError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ImportError::UnsafeReference(path.to_owned()));
    }
    Ok(())
}

fn validate_existing_parent_chain(root: &Path, relative: &Path) -> Result<(), ImportError> {
    let Some(parent) = relative.parent() else {
        return Err(ImportError::UnsafeReference(relative.to_owned()));
    };
    let mut current = root.to_owned();
    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(ImportError::UnsafeReference(relative.to_owned()));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(ImportError::UnsafeReference(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn prune_empty_import_directories(directory: &Path, system_root: &Path) -> Result<(), ImportError> {
    let mut current = directory.to_owned();
    while current.starts_with(system_root) && current != system_root {
        match fs::remove_dir(&current) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(error) => return Err(error.into()),
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_owned();
    }
    Ok(())
}

fn safe_component(title: &str, fallback: &str) -> String {
    let sanitized = title
        .chars()
        .map(|character| match character {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            character if character.is_control() => '_',
            _ => character,
        })
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_owned();
    if sanitized.is_empty() {
        fallback.replace('/', "--")
    } else {
        sanitized
    }
}

fn descriptor_error(path: &Path, message: &str) -> ImportError {
    ImportError::Descriptor {
        path: path.to_owned(),
        message: message.to_owned(),
    }
}

fn manifest_path(layout: &PortableLayout, catalog_id: &str) -> PathBuf {
    layout
        .imported_root()
        .join(format!("{}.json", catalog_id.replace('/', "--")))
}

fn write_manifest(layout: &PortableLayout, manifest: &ImportedManifest) -> Result<(), ImportError> {
    fs::create_dir_all(layout.imported_root())?;
    let final_path = manifest_path(layout, &manifest.catalog_id);
    let temporary = final_path.with_extension("json.tmp");
    let mut output = File::create(&temporary)?;
    serde_json::to_writer_pretty(&mut output, manifest)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    drop(output);
    fs::rename(temporary, final_path)?;
    Ok(())
}
