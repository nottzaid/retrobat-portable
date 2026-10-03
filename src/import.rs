use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::browse::BrowseEntry;
use crate::install::{InstallError, ensure_safe_parent};
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
    #[error("cannot read {path}: {source}")]
    Source { path: PathBuf, source: io::Error },
    #[error("the selected path is not a regular file: {0}")]
    NotAFile(PathBuf),
    #[error("the selected path is not a safe directory: {0}")]
    NotADirectory(PathBuf),
    #[error("no launchable game file was found inside {0}")]
    NoDirectoryLaunch(PathBuf),
    #[error("the selected file type {extension} is not accepted for {system}")]
    UnsupportedExtension { system: String, extension: String },
    #[error(
        "MAME plays the intact ROM-set ZIP named after the machine (for example mspacman.zip). \
         Files such as {0} are chips extracted from that ZIP; select the ZIP itself"
    )]
    MameNeedsRomSet(String),
    #[error(
        "MAME plays the intact ROM-set ZIP named after the machine (for example mspacman.zip). \
         The folder {0} holds no such ZIP; select the ZIP file instead of a folder of its chips"
    )]
    MameNeedsRomSetNotFolder(String),
    #[error("this card is already imported; REMOVE it before importing another copy")]
    AlreadyImported,
    #[error("archive import needs 7-Zip, but no usable extractor was found")]
    ArchiveToolMissing,
    #[error("could not {action} the archive: {message}")]
    ArchiveCommand {
        action: &'static str,
        message: String,
    },
    #[error("the archive contains no files")]
    EmptyArchive,
    #[error("disc playlist or descriptor references an unsafe path: {0}")]
    UnsafeReference(PathBuf),
    #[error("disc playlist or descriptor references a missing file: {0}")]
    MissingReferencedFile(PathBuf),
    #[error("could not parse disc descriptor {path}: {message}")]
    Descriptor { path: PathBuf, message: String },
    #[error("no free destination name remains for {0}")]
    DestinationExhausted(PathBuf),
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
    /// Directories that belong to the import even while empty (PC games
    /// often expect their save or config folders to exist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<PathBuf>,
    /// A libretro core this content needs instead of the system default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<String>,
    pub imported_at_unix: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedFile {
    pub relative_path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SystemProfile {
    pub(crate) extensions: BTreeSet<String>,
    pub(crate) rom_folder: String,
}

/// Where a staged file comes from.
#[derive(Clone, Debug)]
enum Origin {
    /// A user's file: copied, never modified.
    Copy(PathBuf),
    /// A file RetroPort created in staging (download or extraction): moved.
    Owned(PathBuf),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Shape {
    /// One file directly in the system folder.
    Single,
    /// A game folder named after the title; `marker` makes it a RetroBat
    /// directory launch target (PS3/PS4).
    Folder { marker: Option<&'static str> },
}

/// Everything one import will place, before any destination is chosen.
struct Payload {
    files: BTreeMap<PathBuf, Origin>,
    directories: BTreeSet<PathBuf>,
    launch: PathBuf,
    shape: Shape,
    core: Option<String>,
}

pub struct GameImporter<'a> {
    layout: &'a PortableLayout,
}

impl<'a> GameImporter<'a> {
    pub fn new(layout: &'a PortableLayout) -> Self {
        Self { layout }
    }

    /// Imports whatever the user selected: a game file (with any disc
    /// tracks it references), a RAR archive, or a complete game folder.
    pub fn import(&self, entry: &BrowseEntry, source: &Path) -> Result<ImportReport, ImportError> {
        let metadata = fs::symlink_metadata(source).map_err(|error| ImportError::Source {
            path: source.to_owned(),
            source: error,
        })?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            return self.import_directory(entry, source);
        }
        if normalized_extension(source) == ".rar" {
            return self.import_rar(entry, source);
        }
        self.import_file(entry, source)
    }

    fn import_file(&self, entry: &BrowseEntry, source: &Path) -> Result<ImportReport, ImportError> {
        self.import_file_with(entry, source, false, None)
    }

    fn import_file_with(
        &self,
        entry: &BrowseEntry,
        source: &Path,
        owned: bool,
        core: Option<String>,
    ) -> Result<ImportReport, ImportError> {
        let profiles = load_system_profiles(&self.layout.systems_config())?;
        reject_non_file_or_symlink(source)?;
        // `.cgb` is another name for a Game Boy Color ROM, which RetroBat
        // lists only as `.gbc`; the copy takes that name.
        let renamed = (normalized_extension(source) == ".cgb").then(|| ".gbc".to_owned());
        let extension = renamed
            .clone()
            .unwrap_or_else(|| normalized_extension(source));
        let profile = resolve_system_for_import(entry, &extension, &profiles)
            .map(|name| profiles[&name].clone())
            .ok_or_else(|| ImportError::UnknownSystem(entry.system.clone()))?;
        let profile = if profile.extensions.contains(&extension) {
            profile
        } else {
            sibling_profile(&profile, &extension, &profiles).unwrap_or(profile)
        };
        if !profile.extensions.contains(&extension) {
            if profile.rom_folder == "mame" {
                return Err(ImportError::MameNeedsRomSet(file_name_text(source)));
            }
            return Err(ImportError::UnsupportedExtension {
                system: profile.rom_folder,
                extension,
            });
        }

        let source_root = source
            .parent()
            .ok_or(ImportError::MissingFilename)?
            .canonicalize()?;
        let source = source.canonicalize()?;
        let mut related = BTreeMap::new();
        collect_related_files(&source_root, &source, &mut related, &mut BTreeSet::new())?;
        let mut launch = source
            .strip_prefix(&source_root)
            .map_err(|_| ImportError::UnsafeReference(source.clone()))?
            .to_owned();
        if renamed.is_some()
            && let Some(origin) = related.remove(&launch)
        {
            launch.set_extension("gbc");
            related.insert(launch.clone(), origin);
        }
        let shape = if related.len() > 1 {
            Shape::Folder { marker: None }
        } else {
            Shape::Single
        };
        let files = related
            .into_iter()
            .map(|(relative, path)| {
                let origin = if owned {
                    Origin::Owned(path)
                } else {
                    Origin::Copy(path)
                };
                (relative, origin)
            })
            .collect();
        self.commit(
            entry,
            &profile,
            Payload {
                files,
                directories: BTreeSet::new(),
                launch,
                shape,
                core,
            },
        )
    }

    fn import_rar(&self, entry: &BrowseEntry, source: &Path) -> Result<ImportReport, ImportError> {
        reject_non_file_or_symlink(source)?;
        let source = source.canonicalize()?;
        let stage = StagingDirectory::new(self.layout, "import-rar")?;
        let payload = stage.path().join("payload");
        fs::create_dir_all(&payload)?;
        extract_archive(self.layout, &source, &payload)?;
        self.import_tree(entry, &payload, None, true, None)
    }

    /// Imports a complete extracted game or application folder. This is the
    /// normal shape for PS3, PS4, Wii U, PSP homebrew and many PC games;
    /// copying only the executable would silently omit required assets.
    pub fn import_directory(
        &self,
        entry: &BrowseEntry,
        source: &Path,
    ) -> Result<ImportReport, ImportError> {
        self.import_tree(entry, source, None, false, None)
    }

    pub(crate) fn import_tree(
        &self,
        entry: &BrowseEntry,
        source: &Path,
        launch: Option<&Path>,
        owned: bool,
        core: Option<String>,
    ) -> Result<ImportReport, ImportError> {
        let profiles = load_system_profiles(&self.layout.systems_config())?;
        reject_non_directory_or_symlink(source)?;
        let source = source.canonicalize()?;
        let mut sources = BTreeMap::new();
        let mut directories = BTreeSet::new();
        collect_directory_files(&source, &source, &mut sources, &mut directories)?;
        if sources.is_empty() {
            return Err(ImportError::NoDirectoryLaunch(source));
        }
        let candidates = import_route_systems(entry, &profiles);
        let (profile, launch) = candidates
            .iter()
            .find_map(|name| {
                let profile = &profiles[name];
                let launch = match launch {
                    Some(launch) => sources.contains_key(launch).then(|| launch.to_owned()),
                    None => select_directory_launch(profile, &sources),
                };
                launch.map(|launch| (profile.clone(), launch))
            })
            .ok_or_else(|| {
                if candidates.is_empty() {
                    ImportError::UnknownSystem(entry.system.clone())
                } else if candidates
                    .iter()
                    .any(|name| profiles[name].rom_folder == "mame")
                {
                    ImportError::MameNeedsRomSetNotFolder(file_name_text(&source))
                } else {
                    ImportError::NoDirectoryLaunch(source.clone())
                }
            })?;
        let marker = match profile.rom_folder.to_ascii_lowercase().as_str() {
            "ps3" => Some("ps3"),
            "ps4" => Some("ps4"),
            _ => None,
        };
        let files = sources
            .into_iter()
            .map(|(relative, path)| {
                let origin = if owned {
                    Origin::Owned(path)
                } else {
                    Origin::Copy(path)
                };
                (relative, origin)
            })
            .collect();
        self.commit(
            entry,
            &profile,
            Payload {
                files,
                directories,
                launch,
                shape: Shape::Folder { marker },
                core,
            },
        )
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

    /// Records a game that RetroBat's store installer placed under roms/.
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
        let digest = hash_file(
            &launch_file,
            should_verify_sha1(&profile.rom_folder, &extension),
        )?;
        let matched_catalog_sha1 = (!entry.known_sha1.is_empty()).then(|| {
            digest
                .sha1
                .as_ref()
                .is_some_and(|actual| entry.known_sha1.contains(actual))
        });
        let manifest = ImportedManifest {
            schema_version: 1,
            catalog_id: entry.id.clone(),
            title: entry.title.clone(),
            system: profile.rom_folder.clone(),
            launch_relative_path: launch_relative_path.clone(),
            source_sha1: digest.sha1,
            matched_catalog_sha1,
            files: vec![ImportedFile {
                relative_path: launch_relative_path,
                sha256: digest.sha256,
                size: digest.size,
            }],
            directories: Vec::new(),
            core: None,
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
            imported_bytes: digest.size,
        })
    }

    /// Places a payload transactionally: everything is staged and hashed on
    /// the destination volume first, then moved into a destination nobody
    /// else owns, then recorded. Any failure leaves no partial game behind.
    fn commit(
        &self,
        entry: &BrowseEntry,
        profile: &SystemProfile,
        payload: Payload,
    ) -> Result<ImportReport, ImportError> {
        if manifest_path(self.layout, &entry.id).exists() {
            return Err(ImportError::AlreadyImported);
        }
        for relative in payload.files.keys().chain(&payload.directories) {
            validate_relative(relative)?;
        }
        validate_relative(&payload.launch)?;
        let system_root = PathBuf::from("RetroBat")
            .join("roms")
            .join(&profile.rom_folder);
        validate_relative(&system_root)?;
        ensure_safe_parent(&self.layout.root, &system_root.join("placeholder"))?;

        let stage = StagingDirectory::new(self.layout, "import")?;
        let staged_root = stage.path().join("payload");
        fs::create_dir_all(&staged_root)?;
        let wants_sha1 =
            should_verify_sha1(&profile.rom_folder, &normalized_extension(&payload.launch));
        let mut staged = Vec::with_capacity(payload.files.len());
        for (relative, origin) in &payload.files {
            let destination = staged_root.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let sha1 = wants_sha1 && relative == &payload.launch;
            let digest = match origin {
                Origin::Copy(source) => copy_hashing(source, &destination, sha1)?,
                Origin::Owned(source) => {
                    fs::rename(source, &destination)?;
                    hash_file(&destination, sha1)?
                }
            };
            staged.push((relative.clone(), digest));
        }
        for directory in &payload.directories {
            fs::create_dir_all(staged_root.join(directory))?;
        }

        let title = safe_component(&entry.title, &entry.id);
        let mut placed = Vec::new();
        // `base` is the bundle-relative folder the payload's paths hang from.
        let (launch_relative_path, base) = match payload.shape {
            Shape::Single => {
                let file_name = payload
                    .launch
                    .file_name()
                    .ok_or(ImportError::MissingFilename)?;
                let direct = system_root.join(file_name);
                ensure_safe_parent(&self.layout.root, &direct)?;
                if reserve_file(&self.layout.root.join(&direct))? {
                    let staged_file = staged_root.join(&payload.launch);
                    if let Err(error) = fs::rename(&staged_file, self.layout.root.join(&direct)) {
                        let _ = fs::remove_file(self.layout.root.join(&direct));
                        return Err(error.into());
                    }
                    placed.push(direct.clone());
                    (direct, system_root.clone())
                } else {
                    // A different file already owns this name. Keep the
                    // file name intact (MAME and disc descriptors depend on
                    // it) inside a folder of its own instead.
                    let folder = self.place_folder(&staged_root, &system_root, &title, None)?;
                    placed.extend(payload.files.keys().map(|relative| folder.join(relative)));
                    (folder.join(&payload.launch), folder)
                }
            }
            Shape::Folder { marker } => {
                let folder = self.place_folder(&staged_root, &system_root, &title, marker)?;
                placed.extend(payload.files.keys().map(|relative| folder.join(relative)));
                let launch = if marker.is_some() {
                    folder.clone()
                } else {
                    folder.join(&payload.launch)
                };
                (launch, folder)
            }
        };
        let mut source_sha1 = None;
        let files = staged
            .into_iter()
            .map(|(relative, digest)| {
                if relative == payload.launch {
                    source_sha1 = digest.sha1.clone();
                }
                ImportedFile {
                    relative_path: base.join(&relative),
                    sha256: digest.sha256,
                    size: digest.size,
                }
            })
            .collect::<Vec<_>>();
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
            directories: payload
                .directories
                .iter()
                .map(|directory| base.join(directory))
                .collect(),
            files,
            core: payload.core,
            imported_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        if let Err(error) = write_manifest(self.layout, &manifest) {
            for relative in &placed {
                let _ = fs::remove_file(self.layout.root.join(relative));
            }
            if base != system_root {
                let _ = fs::remove_dir_all(self.layout.root.join(&base));
            }
            return Err(error);
        }
        drop(stage);
        Ok(ImportReport {
            system: manifest.system,
            launch_file: self.layout.root.join(launch_relative_path),
            imported_files: manifest.files.len(),
            imported_bytes: manifest.files.iter().map(|file| file.size).sum(),
        })
    }

    /// Moves the staged tree into the first free `<title>[ (n)][.marker]`
    /// folder of the system and returns its bundle-relative path.
    fn place_folder(
        &self,
        staged_root: &Path,
        system_root: &Path,
        title: &str,
        marker: Option<&str>,
    ) -> Result<PathBuf, ImportError> {
        for attempt in 1..=999u32 {
            let name = match (attempt, marker) {
                (1, Some(marker)) => format!("{title}.{marker}"),
                (1, None) => title.to_owned(),
                (n, Some(marker)) => format!("{title} ({n}).{marker}"),
                (n, None) => format!("{title} ({n})"),
            };
            let relative = system_root.join(name);
            ensure_safe_parent(&self.layout.root, &relative)?;
            let destination = self.layout.root.join(&relative);
            // create_dir is the atomic claim: it fails if anything exists.
            match fs::create_dir(&destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
            let moved = (|| -> io::Result<()> {
                for entry in fs::read_dir(staged_root)? {
                    let entry = entry?;
                    fs::rename(entry.path(), destination.join(entry.file_name()))?;
                }
                Ok(())
            })();
            if let Err(error) = moved {
                let _ = fs::remove_dir_all(&destination);
                return Err(error.into());
            }
            return Ok(relative);
        }
        Err(ImportError::DestinationExhausted(system_root.join(title)))
    }
}

/// Removes a staging directory on every exit path.
pub(crate) struct StagingDirectory(PathBuf);

impl StagingDirectory {
    pub(crate) fn new(layout: &PortableLayout, purpose: &str) -> io::Result<Self> {
        let path = layout.staging_root().join(format!(
            "{purpose}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Claims `path` for a new file: true when it did not exist and is now an
/// empty placeholder owned by this import, false when something else is there.
fn reserve_file(path: &Path) -> io::Result<bool> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

struct FileDigest {
    size: u64,
    sha256: String,
    sha1: Option<String>,
}

const IO_BUFFER: usize = 1 << 20;

/// Copies once, hashing the bytes as they pass: one read and one write per
/// file, instead of a copy followed by separate hashing reads.
fn copy_hashing(source: &Path, destination: &Path, sha1: bool) -> io::Result<FileDigest> {
    let mut input = File::open(source)?;
    let mut output = File::create(destination)?;
    let mut sha256 = Sha256::new();
    let mut sha1 = sha1.then(Sha1::new);
    let mut size = 0u64;
    let mut buffer = vec![0u8; IO_BUFFER];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        sha256.update(&buffer[..read]);
        if let Some(sha1) = &mut sha1 {
            sha1.update(&buffer[..read]);
        }
        size += read as u64;
    }
    if let Ok(metadata) = input.metadata() {
        let _ = output.set_permissions(metadata.permissions());
    }
    Ok(FileDigest {
        size,
        sha256: hex::encode(sha256.finalize()),
        sha1: sha1.map(|sha1| hex::encode(sha1.finalize())),
    })
}

fn hash_file(path: &Path, sha1: bool) -> io::Result<FileDigest> {
    let mut input = File::open(path)?;
    let mut sha256 = Sha256::new();
    let mut sha1 = sha1.then(Sha1::new);
    let mut size = 0u64;
    let mut buffer = vec![0u8; IO_BUFFER];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        sha256.update(&buffer[..read]);
        if let Some(sha1) = &mut sha1 {
            sha1.update(&buffer[..read]);
        }
        size += read as u64;
    }
    Ok(FileDigest {
        size,
        sha256: hex::encode(sha256.finalize()),
        sha1: sha1.map(|sha1| hex::encode(sha1.finalize())),
    })
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

    for relative in manifest
        .files
        .iter()
        .map(|file| &file.relative_path)
        .chain(&manifest.directories)
    {
        validate_relative(relative)?;
        if !relative.starts_with(&system_root_relative) || !seen.insert(relative.clone()) {
            return Err(ImportError::InvalidManifest(relative.display().to_string()));
        }
        validate_existing_parent_chain(&layout.root, relative)?;
    }

    for imported in &manifest.files {
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
                let digest = hash_file(&destination, false)?;
                if digest.size == imported.size && digest.sha256 == imported.sha256 {
                    fs::remove_file(&destination)?;
                    report.removed.push(destination);
                } else {
                    report.preserved_modified.push(destination);
                }
            }
        }
    }
    for directory in &manifest.directories {
        parents.insert(layout.root.join(directory));
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
    let manifest: ImportedManifest = serde_json::from_reader(io::BufReader::new(input))?;
    if manifest.catalog_id != catalog_id {
        return Err(ImportError::InvalidManifest(path.display().to_string()));
    }
    validate_manifest(layout, &manifest, &path)?;
    Ok(Some(manifest))
}

/// Every valid import record, keyed by catalogue id, each file read once.
/// Records that fail validation are skipped, so a damaged record cannot
/// offer PLAY for a file outside the installation.
pub fn imported_manifests(layout: &PortableLayout) -> BTreeMap<String, ImportedManifest> {
    let Ok(entries) = fs::read_dir(layout.imported_root()) else {
        return BTreeMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|entry| {
            let path = entry.path();
            let file = File::open(&path).ok()?;
            let manifest: ImportedManifest =
                serde_json::from_reader(io::BufReader::new(file)).ok()?;
            (manifest_path(layout, &manifest.catalog_id) == path
                && validate_manifest(layout, &manifest, &path).is_ok())
            .then(|| (manifest.catalog_id.clone(), manifest))
        })
        .collect()
}

fn validate_manifest(
    layout: &PortableLayout,
    manifest: &ImportedManifest,
    path: &Path,
) -> Result<(), ImportError> {
    if manifest.schema_version != 1 {
        return Err(ImportError::InvalidManifest(path.display().to_string()));
    }
    validate_relative(&manifest.launch_relative_path)?;
    let expected_prefix = Path::new("RetroBat").join("roms");
    if !manifest.launch_relative_path.starts_with(&expected_prefix) {
        return Err(ImportError::InvalidManifest(
            manifest.launch_relative_path.display().to_string(),
        ));
    }
    if let Some(core) = &manifest.core
        && !core
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ImportError::InvalidManifest(core.clone()));
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
    Ok(())
}

pub(crate) fn load_system_profiles(
    path: &Path,
) -> Result<BTreeMap<String, SystemProfile>, ImportError> {
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
                        .filter(|folder| {
                            !folder.is_empty()
                                && folder != "."
                                && folder != ".."
                                && !folder.contains(':')
                        })
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

pub(crate) fn resolve_system(
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

/// Homebrew Hub entries of unknown platform are identified by the ROM.
const UNKNOWN_HOMEBREW_SYSTEMS: [&str; 4] = ["gb", "gbc", "gba", "nes"];

/// Systems that share their emulators, so a card of one plays the other's
/// files: Game Boy games run on Game Boy Color cards and the reverse.
const SIBLING_SYSTEMS: [&[&str]; 1] = [&["gb", "gbc"]];

fn sibling_profile(
    profile: &SystemProfile,
    extension: &str,
    profiles: &BTreeMap<String, SystemProfile>,
) -> Option<SystemProfile> {
    let family = SIBLING_SYSTEMS
        .iter()
        .find(|family| family.contains(&profile.rom_folder.as_str()))?;
    profiles
        .values()
        .find(|candidate| {
            family.contains(&candidate.rom_folder.as_str())
                && candidate.extensions.contains(extension)
        })
        .cloned()
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
        return UNKNOWN_HOMEBREW_SYSTEMS
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

fn file_name_text(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn should_verify_sha1(system: &str, extension: &str) -> bool {
    system == "mame"
        || !matches!(
            extension,
            ".zip" | ".7z" | ".cue" | ".gdi" | ".m3u" | ".chd" | ".iso" | ".cso" | ".rvz" | ".wbfs"
        )
}

/// Extracts a ZIP, 7z or RAR archive into `destination`, exactly as it is,
/// after checking that no member could escape it.
pub(crate) fn extract_archive(
    layout: &PortableLayout,
    archive: &Path,
    destination: &Path,
) -> Result<(), ImportError> {
    extract_one(layout, archive, destination)
}

fn extract_one(
    layout: &PortableLayout,
    archive: &Path,
    destination: &Path,
) -> Result<(), ImportError> {
    let listing = run_7zip(
        layout,
        ["l", "-slt", "-ba"]
            .into_iter()
            .map(Into::into)
            .chain(std::iter::once(archive.as_os_str().to_owned())),
    )?;
    require_archive_success("inspect", &listing)?;
    validate_archive_listing(&listing.stdout)?;
    let output_directory = {
        let mut argument = std::ffi::OsString::from("-o");
        argument.push(destination.as_os_str());
        argument
    };
    let extraction = run_7zip(
        layout,
        ["x", "-y", "-bb0", "-snl-"]
            .into_iter()
            .map(Into::into)
            .chain(std::iter::once(output_directory))
            .chain(std::iter::once(archive.as_os_str().to_owned())),
    )?;
    require_archive_success("extract", &extraction)
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
        // RetroBat ships full 7-Zip (7z.exe with RAR support) beside
        // EmulationStation; 7za.exe cannot open RAR archives.
        vec![
            layout.emulationstation_root().join("7z.exe"),
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

fn validate_archive_listing(listing: &[u8]) -> Result<(), ImportError> {
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
        } else if [
            "Symbolic Link = ",
            "Hard Link = ",
            "Copy Link = ",
            "Link = ",
        ]
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
    empty_directories: &mut BTreeSet<PathBuf>,
) -> Result<(), ImportError> {
    reject_non_directory_or_symlink(directory)?;
    let mut empty = true;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        empty = false;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(ImportError::UnsafeReference(path));
        }
        if file_type.is_dir() {
            collect_directory_files(root, &path, files, empty_directories)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| ImportError::UnsafeReference(path.clone()))?
                .to_owned();
            validate_relative(&relative)?;
            files.insert(relative, path);
        }
    }
    if empty && directory != root {
        let relative = directory
            .strip_prefix(root)
            .map_err(|_| ImportError::UnsafeReference(directory.to_owned()))?
            .to_owned();
        validate_relative(&relative)?;
        empty_directories.insert(relative);
    }
    Ok(())
}

/// Picks the file RetroBat should launch from a game folder: descriptors
/// before the tracks they list, then the shallowest, shortest match.
fn select_directory_launch(
    profile: &SystemProfile,
    files: &BTreeMap<PathBuf, PathBuf>,
) -> Option<PathBuf> {
    let system = profile.rom_folder.to_ascii_lowercase();
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
                "psp" => {
                    name == "eboot.pbp" || profile.extensions.contains(&normalized_extension(path))
                }
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
        let name = path
            .file_stem()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let utility =
            name.contains("launcher") || name.contains("config") || name.contains("crash");
        let preference = match normalized_extension(path).as_str() {
            ".m3u" => 0,
            ".cue" | ".gdi" | ".ccd" | ".toc" | ".mds" => 1,
            ".pbp" => 1,
            ".scummvm" => 0,
            _ => 2,
        };
        (
            utility,
            path.components().count(),
            preference,
            path.to_string_lossy().len(),
            path.clone(),
        )
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
        let referenced = locate_reference(descriptor_dir, &reference)
            .ok_or_else(|| ImportError::MissingReferencedFile(descriptor_dir.join(&reference)))?;
        collect_related_files(root, &referenced, files, visited_descriptors)?;
    }
    Ok(())
}

/// Resolves a descriptor reference the way the Windows tools that write
/// most CUE/GDI/M3U files do: exact path first, then case-insensitively.
fn locate_reference(directory: &Path, reference: &Path) -> Option<PathBuf> {
    let exact = directory.join(reference);
    if exact.is_file() {
        return Some(exact);
    }
    let mut current = directory.to_owned();
    for component in reference.components() {
        let Component::Normal(wanted) = component else {
            return None;
        };
        let wanted = wanted.to_string_lossy().to_lowercase();
        let found = fs::read_dir(&current)
            .ok()?
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().to_lowercase() == wanted)?;
        current = found.path();
    }
    current.is_file().then_some(current)
}

/// Descriptor lines as raw bytes: no BOM, no line terminators. Names stay
/// bytes because CUE files are often written in a legacy code page.
fn descriptor_lines(path: &Path) -> Result<Vec<Vec<u8>>, ImportError> {
    let mut bytes = fs::read(path)?;
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.drain(..3);
    }
    Ok(bytes
        .split(|byte| *byte == b'\n')
        .map(|line| {
            let mut line = line.to_vec();
            while line.last().is_some_and(|byte| *byte == b'\r') {
                line.pop();
            }
            line
        })
        .collect())
}

fn trim_bytes(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &value[start..end]
}

/// A descriptor's file name as a relative path on this host.
fn reference_path(raw: &[u8]) -> PathBuf {
    let normalized = raw
        .iter()
        .map(|byte| if *byte == b'\\' { b'/' } else { *byte })
        .collect::<Vec<_>>();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(&normalized))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(&normalized).into_owned())
    }
}

fn parse_cue(path: &Path) -> Result<Vec<PathBuf>, ImportError> {
    let mut references = Vec::new();
    for line in descriptor_lines(path)? {
        let trimmed = trim_bytes(&line);
        if trimmed.len() < 5 || !trimmed[..5].eq_ignore_ascii_case(b"FILE ") {
            continue;
        }
        let remainder = trim_bytes(&trimmed[5..]);
        let name = if let Some(quoted) = remainder.strip_prefix(b"\"") {
            let end = quoted
                .iter()
                .position(|byte| *byte == b'"')
                .ok_or_else(|| descriptor_error(path, "unterminated quoted FILE name"))?;
            &quoted[..end]
        } else {
            let end = remainder
                .iter()
                .rposition(u8::is_ascii_whitespace)
                .ok_or_else(|| descriptor_error(path, "FILE line has no type"))?;
            trim_bytes(&remainder[..end])
        };
        references.push(reference_path(name));
    }
    Ok(references)
}

fn parse_gdi(path: &Path) -> Result<Vec<PathBuf>, ImportError> {
    let mut references = Vec::new();
    for (index, line) in descriptor_lines(path)?.into_iter().enumerate() {
        if index == 0 || trim_bytes(&line).is_empty() {
            continue;
        }
        let tokens = split_quoted(&line);
        if tokens.len() < 6 {
            return Err(descriptor_error(
                path,
                "track line has fewer than six fields",
            ));
        }
        references.push(reference_path(&tokens[4]));
    }
    Ok(references)
}

fn parse_m3u(path: &Path) -> Result<Vec<PathBuf>, ImportError> {
    Ok(descriptor_lines(path)?
        .iter()
        .map(|line| trim_bytes(line))
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .map(reference_path)
        .collect())
}

fn split_quoted(line: &[u8]) -> Vec<Vec<u8>> {
    let mut output = Vec::new();
    let mut current = Vec::new();
    let mut quoted = false;
    for byte in line {
        match byte {
            b'"' => quoted = !quoted,
            byte if byte.is_ascii_whitespace() && !quoted => {
                if !current.is_empty() {
                    output.push(std::mem::take(&mut current));
                }
            }
            byte => current.push(*byte),
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

/// `title` in plain ASCII. Several Windows emulator cores open content
/// through the ANSI file API, which cannot reach a path outside the
/// machine's code page (ScummVM aborts on a folder named "Sołtys"), so a
/// title must never decide whether its own folder can be opened.
fn ascii_title(title: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::is_combining_mark;

    let mut ascii = String::with_capacity(title.len());
    for character in title.nfkd() {
        if character.is_ascii() {
            ascii.push(character);
            continue;
        }
        if is_combining_mark(character) {
            continue;
        }
        // Letters and punctuation that have no decomposition.
        ascii.push_str(match character {
            'ł' => "l",
            'Ł' => "L",
            'ß' => "ss",
            'æ' => "ae",
            'Æ' => "AE",
            'œ' => "oe",
            'Œ' => "OE",
            'ø' => "o",
            'Ø' => "O",
            'đ' | 'ð' => "d",
            'Đ' | 'Ð' => "D",
            'þ' => "th",
            'Þ' => "Th",
            'ı' => "i",
            '‘' | '’' | '′' => "'",
            '‐' | '‑' | '‒' | '–' | '—' | '−' => "-",
            _ => " ",
        });
    }
    ascii.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A folder name for `title` that every supported filesystem and emulator
/// accepts.
fn safe_component(title: &str, fallback: &str) -> String {
    const MAX_CHARS: usize = 100;
    let title = ascii_title(title);
    let title = if title
        .chars()
        .any(|character| character.is_ascii_alphanumeric())
    {
        title
    } else {
        String::new()
    };
    let sanitized = title
        .chars()
        .map(|character| match character {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            character if character.is_control() => '_',
            _ => character,
        })
        .take(MAX_CHARS)
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_owned();
    let sanitized = if sanitized.is_empty() {
        fallback.replace('/', "--")
    } else {
        sanitized
    };
    // Windows refuses these device names as file or folder names, with or
    // without an extension.
    let stem = sanitized
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("{sanitized}_")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) -> PathBuf {
        fs::write(path, bytes).unwrap();
        path.to_owned()
    }

    #[test]
    fn cue_references_survive_bom_backslashes_legacy_bytes_and_case() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("Tracks")).unwrap();
        write(
            &directory.path().join("Tracks").join("Track 01.bin"),
            b"data",
        );
        let cue = write(
            &directory.path().join("Game.cue"),
            b"\xEF\xBB\xBFFILE \"tracks\\TRACK 01.BIN\" BINARY\r\n  TRACK 01 MODE1/2352\r\n",
        );
        let references = parse_cue(&cue).unwrap();
        assert_eq!(references, [PathBuf::from("tracks/TRACK 01.BIN")]);
        assert_eq!(
            locate_reference(directory.path(), &references[0]).unwrap(),
            directory.path().join("Tracks").join("Track 01.bin")
        );
        let m3u = write(
            &directory.path().join("Game.m3u"),
            b"\xEF\xBB\xBFGame.cue\n# comment\n\n",
        );
        assert_eq!(parse_m3u(&m3u).unwrap(), [PathBuf::from("Game.cue")]);
    }

    #[cfg(unix)]
    #[test]
    fn cue_names_in_legacy_code_pages_keep_their_exact_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let latin1 = OsStr::from_bytes(b"Pok\xe9mon.bin");
        write(&directory.path().join(latin1), b"data");
        let cue = write(
            &directory.path().join("Game.cue"),
            b"FILE \"Pok\xe9mon.bin\" BINARY\n",
        );
        let references = parse_cue(&cue).unwrap();
        assert_eq!(references[0].as_os_str(), latin1);
        assert!(locate_reference(directory.path(), &references[0]).is_some());
    }

    #[test]
    fn descriptors_launch_before_the_tracks_they_list() {
        let profile = SystemProfile {
            extensions: [".cue", ".bin", ".m3u"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            rom_folder: "psx".into(),
        };
        let files = ["Game.bin", "Game.cue"]
            .into_iter()
            .map(|name| (PathBuf::from(name), PathBuf::from(name)))
            .collect();
        assert_eq!(
            select_directory_launch(&profile, &files).unwrap(),
            PathBuf::from("Game.cue")
        );
        let files = ["Disc 1.cue", "Disc 1.bin", "Game.m3u"]
            .into_iter()
            .map(|name| (PathBuf::from(name), PathBuf::from(name)))
            .collect();
        assert_eq!(
            select_directory_launch(&profile, &files).unwrap(),
            PathBuf::from("Game.m3u")
        );
    }

    #[test]
    fn folder_names_are_valid_on_windows() {
        assert_eq!(safe_component("Con", "x"), "Con_");
        assert_eq!(safe_component("com1.game", "x"), "com1.game_");
        assert_eq!(safe_component("Rogue: The Game?", "x"), "Rogue_ The Game_");
        assert_eq!(safe_component("...", "source/id"), "source--id");
        assert_eq!(safe_component(&"x".repeat(300), "x").chars().count(), 100);
    }

    #[test]
    fn folder_names_are_plain_ascii() {
        assert_eq!(safe_component("Sołtys", "x"), "Soltys");
        assert_eq!(safe_component("Petko: Das Debüt", "x"), "Petko_ Das Debut");
        assert_eq!(safe_component("Gejmbåj", "x"), "Gejmbaj");
        assert_eq!(safe_component("Eggy’s Maze", "x"), "Eggy's Maze");
        assert_eq!(
            safe_component("JINJ2 – Belmonte’s Revenge", "x"),
            "JINJ2 - Belmonte's Revenge"
        );
        assert_eq!(safe_component("Symbol ★ Merged", "x"), "Symbol Merged");
        assert_eq!(
            safe_component("Rayslinger™ (GBC)", "x"),
            "RayslingerTM (GBC)"
        );
        assert_eq!(
            safe_component("金曜日の牛乳", "homebrew-hub/fridaymilk"),
            "homebrew-hub--fridaymilk"
        );
    }

    #[test]
    fn archive_listings_reject_traversal_and_links_before_extraction() {
        validate_archive_listing(
            b"Path = game/Game.a26\nFolder = -\nSymbolic Link = \nHard Link = \nCopy Link = \nAlternate Stream = -\n",
        )
        .unwrap();
        assert!(matches!(
            validate_archive_listing(b"Path = ../outside.a26\nFolder = -\n"),
            Err(ImportError::UnsafeReference(_))
        ));
        assert!(matches!(
            validate_archive_listing(
                b"Path = game.a26\nFolder = -\nSymbolic Link = ../outside.a26\n"
            ),
            Err(ImportError::UnsafeReference(_))
        ));
    }
}
