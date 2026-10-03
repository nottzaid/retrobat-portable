//! Functional tests: the real binary, a real installation, real games.
//!
//! Run with `tools/functional_test.sh`, which prepares Xvfb, an isolated
//! Wine prefix, and the release build. Every test here downloads from the
//! real publishers, starts real emulators, and inspects what they show.

mod support;

use std::fs;
use std::path::Path;

use retrobat_portable::firmware::{firmware_record, import_firmware};
use retrobat_portable::paths::PortableLayout;
use retrobat_portable::readiness::{FirmwareState, ReadinessReport};
use support::{Bundle, Game, extract, fetch, fetch_with, play, play_windows};

const FUNCTIONAL: &str = "functional: run tools/functional_test.sh";

const PS1_240P: Game = Game {
    url: "https://github.com/filipalac/240pTestSuite-PS1/releases/download/19122020/240pTestSuitePS1-EMU.zip",
    sha256: "63f64dcd61bd35d277f9164d17563afc2de8e1adcd316a8e0a45ea753bfbf542",
};
const SATURN_ISO: Game = Game {
    url: "https://github.com/slinga-homebrew/Save-Game-Copier/releases/download/3.6.18/game.iso",
    sha256: "ccc4730f37c7be9db8e1bed06d555bbc6713839af06ec034cc608d2a31504ebe",
};
const SATURN_CUE: Game = Game {
    url: "https://github.com/slinga-homebrew/Save-Game-Copier/releases/download/3.6.18/game.cue",
    sha256: "dd22ff63186cc0854add65a5c533587a22b384309b9b6ab53980675b8b185607",
};
const N64_SBLOBBER: Game = Game {
    url: "https://github.com/vrgl117-games/sblobber64/releases/download/n64brew-jam-1/sblobber64.z64",
    sha256: "9ca33f811658b79d2a94fab3790fee955a389eb8c16c8bab2bed7cceb4df90c5",
};
const NDS_JAMCLOWN: Game = Game {
    url: "https://github.com/lorenzolanglois/JamClown/releases/download/v1.0/jamclown.nds",
    sha256: "21c020cb71997de4cb487d65f0a301679bfb3d4855baa49c36ee72d17d866ccd",
};
const N3DS_FLAPPY: Game = Game {
    url: "https://github.com/MillKeny/flappy/releases/download/v1.1/flappy.3dsx",
    sha256: "3a71de98b90a17ff219a4e3474df06103b4179ef261d4d681fc752ee75e46fc4",
};
const PSP_JOKER_POKER: Game = Game {
    url: "https://github.com/kwerenta/joker-poker/releases/download/v0.40/joker-poker.zip",
    sha256: "76c5aad44c5a89cdf77c4d4f0c3bd565615d2ba65afdddfd3edb97d789cffbc3",
};
const SUDOKUL_GAMECUBE: Game = Game {
    url: "https://github.com/Mode8fx/SuDokuL/releases/download/v1.5/SuDokuL-v1.5-gamecube.zip",
    sha256: "9e3975dea71f656b26bf661cb64e7f0cba581c502afdc7e5e298b7c587dea9f1",
};
const SUDOKUL_WII: Game = Game {
    url: "https://github.com/Mode8fx/SuDokuL/releases/download/v1.5/SuDokuL-v1.5-wii.zip",
    sha256: "1e6351db1c0b031499df1bae2c123e7300180ea3d32924d874b931c5ce88f6de",
};
const SUDOKUL_WIIU: Game = Game {
    url: "https://github.com/Mode8fx/SuDokuL/releases/download/v1.5/SuDokuL-v1.5-wiiu.zip",
    sha256: "c3b19f129866d4f5a4551e46f848abe0c3f00ef769b13e68388098a7a34d983c",
};
const SUDOKUL_WINDOWS: Game = Game {
    url: "https://github.com/Mode8fx/SuDokuL/releases/download/v1.5/SuDokuL-v1.5-x64.zip",
    sha256: "9c12f2485e03c1bb29f274eaa697a9d2ac4757d38870fe117a2552a53c12695a",
};

// Catalogue cards used as import targets (any card of the system works:
// import checks compatibility, not one canonical dump).
const PSX_CARD: &str = "libretro-classics/psx-98-koushien-koukou-yakyuu-simulation-22d77fa0fbda";
const SATURN_CARD: &str = "libretro-classics/saturn-2do-aru-koto-wa-sando-r-e3f27087dfb5";
const N64_CARD: &str = "libretro-classics/n64-007-the-world-is-not-enough-464fa19d8838";
const NDS_CARD: &str = "libretro-classics/nds-007-blood-stone-ce2a18f6f065";
const N3DS_CARD: &str =
    "libretro-classics/3ds-100-pascal-sensei-kanpeki-paint-bombers-732c4f0dad38";
const PSP_CARD: &str = "libretro-classics/psp-8-6fdaf0ec77da";
const GAMECUBE_CARD: &str = "libretro-classics/gamecube-007-agent-im-kreuzfeuer-2f65756414f5";
const WII_CARD: &str = "libretro-classics/wii-1-000-000-pyramid-the-41f98340b5c9";
const WIIU_CARD: &str = "libretro-classics/wiiu-1080-teneighty-snowboarding-europe-n64-virtual-console-eshop-974ee8170483";
const WINDOWS_CARD: &str = "iconic-evidence/age-of-empires-c2b783f8b190";
const MAME_CARD: &str = "libretro-classics/mame-ms-pac-man-408fe55e438d";

fn path(value: &Path) -> &str {
    value.to_str().unwrap()
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn self_check_accounts_for_the_whole_installation() {
    let _ = FUNCTIONAL;
    let report = Bundle::fresh("self-check").self_check();
    assert_eq!(report["browse_entries"], 80_734);
    assert_eq!(report["browse_sources"], 10);
    let artwork = &report["bundled_artwork"];
    assert_eq!(artwork["declared_assets"], artwork["verified_assets"]);
    assert_eq!(artwork["failed_assets"], 0);
    let downloads = &report["download_coverage"];
    assert_eq!(downloads["total_entries"], 4_153);
    assert!(downloads["verified_entries"].as_u64().unwrap() >= 4_100);
    assert!(
        downloads["unrecorded_entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(report["import_coverage"]["covered_entries"], 80_734);
    assert_eq!(
        report["controls_coverage"]["controls_button_entries"],
        80_734
    );
    // A cold self-check must stay quick even on a hard disk.
    let total: u64 = report["timings_ms"]
        .as_object()
        .unwrap()
        .values()
        .map(|value| value.as_u64().unwrap())
        .sum();
    assert!(total < 15_000, "self-check took {total} ms");
}

/// One real download per install recipe: fetched from the publisher,
/// verified, installed, played (RetroArch's own frame), then removed.
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn every_download_recipe_installs_verifies_plays_and_removes() {
    let bundle = Bundle::fresh("downloads");
    for (id, recipe) in [
        ("homebrew-hub/dango-dash", "file"),
        ("libretro-content/chip-8-8ceattourny-d3-ch8", "file"),
        (
            "libretro-content/handheld-electronic-game-banana-vtech-time-fun-zip",
            "file + gw core",
        ),
        ("mame-authorized/falcnwld", "file (variant set)"),
        ("dos-games-archive/berlin-1955", "file"),
        ("freedos/vitetris", "file"),
        (
            "msxdev/2014-03-gorgeous-gemma-in-escape-from-the-space-disposal-planet",
            "compilation member",
        ),
        ("msxdev/2005-01-the-cure", "nested archive member"),
        ("scummvm-freeware/soltys", "scummvm folder"),
        ("retrobat-store/gb-tobu", "store package"),
    ] {
        let output = bundle.ok(&["--download", id]);
        assert!(
            output.contains("Downloaded https://"),
            "{id} ({recipe}): {output}"
        );
        let files = bundle.owned_files(id);
        assert!(
            files.iter().all(|file| file.is_file()),
            "{id}: files missing"
        );
        let played = play(&bundle, id, 15);
        played.assert_ran(id);
        played.assert_retroarch_frame(id);
        bundle.ok(&["--remove", id]);
        assert!(
            bundle.manifest(id).is_none(),
            "{id}: record survived REMOVE"
        );
        assert!(
            files.iter().all(|file| !file.exists()),
            "{id}: REMOVE left owned files behind"
        );
    }
}

const NEVOLUTIONX: Game = Game {
    url: "https://github.com/dracc/NevolutionX/releases/download/v0.2.1/NevolutionX.zip",
    sha256: "97a026b39adce1ddf74b89b21715610cd3cc406c1469d5341e953e642e67e06e",
};
/// Homebrew built with Microsoft's SDK, which Cxbx-Reloaded emulates (its
/// compatibility tracker lists it as perfect); nxdk homebrew such as
/// NevolutionX is outside what Cxbx emulates.
const ROCKBOTX: Game = Game {
    url: "https://dlhb.gamebrew.org/xboxhomebrews/rockbotx_v1.0.rar",
    sha256: "f11e4b1f37145996d520e26559ebcc10f68e918e81a025a32c8ea8080454760a",
};
const XBOX_CARD: &str = "libretro-classics/xbox-007-agent-under-fire-15e174b79ad0";

/// RetroBat starts an Xbox disc image in Cxbx-Reloaded only by mounting it
/// through the Dokan driver; RetroPort unpacks it instead.
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn xbox_discs_and_folders_start_in_cxbx_without_dokan() {
    let bundle = Bundle::fresh("xbox");
    let launcher_log = bundle
        .root
        .join("RetroBat/emulationstation/emulatorLauncher.log");
    let image = extract(&fetch(&NEVOLUTIONX), "nevolutionx").join("NevolutionX.iso");
    bundle.ok(&["--import", XBOX_CARD, "--file", path(&image)]);
    let played = play(&bundle, XBOX_CARD, 20);
    assert!(
        played.event("alive_at_deadline").is_some(),
        "Cxbx did not stay up: {:?}",
        played.events
    );
    let log = fs::read_to_string(&launcher_log).unwrap();
    let run = log.rsplit("[Startup]").next().unwrap_or_default();
    assert!(
        log.contains("cxbxr-ldr.exe /load")
            && log.contains("unpacked-discs")
            && log.contains("default.xbe"),
        "Cxbx was not given the unpacked default.xbe:\n{run}"
    );
    assert!(
        !log.contains("Dokan 2 is required")
            || log.rfind("Dokan").unwrap() < log.rfind("cxbxr-ldr").unwrap()
    );
    let cache = bundle.root.join(".retrobat-portable/cache/unpacked-discs");
    assert!(fs::read_dir(&cache).unwrap().count() > 0);
    bundle.ok(&["--remove", XBOX_CARD]);
    assert_eq!(
        fs::read_dir(&cache).unwrap().count(),
        0,
        "REMOVE left the unpacked disc"
    );

    // An .iso that is not an Xbox disc is refused before anything is copied.
    let saturn = fetch(&SATURN_ISO);
    let refused = bundle.refused(&["--import", XBOX_CARD, "--file", path(&saturn)]);
    assert!(
        refused.contains("not an original-Xbox disc image"),
        "{refused}"
    );
    assert!(bundle.manifest(XBOX_CARD).is_none());

    // A game folder (here inside a RAR) starts as it is.
    let rockbot = fetch_with(
        &ROCKBOTX,
        &[
            "--user-agent",
            "Mozilla/5.0 (X11; Linux x86_64) Firefox/130.0",
            "--referer",
            "https://www.gamebrew.org/wiki/RockbotX_Xbox",
        ],
    );
    let output = bundle.ok(&["--import", XBOX_CARD, "--file", path(&rockbot)]);
    assert!(
        output.contains("Imported 315 file(s), 11959571 bytes"),
        "{output}"
    );
    let played = play(&bundle, XBOX_CARD, 40);
    played.assert_ran("RockbotX");
    let shaders = bundle
        .root
        .join("RetroBat/emulators/cxbx-reloaded/ShaderCache");
    assert!(
        fs::read_dir(&shaders)
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains("RockbotX")),
        "Cxbx compiled no shaders for RockbotX, so it never reached its renderer"
    );
}

/// Official demo discs of commercial games, published in RetroBat's store.
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn commercial_demo_discs_download_and_play() {
    let bundle = Bundle::fresh("demos");
    for id in [
        "retrobat-store/gamecube-sonicdx-demo",
        "retrobat-store/wii-muramasa-the-demon-blade-demo",
        "retrobat-store/psx-metal-gear-solid-usa-demo",
        "retrobat-store/dreamcast-volgarr",
    ] {
        bundle.ok(&["--download", id]);
        let played = play(&bundle, id, 40);
        played.assert_ran(id);
        played.assert_retroarch_frame(id);
    }
    // Play! reads no gzip images; RetroPort hands it the decompressed disc.
    // (Its picture needs a GPU, which this headless display does not have.)
    let id = "retrobat-store/ps2-burnout-2-point-of-impact-demo";
    bundle.ok(&["--download", id]);
    let played = play(&bundle, id, 40);
    played.assert_ran(id);
    let log = fs::read_to_string(
        bundle
            .root
            .join("RetroBat/emulationstation/emulatorLauncher.log"),
    )
    .unwrap();
    assert!(
        log.contains("Play.exe --disc") && log.contains("disc.iso"),
        "Play! was not given the decompressed disc"
    );
}

/// Each system plays on its accuracy reference: Mesen, bsnes, SameBoy.
/// Game Boy files also play from Game Boy Color cards (and `.cgb` ROMs
/// import as `.gbc`).
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn cartridge_systems_play_on_their_accuracy_reference_cores() {
    let bundle = Bundle::fresh("references");
    for (id, system, core) in [
        ("homebrew-hub/240p-test-suite", "nes", "mesen"),
        (
            "libretro-content/nintendo-super-nintendo-entertainment-system-n-warp-daisakusen-europe-zip",
            "snes",
            "bsnes",
        ),
        ("homebrew-hub/2048gb", "gb", "sameboy"),
        ("homebrew-hub/7447", "gbc", "sameboy"),
        ("homebrew-hub/parallax-starfield", "gb", "sameboy"),
    ] {
        bundle.ok(&["--download", id]);
        let played = play(&bundle, id, 15);
        played.assert_ran(id);
        played.assert_retroarch_frame(id);
        let log = fs::read_to_string(
            bundle
                .root
                .join(format!(".retrobat-portable/logs/retroarch-{system}.log")),
        )
        .unwrap();
        assert!(
            log.contains(&format!("{core}_libretro.dll")),
            "{id} did not run on {core}"
        );
    }
    assert!(
        bundle
            .launch_file("homebrew-hub/7447")
            .extension()
            .is_some_and(|extension| extension == "gbc"),
        "the .cgb ROM was not imported as .gbc"
    );
}

/// Homebrew that Vita3K's compatibility list rates playable.
const VITA_SNAKE: Game = Game {
    url: "https://github.com/Grzybojad/vitaSnake/releases/download/v1.5/vitaSnake.vpk",
    sha256: "fa2d5d0443ea6d6cddd72e539c4ed0dbeb5120b39eea6ca48bbe3a34b06191a7",
};
const VITA_CARD: &str = "libretro-classics/psvita-aegis-of-earth-protonovus-assault-5bf87f13a5f9";

/// Sony publishes the PS Vita system software and font package; RetroPort
/// installs both from Sony, installs a package into Vita3K, and plays it.
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn ps_vita_plays_on_sonys_own_system_software() {
    let bundle = Bundle::fresh("vita");
    let layout = PortableLayout::new(&bundle.root);
    // Start without the copied installation's own Vita system software.
    let _ = fs::remove_dir_all(retrobat_portable::vita::data_root(&layout));
    for file in ["PSVUPDAT.PUP", "PSP2UPDAT.PUP"] {
        let _ = fs::remove_file(bundle.root.join("RetroBat/bios").join(file));
    }
    let output = bundle.ok_with_display(&["--install-firmware", "psvita"]);
    assert!(
        output.contains("Vita3K installed bios/PSVUPDAT.PUP"),
        "{output}"
    );
    assert!(
        output.contains("Vita3K installed bios/PSP2UPDAT.PUP"),
        "{output}"
    );
    assert!(retrobat_portable::vita::system_software_installed(&layout));
    assert!(retrobat_portable::vita::fonts_installed(&layout));
    let again = bundle.ok_with_display(&["--install-firmware", "psvita"]);
    assert!(again.matches("already installed").count() == 2, "{again}");

    let package = fetch(&VITA_SNAKE);
    bundle.ok(&["--import", VITA_CARD, "--file", path(&package)]);
    let played = play(&bundle, VITA_CARD, 30);
    played.assert_ran("vitaSnake");
    assert!(
        retrobat_portable::vita::data_root(&layout)
            .join("ux0/app/GRZB00002/eboot.bin")
            .is_file(),
        "the package was not installed under its title ID"
    );
    assert!(played.frame_colours() > 50, "Vita3K showed no picture");
    // A second PLAY reuses the installed app without unpacking again.
    play(&bundle, VITA_CARD, 15).assert_ran("vitaSnake again");
    bundle.ok(&["--remove", VITA_CARD]);
    assert!(
        !retrobat_portable::vita::data_root(&layout)
            .join("ux0/app/GRZB00002")
            .exists(),
        "REMOVE left the installed Vita app"
    );
}

/// Games whose download is a folder of data files: the archive is unpacked
/// as it is, the whole folder imported, and the named file started.
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn content_folders_download_unpack_and_play() {
    let bundle = Bundle::fresh("content");
    let mut failures = Vec::new();
    for id in [
        "libretro-content/doom-doom-shareware-zip",
        "libretro-content/quake-quake-shareware-zip",
        "libretro-content/quake-ii-quake-ii-demo-zip",
        "libretro-content/cave-story-cave-story-en-zip",
        "libretro-content/wolfenstein-3d-wolfenstein-3d-v1-4-shareware-zip",
        "libretro-content/wolfenstein-3d-spear-of-destiny-shareware-zip",
        "libretro-content/tomb-raider-tomb-raider-demo-zip",
        "libretro-content/dinothawr-dinothawr-zip",
        "libretro-content/dinothawr-sokoban-zip",
        "libretro-content/super-bros-war-super-cat-wars-lite-zip",
        "libretro-content/pocketcdg-weatherly-danny-boy-zip",
        "libretro-content/tic-80-bunnymark-zip",
        "libretro-content/sega-dreamcast-volgarr-the-viking-zip",
        "libretro-content/sony-playstation-240ptestsuiteps1-emu-zip",
        "libretro-content/scummvm-hi-res-adventure-1-mystery-house-zip",
    ] {
        let output = bundle.retroport(&["--download", id]);
        if !output.status.success() {
            failures.push(format!(
                "{id}: download failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
            continue;
        }
        let played = play(&bundle, id, 20);
        let screenshot = played.event("screenshot");
        let loaded = played
            .event("terminated")
            .is_some_and(|event| event["backend_log"]["content_loaded"] == true);
        if played.event("alive_at_deadline").is_none()
            || !loaded
            || screenshot.is_none_or(|shot| shot["blank"] != false)
        {
            failures.push(format!("{id}: did not play: {:?}", played.events));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Several Windows emulator cores open content through the ANSI file API,
/// which cannot reach a name outside the machine's code page.
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn names_beyond_ascii_install_and_play() {
    let bundle = Bundle::fresh("unicode");
    for (id, name) in [
        ("scummvm-freeware/soltys", "title Sołtys"),
        ("homebrew-hub/deseo", "file deseo_español.gb"),
        ("msxdev/2022-02-word", "member MSXdev22_WÖRD_v1.1_en.rom"),
        (
            "retrobat-store/gbc-lcdz",
            "package file LCDZ-Le_retour_du_phénix",
        ),
    ] {
        bundle.ok(&["--download", id]);
        let launch = bundle.launch_file(id);
        let folder = launch.parent().unwrap().strip_prefix(&bundle.root).unwrap();
        assert!(
            folder.to_str().is_some_and(|path| path.is_ascii()),
            "{id} ({name}) installed in {}",
            folder.display()
        );
        let played = play(&bundle, id, 15);
        played.assert_ran(id);
        played.assert_retroarch_frame(id);
    }
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn handheld_lcd_games_carry_the_game_and_watch_core() {
    let bundle = Bundle::fresh("lcd");
    let id = "libretro-content/handheld-electronic-game-banana-vtech-time-fun-zip";
    bundle.ok(&["--download", id]);
    assert_eq!(bundle.manifest(id).unwrap()["core"], "gw");
    let played = play(&bundle, id, 12);
    played.assert_ran(id);
    let log = fs::read_to_string(
        bundle
            .root
            .join(".retrobat-portable/logs/retroarch-lcdgames.log"),
    )
    .unwrap();
    assert!(log.contains("gw_libretro"), "the gw core did not run");
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn a_download_that_differs_from_its_pin_is_rejected_and_leaves_nothing() {
    use retrobat_portable::downloads::{VerifiedDownloadError, fetch_verified};
    use retrobat_portable::install::ReqwestDownloader;
    let bundle = Bundle::fresh("tampered");
    let ledger = retrobat_portable::browse_install::ledger().unwrap();
    let pinned = ledger.get("homebrew-hub/dango-dash").unwrap();
    let destination = bundle.root.join("probe.download");
    let downloader = ReqwestDownloader::new().unwrap();
    // The real publisher bytes, checked against a pin they do not match.
    let wrong = "0".repeat(64);
    let result = fetch_verified(&downloader, &pinned.url, pinned.size, &wrong, &destination);
    assert!(
        matches!(result, Err(VerifiedDownloadError::Hash { .. })),
        "{result:?}"
    );
    assert!(!destination.exists(), "rejected bytes were left on disk");
    // A pin smaller than the real file stops the transfer itself.
    let result = fetch_verified(
        &downloader,
        &pinned.url,
        pinned.size / 2,
        &pinned.sha256,
        &destination,
    );
    assert!(
        matches!(result, Err(VerifiedDownloadError::TooLarge { .. })),
        "{result:?}"
    );
    assert!(!destination.exists());
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn a_card_without_a_verified_source_refuses_to_download() {
    let bundle = Bundle::fresh("unavailable");
    let error = bundle.refused(&["--download", "dos-games-archive/doom-ii"]);
    assert!(
        error.contains("no verified download is recorded"),
        "{error}"
    );
    assert!(!bundle.root.join(".retrobat-portable/imported").exists());
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn disc_images_import_with_their_tracks() {
    let bundle = Bundle::fresh("discs");
    let ps1 = extract(&fetch(&PS1_240P), "ps1-240p");
    bundle.ok(&[
        "--import",
        PSX_CARD,
        "--file",
        path(&ps1.join("240pTestSuitePS1-EMU.cue")),
    ]);
    let files = bundle.owned_files(PSX_CARD);
    assert_eq!(files.len(), 2, "the CUE and its BIN track");
    // No PS1 BIOS is installed, so PLAY must use the HLE route.
    let report = bundle.self_check();
    let psx = report["readiness"]["systems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|system| system["catalog_system"] == "psx")
        .unwrap();
    assert_eq!(psx["ready_route"]["core"], "pcsx_rearmed");
    let played = play(&bundle, PSX_CARD, 15);
    played.assert_ran("PS1 240p");
    played.assert_retroarch_frame("PS1 240p");

    let saturn = support::work().join("extracted/saturn");
    fs::create_dir_all(&saturn).unwrap();
    fs::copy(fetch(&SATURN_ISO), saturn.join("game.iso")).unwrap();
    fs::copy(fetch(&SATURN_CUE), saturn.join("game.cue")).unwrap();
    bundle.ok(&[
        "--import",
        SATURN_CARD,
        "--file",
        path(&saturn.join("game.cue")),
    ]);
    assert_eq!(bundle.owned_files(SATURN_CARD).len(), 2);
    // Every installed Saturn core needs the owner's BIOS; RetroPort says so.
    let report = ReadinessReport::audit(
        &PortableLayout::new(&bundle.root),
        &retrobat_portable::browse::BrowseCatalog::built_in()
            .unwrap()
            .entries,
    )
    .unwrap();
    assert_eq!(
        report.for_catalog_system("saturn").unwrap().firmware,
        FirmwareState::RequiredMissing
    );
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn cartridge_and_homebrew_files_import_and_play() {
    let bundle = Bundle::fresh("files");
    let gamecube = extract(&fetch(&SUDOKUL_GAMECUBE), "sudokul-gamecube");
    let wii = extract(&fetch(&SUDOKUL_WII), "sudokul-wii");
    for (card, file) in [
        (N64_CARD, fetch(&N64_SBLOBBER)),
        (NDS_CARD, fetch(&NDS_JAMCLOWN)),
        (N3DS_CARD, fetch(&N3DS_FLAPPY)),
        (
            GAMECUBE_CARD,
            gamecube.join("SuDokuL-gamecube/SuDokuL/SuDokuL.dol"),
        ),
        (WII_CARD, wii.join("SuDokuL-wii/apps/SuDokuL/boot.dol")),
    ] {
        bundle.ok(&["--import", card, "--file", path(&file)]);
        let played = play(&bundle, card, 20);
        played.assert_ran(card);
        played.assert_retroarch_frame(card);
    }
}

/// Game folders keep every file they need, whatever the route.
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn game_folders_import_whole_and_play() {
    let bundle = Bundle::fresh("folders");
    let psp = extract(&fetch(&PSP_JOKER_POKER), "joker-poker");
    bundle.ok(&[
        "--import",
        PSP_CARD,
        "--file",
        path(&psp.join("joker-poker")),
    ]);
    assert_eq!(
        bundle.owned_files(PSP_CARD).len(),
        4,
        "EBOOT.PBP and its res/ assets"
    );
    let played = play(&bundle, PSP_CARD, 20);
    played.assert_ran("PSP folder");
    played.assert_retroarch_frame("PSP folder");

    // Native Linux Cemu: the first PLAY must reach the game, not a wizard.
    let wiiu = extract(&fetch(&SUDOKUL_WIIU), "sudokul-wiiu");
    bundle.ok(&[
        "--import",
        WIIU_CARD,
        "--file",
        path(&wiiu.join("SuDokuL-wiiu/wiiu/apps/SuDokuL")),
    ]);
    assert!(bundle.launch_file(WIIU_CARD).ends_with("SuDokuL.rpx"));
    let played = play(&bundle, WIIU_CARD, 25);
    played.assert_ran("Wii U folder");
    assert!(played.frame_colours() > 50, "Cemu showed no game");

    // A Windows game folder runs under Wine with its DLLs and data.
    let windows = extract(&fetch(&SUDOKUL_WINDOWS), "sudokul-windows");
    bundle.ok(&[
        "--import",
        WINDOWS_CARD,
        "--file",
        path(&windows.join("SuDokuL-x64")),
    ]);
    let played = play(&bundle, WINDOWS_CARD, 20);
    played.assert_ran("Windows folder");
    assert!(
        played.frame_colours() > 50,
        "the Windows game showed nothing"
    );
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn imports_never_overwrite_and_never_dead_end() {
    let bundle = Bundle::fresh("collisions");
    let rom = fetch(&N64_SBLOBBER);
    let other_card = "libretro-classics/n64-1080-snowboarding-9be9d6bdd0e5";
    bundle.ok(&["--import", N64_CARD, "--file", path(&rom)]);
    // The same file on another card gets its own folder; nothing is refused.
    bundle.ok(&["--import", other_card, "--file", path(&rom)]);
    assert_ne!(bundle.launch_file(N64_CARD), bundle.launch_file(other_card));
    // Importing onto a card that is already imported is refused.
    let error = bundle.refused(&["--import", N64_CARD, "--file", path(&rom)]);
    assert!(error.contains("already imported"), "{error}");
    // REMOVE keeps a file the user changed, and the card can import again.
    let launch = bundle.launch_file(N64_CARD);
    fs::write(&launch, b"user changed this file").unwrap();
    bundle.ok(&["--remove", N64_CARD]);
    assert_eq!(fs::read(&launch).unwrap(), b"user changed this file");
    bundle.ok(&["--import", N64_CARD, "--file", path(&rom)]);
    assert_ne!(
        bundle.launch_file(N64_CARD),
        launch,
        "the kept file was overwritten"
    );
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn mame_needs_the_rom_set_zip_and_says_so() {
    let bundle = Bundle::fresh("mame");
    let zip = support::work().join("cache/supertnk.zip");
    if !zip.is_file() {
        let status = std::process::Command::new("curl")
            .args(["--fail", "--location", "--silent", "--output"])
            .arg(&zip)
            .arg("https://www.mamedev.org/roms/supertnk/supertnk.zip")
            .status()
            .unwrap();
        assert!(status.success());
    }
    let chips = extract(&zip, "supertnk-chips");
    let chip = fs::read_dir(&chips)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let error = bundle.refused(&["--import", MAME_CARD, "--file", path(&chip)]);
    assert!(error.contains("intact ROM-set ZIP"), "{error}");
    let error = bundle.refused(&["--import", MAME_CARD, "--file", path(&chips)]);
    assert!(error.contains("folder"), "{error}");
    bundle.ok(&["--import", MAME_CARD, "--file", path(&zip)]);
    let played = play(&bundle, MAME_CARD, 15);
    played.assert_ran("MAME ZIP");
    played.assert_retroarch_frame("MAME ZIP");
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn a_rar_archive_imports_exactly_and_removes_completely() {
    // A real publisher RAR (RockbotX, an Xbox game folder); playing it is
    // covered by the Xbox test. Nested archives inside stay archives.
    let bundle = Bundle::fresh("rar");
    let rar = fetch_with(
        &ROCKBOTX,
        &[
            "--user-agent",
            "Mozilla/5.0 (X11; Linux x86_64) Firefox/130.0",
            "--referer",
            "https://www.gamebrew.org/wiki/RockbotX_Xbox",
        ],
    );
    let output = bundle.ok(&["--import", XBOX_CARD, "--file", path(&rar)]);
    assert!(
        output.contains("Imported 315 file(s), 11959571 bytes"),
        "{output}"
    );
    let files = bundle.owned_files(XBOX_CARD);
    assert!(files.iter().any(|file| file.ends_with("source.rar")));
    assert!(
        !files
            .iter()
            .any(|file| file.to_string_lossy().contains(".contents"))
    );
    bundle.ok(&["--remove", XBOX_CARD]);
    assert!(
        files.iter().all(|file| !file.exists()),
        "REMOVE left files behind"
    );
}

#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn firmware_is_recorded_mirrored_and_changes_the_route() {
    let bundle = Bundle::fresh("firmware");
    let layout = PortableLayout::new(&bundle.root);
    let browse = retrobat_portable::browse::BrowseCatalog::built_in().unwrap();
    let report = ReadinessReport::audit(&layout, &browse.entries).unwrap();
    // Switch keys: any non-empty file the owner selects is accepted,
    // recorded with its hash, and mirrored into both Eden profiles.
    let switch = report.for_catalog_system("switch").unwrap();
    let keys = switch
        .firmware_files
        .iter()
        .find(|file| file.relative_path == "eden/keys/prod.keys")
        .unwrap();
    let source = support::work().join("owner-prod.keys");
    fs::write(&source, b"owner supplied key material").unwrap();
    let placed = import_firmware(&layout, keys, &source).unwrap();
    let record = firmware_record(&layout, "eden/keys/prod.keys").unwrap();
    assert_eq!(record.sha256, placed.sha256);
    assert_eq!(record.origin, "owner-prod.keys");
    for mirror in [
        bundle
            .root
            .join("RetroBat/emulators/eden/user/keys/prod.keys"),
        bundle
            .root
            .join(".retrobat-portable/runtime/linux/eden/data/eden/keys/prod.keys"),
    ] {
        assert_eq!(fs::read(mirror).unwrap(), b"owner supplied key material");
    }
    // A PS1 BIOS switches PLAY from the HLE route to Beetle PSX.
    fs::write(bundle.root.join("RetroBat/bios/scph5501.bin"), b"bios").unwrap();
    let report = ReadinessReport::audit(&layout, &browse.entries).unwrap();
    assert_eq!(
        report
            .for_catalog_system("psx")
            .unwrap()
            .ready_route
            .as_ref()
            .unwrap()
            .core
            .as_deref(),
        Some("mednafen_psx_hw")
    );
}

/// The Windows build's own PLAY routes, under Wine: RetroArch games through
/// EmulatorLauncher (Linux starts RetroArch directly), and PS Vita through
/// Vita3K.exe with Sony's firmware installed by the Windows build.
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn the_windows_build_plays_through_its_own_routes() {
    let exe = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/x86_64-pc-windows-msvc/release/retrobat-portable.exe");
    let bundle = Bundle::fresh("windows-play");
    fs::copy(&exe, bundle.root.join("RetroPort.exe")).unwrap();
    let wine = |arguments: &[&str]| {
        let output = std::process::Command::new("wine")
            .arg(bundle.root.join("RetroPort.exe"))
            .args(arguments)
            .env(
                "WINEPREFIX",
                support::xdg_data().join("retrobat-portable/wine-prefix"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "RetroPort.exe {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    for id in [
        "homebrew-hub/dango-dash",
        "libretro-content/sony-playstation-240ptestsuiteps1-emu-zip",
    ] {
        wine(&["--download", id]);
        let played = play_windows(&bundle, id, 25);
        played.assert_ran(id);
        assert!(played.frame_colours() > 8, "{id} showed no picture");
        let log = fs::read_to_string(
            bundle
                .root
                .join("RetroBat/emulationstation/emulatorLauncher.log"),
        )
        .unwrap();
        assert!(
            log.rsplit("[Startup]")
                .next()
                .unwrap_or_default()
                .contains("retroarch.exe"),
            "{id} did not start through EmulatorLauncher's RetroArch route"
        );
    }

    // PS Vita: the Windows build installs Sony's firmware through Vita3K.exe
    // and plays a package installed by title ID.
    let layout = PortableLayout::new(&bundle.root);
    let _ = fs::remove_dir_all(retrobat_portable::vita::data_root(&layout));
    for file in ["PSVUPDAT.PUP", "PSP2UPDAT.PUP"] {
        let _ = fs::remove_file(bundle.root.join("RetroBat/bios").join(file));
    }
    let installed = bundle.ok_with_wine_display(&["--install-firmware", "psvita"]);
    assert!(
        installed.contains("Vita3K installed bios/PSVUPDAT.PUP"),
        "{installed}"
    );
    assert!(retrobat_portable::vita::system_software_installed(&layout));
    assert!(retrobat_portable::vita::fonts_installed(&layout));
    let package = fetch(&VITA_SNAKE);
    wine(&["--import", VITA_CARD, "--file", path(&package)]);
    let played = play_windows(&bundle, VITA_CARD, 30);
    played.assert_ran("vitaSnake on Windows");
    assert!(played.frame_colours() > 50, "Vita3K.exe showed no picture");
    wine(&["--remove", VITA_CARD]);
    assert!(
        !retrobat_portable::vita::data_root(&layout)
            .join("ux0/app/GRZB00002")
            .exists(),
        "REMOVE left the installed Vita app"
    );
}

/// The Windows launcher, run through Wine against the same installation.
#[test]
#[ignore = "functional: run tools/functional_test.sh"]
fn the_windows_build_checks_downloads_and_imports() {
    let exe = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/x86_64-pc-windows-msvc/release/retrobat-portable.exe");
    assert!(
        exe.is_file(),
        "build it first: cargo xwin build --release --target x86_64-pc-windows-msvc"
    );
    let bundle = Bundle::fresh("windows-build");
    fs::copy(&exe, bundle.root.join("RetroPort.exe")).unwrap();
    let wine = |arguments: &[&str]| {
        std::process::Command::new("wine")
            .arg(bundle.root.join("RetroPort.exe"))
            .args(arguments)
            .env(
                "WINEPREFIX",
                support::xdg_data().join("retrobat-portable/wine-prefix"),
            )
            .output()
            .unwrap()
    };
    let check = wine(&["--self-check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(report["target_platform"], "windows");
    assert_eq!(report["browse_entries"], 80_734);
    let download = wine(&["--download", "homebrew-hub/dango-dash"]);
    assert!(
        download.status.success(),
        "{}",
        String::from_utf8_lossy(&download.stderr)
    );
    assert!(bundle.manifest("homebrew-hub/dango-dash").is_some());
    let remove = wine(&["--remove", "homebrew-hub/dango-dash"]);
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
}
