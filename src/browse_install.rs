use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Component, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use percent_encoding::percent_decode_str;
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::browse::BrowseEntry;
use crate::import::{GameImporter, ImportError, ImportReport};
use crate::install::{DownloadClient, DownloadError};
use crate::paths::PortableLayout;

const GB_DATABASE: &str =
    "https://raw.githubusercontent.com/gbdev/database/8a36461e5e2fada5c73484afd87b7e9a9d4e05df";
const GBA_DATABASE: &str =
    "https://raw.githubusercontent.com/gbadev-org/games/9111a814b212318db107a91adb0947b63d1e19a7";
const NES_DATABASE: &str = "https://raw.githubusercontent.com/nesdev-org/homebrew-db/95ba342830260e3b7587b5ed230b65f72ec11c2b";

#[derive(Debug, Error)]
pub enum BrowseInstallError {
    #[error("automatic download is not implemented for source {0}")]
    UnsupportedSource(String),
    #[error("browse entry has an invalid source identifier: {0}")]
    InvalidId(String),
    #[error("source metadata download failed: {0}")]
    MetadataDownload(DownloadError),
    #[error("source metadata is invalid: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("source metadata has no default playable file")]
    NoPlayableFile,
    #[error("source metadata contains an unsafe filename: {0}")]
    UnsafeFilename(String),
    #[error("game download failed: {0}")]
    GameDownload(DownloadError),
    #[error("RetroBat Store package metadata is missing or invalid: {0}")]
    StorePackage(String),
    #[error("RetroBat Store installer failed: {0}")]
    StoreInstaller(String),
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
    pub covered_entries: usize,
    pub uncovered_by_source: BTreeMap<String, usize>,
}

#[derive(Debug, Deserialize)]
struct HomebrewMetadata {
    files: Vec<HomebrewFile>,
}

#[derive(Debug, Deserialize)]
struct HomebrewFile {
    #[serde(default)]
    default: bool,
    filename: String,
    #[serde(default)]
    playable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RetroBatStorePackage {
    name: String,
    system: String,
    game_path: PathBuf,
}

pub struct BrowseInstaller<'a, D: DownloadClient> {
    layout: &'a PortableLayout,
    downloader: &'a D,
}

pub fn supports_direct_download(entry: &BrowseEntry) -> bool {
    matches!(
        entry.source_id.as_str(),
        "homebrew-hub"
            | "libretro-content"
            | "mame-authorized"
            | "freedos"
            | "msxdev"
            | "dos-games-archive"
            | "scummvm-freeware"
            | "retrobat-store"
    )
}

pub fn audit_download_coverage(
    entries: &[BrowseEntry],
    trusted_ids: &HashSet<&str>,
) -> DownloadCoverage {
    let direct = entries
        .iter()
        .filter(|entry| entry.acquisition == crate::browse::Acquisition::DirectDownload);
    let mut total_entries = 0;
    let mut covered_entries = 0;
    let mut uncovered_by_source = BTreeMap::new();
    for entry in direct {
        total_entries += 1;
        if trusted_ids.contains(entry.id.as_str()) || supports_direct_download(entry) {
            covered_entries += 1;
        } else {
            *uncovered_by_source
                .entry(entry.source_id.clone())
                .or_insert(0) += 1;
        }
    }
    DownloadCoverage {
        total_entries,
        covered_entries,
        uncovered_by_source,
    }
}

impl<'a, D: DownloadClient> BrowseInstaller<'a, D> {
    pub fn new(layout: &'a PortableLayout, downloader: &'a D) -> Self {
        Self { layout, downloader }
    }

    pub fn supports(entry: &BrowseEntry) -> bool {
        supports_direct_download(entry)
    }

    pub fn install(&self, entry: &BrowseEntry) -> Result<BrowseInstallReport, BrowseInstallError> {
        if !Self::supports(entry) {
            return Err(BrowseInstallError::UnsupportedSource(
                entry.source_id.clone(),
            ));
        }
        if entry.source_id == "libretro-content" {
            return self.install_libretro_content(entry);
        }
        if entry.source_id == "homebrew-hub" {
            return self.install_homebrew_hub(entry);
        }
        if entry.source_id == "retrobat-store" {
            return self.install_retrobat_store(entry);
        }
        self.install_from_source_page(entry)
    }

    fn install_homebrew_hub(
        &self,
        entry: &BrowseEntry,
    ) -> Result<BrowseInstallReport, BrowseInstallError> {
        let slug = entry
            .id
            .strip_prefix("homebrew-hub/")
            .filter(|slug| {
                !slug.is_empty()
                    && slug.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || byte == b'-'
                            || byte == b'_'
                    })
            })
            .ok_or_else(|| BrowseInstallError::InvalidId(entry.id.clone()))?;
        let base = match entry.system.as_str() {
            "gba" => GBA_DATABASE,
            "nes" => NES_DATABASE,
            _ => GB_DATABASE,
        };
        let metadata_url = source_url(base, slug, "game.json")?;
        let mut metadata_bytes = Vec::new();
        self.downloader
            .fetch(metadata_url.as_str(), &mut metadata_bytes)
            .map_err(BrowseInstallError::MetadataDownload)?;
        let metadata: HomebrewMetadata = serde_json::from_slice(&metadata_bytes)?;
        let playable = metadata
            .files
            .iter()
            .find(|file| file.playable && file.default)
            .or_else(|| metadata.files.iter().find(|file| file.playable))
            .ok_or(BrowseInstallError::NoPlayableFile)?;
        validate_filename(&playable.filename)?;
        let game_url = source_url(base, slug, &playable.filename)?;
        self.download_and_import(entry, game_url, &playable.filename)
    }

    fn install_libretro_content(
        &self,
        entry: &BrowseEntry,
    ) -> Result<BrowseInstallReport, BrowseInstallError> {
        let value = entry
            .detail_url
            .as_deref()
            .ok_or_else(|| BrowseInstallError::InvalidId(entry.id.clone()))?;
        let url = Url::parse(value).map_err(|_| BrowseInstallError::InvalidId(entry.id.clone()))?;
        if url.scheme() != "https"
            || url.host_str() != Some("buildbot.libretro.com")
            || !url.path().starts_with("/assets/cores/")
        {
            return Err(BrowseInstallError::InvalidId(entry.id.clone()));
        }
        let encoded_filename = url
            .path_segments()
            .and_then(Iterator::last)
            .ok_or_else(|| BrowseInstallError::InvalidId(entry.id.clone()))?;
        let filename = percent_decode_str(encoded_filename)
            .decode_utf8()
            .map_err(|_| BrowseInstallError::InvalidId(entry.id.clone()))?
            .into_owned();
        validate_filename(&filename)?;
        self.download_and_import(entry, url, &filename)
    }

    fn install_from_source_page(
        &self,
        entry: &BrowseEntry,
    ) -> Result<BrowseInstallReport, BrowseInstallError> {
        let detail = entry
            .detail_url
            .as_deref()
            .ok_or_else(|| BrowseInstallError::InvalidId(entry.id.clone()))?;
        let detail_url =
            Url::parse(detail).map_err(|_| BrowseInstallError::InvalidId(entry.id.clone()))?;
        let (game_url, filename) = match entry.source_id.as_str() {
            "mame-authorized" => {
                require_https_host(&detail_url, "www.mamedev.org")?;
                let local_id = entry
                    .id
                    .strip_prefix("mame-authorized/")
                    .ok_or_else(|| BrowseInstallError::InvalidId(entry.id.clone()))?;
                let filename = format!("{local_id}.zip");
                let mut directory = detail_url.clone();
                if !directory.path().ends_with('/') {
                    directory.set_path(&format!("{}/", directory.path()));
                }
                (
                    directory
                        .join(&filename)
                        .map_err(|_| BrowseInstallError::InvalidId(entry.id.clone()))?,
                    filename,
                )
            }
            "freedos" => {
                require_https_host(&detail_url, "www.ibiblio.org")?;
                let page = self.fetch_text(&detail_url)?;
                resolve_page_link(&detail_url, &page, |href| {
                    href.to_ascii_lowercase().ends_with(".zip") && href.contains("/games/")
                })?
            }
            "msxdev" => {
                require_https_host(&detail_url, "www.msxdev.org")?;
                let page = self.fetch_text(&detail_url)?;
                resolve_page_link(&detail_url, &page, |href| {
                    href.contains("/wp-content/uploads/")
                        && href.to_ascii_lowercase().ends_with(".zip")
                })?
            }
            "dos-games-archive" => {
                require_https_host(&detail_url, "www.dosgamesarchive.com")?;
                self.resolve_dos_games_archive(&detail_url)?
            }
            "scummvm-freeware" => {
                require_https_host(&detail_url, "www.scummvm.org")?;
                self.resolve_scummvm(&detail_url)?
            }
            _ => {
                return Err(BrowseInstallError::UnsupportedSource(
                    entry.source_id.clone(),
                ));
            }
        };
        validate_filename(&filename)?;
        self.download_and_import(entry, game_url, &filename)
    }

    fn install_retrobat_store(
        &self,
        entry: &BrowseEntry,
    ) -> Result<BrowseInstallReport, BrowseInstallError> {
        const STORE_URL: &str = "https://www.retrobat.ovh/repo/games/store.xml";
        let mut xml = Vec::new();
        self.downloader
            .fetch(STORE_URL, &mut xml)
            .map_err(BrowseInstallError::MetadataDownload)?;
        let package = parse_retrobat_store_package(&xml, &entry.id)?;
        let store = self
            .layout
            .emulationstation_root()
            .join("batocera-store.exe");
        let mut command = if cfg!(target_os = "windows") {
            Command::new(&store)
        } else if cfg!(target_os = "linux") {
            let mut command = Command::new("wine");
            command.arg(&store);
            let data = dirs::data_local_dir().ok_or_else(|| {
                BrowseInstallError::StoreInstaller(
                    "cannot determine the Wine prefix location".into(),
                )
            })?;
            command.env(
                "WINEPREFIX",
                data.join("retrobat-portable").join("wine-prefix"),
            );
            command
        } else {
            return Err(BrowseInstallError::StoreInstaller(
                "unsupported host platform".into(),
            ));
        };
        let output = command
            .args(["install", package.name.as_str()])
            .current_dir(self.layout.emulationstation_root())
            .output()?;
        if !output.status.success() {
            return Err(BrowseInstallError::StoreInstaller(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        let launch_file = self
            .layout
            .retrobat_root()
            .join("roms")
            .join(package.system.to_ascii_lowercase())
            .join(&package.game_path);
        let import = GameImporter::new(self.layout).register_existing(
            entry,
            &package.system,
            &launch_file,
        )?;
        Ok(BrowseInstallReport {
            source_url: STORE_URL.into(),
            import,
        })
    }

    fn fetch_text(&self, url: &Url) -> Result<String, BrowseInstallError> {
        let mut bytes = Vec::new();
        self.downloader
            .fetch(url.as_str(), &mut bytes)
            .map_err(BrowseInstallError::MetadataDownload)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn resolve_dos_games_archive(
        &self,
        detail_url: &Url,
    ) -> Result<(Url, String), BrowseInstallError> {
        let detail_page = self.fetch_text(detail_url)?;
        let file_page_url = resolve_page_link(detail_url, &detail_page, |href| {
            href.starts_with("/file/") && !href.ends_with(".php")
        })?
        .0;
        require_https_host(&file_page_url, "www.dosgamesarchive.com")?;
        let file_page = self.fetch_text(&file_page_url)?;
        let download_url = resolve_page_link(&file_page_url, &file_page, |href| {
            href.starts_with("/file.php?id=")
        })?
        .0;
        let filename = text_between(&file_page, "<h1 class=\"download\">Download ", "</h1>")
            .filter(|value| value.to_ascii_lowercase().ends_with(".zip"))
            .ok_or(BrowseInstallError::NoPlayableFile)?
            .to_owned();
        Ok((download_url, filename))
    }

    fn resolve_scummvm(&self, detail_url: &Url) -> Result<(Url, String), BrowseInstallError> {
        let fragment = detail_url
            .fragment()
            .ok_or_else(|| BrowseInstallError::InvalidId(detail_url.to_string()))?;
        let mut page_url = detail_url.clone();
        page_url.set_fragment(None);
        let page = self.fetch_text(&page_url)?;
        let marker = format!("id=\"{fragment}\"");
        let section_start = page
            .find(&marker)
            .ok_or(BrowseInstallError::NoPlayableFile)?;
        let remainder = &page[section_start + marker.len()..];
        let section_end = remainder
            .find("<div class=\"subhead\"")
            .unwrap_or(remainder.len());
        resolve_page_link(&page_url, &remainder[..section_end], |href| {
            href.starts_with("https://downloads.scummvm.org/")
                && href.to_ascii_lowercase().ends_with(".zip")
        })
    }

    fn download_and_import(
        &self,
        entry: &BrowseEntry,
        game_url: Url,
        filename: &str,
    ) -> Result<BrowseInstallReport, BrowseInstallError> {
        let operation_id = format!(
            "source-download-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let stage = self.layout.staging_root().join(operation_id);
        fs::create_dir_all(&stage)?;
        let source = stage.join(filename);
        let result = (|| {
            let mut output = File::create(&source)?;
            self.downloader
                .fetch(game_url.as_str(), &mut output)
                .map_err(BrowseInstallError::GameDownload)?;
            output.sync_all()?;
            drop(output);
            let import = GameImporter::new(self.layout).import(entry, &source)?;
            Ok(BrowseInstallReport {
                source_url: game_url.to_string(),
                import,
            })
        })();
        let _ = fs::remove_dir_all(stage);
        result
    }
}

fn require_https_host(url: &Url, expected: &str) -> Result<(), BrowseInstallError> {
    if url.scheme() == "https" && url.host_str() == Some(expected) {
        Ok(())
    } else {
        Err(BrowseInstallError::InvalidId(url.to_string()))
    }
}

fn resolve_page_link(
    base: &Url,
    html: &str,
    predicate: impl Fn(&str) -> bool,
) -> Result<(Url, String), BrowseInstallError> {
    for href in html_hrefs(html) {
        if !predicate(href) {
            continue;
        }
        let url = base
            .join(href)
            .map_err(|_| BrowseInstallError::InvalidId(href.to_owned()))?;
        let encoded = url
            .path_segments()
            .and_then(Iterator::last)
            .ok_or_else(|| BrowseInstallError::InvalidId(url.to_string()))?;
        let filename = percent_decode_str(encoded)
            .decode_utf8()
            .map_err(|_| BrowseInstallError::InvalidId(url.to_string()))?
            .into_owned();
        return Ok((url, filename));
    }
    Err(BrowseInstallError::NoPlayableFile)
}

fn html_hrefs(html: &str) -> impl Iterator<Item = &str> {
    html.split("href=\"")
        .skip(1)
        .filter_map(|remainder| remainder.split_once('"').map(|(href, _)| href))
}

fn text_between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let start = text.find(start)? + start.len();
    let remainder = &text[start..];
    let end = remainder.find(end)?;
    Some(&remainder[..end])
}

fn parse_retrobat_store_package(
    xml: &[u8],
    entry_id: &str,
) -> Result<RetroBatStorePackage, BrowseInstallError> {
    let local_id = entry_id
        .strip_prefix("retrobat-store/")
        .ok_or_else(|| BrowseInstallError::InvalidId(entry_id.to_owned()))?;
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut in_package = false;
    let mut in_game = false;
    let mut field = None;
    let mut name = None;
    let mut system = None;
    let mut game_path = None;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) if start.name().as_ref() == b"package" => {
                in_package = true;
                name = None;
                system = None;
                game_path = None;
            }
            Ok(Event::Start(start)) if in_package && start.name().as_ref() == b"name" => {
                field = Some("name");
            }
            Ok(Event::Start(start)) if in_package && start.name().as_ref() == b"game" => {
                in_game = true;
                for attribute in start.attributes().flatten() {
                    if attribute.key.as_ref() == b"system" {
                        system = Some(String::from_utf8_lossy(&attribute.value).into_owned());
                    }
                }
            }
            Ok(Event::Start(start))
                if in_package && in_game && start.name().as_ref() == b"path" =>
            {
                field = Some("path");
            }
            Ok(Event::Text(text)) if in_package => {
                let value = text
                    .decode()
                    .map_err(|error| BrowseInstallError::StorePackage(error.to_string()))?
                    .into_owned();
                match field {
                    Some("name") if !in_game => name = Some(value),
                    Some("path") if in_game => game_path = Some(value),
                    _ => {}
                }
            }
            Ok(Event::End(end)) if end.name().as_ref() == b"name" => field = None,
            Ok(Event::End(end)) if end.name().as_ref() == b"path" => field = None,
            Ok(Event::End(end)) if end.name().as_ref() == b"game" => in_game = false,
            Ok(Event::End(end)) if end.name().as_ref() == b"package" => {
                if let (Some(name), Some(system), Some(path)) =
                    (name.take(), system.take(), game_path.take())
                    && slug_ascii(&name) == local_id
                {
                    let path = PathBuf::from(path.trim_start_matches("./"));
                    if path.as_os_str().is_empty()
                        || path.is_absolute()
                        || path
                            .components()
                            .any(|component| !matches!(component, Component::Normal(_)))
                    {
                        return Err(BrowseInstallError::StorePackage(path.display().to_string()));
                    }
                    return Ok(RetroBatStorePackage {
                        name,
                        system,
                        game_path: path,
                    });
                }
                in_package = false;
                in_game = false;
                field = None;
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(BrowseInstallError::StorePackage(error.to_string())),
        }
        buffer.clear();
    }
    Err(BrowseInstallError::StorePackage(entry_id.to_owned()))
}

fn slug_ascii(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('-');
            }
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    output
}

fn source_url(base: &str, slug: &str, filename: &str) -> Result<Url, BrowseInstallError> {
    let mut url = Url::parse(base).map_err(|_| BrowseInstallError::InvalidId(base.to_owned()))?;
    url.path_segments_mut()
        .map_err(|_| BrowseInstallError::InvalidId(base.to_owned()))?
        .extend(["entries", slug, filename]);
    Ok(url)
}

fn validate_filename(filename: &str) -> Result<(), BrowseInstallError> {
    let path = PathBuf::from(filename);
    if filename.is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(BrowseInstallError::UnsafeFilename(filename.to_owned()));
    }
    Ok(())
}
