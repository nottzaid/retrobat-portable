use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct PortableLayout {
    pub root: PathBuf,
}

impl PortableLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn from_executable(executable: &Path) -> Self {
        Self::new(
            executable
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )
    }

    pub fn discover(executable: &Path) -> Self {
        let fallback = Self::from_executable(executable);
        let mut candidates = Vec::new();
        if let Some(parent) = executable.parent() {
            candidates.extend(parent.ancestors().take(6).map(Path::to_path_buf));
        }

        candidates
            .into_iter()
            .find(|root| Self::new(root).is_complete_bundle())
            .map(Self::new)
            .unwrap_or(fallback)
    }

    pub fn is_complete_bundle(&self) -> bool {
        self.retrobat_executable().is_file()
            && self.emulator_launcher_executable().is_file()
            && self.systems_config().is_file()
    }

    pub fn retrobat_root(&self) -> PathBuf {
        self.root.join("RetroBat")
    }

    pub fn retrobat_executable(&self) -> PathBuf {
        self.retrobat_root().join("RetroBat.exe")
    }

    pub fn emulationstation_root(&self) -> PathBuf {
        self.retrobat_root().join("emulationstation")
    }

    pub fn emulator_launcher_executable(&self) -> PathBuf {
        self.emulationstation_root().join("emulatorLauncher.exe")
    }

    pub fn retroarch_root(&self) -> PathBuf {
        self.emulator_root("retroarch")
    }

    pub fn emulator_root(&self, emulator: &str) -> PathBuf {
        self.retrobat_root().join("emulators").join(emulator)
    }

    pub fn linux_runtime_root(&self) -> PathBuf {
        self.root.join("Runtime").join("Linux")
    }

    pub fn rpcs3_executable(&self) -> PathBuf {
        self.emulator_root("rpcs3").join("rpcs3.exe")
    }

    pub fn retroarch_executable(&self) -> PathBuf {
        self.retroarch_root().join("retroarch.exe")
    }

    pub fn retroarch_core(&self, core: &str) -> PathBuf {
        self.retroarch_root()
            .join("cores")
            .join(format!("{core}_libretro.dll"))
    }

    pub fn retroarch_core_info(&self, core: &str) -> PathBuf {
        self.retroarch_root()
            .join("info")
            .join(format!("{core}_libretro.info"))
    }

    pub fn metadata_root(&self) -> PathBuf {
        self.root.join(".retrobat-portable")
    }

    pub fn installed_root(&self) -> PathBuf {
        self.metadata_root().join("installed")
    }

    pub fn staging_root(&self) -> PathBuf {
        self.metadata_root().join("staging")
    }

    pub fn imported_root(&self) -> PathBuf {
        self.metadata_root().join("imported")
    }

    /// Where an imported disc image is unpacked for an emulator that cannot
    /// read its format (an Xbox image for Cxbx-Reloaded, a gzip-compressed
    /// PS2 image for Play!). It is derived from the image's path, so REMOVE
    /// finds it again.
    pub fn unpacked_disc(&self, image: &Path) -> PathBuf {
        use sha2::{Digest, Sha256};
        let relative = image.strip_prefix(&self.root).unwrap_or(image);
        let digest = Sha256::digest(relative.to_string_lossy().as_bytes());
        self.metadata_root()
            .join("cache")
            .join("unpacked-discs")
            .join(hex::encode(&digest[..12]))
    }

    pub fn systems_config(&self) -> PathBuf {
        self.retrobat_root()
            .join("emulationstation")
            .join(".emulationstation")
            .join("es_systems.cfg")
    }

    pub fn bios_catalog(&self) -> PathBuf {
        self.retrobat_root()
            .join("system")
            .join("modules")
            .join("rb_gui")
            .join("bios_local.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_walks_up_from_a_nested_development_binary() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("portable");
        let executable = bundle.join("target/release/RetroPort-Linux");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(bundle.join("RetroBat/emulationstation/.emulationstation"))
            .unwrap();
        std::fs::write(bundle.join("RetroBat/RetroBat.exe"), b"exe").unwrap();
        std::fs::write(
            bundle.join("RetroBat/emulationstation/emulatorLauncher.exe"),
            b"exe",
        )
        .unwrap();
        std::fs::write(
            bundle.join("RetroBat/emulationstation/.emulationstation/es_systems.cfg"),
            b"config",
        )
        .unwrap();

        assert_eq!(PortableLayout::discover(&executable).root, bundle);
    }

    #[test]
    fn discovery_never_uses_an_unrelated_sibling_bundle() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("source/target/release/RetroPort-Linux");
        let unrelated = root.path().join("unrelated-installation");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(unrelated.join("RetroBat/emulationstation/.emulationstation"))
            .unwrap();
        std::fs::write(unrelated.join("RetroBat/RetroBat.exe"), b"exe").unwrap();
        std::fs::write(
            unrelated.join("RetroBat/emulationstation/emulatorLauncher.exe"),
            b"exe",
        )
        .unwrap();
        std::fs::write(
            unrelated.join("RetroBat/emulationstation/.emulationstation/es_systems.cfg"),
            b"config",
        )
        .unwrap();

        assert_eq!(
            PortableLayout::discover(&executable).root,
            executable.parent().unwrap()
        );
    }
}
