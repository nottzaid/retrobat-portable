use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::Digest;
use thiserror::Error;

use crate::browse::{BrowseEntry, BundledArtwork};
use crate::catalog::Artwork;
use crate::install::{
    DownloadClient, DownloadError, InstallError, digest_file, ensure_safe_parent,
};
use crate::paths::PortableLayout;

#[derive(Debug, Error)]
pub enum ArtworkError {
    #[error("artwork download failed: {0}")]
    Download(#[from] DownloadError),
    #[error("artwork cache operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("artwork cache path is unsafe: {0}")]
    UnsafePath(#[from] InstallError),
    #[error("artwork size mismatch: expected {expected}, got {actual}")]
    Size { expected: u64, actual: u64 },
    #[error("artwork SHA-256 mismatch: expected {expected}, got {actual}")]
    Hash { expected: String, actual: String },
    #[error("artwork exceeds the {0} byte download limit")]
    TooLarge(usize),
    #[error("bundled artwork is not a regular file: {0}")]
    NotAFile(PathBuf),
    #[error("artwork source did not return an image: {0}")]
    NotAnImage(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BundledArtworkAudit {
    pub declared_assets: usize,
    /// Present as a regular file of the recorded size.
    pub verified_assets: usize,
    pub failed_assets: usize,
    pub failure_examples: Vec<String>,
    /// What "verified" means here. Content hashes are checked every time a
    /// cover is displayed and by the VERIFY scripts; re-reading every cover
    /// here would cost tens of seconds on a cold hard disk.
    pub method: &'static str,
}

impl BundledArtworkAudit {
    pub fn is_complete(&self) -> bool {
        self.declared_assets == self.verified_assets && self.failed_assets == 0
    }
}

pub fn audit_bundled_artwork(
    layout: &PortableLayout,
    entries: &[BrowseEntry],
) -> BundledArtworkAudit {
    let mut assets = entries
        .iter()
        .filter_map(|entry| entry.artwork_asset.as_ref().map(|asset| (entry, asset)))
        .collect::<Vec<_>>();
    // Path order follows directory order, which keeps metadata reads local.
    assets.sort_by(|(_, left), (_, right)| left.path.cmp(&right.path));
    let failures = assets
        .iter()
        .filter_map(|(entry, asset)| {
            let relative = PathBuf::from(&asset.path);
            let problem = match ensure_safe_parent(&layout.root, &relative) {
                Err(error) => Some(error.to_string()),
                Ok(()) => match fs::symlink_metadata(layout.root.join(&relative)) {
                    Err(error) => Some(error.to_string()),
                    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                        Some("not a regular file".to_owned())
                    }
                    Ok(metadata) if metadata.len() != asset.size => Some(format!(
                        "size {} differs from the recorded {}",
                        metadata.len(),
                        asset.size
                    )),
                    Ok(_) => None,
                },
            };
            problem.map(|problem| format!("{}: {problem}", entry.id))
        })
        .collect::<Vec<_>>();
    BundledArtworkAudit {
        declared_assets: assets.len(),
        verified_assets: assets.len() - failures.len(),
        failed_assets: failures.len(),
        failure_examples: failures.into_iter().take(20).collect(),
        method: "present with the recorded size; SHA-256 is checked on display and by VERIFY-*",
    }
}

pub fn load_bundled_artwork(
    layout: &PortableLayout,
    artwork: &BundledArtwork,
) -> Result<Vec<u8>, ArtworkError> {
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    let relative = PathBuf::from(&artwork.path);
    ensure_safe_parent(&layout.root, &relative)?;
    let path = layout.root.join(&relative);
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ArtworkError::NotAFile(path));
    }
    if artwork.size > MAX_BYTES || metadata.len() > MAX_BYTES {
        return Err(ArtworkError::TooLarge(MAX_BYTES as usize));
    }
    // Read once; verify the bytes that will be shown.
    let bytes = fs::read(path)?;
    if bytes.len() as u64 != artwork.size {
        return Err(ArtworkError::Size {
            expected: artwork.size,
            actual: bytes.len() as u64,
        });
    }
    let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
    if sha256 != artwork.sha256 {
        return Err(ArtworkError::Hash {
            expected: artwork.sha256.clone(),
            actual: sha256,
        });
    }
    Ok(bytes)
}

pub fn load_snapshot_artwork<D: DownloadClient>(
    layout: &PortableLayout,
    url: &str,
    downloader: &D,
) -> Result<Vec<u8>, ArtworkError> {
    const MAX_BYTES: usize = 8 * 1024 * 1024;
    let url_hash = hex::encode(sha2::Sha256::digest(url.as_bytes()));
    let relative = PathBuf::from(".retrobat-portable")
        .join("cache")
        .join("browse-artwork")
        .join(format!("{url_hash}.image"));
    ensure_safe_parent(&layout.root, &relative)?;
    let cache_path = layout.root.join(&relative);
    if cache_path.is_file() {
        let metadata = fs::metadata(&cache_path)?;
        if metadata.len() <= MAX_BYTES as u64 {
            return Ok(fs::read(cache_path)?);
        }
        fs::remove_file(&cache_path)?;
    }

    let mut bytes = LimitedBytes::new(MAX_BYTES);
    downloader.fetch(url, &mut bytes)?;
    let bytes = bytes.into_inner()?;
    // A server can answer 200 with an error page; only an image is cached.
    if image::guess_format(&bytes).is_err() {
        return Err(ArtworkError::NotAnImage(url.to_owned()));
    }
    let temporary = cache_path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| {
        let mut output = File::create(&temporary)?;
        output.write_all(&bytes)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary, &cache_path)?;
        Ok(bytes)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

struct LimitedBytes {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedBytes {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    fn into_inner(self) -> Result<Vec<u8>, ArtworkError> {
        if self.exceeded {
            Err(ArtworkError::TooLarge(self.limit))
        } else {
            Ok(self.bytes)
        }
    }
}

impl Write for LimitedBytes {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if buffer.len() > remaining {
            self.exceeded = true;
            return Err(io::Error::other("artwork download limit exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn load_or_fetch<D: DownloadClient>(
    layout: &PortableLayout,
    artwork: &Artwork,
    downloader: &D,
) -> Result<Vec<u8>, ArtworkError> {
    let relative = PathBuf::from(".retrobat-portable")
        .join("cache")
        .join("artwork")
        .join(format!("{}.image", artwork.sha256));
    ensure_safe_parent(&layout.root, &relative)?;
    let cache_path = layout.root.join(&relative);

    if cache_path.is_file() {
        let bytes = fs::read(&cache_path)?;
        if bytes.len() as u64 == artwork.size
            && hex::encode(sha2::Sha256::digest(&bytes)) == artwork.sha256
        {
            return Ok(bytes);
        }
        fs::remove_file(&cache_path)?;
    }

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = cache_path.with_extension(format!("{}.tmp", unique));
    let result = (|| {
        let mut output = File::create(&temporary)?;
        downloader.fetch(&artwork.url, &mut output)?;
        output.sync_all()?;
        drop(output);
        verify(&temporary, artwork)?;
        fs::rename(&temporary, &cache_path)?;
        Ok(fs::read(&cache_path)?)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn verify(path: &std::path::Path, artwork: &Artwork) -> Result<(), ArtworkError> {
    let (actual_size, actual_hash) = digest_file(path)?;
    if actual_size != artwork.size {
        return Err(ArtworkError::Size {
            expected: artwork.size,
            actual: actual_size,
        });
    }
    if actual_hash != artwork.sha256 {
        return Err(ArtworkError::Hash {
            expected: artwork.sha256.clone(),
            actual: actual_hash,
        });
    }
    Ok(())
}
