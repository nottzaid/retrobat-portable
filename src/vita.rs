//! PS Vita games through Vita3K. RetroPort installs a `.vpk` package (a ZIP
//! of the app) into Vita3K's `ux0:app/<TITLE_ID>` itself and starts it by
//! title ID: the current Vita3K ignores a package passed on its command line.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::import::{ImportError, StagingDirectory, extract_archive};
use crate::paths::PortableLayout;

const INSTALLED: &str = ".retroport-installed";

#[derive(Debug, Error)]
pub enum VitaError {
    #[error("could not unpack the PS Vita package: {0}")]
    Unpack(#[from] ImportError),
    #[error("{0} is not a PS Vita package (it has no sce_sys/param.sfo naming a title ID)")]
    NotAPackage(PathBuf),
    #[error("could not install the PS Vita package: {0}")]
    Io(#[from] io::Error),
}

/// Vita3K's emulated memory card and system partitions. RetroBat's Vita3K
/// configuration uses the same folder, so both hosts share games and saves.
pub fn data_root(layout: &PortableLayout) -> PathBuf {
    layout
        .retrobat_root()
        .join("saves")
        .join("psvita")
        .join("vita3k")
}

/// Sony's system software, installed by Vita3K from PSVUPDAT.PUP.
pub fn system_software_installed(layout: &PortableLayout) -> bool {
    data_root(layout).join("vs0/vsh/shell/shell.self").is_file()
}

/// Sony's font package, installed by Vita3K from PSP2UPDAT.PUP.
pub fn fonts_installed(layout: &PortableLayout) -> bool {
    data_root(layout)
        .join("sa0/data/font/pvf/ltn0.pvf")
        .is_file()
}

/// Installs `package` once (again only when the package file changes) and
/// returns its title ID. An app installed some other way, such as through
/// Vita3K's own menu, is left exactly as it is.
pub fn install_package(layout: &PortableLayout, package: &Path) -> Result<String, VitaError> {
    let metadata = fs::metadata(package)?;
    let stamp = format!(
        "{} {}\n",
        metadata.len(),
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_nanos())
    );
    // The title ID of a package already installed unchanged is remembered,
    // so PLAY does not unpack the whole package again to read it.
    let remembered = layout.unpacked_disc(package).with_extension("vita-title");
    let apps = data_root(layout).join("ux0").join("app");
    if let Ok(record) = fs::read_to_string(&remembered)
        && let Some(title_id) = record.strip_prefix(&stamp)
        && fs::read_to_string(apps.join(title_id).join(INSTALLED))
            .is_ok_and(|marker| marker == stamp)
    {
        return Ok(title_id.to_owned());
    }
    let stage = StagingDirectory::new(layout, "vita-package")?;
    let unpacked = stage.path().join("app");
    fs::create_dir_all(&unpacked)?;
    extract_archive(layout, package, &unpacked)?;
    let sfo = fs::read(unpacked.join("sce_sys").join("param.sfo"))
        .map_err(|_| VitaError::NotAPackage(package.to_owned()))?;
    let title_id = title_id(&sfo).ok_or_else(|| VitaError::NotAPackage(package.to_owned()))?;
    let app = apps.join(&title_id);
    match fs::read_to_string(app.join(INSTALLED)) {
        Ok(recorded) if recorded == stamp => {}
        Ok(_) => {
            fs::remove_dir_all(&app)?;
            install_unpacked(&unpacked, &apps, &app, &stamp)?;
        }
        Err(_) if app.exists() => return Ok(title_id),
        Err(_) => install_unpacked(&unpacked, &apps, &app, &stamp)?,
    }
    if let Some(parent) = remembered.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&remembered, format!("{stamp}{title_id}"))?;
    Ok(title_id)
}

/// Removes the app RetroPort installed for `package` (identified by its
/// marker), leaving saves, which Vita3K keeps elsewhere, and any app
/// installed another way.
pub fn uninstall_package(layout: &PortableLayout, package: &Path) -> io::Result<()> {
    let remembered = layout.unpacked_disc(package).with_extension("vita-title");
    let record = match fs::read_to_string(&remembered) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if let Some((stamp, title_id)) = record.split_once('\n')
        && title_id.len() == 9
        && title_id.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        let app = data_root(layout).join("ux0").join("app").join(title_id);
        if fs::read_to_string(app.join(INSTALLED))
            .is_ok_and(|marker| marker == format!("{stamp}\n"))
        {
            fs::remove_dir_all(app)?;
        }
    }
    fs::remove_file(remembered)
}

fn install_unpacked(unpacked: &Path, apps: &Path, app: &Path, stamp: &str) -> io::Result<()> {
    fs::create_dir_all(apps)?;
    fs::write(unpacked.join(INSTALLED), stamp)?;
    fs::rename(unpacked, app)
}

/// The TITLE_ID entry of a PARAM.SFO (PSF) table, if it is a plausible
/// nine-character ID such as `GRZB00002`.
fn title_id(sfo: &[u8]) -> Option<String> {
    let word = |offset: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            sfo.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let half = |offset: usize| -> Option<u16> {
        Some(u16::from_le_bytes(
            sfo.get(offset..offset + 2)?.try_into().ok()?,
        ))
    };
    if sfo.get(..4)? != b"\0PSF" {
        return None;
    }
    let keys = word(8)? as usize;
    let values = word(12)? as usize;
    let count = word(16)? as usize;
    for index in 0..count.min(256) {
        let entry = 20 + 16 * index;
        let key_offset = keys + usize::from(half(entry)?);
        let key_end = key_offset + sfo.get(key_offset..)?.iter().position(|&byte| byte == 0)?;
        if &sfo[key_offset..key_end] != b"TITLE_ID" {
            continue;
        }
        let length = word(entry + 4)? as usize;
        let value_offset = values + word(entry + 12)? as usize;
        let value = sfo.get(value_offset..value_offset + length)?;
        let value = String::from_utf8(
            value
                .iter()
                .copied()
                .take_while(|&byte| byte != 0)
                .collect(),
        )
        .ok()?;
        return (value.len() == 9 && value.bytes().all(|byte| byte.is_ascii_alphanumeric()))
            .then_some(value);
    }
    None
}
