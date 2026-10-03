//! DOWNLOAD: fetch a catalogue game's pinned publisher file, verify it while
//! it streams, and install it with the recipe recorded for that entry.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use thiserror::Error;

use crate::browse::{Acquisition, BrowseEntry};
use crate::downloads::{
    DownloadLedger, LedgerError, PinnedDownload, Recipe, VerifiedDownloadError, fetch_verified,
};
use crate::import::{
    GameImporter, ImportError, ImportReport, StagingDirectory, extract_archive,
    extract_archive_for_member, find_by_digest,
};
use crate::install::DownloadClient;
use crate::paths::PortableLayout;

#[derive(Debug, Error)]
pub enum BrowseInstallError {
    #[error("this catalogue entry is imported from a local copy, not downloaded")]
    NotADownload,
    #[error("no verified download is recorded for this game: {0}")]
    Unavailable(String),
    #[error("the download ledger is unusable: {0}")]
    Ledger(#[from] LedgerError),
    #[error("game download failed safely: {0}")]
    Download(#[from] VerifiedDownloadError),
    #[error("the downloaded package is missing its game file {0}")]
    PackageLayout(String),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Import(#[from] ImportError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowseInstallReport {
    pub source_url: String,
    pub import: ImportReport,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct DownloadCoverage {
    pub total_entries: usize,
    /// Entries with a pinned URL, size, SHA-256, and install recipe.
    pub verified_entries: usize,
    /// Entries whose source does not currently serve a usable file.
    pub unavailable_by_source: BTreeMap<String, usize>,
    /// Entries absent from the ledger altogether (a build defect).
    pub unrecorded_entries: Vec<String>,
}

/// The built-in ledger, parsed and validated once per process.
pub fn ledger() -> Result<&'static DownloadLedger, BrowseInstallError> {
    static LEDGER: OnceLock<Result<DownloadLedger, String>> = OnceLock::new();
    LEDGER
        .get_or_init(|| DownloadLedger::built_in().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| BrowseInstallError::Unavailable(error.clone()))
}

/// The verified download for this card, or why it has none.
pub fn download_route(entry: &BrowseEntry) -> Result<&'static PinnedDownload, String> {
    if entry.acquisition != Acquisition::DirectDownload {
        return Err("this game is imported from your own copy".to_owned());
    }
    let ledger = ledger().map_err(|error| error.to_string())?;
    ledger.get(&entry.id).ok_or_else(|| {
        ledger
            .unavailable
            .get(&entry.id)
            .cloned()
            .unwrap_or_else(|| "the download ledger has no record of this game".to_owned())
    })
}

pub fn supports_direct_download(entry: &BrowseEntry) -> bool {
    download_route(entry).is_ok()
}

pub fn audit_download_coverage(entries: &[BrowseEntry]) -> DownloadCoverage {
    let ledger = ledger().ok();
    let mut coverage = DownloadCoverage {
        total_entries: 0,
        verified_entries: 0,
        unavailable_by_source: BTreeMap::new(),
        unrecorded_entries: Vec::new(),
    };
    for entry in entries
        .iter()
        .filter(|entry| entry.acquisition == Acquisition::DirectDownload)
    {
        coverage.total_entries += 1;
        match ledger {
            Some(ledger) if ledger.get(&entry.id).is_some() => coverage.verified_entries += 1,
            Some(ledger) if ledger.unavailable.contains_key(&entry.id) => {
                *coverage
                    .unavailable_by_source
                    .entry(entry.source_id.clone())
                    .or_insert(0) += 1;
            }
            _ => coverage.unrecorded_entries.push(entry.id.clone()),
        }
    }
    coverage
}

pub struct BrowseInstaller<'a, D: DownloadClient> {
    layout: &'a PortableLayout,
    downloader: &'a D,
}

impl<'a, D: DownloadClient> BrowseInstaller<'a, D> {
    pub fn new(layout: &'a PortableLayout, downloader: &'a D) -> Self {
        Self { layout, downloader }
    }

    pub fn install(&self, entry: &BrowseEntry) -> Result<BrowseInstallReport, BrowseInstallError> {
        if entry.acquisition != Acquisition::DirectDownload {
            return Err(BrowseInstallError::NotADownload);
        }
        let pinned = download_route(entry).map_err(BrowseInstallError::Unavailable)?;
        self.install_pinned(entry, pinned)
    }

    pub fn install_pinned(
        &self,
        entry: &BrowseEntry,
        pinned: &PinnedDownload,
    ) -> Result<BrowseInstallReport, BrowseInstallError> {
        let stage = StagingDirectory::new(self.layout, "download")?;
        let downloaded = stage.path().join(&pinned.filename);
        fetch_verified(
            self.downloader,
            &pinned.url,
            pinned.size,
            &pinned.sha256,
            &downloaded,
        )?;
        let importer = GameImporter::new(self.layout);
        let import = match pinned.recipe {
            Recipe::File => importer.import_owned_file(entry, &downloaded, pinned.core.clone())?,
            Recipe::ExtractMember => {
                let extracted = stage.path().join("extracted");
                fs::create_dir_all(&extracted)?;
                extract_archive_for_member(self.layout, &downloaded, &extracted)?;
                let member = find_by_digest(
                    &extracted,
                    pinned.member_size.unwrap_or_default(),
                    pinned.member_sha256.as_deref().unwrap_or_default(),
                )?;
                importer.import_owned_file(entry, &member, pinned.core.clone())?
            }
            Recipe::Scummvm => {
                let extracted = stage.path().join("extracted");
                fs::create_dir_all(&extracted)?;
                extract_archive(self.layout, &downloaded, &extracted)?;
                let game = single_top_level_directory(&extracted)?;
                let game_id = pinned.game_id.as_deref().unwrap_or_default();
                let short = game_id.rsplit(':').next().unwrap_or(game_id);
                let launch = PathBuf::from(format!("{short}.scummvm"));
                // RetroBat's ScummVM system and the libretro core start a game
                // from a .scummvm file in its folder naming the game.
                fs::write(game.join(&launch), game_id)?;
                importer.import_tree(entry, &game, Some(&launch), true, None)?
            }
            Recipe::ExtractTree => {
                let extracted = stage.path().join("extracted");
                fs::create_dir_all(&extracted)?;
                extract_archive(self.layout, &downloaded, &extracted)?;
                let member = PathBuf::from(pinned.member.as_deref().unwrap_or_default());
                // A wrapping folder becomes the game folder, as in the
                // archive; the launch path is relative to it.
                let root = single_top_level_directory(&extracted)?;
                let launch = root
                    .strip_prefix(&extracted)
                    .ok()
                    .and_then(|wrapper| member.strip_prefix(wrapper).ok())
                    .map(Path::to_path_buf)
                    .unwrap_or(member);
                if !root.join(&launch).is_file() {
                    return Err(BrowseInstallError::PackageLayout(
                        launch.display().to_string(),
                    ));
                }
                importer.import_tree(entry, &root, Some(&launch), true, pinned.core.clone())?
            }
            Recipe::RetrobatStore => {
                let extracted = stage.path().join("extracted");
                fs::create_dir_all(&extracted)?;
                extract_archive(self.layout, &downloaded, &extracted)?;
                let member = pinned.member.as_deref().unwrap_or_default();
                let mut parts = Path::new(member).components();
                parts.next(); // roms
                let system = parts
                    .next()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .ok_or_else(|| BrowseInstallError::PackageLayout(member.to_owned()))?;
                let launch = parts.as_path().to_owned();
                let tree = extracted.join("roms").join(&system);
                if !tree.join(&launch).is_file() {
                    return Err(BrowseInstallError::PackageLayout(member.to_owned()));
                }
                importer.import_system_tree(entry, &system, &tree, &launch)?
            }
        };
        Ok(BrowseInstallReport {
            source_url: pinned.url.clone(),
            import,
        })
    }
}

/// Distribution archives usually wrap the game in one folder; use it as the
/// game folder so the launch file sits beside the game's data.
fn single_top_level_directory(root: &Path) -> io::Result<PathBuf> {
    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    if entries.len() == 1 && entries[0].file_type()?.is_dir() {
        return Ok(entries.remove(0).path());
    }
    Ok(root.to_owned())
}
