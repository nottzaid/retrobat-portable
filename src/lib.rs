pub mod artwork;
pub mod browse;
pub mod browse_install;
pub mod catalog;
pub mod controller_guard;
pub mod controls;
pub mod featured;
pub mod firmware;
pub mod import;
pub mod install;
pub mod launch;
pub mod paths;
pub mod readiness;
pub mod session;
pub mod sources;

use serde::Serialize;
use std::collections::HashSet;
use thiserror::Error;

use crate::{
    artwork::{BundledArtworkAudit, audit_bundled_artwork},
    browse::BrowseCatalog,
    browse_install::{DownloadCoverage, audit_download_coverage},
    catalog::Catalog,
    controls::{ControlsCatalog, ControlsCoverage},
    featured::{FeaturedCatalog, FeaturedReadinessAudit},
    import::{GameImporter, ImportCoverage},
    launch::HostPlatform,
    paths::PortableLayout,
    readiness::ReadinessReport,
};

#[derive(Debug, Serialize)]
pub struct SelfCheck {
    pub catalog_entries: usize,
    pub browse_sources: usize,
    pub browse_entries: usize,
    pub browse_artwork_entries: usize,
    pub browse_generated_artwork_entries: usize,
    pub browse_visual_artwork_entries: usize,
    pub bundled_artwork: BundledArtworkAudit,
    pub catalog_valid: bool,
    pub bundle_root: String,
    pub retrobat_present: bool,
    pub emulator_launcher_present: bool,
    pub chip8_core_present: bool,
    pub import_coverage: Option<ImportCoverage>,
    pub download_coverage: DownloadCoverage,
    pub readiness: Option<ReadinessReport>,
    pub featured_readiness: Option<FeaturedReadinessAudit>,
    pub controls_coverage: ControlsCoverage,
    pub target_platform: &'static str,
}

#[derive(Debug, Error)]
pub enum SelfCheckError {
    #[error(transparent)]
    Catalog(#[from] catalog::CatalogError),
    #[error(transparent)]
    Browse(#[from] browse::BrowseError),
    #[error(transparent)]
    Import(#[from] import::ImportError),
    #[error(transparent)]
    Readiness(#[from] readiness::ReadinessError),
    #[error(transparent)]
    Featured(#[from] featured::FeaturedError),
    #[error(transparent)]
    Controls(#[from] controls::ControlsError),
}

pub fn self_check(layout: &PortableLayout) -> Result<SelfCheck, SelfCheckError> {
    let catalog = Catalog::built_in()?;
    let browse = BrowseCatalog::built_in()?;
    let host = HostPlatform::current();
    let trusted_ids = catalog
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<HashSet<_>>();
    let download_coverage = audit_download_coverage(&browse.entries, &trusted_ids);
    let import_coverage = if layout.systems_config().is_file() {
        Some(GameImporter::new(layout).audit_coverage(&browse.entries)?)
    } else {
        None
    };
    let readiness = if layout.systems_config().is_file() {
        Some(ReadinessReport::audit(layout, &browse.entries)?)
    } else {
        None
    };
    let bundled_artwork = audit_bundled_artwork(layout, &browse.entries);
    let featured = FeaturedCatalog::built_in(&browse)?;
    let controls_coverage = ControlsCatalog::built_in()?.audit_coverage(&browse);
    let featured_readiness = readiness
        .as_ref()
        .map(|report| featured.audit_readiness(&browse, report));
    let sourced_artwork_entries = browse
        .entries
        .iter()
        .filter(|entry| entry.artwork_url.is_some() || entry.artwork_asset.is_some())
        .count();
    Ok(SelfCheck {
        catalog_entries: catalog.entries.len(),
        browse_sources: browse.sources.len(),
        browse_entries: browse.entries.len(),
        browse_artwork_entries: sourced_artwork_entries,
        browse_generated_artwork_entries: browse.entries.len() - sourced_artwork_entries,
        // Every card without sourced imagery is rendered by the deterministic
        // title/system cover path in the GUI; there is no blank-art branch.
        browse_visual_artwork_entries: browse.entries.len(),
        bundled_artwork,
        catalog_valid: true,
        bundle_root: layout.root.display().to_string(),
        retrobat_present: layout.retrobat_executable().is_file(),
        emulator_launcher_present: layout.emulator_launcher_executable().is_file(),
        chip8_core_present: layout.retroarch_core("jaxe").is_file(),
        import_coverage,
        download_coverage,
        readiness,
        featured_readiness,
        controls_coverage,
        target_platform: host.as_str(),
    })
}
