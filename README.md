# RetroPort

An artwork-first game library for Windows and Linux over RetroBat. Browse
80,734 games by cover, source, or system, then act from the card:

- **DOWNLOAD** fetches the publisher's exact file, verifies size and SHA-256
  while it streams, and installs it.
- **IMPORT GAME** copies your own ROM, disc set, archive, or game folder.
- **PLAY** starts that copy on the best installed emulator; **TERMINATE** stops
  its whole process tree.
- **CONTROLS** shows the mapping and names its evidence; it never guesses.
- **REMOVE** deletes what the card installed, keeping files you changed.

## Set up

Git holds the source, catalogues, tools, and artwork, not the ~5 GB runtime.
On Debian or Ubuntu:

```sh
sudo apt-get install -y build-essential curl git p7zip-full python3 rsync wine64
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
git clone https://github.com/muradkant/retrobat-portable.git
cd retrobat-portable && ./tools/bootstrap_bundle.sh
```

The bootstrap downloads every runtime from its official release, refuses any
file whose SHA-256 differs from its pin, builds both launchers, and verifies
the result (about 7 GB; cached; `RETROPORT_DOWNLOAD_CACHE` moves the cache).
Then run `RetroPort.exe` on Windows or `./RetroPort-Linux` on Linux.

## Play

- Import a game made of many files (PS3, PS4, Wii U, PC, homebrew) as a folder.
  CUE, GDI, and M3U imports bring every track. RAR and 7z archives unpack as
  they are (Linux needs `p7zip-rar` for RAR). Arcade games import as the intact
  MAME ROM-set ZIP.
- Imports never overwrite a file RetroPort does not own; a taken name gets its
  own `Title (2)` folder.
- On Linux the first Wine-based PLAY prepares a Wine prefix (about a minute)
  and installs Wine Mono from Wine's cache, or says exactly what to install.
- Connect controllers before PLAY.

## Emulators

| System | Emulator |
|---|---|
| NES, SNES, Game Boy / Color, GBA, DS, 3DS | Mesen, bsnes, SameBoy, mGBA, melonDS, Azahar |
| N64, GameCube / Wii, Wii U, Switch | Mupen64Plus-Next, Dolphin, Cemu, Eden |
| PS1, PS2 | Beetle PSX HW, PCSX2 with your BIOS; PCSX ReARMed, Play! until then |
| PS3, PS4, PSP, PS Vita | RPCS3, shadPS4, PPSSPP, Vita3K |
| Xbox, Xbox 360 | xemu with your BIOS, Cxbx-Reloaded until then; Xenia Canary |
| Mega Drive / Sega CD, Saturn, Dreamcast | Genesis Plus GX; Beetle Saturn (YabaSanshiro with `saturn_bios.bin`); Flycast |
| Arcade, DOS, PC Engine, Amiga | MAME, DOSBox Pure, Beetle PCE, PUAE |

Standalone emulators are pinned releases with native Linux builds where they
exist; RetroPort turns off the update checks of Cemu, RPCS3, PCSX2, xemu, and
Vita3K. Systems whose emulator is not installed say so and offer neither PLAY
nor DOWNLOAD.

## Firmware

RetroPort uses official firmware: from the maker where the maker publishes it,
otherwise dumped from your own console. Your file always takes over from a
built-in stand-in. Each card names the file, why it is needed, and where it
comes from.

| Source | Systems |
|---|---|
| Sony, free: **INSTALL FIRMWARE** downloads, verifies, installs | PS3 system software; PS Vita system software and fonts |
| Your console, switches to the reference emulator | PS1 → Beetle PSX; PS2 → PCSX2; Xbox (MCPX + flash) → xemu |
| Your console, required | Saturn (`mpr-17933.bin`/`sega_101.bin`, or `saturn_bios.bin`), Sega CD (`bios_CD_U/E/J.bin`), PC Engine CD (`syscard3.pce`), Neo Geo (`neogeo.zip`), Neo Geo CD, 3DO, FDS (`disksys.rom`), 64DD (`IPL.n64`), Lynx, ColecoVision, Intellivision, Atari ST (`tos.img`), X68000, Switch (`prod.keys`) |
| Optional, built-in stand-in otherwise | Dreamcast, GBA, DS, Atari 5200; Amiga (Kickstart, sold by [Cloanto](https://www.amigaforever.com/)) |

To plug files in, use **IMPORT FIRMWARE** on a card. Pick a file for a
target, or press **RECOGNISE EVERY BIOS IN THIS FOLDER** (or drop a folder);
RetroPort identifies each file by fingerprint and places it wherever its
emulators look. From a terminal: `--import-firmware <file-or-folder>` and
`--install-firmware ps3|psvita`. Every placed file's SHA-256 and origin are
recorded; nothing you select is rejected.

Encrypted Wii U discs (`.wud`/`.wux`) need their disc keys, and encrypted 3DS
dumps need decrypting on your console; decrypted dumps need nothing.

## Catalogue

- 80,734 records from 10 established sources, each with a cover, import route,
  and controls view.
- 4,153 downloadable records: 4,127 pinned to the publisher's exact file and
  checked to end in a format their system plays; 26 say why they cannot be.
- 76,581 commercial records you import from your own copies.
- FEATURED: 410 titles found on six or more best-of lists or in the World
  Video Game Hall of Fame.

## Installation

```text
RetroPort/
├── RetroPort.exe, RetroPort-Linux(.desktop)
├── RetroBat/            emulators, games, saves, BIOS
├── Runtime/Linux/       native AppImages
├── Artwork/             bundled MAME artwork
├── .retrobat-portable/  import and firmware records, logs, native state
├── Source/RetroPort-source.zip
└── SHA256SUMS, VERIFY-LINUX.sh, VERIFY-WINDOWS.cmd, README-FIRST.txt
```

It is self-contained: nothing is borrowed from another installation. On Linux
only the Wine prefix lives outside it (`~/.local/share/retrobat-portable/`).

What RetroPort guarantees:

- Downloads come only from pinned publisher URLs and enter staging; bytes that
  do not match are deleted. Archive members are found by hash, not name.
- Imports reject traversal and symlinks, never overwrite unowned files, and
  record every file they own. REMOVE deletes only files still matching their
  record.
- Folder names RetroPort creates are ASCII, so emulators that use Windows'
  ANSI file API can open them.
- Xbox disc images are unpacked by RetroPort for Cxbx (RetroBat would need the
  Dokan driver); gzipped PS2 images are decompressed for Play!; Vita packages
  are installed into Vita3K by title ID.
- A launch that fails, including one EmulatorLauncher refuses, is reported with
  its reason and log.
- `SHA256SUMS` covers launchers, source, documentation, emulator binaries, and
  artwork; games, saves, BIOS, and emulator state are yours and outside it.

## Verify and develop

```sh
./VERIFY-LINUX.sh                      # or VERIFY-WINDOWS.cmd: integrity
./RetroPort-Linux --self-check         # catalogue, pins, coverage, artwork
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets
cargo xwin build --release --target x86_64-pc-windows-msvc
./tools/functional_test.sh             # real downloads, imports, and PLAY
```

The functional suite drives the real binary against private copies of the
installation. It downloads and plays one game per recipe, content-folder
games, and commercial demo discs. It imports homebrew for every system family,
installs Sony's Vita firmware and plays a Vita game, checks the reference
emulators, and runs the Windows build under Wine through its own PLAY routes. It needs Xvfb, ffmpeg,
7-Zip, curl, and Wine, and uses its own Wine prefix. In that virtual display,
Wine's OpenGL and Direct3D render in software, so PS2 and Xbox tests check that
the game ran, not its picture.

- `--gameplay-probe ID` plays an installed card, captures RetroArch's frame,
  and exits non-zero on any failure (`--help` lists every option).
- `tools/build_download_ledger.py [--refresh SOURCE]` regenerates the download
  pins.
- `tools/deploy_bundle.sh DEST` updates another installation, replacing only
  files in `SHA256SUMS`.

## Provenance

Built against RetroBat [`c90884f`](https://github.com/RetroBat-Official/retrobat),
EmulatorLauncher [`1a9571a`](https://github.com/RetroBat-Official/emulatorlauncher),
and EmulationStation [`d77fbf1`](https://github.com/RetroBat-Official/emulationstation).
Catalogue and artwork come from Libretro, LaunchBox, Homebrew Hub, RetroBat,
ScummVM, FreeDOS, MAMEdev, MSXdev, DOS Games Archive, and Progetto-SNAPS.
Controls evidence comes from MAME `-listxml`, the Libretro MAME DAT, and
RetroBat's `gamesdb.xml`. [`THIRD-PARTY-ASSETS.txt`](packaging/THIRD-PARTY-ASSETS.txt)
records every runtime's URL, version, hash, and licence.
