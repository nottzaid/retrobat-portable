use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::catalog::{CatalogEntry, CatalogError};
use crate::paths::PortableLayout;

pub trait DownloadClient {
    fn fetch(&self, url: &str, output: &mut dyn Write) -> Result<(), DownloadError>;
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct DownloadError {
    message: String,
}

impl DownloadError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub struct ReqwestDownloader {
    client: reqwest::blocking::Client,
}

impl ReqwestDownloader {
    pub fn new() -> Result<Self, InstallError> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!(
                "retrobat-portable/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/gbdev/homebrewhub)"
            ))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| InstallError::Download(DownloadError::new(error.to_string())))?;
        Ok(Self { client })
    }
}

impl DownloadClient for ReqwestDownloader {
    fn fetch(&self, url: &str, output: &mut dyn Write) -> Result<(), DownloadError> {
        let mut response = self
            .client
            .get(url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|error| DownloadError::new(error.to_string()))?;
        io::copy(&mut response, output).map_err(|error| DownloadError::new(error.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("download failed: {0}")]
    Download(#[from] DownloadError),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("downloaded size mismatch: expected {expected}, got {actual}")]
    Size { expected: u64, actual: u64 },
    #[error("downloaded SHA-256 mismatch: expected {expected}, got {actual}")]
    Hash { expected: String, actual: String },
    #[error("unsafe destination path: {0}")]
    UnsafePath(PathBuf),
    #[error("destination already exists and is not owned by this installer: {0}")]
    DestinationExists(PathBuf),
    #[error("installed manifest is invalid: {0}")]
    Manifest(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InstalledManifest {
    pub schema_version: u32,
    pub catalog_id: String,
    pub relative_path: PathBuf,
    pub sha256: String,
    pub size: u64,
    pub installed_at_unix: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallReport {
    pub destination: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UninstallReport {
    pub removed: Vec<PathBuf>,
    pub preserved_modified: Vec<PathBuf>,
    pub already_missing: Vec<PathBuf>,
}

pub struct Installer<'a, D: DownloadClient> {
    layout: &'a PortableLayout,
    downloader: &'a D,
}

impl<'a, D: DownloadClient> Installer<'a, D> {
    pub fn new(layout: &'a PortableLayout, downloader: &'a D) -> Self {
        Self { layout, downloader }
    }

    pub fn install(&self, entry: &CatalogEntry) -> Result<InstallReport, InstallError> {
        entry.validate()?;
        let relative = entry.install_relative_path();
        validate_relative_path(&relative)?;

        let destination = self.layout.root.join(&relative);
        ensure_safe_parent(&self.layout.root, &relative)?;
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(InstallError::DestinationExists(destination));
        }

        let stage = crate::import::StagingDirectory::new(self.layout, "install")?;
        let staged = stage.path().join("artifact.download");
        crate::downloads::fetch_verified(
            self.downloader,
            &entry.artifact.url,
            entry.artifact.size,
            &entry.artifact.sha256,
            &staged,
        )
        .map_err(|error| match error {
            crate::downloads::VerifiedDownloadError::Download(error) => {
                InstallError::Download(error)
            }
            crate::downloads::VerifiedDownloadError::Io(error) => InstallError::Io(error),
            crate::downloads::VerifiedDownloadError::TooLarge { expected } => InstallError::Size {
                expected,
                actual: expected + 1,
            },
            crate::downloads::VerifiedDownloadError::Size { expected, actual } => {
                InstallError::Size { expected, actual }
            }
            crate::downloads::VerifiedDownloadError::Hash { expected, actual } => {
                InstallError::Hash { expected, actual }
            }
        })?;
        // create_new claims the name, so a file that appeared since the
        // check above is never overwritten; the rename then replaces only
        // RetroPort's own empty placeholder.
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
        {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(InstallError::DestinationExists(destination));
            }
            Err(error) => return Err(error.into()),
        }
        if let Err(error) = fs::rename(&staged, &destination) {
            let _ = fs::remove_file(&destination);
            return Err(error.into());
        }
        let manifest = InstalledManifest {
            schema_version: 1,
            catalog_id: entry.id.clone(),
            relative_path: relative,
            sha256: entry.artifact.sha256.to_ascii_lowercase(),
            size: entry.artifact.size,
            installed_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        if let Err(error) = write_manifest(self.layout, &manifest) {
            let _ = fs::remove_file(&destination);
            return Err(error);
        }
        Ok(InstallReport {
            destination,
            bytes: manifest.size,
            sha256: manifest.sha256,
        })
    }

    pub fn uninstall(&self, entry: &CatalogEntry) -> Result<UninstallReport, InstallError> {
        let manifest_path = manifest_path(self.layout, &entry.id);
        let manifest: InstalledManifest = serde_json::from_reader(File::open(&manifest_path)?)?;
        validate_relative_path(&manifest.relative_path)?;
        let destination = self.layout.root.join(&manifest.relative_path);
        let mut report = UninstallReport::default();

        match fs::symlink_metadata(&destination) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                report.already_missing.push(destination);
                fs::remove_file(manifest_path)?;
            }
            Err(error) => return Err(error.into()),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                report.preserved_modified.push(destination);
            }
            Ok(_) => {
                let (_, actual_hash) = digest_file(&destination)?;
                if actual_hash == manifest.sha256 {
                    fs::remove_file(&destination)?;
                    report.removed.push(destination);
                    fs::remove_file(manifest_path)?;
                } else {
                    report.preserved_modified.push(destination);
                }
            }
        }
        Ok(report)
    }

    pub fn is_installed(&self, entry: &CatalogEntry) -> bool {
        is_installed(self.layout, entry)
    }
}

pub fn is_installed(layout: &PortableLayout, entry: &CatalogEntry) -> bool {
    manifest_path(layout, &entry.id).is_file()
}

pub(crate) fn digest_file(path: &Path) -> Result<(u64, String), io::Error> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((size, hex::encode(hasher.finalize())))
}

fn manifest_path(layout: &PortableLayout, id: &str) -> PathBuf {
    layout
        .installed_root()
        .join(format!("{}.json", id.replace('/', "--")))
}

fn write_manifest(
    layout: &PortableLayout,
    manifest: &InstalledManifest,
) -> Result<(), InstallError> {
    fs::create_dir_all(layout.installed_root())?;
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

fn validate_relative_path(path: &Path) -> Result<(), InstallError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(InstallError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

pub(crate) fn ensure_safe_parent(root: &Path, relative: &Path) -> Result<(), InstallError> {
    validate_relative_path(relative)?;
    fs::create_dir_all(root)?;
    let mut current = root.to_owned();
    let parent = relative
        .parent()
        .ok_or_else(|| InstallError::UnsafePath(relative.to_owned()))?;
    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(InstallError::UnsafePath(relative.to_owned()));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(InstallError::UnsafePath(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
