use retrobat_portable::artwork::load_or_fetch;
use retrobat_portable::catalog::Catalog;
use retrobat_portable::install::{Installer, ReqwestDownloader};
use retrobat_portable::paths::PortableLayout;
use retrobat_portable::sources::homebrew_hub::HomebrewHubEntry;
use tempfile::tempdir;

#[test]
#[ignore = "requires live network access"]
fn upstream_metadata_and_artifact_match_the_audited_catalog() {
    let entry = Catalog::built_in().unwrap().entries.remove(0);
    let upstream = HomebrewHubEntry::fetch("2048gb").unwrap();
    upstream.audit_against(&entry).unwrap();

    let temp = tempdir().unwrap();
    let layout = PortableLayout::new(temp.path());
    let downloader = ReqwestDownloader::new().unwrap();
    let artwork = load_or_fetch(&layout, &entry.artwork[0], &downloader).unwrap();
    let decoded = image::load_from_memory(&artwork).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (320, 288));

    let installer = Installer::new(&layout, &downloader);
    let report = installer.install(&entry).unwrap();

    assert_eq!(report.bytes, entry.artifact.size);
    assert_eq!(report.sha256, entry.artifact.sha256);
    assert_eq!(
        std::fs::read(&report.destination).unwrap().len() as u64,
        entry.artifact.size
    );

    let uninstall = installer.uninstall(&entry).unwrap();
    assert_eq!(uninstall.removed, vec![report.destination]);
}
