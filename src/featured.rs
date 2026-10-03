use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::browse::BrowseCatalog;
use crate::readiness::{BackendState, FirmwareState, ReadinessReport};

const FEATURED: &str = include_str!("../catalog/featured-v1.json");

#[derive(Clone, Debug, Deserialize)]
struct FeaturedSnapshot {
    schema_version: u32,
    sources: Vec<FeaturedSource>,
    titles: Vec<FeaturedTitle>,
    matched_entry_count: usize,
    unmatched_title_count: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FeaturedSource {
    pub id: String,
    pub name: String,
    pub url: String,
    pub methodology: String,
    pub title_count: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FeaturedTitle {
    pub title: String,
    pub source_id: String,
    pub matched_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct FeaturedCatalog {
    pub entry_ids: HashSet<String>,
    pub sources: Vec<FeaturedSource>,
    pub titles: Vec<FeaturedTitle>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeaturedReadinessAudit {
    pub evidence_title_records: usize,
    pub unique_catalog_entries: usize,
    pub ready_now_titles: usize,
    pub emulator_missing_titles: usize,
    pub firmware_blocked_titles: usize,
    pub unresolved_titles: usize,
    pub sourced_artwork_titles: usize,
    pub attention: Vec<FeaturedTitleReadiness>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeaturedTitleReadiness {
    pub title: String,
    pub state: &'static str,
    pub systems: Vec<String>,
    pub matched_entries: usize,
}

#[derive(Debug, Error)]
pub enum FeaturedError {
    #[error("featured snapshot is invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported featured snapshot schema {0}")]
    Schema(u32),
    #[error("featured source {0} is duplicated or empty")]
    Source(String),
    #[error("featured title references unknown source {0}")]
    UnknownSource(String),
    #[error("featured collection references unknown browse entry {0}")]
    UnknownEntry(String),
    #[error("evidence-backed featured title has no catalogue entry: {0}")]
    UnmatchedTitle(String),
    #[error("featured snapshot declares {0} unmatched evidence title(s)")]
    UnmatchedCount(usize),
    #[error("featured snapshot declares {declared} entries but contains {actual}")]
    Count { declared: usize, actual: usize },
}

impl FeaturedCatalog {
    pub fn built_in(browse: &BrowseCatalog) -> Result<Self, FeaturedError> {
        let snapshot: FeaturedSnapshot = serde_json::from_str(FEATURED)?;
        if snapshot.schema_version != 1 {
            return Err(FeaturedError::Schema(snapshot.schema_version));
        }
        let mut source_ids = HashSet::new();
        for source in &snapshot.sources {
            if source.id.is_empty() || !source_ids.insert(source.id.as_str()) {
                return Err(FeaturedError::Source(source.id.clone()));
            }
        }
        let browse_ids: HashSet<&str> = browse
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        let mut entry_ids = HashSet::new();
        if snapshot.unmatched_title_count != 0 {
            return Err(FeaturedError::UnmatchedCount(
                snapshot.unmatched_title_count,
            ));
        }
        let titles = snapshot.titles;
        for title in &titles {
            if !source_ids.contains(title.source_id.as_str()) {
                return Err(FeaturedError::UnknownSource(title.source_id.clone()));
            }
            if title.matched_ids.is_empty() {
                return Err(FeaturedError::UnmatchedTitle(title.title.clone()));
            }
            for id in &title.matched_ids {
                if !browse_ids.contains(id.as_str()) {
                    return Err(FeaturedError::UnknownEntry(id.clone()));
                }
                entry_ids.insert(id.clone());
            }
        }
        if entry_ids.len() != snapshot.matched_entry_count {
            return Err(FeaturedError::Count {
                declared: snapshot.matched_entry_count,
                actual: entry_ids.len(),
            });
        }
        Ok(Self {
            entry_ids,
            sources: snapshot.sources,
            titles,
        })
    }

    pub fn audit_readiness(
        &self,
        browse: &BrowseCatalog,
        readiness: &ReadinessReport,
    ) -> FeaturedReadinessAudit {
        let by_id = browse
            .entries
            .iter()
            .map(|entry| (entry.id.as_str(), entry))
            .collect::<std::collections::HashMap<_, _>>();
        let mut audit = FeaturedReadinessAudit {
            evidence_title_records: self.titles.len(),
            unique_catalog_entries: self.entry_ids.len(),
            ready_now_titles: 0,
            emulator_missing_titles: 0,
            firmware_blocked_titles: 0,
            unresolved_titles: 0,
            sourced_artwork_titles: 0,
            attention: Vec::new(),
        };
        for title in &self.titles {
            let entries = title
                .matched_ids
                .iter()
                .filter_map(|id| by_id.get(id.as_str()).copied())
                .collect::<Vec<_>>();
            let has_sourced_artwork = entries
                .iter()
                .any(|entry| entry.artwork_url.is_some() || entry.artwork_asset.is_some());
            if has_sourced_artwork {
                audit.sourced_artwork_titles += 1;
            }
            let mut ready = false;
            let mut emulator_missing = false;
            let mut firmware_blocked = false;
            let mut systems = entries
                .iter()
                .map(|entry| entry.system.clone())
                .collect::<Vec<_>>();
            systems.sort();
            systems.dedup();
            for entry in &entries {
                let Some(system) = readiness.for_catalog_system(&entry.system) else {
                    continue;
                };
                let firmware_ready = matches!(
                    system.firmware,
                    FirmwareState::NotRequired | FirmwareState::AllRequiredPresent
                );
                match system.backend {
                    BackendState::ReadyNow if firmware_ready => ready = true,
                    BackendState::ReadyNow => firmware_blocked = true,
                    BackendState::EmulatorMissing => emulator_missing = true,
                    BackendState::Unresolved => {}
                }
            }
            let state = if ready {
                audit.ready_now_titles += 1;
                "ready_now"
            } else if emulator_missing {
                audit.emulator_missing_titles += 1;
                "emulator_missing"
            } else if firmware_blocked {
                audit.firmware_blocked_titles += 1;
                "firmware_blocked"
            } else {
                audit.unresolved_titles += 1;
                "unresolved"
            };
            if state != "ready_now" {
                audit.attention.push(FeaturedTitleReadiness {
                    title: title.title.clone(),
                    state,
                    systems,
                    matched_entries: entries.len(),
                });
            }
        }
        audit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn featured_snapshot_is_evidence_backed_and_resolves_to_games() {
        let browse = BrowseCatalog::built_in().unwrap();
        let featured = FeaturedCatalog::built_in(&browse).unwrap();
        assert_eq!(featured.sources.len(), 2);
        assert_eq!(featured.entry_ids.len(), 935);
        for title in [
            "Super Mario Bros.",
            "Sonic the Hedgehog",
            "Tetris",
            "Final Fantasy VII",
            "GoldenEye 007",
        ] {
            assert!(browse.entries.iter().any(|entry| {
                entry.title.eq_ignore_ascii_case(title) && featured.entry_ids.contains(&entry.id)
            }));
        }
    }
}
