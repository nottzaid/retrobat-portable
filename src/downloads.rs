//! The pinned download ledger: for every DOWNLOAD card, the exact publisher
//! file, its size and SHA-256, and how RetroPort installs it.
//!
//! `tools/build_download_ledger.py` generates the snapshot. At runtime only
//! these URLs are fetched, bytes are verified while they stream, and nothing
//! unverified reaches an emulator.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path};

use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::install::{DownloadClient, DownloadError};

const LEDGER: &[u8] = include_bytes!("../catalog/downloads-v1.json.gz");

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Recipe {
    /// Import the downloaded file as published.
    File,
    /// Import one file located inside the archive by its pinned hash.
    ExtractMember,
    /// Extract the game folder and add a ScummVM launch file.
    Scummvm,
    /// Unpack a RetroBat store package's `roms/<system>/` tree.
    RetrobatStore,
    /// Unpack the archive as it is and import the whole game folder,
    /// starting `member` (data-file games such as Quake or Cave Story).
    ExtractTree,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PinnedDownload {
    pub url: String,
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub recipe: Recipe,
    #[serde(default)]
    pub member: Option<String>,
    #[serde(default)]
    pub member_size: Option<u64>,
    #[serde(default)]
    pub member_sha256: Option<String>,
    #[serde(default)]
    pub game_id: Option<String>,
    /// A libretro core the content needs instead of the system default.
    #[serde(default)]
    pub core: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DownloadLedger {
    pub schema_version: u32,
    pub generated_at: String,
    pub entries: HashMap<String, PinnedDownload>,
    /// Entries whose source does not serve a usable file, with the reason.
    #[serde(default)]
    pub unavailable: HashMap<String, String>,
}

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("download ledger could not be read: {0}")]
    Io(#[from] io::Error),
    #[error("download ledger is invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported download ledger schema {0}")]
    Schema(u32),
    #[error("download ledger entry {id} is unsafe: {reason}")]
    Unsafe { id: String, reason: String },
}

/// Hosts a pinned download may come from, per catalogue source.
fn allowed_hosts(source_id: &str) -> &'static [&'static str] {
    match source_id {
        "homebrew-hub" => &["raw.githubusercontent.com"],
        "libretro-content" => &["buildbot.libretro.com"],
        "mame-authorized" => &["www.mamedev.org"],
        "freedos" => &["www.ibiblio.org"],
        "msxdev" => &["www.msxdev.org", "msxdev.org"],
        "dos-games-archive" => &["www.dosgamesarchive.com"],
        "scummvm-freeware" => &["downloads.scummvm.org"],
        "retrobat-store" => &["www.retrobat.ovh"],
        _ => &[],
    }
}

impl DownloadLedger {
    pub fn built_in() -> Result<Self, LedgerError> {
        let mut decoded = String::new();
        GzDecoder::new(LEDGER).read_to_string(&mut decoded)?;
        let ledger: Self = serde_json::from_str(&decoded)?;
        ledger.validate()?;
        Ok(ledger)
    }

    pub fn get(&self, id: &str) -> Option<&PinnedDownload> {
        self.entries.get(id)
    }

    pub fn validate(&self) -> Result<(), LedgerError> {
        if self.schema_version != 1 {
            return Err(LedgerError::Schema(self.schema_version));
        }
        for (id, pinned) in &self.entries {
            let unsafe_entry = |reason: &str| LedgerError::Unsafe {
                id: id.clone(),
                reason: reason.to_owned(),
            };
            let source = id.split_once('/').map_or("", |(source, _)| source);
            let url = Url::parse(&pinned.url).map_err(|_| unsafe_entry("invalid URL"))?;
            if url.scheme() != "https"
                || !url
                    .host_str()
                    .is_some_and(|host| allowed_hosts(source).contains(&host))
            {
                return Err(unsafe_entry(
                    "URL is not HTTPS on the source's publisher host",
                ));
            }
            if !is_single_component(&pinned.filename) {
                return Err(unsafe_entry("filename is not a single path component"));
            }
            if !is_sha256(&pinned.sha256) || pinned.size == 0 {
                return Err(unsafe_entry("missing size or SHA-256"));
            }
            match pinned.recipe {
                Recipe::File => {}
                Recipe::ExtractMember => {
                    if !pinned.member_sha256.as_deref().is_some_and(is_sha256)
                        || pinned.member_size.is_none()
                    {
                        return Err(unsafe_entry("member has no pinned size and SHA-256"));
                    }
                }
                Recipe::Scummvm => {
                    if !pinned.game_id.as_deref().is_some_and(|game| {
                        !game.is_empty()
                            && game.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_')
                            })
                    }) {
                        return Err(unsafe_entry("ScummVM game id is missing or unsafe"));
                    }
                }
                Recipe::ExtractTree => {
                    let member = Path::new(pinned.member.as_deref().unwrap_or_default());
                    if member.as_os_str().is_empty()
                        || member
                            .components()
                            .any(|component| !matches!(component, Component::Normal(_)))
                    {
                        return Err(unsafe_entry("launch member is missing or unsafe"));
                    }
                }
                Recipe::RetrobatStore => {
                    let member = pinned.member.as_deref().unwrap_or_default();
                    let path = Path::new(member);
                    if !member.starts_with("roms/")
                        || path
                            .components()
                            .any(|component| !matches!(component, Component::Normal(_)))
                        || path.components().count() < 3
                    {
                        return Err(unsafe_entry(
                            "store launch path is not under roms/<system>/",
                        ));
                    }
                }
            }
            if let Some(core) = &pinned.core
                && !core
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            {
                return Err(unsafe_entry("core name is unsafe"));
            }
        }
        Ok(())
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_single_component(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !value.contains(['/', '\\'])
        && path.components().count() == 1
        && matches!(path.components().next(), Some(Component::Normal(_)))
}

#[derive(Debug, Error)]
pub enum VerifiedDownloadError {
    #[error("{0}")]
    Download(#[from] DownloadError),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("the publisher sent more than the pinned {expected} bytes")]
    TooLarge { expected: u64 },
    #[error("downloaded size mismatch: expected {expected}, got {actual}")]
    Size { expected: u64, actual: u64 },
    #[error("downloaded SHA-256 mismatch: expected {expected}, got {actual}")]
    Hash { expected: String, actual: String },
}

/// A file writer that hashes as it writes and refuses bytes past `limit`.
pub struct VerifyingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    written: u64,
    limit: u64,
    exceeded: bool,
}

impl<W: Write> VerifyingWriter<W> {
    pub fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            written: 0,
            limit,
            exceeded: false,
        }
    }

    /// Returns the writer, the byte count, and the lowercase SHA-256.
    pub fn finish(self) -> (W, u64, String) {
        (
            self.inner,
            self.written,
            hex::encode(self.hasher.finalize()),
        )
    }
}

impl<W: Write> Write for VerifyingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.written + buffer.len() as u64 > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("download exceeded its pinned size"));
        }
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Streams `url` into `destination`, verifying size and SHA-256 on the fly.
/// On any mismatch the partial file is removed before returning.
pub fn fetch_verified<D: DownloadClient + ?Sized>(
    downloader: &D,
    url: &str,
    size: u64,
    sha256: &str,
    destination: &Path,
) -> Result<(), VerifiedDownloadError> {
    let result = (|| {
        let mut writer = VerifyingWriter::new(File::create(destination)?, size);
        let fetched = downloader.fetch(url, &mut writer);
        if writer.exceeded {
            return Err(VerifiedDownloadError::TooLarge { expected: size });
        }
        fetched?;
        let (file, actual_size, actual_hash) = writer.finish();
        file.sync_all()?;
        if actual_size != size {
            return Err(VerifiedDownloadError::Size {
                expected: size,
                actual: actual_size,
            });
        }
        if actual_hash != sha256.to_ascii_lowercase() {
            return Err(VerifiedDownloadError::Hash {
                expected: sha256.to_owned(),
                actual: actual_hash,
            });
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifying_writer_hashes_and_refuses_bytes_past_the_pin() {
        let mut writer = VerifyingWriter::new(Vec::new(), 5);
        writer.write_all(b"abc").unwrap();
        assert!(writer.write_all(b"def").is_err());
        let (bytes, size, hash) = VerifyingWriter::new(Vec::new(), 3)
            .tap_write(b"abc")
            .finish();
        assert_eq!(bytes, b"abc");
        assert_eq!(size, 3);
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    trait TapWrite {
        fn tap_write(self, bytes: &[u8]) -> Self;
    }

    impl<W: Write> TapWrite for VerifyingWriter<W> {
        fn tap_write(mut self, bytes: &[u8]) -> Self {
            self.write_all(bytes).unwrap();
            self
        }
    }

    #[test]
    fn single_component_filenames_reject_traversal_and_separators() {
        assert!(is_single_component("Game.zip"));
        for unsafe_name in ["", "..", ".", "a/b.zip", "a\\b.zip", "/abs.zip"] {
            assert!(!is_single_component(unsafe_name), "{unsafe_name}");
        }
    }
}
