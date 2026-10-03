use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::downloads::{VerifiedDownloadError, fetch_verified};
use crate::import::StagingDirectory;
use crate::install::{
    DownloadClient, DownloadError, InstallError, digest_file, ensure_safe_parent,
};
use crate::paths::PortableLayout;
use crate::readiness::FirmwareFileStatus;

#[derive(Debug, Error)]
pub enum FirmwareImportError {
    #[error("the selected firmware path is not a regular file: {0}")]
    NotAFile(PathBuf),
    #[error("the firmware destination is unsafe: {0}")]
    UnsafeDestination(String),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("destination safety check failed: {0}")]
    Safety(#[from] InstallError),
    #[error("this firmware record has no publisher-authorized download")]
    NotDownloadable,
    #[error("publisher download failed: {0}")]
    Download(#[from] DownloadError),
    #[error("publisher download size mismatch: expected {expected}, got {actual}")]
    Size { expected: u64, actual: u64 },
    #[error("publisher download SHA-256 mismatch: expected {expected}, got {actual}")]
    Hash { expected: String, actual: String },
}

/// What RetroPort recorded about a firmware file it placed: the evidence a
/// user or maintainer needs to diagnose a dump that does not work.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FirmwareRecord {
    pub relative_path: String,
    pub sha256: String,
    pub size: u64,
    /// The selected file's name, or the publisher URL for a download.
    pub origin: String,
    pub recorded_at_unix: u64,
}

fn record_path(layout: &PortableLayout, relative_path: &str) -> PathBuf {
    layout
        .metadata_root()
        .join("firmware")
        .join(format!("{}.json", relative_path.replace(['/', '\\'], "--")))
}

/// The record of the firmware RetroPort last placed at `relative_path`
/// (relative to RetroBat/bios), if it is still the file on disk.
pub fn firmware_record(layout: &PortableLayout, relative_path: &str) -> Option<FirmwareRecord> {
    let record: FirmwareRecord =
        serde_json::from_reader(File::open(record_path(layout, relative_path)).ok()?).ok()?;
    let placed = layout
        .retrobat_root()
        .join("bios")
        .join(&record.relative_path);
    fs::metadata(placed)
        .is_ok_and(|metadata| metadata.len() == record.size)
        .then_some(record)
}

fn write_record(layout: &PortableLayout, record: &FirmwareRecord) -> io::Result<()> {
    let path = record_path(layout, &record.relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut output = File::create(&temporary)?;
    serde_json::to_writer_pretty(&mut output, record).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    fs::rename(temporary, path)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirmwareImportReport {
    pub destination: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub replaced_existing: bool,
}

pub fn install_official_firmware<D: DownloadClient>(
    layout: &PortableLayout,
    firmware: &FirmwareFileStatus,
    downloader: &D,
) -> Result<FirmwareImportReport, FirmwareImportError> {
    let download = firmware
        .download
        .as_ref()
        .ok_or(FirmwareImportError::NotDownloadable)?;
    // The verified publisher file may already be in place (for example when
    // the user closed RPCS3's installer); then only the installer reruns.
    let destination = layout
        .retrobat_root()
        .join("bios")
        .join(&firmware.relative_path);
    if fs::metadata(&destination).is_ok_and(|metadata| metadata.len() == download.size)
        && digest_file(&destination)?.1 == download.sha256.to_ascii_lowercase()
    {
        return Ok(FirmwareImportReport {
            destination,
            bytes: download.size,
            sha256: download.sha256.to_ascii_lowercase(),
            replaced_existing: false,
        });
    }
    let stage = StagingDirectory::new(layout, "firmware-download")?;
    let staged = stage.path().join("publisher-download");
    fetch_verified(
        downloader,
        &download.url,
        download.size,
        &download.sha256,
        &staged,
    )
    .map_err(|error| match error {
        VerifiedDownloadError::Download(error) => FirmwareImportError::Download(error),
        VerifiedDownloadError::Io(error) => FirmwareImportError::Io(error),
        VerifiedDownloadError::TooLarge { expected } => FirmwareImportError::Size {
            expected,
            actual: expected + 1,
        },
        VerifiedDownloadError::Size { expected, actual } => {
            FirmwareImportError::Size { expected, actual }
        }
        VerifiedDownloadError::Hash { expected, actual } => {
            FirmwareImportError::Hash { expected, actual }
        }
    })?;
    import_firmware_from(layout, firmware, &staged, Some(&download.url))
}

pub fn import_firmware(
    layout: &PortableLayout,
    firmware: &FirmwareFileStatus,
    source: &Path,
) -> Result<FirmwareImportReport, FirmwareImportError> {
    if firmware.alternatives.is_empty() {
        return import_firmware_from(layout, firmware, source, None);
    }
    // Any one of several files satisfies this requirement (another region's
    // BIOS, say). A file RetroPort recognises keeps the name its emulator
    // expects for it; anything else takes the preferred name.
    let digest = FirmwareDigest::of(source)?;
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let known = FirmwareCatalogue::load(layout).identify(&name, &digest);
    let mut target = firmware.clone();
    if let Some(path) = std::iter::once(&firmware.relative_path)
        .chain(&firmware.alternatives)
        .find(|path| known.iter().any(|(known, _)| known == *path))
    {
        target.relative_path.clone_from(path);
    }
    import_firmware_from(layout, &target, source, None)
}

/// A file's identity as emulators publish it.
pub struct FirmwareDigest {
    pub size: u64,
    pub md5: String,
    pub sha1: String,
    /// Recognised by its structure: a PlayStation 2 BIOS (any region or
    /// revision), which PCSX2 accepts under any name.
    pub ps2_bios: bool,
}

impl FirmwareDigest {
    pub fn of(path: &Path) -> io::Result<Self> {
        use sha1::Digest;
        let mut file = File::open(path)?;
        let mut md5 = md5::Context::new();
        let mut sha1 = sha1::Sha1::new();
        let mut head = Vec::new();
        let mut size = 0u64;
        let mut buffer = vec![0u8; 256 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            if head.len() < 64 * 1024 {
                let wanted = (64 * 1024 - head.len()).min(read);
                head.extend_from_slice(&buffer[..wanted]);
            }
            md5.consume(&buffer[..read]);
            sha1.update(&buffer[..read]);
            size += read as u64;
        }
        let contains = |needle: &[u8]| head.windows(needle.len()).any(|window| window == needle);
        Ok(Self {
            size,
            md5: format!("{:x}", md5.compute()),
            sha1: hex::encode(sha1.finalize()),
            // The ROM directory of every PS2 BIOS starts with these entries.
            ps2_bios: size == 4 * 1024 * 1024
                && contains(b"RESET\0")
                && contains(b"ROMDIR\0")
                && contains(b"ROMVER\0"),
        })
    }
}

/// How a file was recognised.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recognition {
    /// Its MD5 or SHA-1 matches a published fingerprint.
    Fingerprint,
    /// Its structure identifies it (PS2 BIOS).
    Structure,
    /// Its file name is the one an emulator looks for.
    Name,
}

/// Every firmware file RetroPort can recognise: the fingerprints RetroBat's
/// BIOS catalogue and each installed emulator core publish, and the file
/// names those emulators look for.
#[derive(Default)]
pub struct FirmwareCatalogue {
    by_md5: HashMap<String, BTreeSet<String>>,
    by_sha1: HashMap<String, BTreeSet<String>>,
    by_name: HashMap<String, BTreeSet<String>>,
}

/// Names too generic to identify a firmware file on their own.
const GENERIC_STEMS: [&str; 9] = [
    "bios", "boot", "rom", "system", "firmware", "kernel", "basic", "font", "char",
];
const MAX_FIRMWARE_BYTES: u64 = 256 * 1024 * 1024;

impl FirmwareCatalogue {
    pub fn load(layout: &PortableLayout) -> Self {
        let mut catalogue = Self::default();
        if let Ok(text) = fs::read_to_string(layout.bios_catalog())
            && let Ok(systems) = serde_json::from_str::<serde_json::Value>(&text)
        {
            for system in systems
                .as_object()
                .into_iter()
                .flat_map(|systems| systems.values())
            {
                for file in system["biosFiles"].as_array().into_iter().flatten() {
                    if let Some(path) = file["file"]
                        .as_str()
                        .and_then(|path| path.strip_prefix("bios/"))
                        && !path.starts_with("mame/")
                    {
                        catalogue.add(path, file["md5"].as_str().unwrap_or_default(), "");
                    }
                }
            }
        }
        if let Ok(entries) = fs::read_dir(layout.retroarch_root().join("info")) {
            for entry in entries.flatten() {
                if let Ok(text) = fs::read_to_string(entry.path()) {
                    for (path, md5, sha1) in core_info_firmware(&text) {
                        catalogue.add(&path, &md5, &sha1);
                    }
                }
            }
        }
        // Standalone emulators' files, which carry no published fingerprint.
        for path in [
            "eden/keys/prod.keys",
            "eden/keys/title.keys",
            "PS3UPDAT.PUP",
        ] {
            catalogue.add(path, "", "");
        }
        catalogue
    }

    fn add(&mut self, path: &str, md5: &str, sha1: &str) {
        let path = path.replace('\\', "/");
        if !safe_relative(&path) {
            return;
        }
        if md5.len() == 32 {
            self.by_md5
                .entry(md5.to_ascii_lowercase())
                .or_default()
                .insert(path.clone());
        }
        if sha1.len() == 40 {
            self.by_sha1
                .entry(sha1.to_ascii_lowercase())
                .or_default()
                .insert(path.clone());
        }
        let name = path
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let stem = name.split('.').next().unwrap_or_default();
        // Directory-shaped entries ("pcsx2/bios") have no extension.
        if name.contains('.') && !GENERIC_STEMS.contains(&stem) {
            self.by_name.entry(name).or_default().insert(path);
        }
    }

    /// Where a file belongs (paths relative to RetroBat/bios), and how it
    /// was recognised. A fingerprint outranks a name.
    pub fn identify(&self, file_name: &str, digest: &FirmwareDigest) -> Vec<(String, Recognition)> {
        let fingerprinted = self
            .by_md5
            .get(&digest.md5)
            .into_iter()
            .chain(self.by_sha1.get(&digest.sha1))
            .flatten()
            .cloned()
            .collect::<BTreeSet<_>>();
        if !fingerprinted.is_empty() {
            return fingerprinted
                .into_iter()
                .map(|path| (path, Recognition::Fingerprint))
                .collect();
        }
        if digest.ps2_bios {
            return vec![(format!("pcsx2/bios/{file_name}"), Recognition::Structure)];
        }
        self.by_name
            .get(&file_name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .map(|path| (path.clone(), Recognition::Name))
            .collect()
    }
}

/// Each firmware path a core's .info declares, with the MD5 or SHA-1 its
/// notes publish ("(!) neocd/neocd_f.rom (sha1): a5f4…").
fn core_info_firmware(text: &str) -> Vec<(String, String, String)> {
    let mut paths = Vec::new();
    let mut hashes = HashMap::<String, (String, String)>::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim().trim_matches('"'));
        if key.starts_with("firmware") && key.ends_with("_path") {
            paths.push(value.to_owned());
        } else if key == "notes" {
            for note in value.split('|') {
                let note = note.trim().trim_start_matches("(!)").trim();
                for (marker, is_md5) in [(" (md5):", true), (" (sha1):", false)] {
                    if let Some((path, hash)) = note.rsplit_once(marker) {
                        let entry = hashes.entry(path.trim().to_owned()).or_default();
                        if is_md5 {
                            entry.0 = hash.trim().to_owned();
                        } else {
                            entry.1 = hash.trim().to_owned();
                        }
                    }
                }
            }
        }
    }
    let mut firmware = paths
        .into_iter()
        .map(|path| {
            let (md5, sha1) = hashes.remove(&path).unwrap_or_default();
            (path, md5, sha1)
        })
        .collect::<Vec<_>>();
    firmware.extend(
        hashes
            .into_iter()
            .map(|(path, (md5, sha1))| (path, md5, sha1)),
    );
    firmware
}

fn safe_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FirmwareFolderReport {
    /// (source, placed at bios/…, how it was recognised)
    pub placed: Vec<(PathBuf, String, Recognition)>,
    /// Destinations that already held this exact file.
    pub already_present: usize,
    /// Destinations that hold a different file, which RetroPort kept.
    pub kept_existing: Vec<String>,
    pub examined: usize,
    pub unrecognised: usize,
}

/// Recognises every firmware file at `path` (a file, or a folder searched
/// recursively) and places each wherever its emulators look for it,
/// recording each like a single import. A different file already in place
/// is never replaced.
pub fn import_firmware_folder(
    layout: &PortableLayout,
    path: &Path,
) -> Result<FirmwareFolderReport, FirmwareImportError> {
    const MAX_DEPTH: usize = 8;
    const MAX_FILES: usize = 50_000;
    let catalogue = FirmwareCatalogue::load(layout);
    let mut report = FirmwareFolderReport::default();
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        place_recognised(layout, &catalogue, path, metadata.len(), &mut report)?;
        return Ok(report);
    }
    if !metadata.is_dir() {
        return Err(FirmwareImportError::NotAFile(path.to_owned()));
    }
    let mut pending = vec![(path.to_owned(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        for entry in fs::read_dir(&directory)?.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth < MAX_DEPTH {
                    pending.push((path, depth + 1));
                }
                continue;
            }
            if !file_type.is_file() || report.examined >= MAX_FILES {
                continue;
            }
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            place_recognised(layout, &catalogue, &path, size, &mut report)?;
        }
    }
    Ok(report)
}

fn place_recognised(
    layout: &PortableLayout,
    catalogue: &FirmwareCatalogue,
    path: &Path,
    size: u64,
    report: &mut FirmwareFolderReport,
) -> Result<(), FirmwareImportError> {
    report.examined += 1;
    if size == 0 || size > MAX_FIRMWARE_BYTES {
        report.unrecognised += 1;
        return Ok(());
    }
    let bios = layout.retrobat_root().join("bios");
    let digest = FirmwareDigest::of(path)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let targets = catalogue.identify(&name, &digest);
    if targets.is_empty() {
        report.unrecognised += 1;
        return Ok(());
    }
    {
        {
            for (target, recognition) in targets {
                let destination = bios.join(&target);
                if destination.is_file() {
                    if digest_file(&destination)?.0 == digest.size
                        && FirmwareDigest::of(&destination)?.md5 == digest.md5
                    {
                        report.already_present += 1;
                    } else {
                        report.kept_existing.push(target);
                    }
                    continue;
                }
                let status = FirmwareFileStatus {
                    relative_path: target.clone(),
                    description: String::new(),
                    directory: false,
                    optional: false,
                    present: false,
                    alternatives: Vec::new(),
                    guidance_url: String::new(),
                    guidance: String::new(),
                    download: None,
                };
                import_firmware_from(layout, &status, path, None)?;
                report.placed.push((path.to_owned(), target, recognition));
            }
        }
    }
    Ok(())
}

fn import_firmware_from(
    layout: &PortableLayout,
    firmware: &FirmwareFileStatus,
    source: &Path,
    origin: Option<&str>,
) -> Result<FirmwareImportReport, FirmwareImportError> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(FirmwareImportError::NotAFile(source.to_owned()));
    }
    let firmware_path = Path::new(&firmware.relative_path);
    if firmware_path.as_os_str().is_empty()
        || firmware_path.is_absolute()
        || firmware_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(FirmwareImportError::UnsafeDestination(
            firmware.relative_path.clone(),
        ));
    }
    let relative_destination = if firmware.directory {
        let Some(filename) = source.file_name() else {
            return Err(FirmwareImportError::NotAFile(source.to_owned()));
        };
        if !matches!(
            Path::new(filename).components().next(),
            Some(Component::Normal(_))
        ) {
            return Err(FirmwareImportError::UnsafeDestination(
                source.display().to_string(),
            ));
        }
        Path::new("RetroBat")
            .join("bios")
            .join(firmware_path)
            .join(filename)
    } else {
        Path::new("RetroBat").join("bios").join(firmware_path)
    };
    ensure_safe_parent(&layout.root, &relative_destination)?;
    let destination = layout.root.join(&relative_destination);
    let replaced_existing = match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(FirmwareImportError::UnsafeDestination(
                destination.display().to_string(),
            ));
        }
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };

    let operation = format!(
        "firmware-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let stage = layout.staging_root().join(operation);
    fs::create_dir_all(&stage)?;
    let staged = stage.join("payload");
    let previous = stage.join("previous");
    let result = (|| {
        // A verified publisher download is already staged on this volume and
        // is moved; a user's file is copied and never modified.
        if origin.is_some() {
            fs::rename(source, &staged)?;
        } else {
            fs::copy(source, &staged)?;
        }
        let (bytes, sha256) = digest_file(&staged)?;
        if bytes == 0 {
            return Err(FirmwareImportError::NotAFile(source.to_owned()));
        }
        if replaced_existing {
            fs::rename(&destination, &previous)?;
        }
        if let Err(error) = fs::rename(&staged, &destination) {
            if replaced_existing {
                let _ = fs::rename(&previous, &destination);
            }
            return Err(error.into());
        }
        mirror_emulator_firmware(layout, firmware, &destination)?;
        let relative = if firmware.directory {
            format!(
                "{}/{}",
                firmware.relative_path,
                destination
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            )
        } else {
            firmware.relative_path.clone()
        };
        write_record(
            layout,
            &FirmwareRecord {
                relative_path: relative,
                sha256: sha256.clone(),
                size: bytes,
                origin: origin.map(str::to_owned).unwrap_or_else(|| {
                    source
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                }),
                recorded_at_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            },
        )?;
        Ok(FirmwareImportReport {
            destination,
            bytes,
            sha256,
            replaced_existing,
        })
    })();
    let _ = fs::remove_dir_all(stage);
    result
}

fn mirror_emulator_firmware(
    layout: &PortableLayout,
    firmware: &FirmwareFileStatus,
    source: &Path,
) -> Result<(), io::Error> {
    if let Some(keys) = ["prod.keys", "title.keys"]
        .into_iter()
        .find(|keys| firmware.relative_path == format!("eden/keys/{keys}"))
    {
        let destinations = [
            layout
                .emulator_root("eden")
                .join("user")
                .join("keys")
                .join(keys),
            layout
                .metadata_root()
                .join("runtime/linux/eden/data/eden/keys")
                .join(keys),
        ];
        for destination in destinations {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source, destination)?;
        }
    }
    Ok(())
}
