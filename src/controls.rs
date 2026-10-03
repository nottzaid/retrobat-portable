use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::browse::{BrowseCatalog, BrowseEntry};
use crate::import::{ImportedManifest, canonical_system_alias};
use crate::launch::{HostPlatform, retroarch_input_overrides, uses_direct_retroarch};
use crate::paths::PortableLayout;
use crate::readiness::ReadinessReport;

const CONTROLS: &[u8] = include_bytes!("../catalog/controls-v1.json.gz");

#[derive(Clone, Debug, Deserialize)]
struct ControlsSnapshot {
    schema_version: u32,
    sources: Vec<ControlSource>,
    mame_catalog_entries: usize,
    mame_profiles: HashMap<String, MameProfile>,
    #[serde(default)]
    retrobat_profiles: HashMap<String, Vec<SpecialDevice>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlSource {
    pub name: String,
    pub version: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct MameProfile {
    variants: Vec<MameVariant>,
}

#[derive(Clone, Debug, Deserialize)]
struct MameVariant {
    machine: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    players: u16,
    #[serde(default)]
    coins: u16,
    #[serde(default)]
    service: bool,
    #[serde(default)]
    tilt: bool,
    #[serde(default)]
    controls: Vec<HashMap<String, String>>,
    #[serde(default)]
    special_devices: Vec<SpecialDevice>,
}

#[derive(Clone, Debug, Deserialize)]
struct SpecialDevice {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    attributes: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct ControlsCatalog {
    snapshot: ControlsSnapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct ControlsCoverage {
    pub total_entries: usize,
    pub controls_button_entries: usize,
    pub mame_catalog_profile_entries: usize,
    pub exact_current_mame_input_entries: usize,
    pub installed_system_profile_entries: usize,
    pub exact_retrobat_game_entries: usize,
    pub missing_entries: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GameControls {
    pub title: String,
    pub scope: String,
    pub confidence: String,
    pub device_summary: Vec<String>,
    pub keyboard: Vec<ControlBinding>,
    pub controller: Vec<ControlBinding>,
    pub notes: Vec<String>,
    pub sources: Vec<ControlSource>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlBinding {
    pub input: String,
    pub function: String,
}

#[derive(Debug, Error)]
pub enum ControlsError {
    #[error("controls snapshot is invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("controls snapshot could not be decompressed: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported controls snapshot schema {0}")]
    Schema(u32),
    #[error("controls snapshot maps {actual} MAME entries but declares {declared}")]
    MameCount { declared: usize, actual: usize },
}

impl ControlsCatalog {
    pub fn built_in() -> Result<Self, ControlsError> {
        let mut decoder = GzDecoder::new(CONTROLS);
        let mut decoded = String::new();
        decoder.read_to_string(&mut decoded)?;
        let snapshot: ControlsSnapshot = serde_json::from_str(&decoded)?;
        if snapshot.schema_version != 1 {
            return Err(ControlsError::Schema(snapshot.schema_version));
        }
        if snapshot.mame_profiles.len() != snapshot.mame_catalog_entries {
            return Err(ControlsError::MameCount {
                declared: snapshot.mame_catalog_entries,
                actual: snapshot.mame_profiles.len(),
            });
        }
        Ok(Self { snapshot })
    }

    pub fn audit_coverage(&self, browse: &BrowseCatalog) -> ControlsCoverage {
        let mame_catalog_profile_entries = browse
            .entries
            .iter()
            .filter(|entry| self.snapshot.mame_profiles.contains_key(&entry.id))
            .count();
        let exact_current_mame_input_entries = browse
            .entries
            .iter()
            .filter(|entry| {
                self.snapshot
                    .mame_profiles
                    .get(&entry.id)
                    .is_some_and(|profile| {
                        profile
                            .variants
                            .iter()
                            .any(|variant| !variant.description.is_empty())
                    })
            })
            .count();
        let missing_entries = browse
            .entries
            .iter()
            .filter(|entry| entry.system.trim().is_empty())
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        ControlsCoverage {
            total_entries: browse.entries.len(),
            controls_button_entries: browse.entries.len() - missing_entries.len(),
            mame_catalog_profile_entries,
            exact_current_mame_input_entries,
            installed_system_profile_entries: browse.entries.len() - mame_catalog_profile_entries,
            exact_retrobat_game_entries: browse
                .entries
                .iter()
                .filter(|entry| self.snapshot.retrobat_profiles.contains_key(&entry.id))
                .count(),
            missing_entries,
        }
    }

    pub fn for_game(
        &self,
        layout: &PortableLayout,
        entry: &BrowseEntry,
        imported: Option<&ImportedManifest>,
        readiness: Option<&ReadinessReport>,
    ) -> GameControls {
        // The RetroBat system and route PLAY would use for this card.
        let system_readiness =
            readiness.and_then(|report| report.for_catalog_system(&entry.system));
        let retrobat_system = imported
            .map(|manifest| manifest.system.clone())
            .or_else(|| {
                system_readiness.and_then(|system| system.retrobat_systems.first().cloned())
            })
            .unwrap_or_else(|| {
                canonical_system_alias(&entry.system)
                    .unwrap_or(&entry.system)
                    .to_owned()
            });
        let route = system_readiness.and_then(|system| system.ready_route.as_ref());
        let direct = uses_direct_retroarch(
            HostPlatform::current(),
            &retrobat_system,
            route.map(|route| route.emulator.as_str()),
        );
        let config = effective_retroarch_config(layout, &retrobat_system, direct);
        let mut keyboard = keyboard_bindings(&config);
        let controller_device = controller_profile(layout);
        let mut controller = controller_bindings(controller_device.as_ref());
        let mut sources = vec![ControlSource {
            name: if direct {
                "Installed RetroArch configuration with RetroPort's launch overrides".to_owned()
            } else {
                "Installed RetroArch/RetroBat input configuration".to_owned()
            },
            version: layout
                .retroarch_root()
                .join("retroarch.cfg")
                .display()
                .to_string(),
            url: "https://docs.libretro.com/guides/input-and-controls/".to_owned(),
        }];
        let mut notes = Vec::new();
        let mut device_summary = vec![format!(
            "Backend system: {}{} · virtual input surface: RetroPad",
            retrobat_system.to_ascii_uppercase(),
            route
                .map(|route| format!(" through {}", route.label()))
                .unwrap_or_default()
        )];
        let mut scope = "Installed system/backend profile".to_owned();
        let mut confidence =
            "Verified input surface; game action labels are supplied by the running core"
                .to_owned();

        if let Some(profile) = self.snapshot.mame_profiles.get(&entry.id) {
            let launch_machine = imported
                .and_then(|manifest| manifest.launch_relative_path.file_stem())
                .and_then(|stem| stem.to_str());
            let selected = launch_machine
                .and_then(|machine| profile.variants.iter().find(|item| item.machine == machine))
                .or_else(|| {
                    profile
                        .variants
                        .iter()
                        .find(|item| !item.description.is_empty())
                })
                .or_else(|| profile.variants.first());
            if let Some(machine) = selected {
                let current_mame_declaration = !machine.description.is_empty();
                scope = if launch_machine == Some(machine.machine.as_str()) {
                    "Exact imported MAME machine".to_owned()
                } else if current_mame_declaration {
                    "MAME machine metadata for the catalogue edition".to_owned()
                } else {
                    "Libretro MAME DAT machine association".to_owned()
                };
                confidence = if current_mame_declaration {
                    "Machine-declared controls from the pinned MAME -listxml release".to_owned()
                } else {
                    "Exact catalogue-to-machine association; the pinned current MAME release has no declaration for this historical machine name".to_owned()
                };
                device_summary.clear();
                if current_mame_declaration {
                    device_summary.push(format!(
                        "{} ({}) · {} player(s) · {} coin slot(s)",
                        machine.description, machine.machine, machine.players, machine.coins
                    ));
                    for control in &machine.controls {
                        let kind = control.get("type").map(String::as_str).unwrap_or("control");
                        let player = control.get("player").map(String::as_str).unwrap_or("1");
                        let ways = control
                            .get("ways")
                            .map(|ways| format!(", {ways}-way"))
                            .unwrap_or_default();
                        let buttons = control
                            .get("buttons")
                            .map(|buttons| format!(", {buttons} button(s)"))
                            .unwrap_or_default();
                        device_summary.push(format!("Player {player}: {kind}{ways}{buttons}"));
                    }
                } else {
                    device_summary.push(format!(
                        "{} ({}) · historical DAT association; no input declaration in the pinned current MAME release",
                        entry.title, machine.machine
                    ));
                }
                for device in &machine.special_devices {
                    let detail = if device.attributes.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " ({})",
                            device
                                .attributes
                                .iter()
                                .map(|(key, value)| format!("{key}={value}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    device_summary.push(format!("Special device: {}{detail}", device.kind));
                }
                if machine.coins > 0 {
                    relabel_binding(&mut keyboard, "Select / Back", "Insert coin");
                    relabel_binding(&mut keyboard, "Start", "Start player 1");
                    relabel_binding(&mut controller, "Select / Back", "Insert coin");
                    relabel_binding(&mut controller, "Start", "Start player 1");
                }
                if machine.service {
                    notes.push("This machine declares a service input.".to_owned());
                }
                if machine.tilt {
                    notes.push("This machine declares a cabinet tilt input.".to_owned());
                }
                if profile.variants.len() > 1 && launch_machine.is_none() {
                    notes.push(format!(
                        "This catalogue title has {} MAME variants; importing a ROM selects its exact machine profile.",
                        profile.variants.len()
                    ));
                }
                sources.extend(self.snapshot.sources.clone());
            }
        } else {
            notes.push(
                "Exact action names may change by game. While running a Libretro title, Quick Menu → Controls shows the core-declared per-game mapping."
                    .to_owned(),
            );
        }
        if let Some(devices) = self.snapshot.retrobat_profiles.get(&entry.id) {
            for device in devices {
                let detail = if device.attributes.is_empty() {
                    String::new()
                } else {
                    format!(
                        " ({})",
                        device
                            .attributes
                            .iter()
                            .map(|(key, value)| format!("{key}={value}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let line = format!("RetroBat game profile: {}{detail}", device.kind);
                if !device_summary.contains(&line) {
                    device_summary.push(line);
                }
            }
            sources.push(ControlSource {
                name: "RetroBat per-game input metadata".to_owned(),
                version: layout
                    .emulationstation_root()
                    .join("resources/gamesdb.xml")
                    .display()
                    .to_string(),
                url: "https://wiki.retrobat.org/controllers".to_owned(),
            });
            if !scope.starts_with("Exact imported MAME") {
                scope = "Exact RetroBat game profile plus installed backend mapping".to_owned();
                confidence =
                    "Game-specific device metadata from the installed RetroBat database".to_owned();
            }
        }
        if let Some(device) = controller_device {
            notes.push(format!("Connected controller profile: {}", device.name));
        } else {
            notes.push(
                "No controller profile is currently detected; keyboard bindings remain available."
                    .to_owned(),
            );
        }
        if direct {
            notes.push("Esc closes the game; F1 opens RetroArch's Quick Menu.".to_owned());
        } else {
            notes.push(
                "This system starts through its own emulator; TERMINATE on the card always stops it."
                    .to_owned(),
            );
        }
        GameControls {
            title: entry.title.clone(),
            scope,
            confidence,
            device_summary,
            keyboard,
            controller,
            notes,
            sources,
        }
    }
}

fn relabel_binding(bindings: &mut [ControlBinding], from: &str, to: &str) {
    if let Some(binding) = bindings.iter_mut().find(|binding| binding.function == from) {
        binding.function = to.to_owned();
    }
}

#[derive(Clone, Debug)]
struct ControllerProfile {
    name: String,
    values: HashMap<String, String>,
}

fn parse_config(path: &Path) -> HashMap<String, String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((
                key.trim().to_owned(),
                value.trim().trim_matches('"').to_owned(),
            ))
        })
        .collect()
}

/// The keyboard and pad binds a launch of `system` runs with: the shared
/// RetroArch configuration, plus the overrides PLAY appends on direct routes.
fn effective_retroarch_config(
    layout: &PortableLayout,
    system: &str,
    direct: bool,
) -> HashMap<String, String> {
    let mut values = parse_config(&layout.retroarch_root().join("retroarch.cfg"));
    if direct {
        values.extend(
            retroarch_input_overrides(system)
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned())),
        );
    }
    values
}

fn keyboard_bindings(values: &HashMap<String, String>) -> Vec<ControlBinding> {
    const INPUTS: [(&str, &str); 10] = [
        ("up", "D-pad up"),
        ("down", "D-pad down"),
        ("left", "D-pad left"),
        ("right", "D-pad right"),
        ("a", "RetroPad A"),
        ("b", "RetroPad B"),
        ("x", "RetroPad X"),
        ("y", "RetroPad Y"),
        ("start", "Start"),
        ("select", "Select / Back"),
    ];
    INPUTS
        .iter()
        .filter_map(|(key, function)| {
            let value = values.get(&format!("input_player1_{key}"))?;
            (value != "nul").then(|| ControlBinding {
                input: display_key(value),
                function: (*function).to_owned(),
            })
        })
        .collect()
}

fn display_key(value: &str) -> String {
    match value {
        "num1" => "1".to_owned(),
        "num5" => "5".to_owned(),
        other => other.replace('_', " ").to_ascii_uppercase(),
    }
}

fn controller_bindings(profile: Option<&ControllerProfile>) -> Vec<ControlBinding> {
    const INPUTS: [(&str, &str); 10] = [
        ("up", "D-pad up"),
        ("down", "D-pad down"),
        ("left", "D-pad left"),
        ("right", "D-pad right"),
        ("a", "RetroPad A"),
        ("b", "RetroPad B"),
        ("x", "RetroPad X"),
        ("y", "RetroPad Y"),
        ("start", "Start"),
        ("select", "Select / Back"),
    ];
    INPUTS
        .iter()
        .map(|(key, function)| {
            let label = profile
                .and_then(|profile| profile.values.get(&format!("input_{key}_btn_label")))
                .cloned()
                .unwrap_or_else(|| function.to_string());
            ControlBinding {
                input: label,
                function: (*function).to_owned(),
            }
        })
        .collect()
}

fn controller_profile(layout: &PortableLayout) -> Option<ControllerProfile> {
    let (name, vendor, product) = connected_controller()?;
    let directory = layout.retroarch_root().join("autoconfig/sdl2");
    let entries = fs::read_dir(directory).ok()?;
    let mut fallback = None;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("cfg") {
            continue;
        }
        let values = parse_config(&path);
        let ids_match = values.get("input_vendor_id") == Some(&vendor)
            && values.get("input_product_id") == Some(&product);
        let names_match = values
            .get("input_device")
            .is_some_and(|device| name.contains(device) || device.contains(&name));
        let profile = ControllerProfile {
            name: values
                .get("input_device_display_name")
                .cloned()
                .unwrap_or_else(|| name.clone()),
            values,
        };
        if ids_match {
            return Some(profile);
        }
        if names_match {
            fallback = Some(profile);
        }
    }
    fallback.or_else(|| {
        Some(ControllerProfile {
            name,
            values: HashMap::new(),
        })
    })
}

#[cfg(target_os = "linux")]
fn connected_controller() -> Option<(String, String, String)> {
    let inputs = fs::read_dir("/sys/class/input").ok()?;
    for entry in inputs.filter_map(Result::ok) {
        if !entry.file_name().to_string_lossy().starts_with("js") {
            continue;
        }
        let device = entry.path().join("device");
        let name = fs::read_to_string(device.join("name"))
            .ok()?
            .trim()
            .to_owned();
        let vendor = u16::from_str_radix(
            fs::read_to_string(device.join("id/vendor")).ok()?.trim(),
            16,
        )
        .ok()?;
        let product = u16::from_str_radix(
            fs::read_to_string(device.join("id/product")).ok()?.trim(),
            16,
        )
        .ok()?;
        return Some((name, vendor.to_string(), product.to_string()));
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn connected_controller() -> Option<(String, String, String)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_browse_entry_has_a_controls_provider() {
        let browse = BrowseCatalog::built_in().unwrap();
        let controls = ControlsCatalog::built_in().unwrap();
        let coverage = controls.audit_coverage(&browse);
        assert_eq!(coverage.total_entries, 80_734);
        assert_eq!(coverage.controls_button_entries, coverage.total_entries);
        assert!(coverage.missing_entries.is_empty());
        assert_eq!(coverage.mame_catalog_profile_entries, 15_605);
        assert_eq!(coverage.exact_current_mame_input_entries, 14_845);
    }

    #[test]
    fn ms_pac_man_uses_exact_mame_machine_metadata() {
        let browse = BrowseCatalog::built_in().unwrap();
        let entry = browse
            .entries
            .iter()
            .find(|entry| entry.id == "libretro-classics/mame-ms-pac-man-408fe55e438d")
            .unwrap();
        let controls = ControlsCatalog::built_in().unwrap();
        let layout = PortableLayout::new("/tmp/unused");
        let profile = controls.for_game(&layout, entry, None, None);
        assert!(profile.scope.contains("MAME"));
        assert!(
            profile
                .device_summary
                .iter()
                .any(|line| line.contains("4-way"))
        );
        assert!(
            profile
                .sources
                .iter()
                .any(|source| source.name == "MAME -listxml")
        );
    }
}
