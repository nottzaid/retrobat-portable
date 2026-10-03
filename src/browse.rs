use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

const LIBRARY: &str = include_str!("../catalog/browse-library-v2.json");
const CLASSICS_LIBRARY: &[u8] = include_bytes!("../catalog/classics-library-v1.json.gz");
const ICONIC_LIBRARY: &str = include_str!("../catalog/iconic-library-v1.json");

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BrowseCatalog {
    pub schema_version: u32,
    pub generated_at: String,
    pub sources: Vec<BrowseSource>,
    pub entries: Vec<BrowseEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BrowseSource {
    pub id: String,
    pub name: String,
    pub homepage: String,
    pub summary: String,
    pub distribution_policy: String,
    pub snapshot_ref: String,
    pub entry_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BrowseEntry {
    pub id: String,
    pub source_id: String,
    pub title: String,
    pub developer: String,
    pub system: String,
    pub kind: String,
    pub tags: Vec<String>,
    pub license: Option<String>,
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub artwork_asset: Option<BundledArtwork>,
    pub detail_url: Option<String>,
    pub description: String,
    pub release_year: Option<u16>,
    pub install_state: InstallState,
    pub acquisition: Acquisition,
    #[serde(default)]
    pub known_sha1: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BundledArtwork {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    Verified,
    AuditRequired,
    BrowseOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Acquisition {
    DirectDownload,
    LocalImport,
}

#[derive(Debug, Error)]
pub enum BrowseError {
    #[error("browse snapshot is invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported browse snapshot schema {0}")]
    Schema(u32),
    #[error("duplicate or empty browse id: {0}")]
    Id(String),
    #[error("duplicate or empty browse source id: {0}")]
    SourceId(String),
    #[error("browse entry {id} references unknown source {source_id}")]
    UnknownSource { id: String, source_id: String },
    #[error("browse source {id} declares {declared} entries but contains {actual}")]
    SourceCount {
        id: String,
        declared: usize,
        actual: usize,
    },
    #[error("browse entry {id} has an invalid artwork URL: {url}")]
    ArtworkUrl { id: String, url: String },
    #[error("browse entry {id} has an invalid bundled artwork asset: {path}")]
    ArtworkAsset { id: String, path: String },
    #[error("browse entry {id} has an invalid detail URL: {url}")]
    DetailUrl { id: String, url: String },
    #[error("browse entry {id} has an invalid SHA-1 identity: {sha1}")]
    Sha1 { id: String, sha1: String },
    #[error("commercial browse snapshot could not be decompressed: {0}")]
    Decompress(#[from] std::io::Error),
}

impl BrowseCatalog {
    pub fn built_in() -> Result<Self, BrowseError> {
        // The classics snapshot is most of the work; decode it while the
        // smaller snapshots parse.
        let (classics, catalog, iconic) = std::thread::scope(|scope| {
            let classics = scope.spawn(|| -> Result<Self, BrowseError> {
                let mut decoded = Vec::with_capacity(64 << 20);
                GzDecoder::new(CLASSICS_LIBRARY).read_to_end(&mut decoded)?;
                Ok(serde_json::from_slice(&decoded)?)
            });
            let catalog = serde_json::from_str::<Self>(LIBRARY);
            let iconic = serde_json::from_str::<Self>(ICONIC_LIBRARY);
            (
                classics.join().expect("classics decoder thread"),
                catalog,
                iconic,
            )
        });
        let (mut catalog, classics, iconic) = (catalog?, classics?, iconic?);
        catalog.sources.extend(classics.sources);
        catalog.entries.extend(classics.entries);
        catalog.sources.extend(iconic.sources);
        catalog.entries.extend(iconic.entries);
        catalog.entries.sort_by_cached_key(|entry| {
            (
                entry.title.to_ascii_lowercase(),
                entry.system.clone(),
                entry.source_id.clone(),
            )
        });
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), BrowseError> {
        if self.schema_version != 2 {
            return Err(BrowseError::Schema(self.schema_version));
        }
        let mut source_ids = HashSet::new();
        for source in &self.sources {
            if source.id.is_empty() || !source_ids.insert(source.id.as_str()) {
                return Err(BrowseError::SourceId(source.id.clone()));
            }
        }

        let mut ids = HashSet::with_capacity(self.entries.len());
        for entry in &self.entries {
            if entry.id.is_empty() || !ids.insert(entry.id.as_str()) {
                return Err(BrowseError::Id(entry.id.clone()));
            }
            if !source_ids.contains(entry.source_id.as_str()) {
                return Err(BrowseError::UnknownSource {
                    id: entry.id.clone(),
                    source_id: entry.source_id.clone(),
                });
            }
        }
        // URL parsing dominates validation; entries are independent.
        let workers = std::thread::available_parallelism()
            .map_or(4, |count| count.get())
            .clamp(1, 8);
        let chunk = self.entries.len().div_ceil(workers).max(1);
        std::thread::scope(|scope| {
            let checks = self
                .entries
                .chunks(chunk)
                .map(|entries| scope.spawn(move || entries.iter().try_for_each(validate_entry)))
                .collect::<Vec<_>>();
            checks
                .into_iter()
                .try_for_each(|check| check.join().expect("validation thread"))
        })?;
        for source in &self.sources {
            let actual = self
                .entries
                .iter()
                .filter(|entry| entry.source_id == source.id)
                .count();
            if source.entry_count != actual {
                return Err(BrowseError::SourceCount {
                    id: source.id.clone(),
                    declared: source.entry_count,
                    actual,
                });
            }
        }
        Ok(())
    }
}

fn validate_entry(entry: &BrowseEntry) -> Result<(), BrowseError> {
    if let Some(url) = &entry.artwork_url {
        validate_https_url(url).map_err(|()| BrowseError::ArtworkUrl {
            id: entry.id.clone(),
            url: url.clone(),
        })?;
    }
    if let Some(asset) = &entry.artwork_asset {
        let path = Path::new(&asset.path);
        let safe_path = !path.as_os_str().is_empty()
            && !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            && path.starts_with("Artwork");
        let valid_hash = asset.sha256.len() == 64
            && asset
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !safe_path || asset.size == 0 || !valid_hash {
            return Err(BrowseError::ArtworkAsset {
                id: entry.id.clone(),
                path: asset.path.clone(),
            });
        }
    }
    if let Some(url) = &entry.detail_url {
        validate_https_url(url).map_err(|()| BrowseError::DetailUrl {
            id: entry.id.clone(),
            url: url.clone(),
        })?;
    }
    for sha1 in &entry.known_sha1 {
        if sha1.len() != 40
            || !sha1
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(BrowseError::Sha1 {
                id: entry.id.clone(),
                sha1: sha1.clone(),
            });
        }
    }
    Ok(())
}

fn validate_https_url(value: &str) -> Result<(), ()> {
    let parsed = Url::parse(value).map_err(|_| ())?;
    if parsed.scheme() == "https" && parsed.host_str().is_some() {
        Ok(())
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_multi_source_snapshot_is_complete_and_valid() {
        let catalog = BrowseCatalog::built_in().unwrap();
        assert_eq!(catalog.sources.len(), 10);
        assert_eq!(catalog.entries.len(), 80_734);
        assert!(
            catalog
                .entries
                .iter()
                .filter(|entry| entry.artwork_url.is_some() || entry.artwork_asset.is_some())
                .count()
                >= 45_800
        );
        assert_eq!(
            catalog
                .entries
                .iter()
                .filter(|entry| entry.install_state == InstallState::Verified)
                .count(),
            1
        );
        assert_eq!(
            catalog
                .sources
                .iter()
                .find(|source| source.id == "dos-games-archive")
                .unwrap()
                .entry_count,
            1_649
        );
        assert_eq!(
            catalog
                .entries
                .iter()
                .filter(|entry| entry.acquisition == Acquisition::LocalImport)
                .count(),
            76_581
        );
        assert_eq!(
            catalog
                .entries
                .iter()
                .filter(|entry| !entry.known_sha1.is_empty())
                .count(),
            76_497
        );
    }
}
