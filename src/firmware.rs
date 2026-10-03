use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

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
    let operation = format!(
        "firmware-download-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let stage = layout.staging_root().join(operation);
    fs::create_dir_all(&stage)?;
    let staged = stage.join("publisher-download");
    let result = (|| {
        let mut output = File::create(&staged)?;
        downloader.fetch(&download.url, &mut output)?;
        output.sync_all()?;
        drop(output);

        let (actual_size, actual_hash) = digest_file(&staged)?;
        if actual_size != download.size {
            return Err(FirmwareImportError::Size {
                expected: download.size,
                actual: actual_size,
            });
        }
        if actual_hash != download.sha256.to_ascii_lowercase() {
            return Err(FirmwareImportError::Hash {
                expected: download.sha256.clone(),
                actual: actual_hash,
            });
        }
        import_firmware(layout, firmware, &staged)
    })();
    let _ = fs::remove_dir_all(stage);
    result
}

pub fn import_firmware(
    layout: &PortableLayout,
    firmware: &FirmwareFileStatus,
    source: &Path,
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
        fs::copy(source, &staged)?;
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
    if firmware.relative_path == "eden/keys/prod.keys" {
        let destinations = [
            layout
                .emulator_root("eden")
                .join("user")
                .join("keys")
                .join("prod.keys"),
            layout
                .metadata_root()
                .join("runtime/linux/eden/data/eden/keys/prod.keys"),
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
