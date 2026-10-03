RETROPORT — START HERE
======================

Keep this installation together. Its launchers, emulators, games, artwork,
saves, and configuration refer only to this folder.

OPEN RETROPORT
--------------

Windows: double-click RetroPort.exe. If Windows warns that this personal build
is unsigned, choose More info, then Run anyway.

Linux: install 64-bit Wine for the legacy systems, then double-click
RetroPort-Linux.desktop or run ./RetroPort-Linux. RPCS3, Cemu, shadPS4, Eden,
and Xenia use the native Linux runtimes included here; the older RetroBat stack
uses Wine.

PLAY A GAME
-----------

1. Browse FEATURED, a source, or a system. ALL SOURCES contains all 80,734
   records; search always spans the full catalogue.
2. Choose the card action:

   DOWNLOAD     Fetch and verify a project- or publisher-hosted game.
   IMPORT GAME  Copy a local ROM, disc set, executable, or game folder.
   PLAY         Launch the imported or downloaded copy.
   CONTROLS     Show detected bindings, hardware needs, and their evidence.

3. PLAY changes to LOADING, then TERMINATE. A second launch is blocked while
   the game runs. TERMINATE stops the emulator's complete process tree.

Import an extracted PS3, PS4, Wii U, or PC game with IMPORT THIS FOLDER; its
executables, libraries, and data must stay together. Import an arcade set such
as mspacman.zip as the original ZIP, not as extracted component files.

Import checks compatibility and safe structure, not one canonical dump hash.
Alternate revisions, translations, and patches remain playable; known-hash
differences are diagnostic only.

CONTROLLERS
-----------

Connect the controller before PLAY. Every card has CONTROLS, derived from this
installation's RetroArch/RetroBat configuration, SDL device profile, and any
MAME or RetroBat device metadata. Unknown per-game action names are labelled,
never guessed.

On Linux, RetroPort detects the optional controller-mouse game guard from
https://github.com/muradkant/linux-zhixu-controller-fix. It suspends desktop
controller-as-mouse input while an emulator runs and restores it on exit.

FIRMWARE
--------

RetroPort uses official firmware. Where the maker publishes it (Sony's PS3
and PS Vita system software), INSTALL FIRMWARE downloads it from the maker,
verifies it, and installs it into the emulator. Everything else comes from
your own console: IMPORT FIRMWARE accepts the file for a listed target, or
recognises every BIOS in a folder you choose by fingerprint and places each
where its emulators look. RetroPort records each file's hash and never
rejects an unfamiliar dump. Switch prod.keys are copied into both Eden
profiles.

Your BIOS switches PLAY to the reference emulator: PCSX2 for PS2, Beetle PSX
for PS1, xemu for Xbox, Beetle Saturn for Saturn. Until then PS2 runs on
Play!, PS1 on PCSX ReARMed, and Xbox on Cxbx-Reloaded, all of which need no
BIOS. Each card names the file, why it is needed, and where it comes from.

VERIFY OR TROUBLESHOOT
----------------------

Windows: double-click VERIFY-WINDOWS.cmd.
Linux:   run ./VERIFY-LINUX.sh.

A successful integrity check ends with “VERIFICATION PASSED.” If a game still
does not start:

1. Read its backend and firmware line.
2. Keep every CUE, GDI, or M3U track/disc beside its descriptor before import.
3. Import an extracted installation as a folder, not one executable.
4. Run the verifier.
5. On Linux, confirm `wine --version` works and the GPU supports Vulkan.

CONTENTS
--------

RetroPort.exe                 Windows application
RetroPort-Linux               Linux application
RetroPort-Linux.desktop       Linux graphical shortcut
RetroBat/                     adapters, emulators, games, saves
Runtime/Linux/                native modern-console runtimes
Artwork/                      verified local artwork
Source/RetroPort-source.zip   corresponding Rust source
SHA256SUMS                    static integrity manifest
VERIFY-LINUX.sh               Linux verifier
VERIFY-WINDOWS.cmd/.ps1       Windows verifier
THIRD-PARTY-ASSETS.txt        upstream versions, URLs, hashes, licences
LICENSE-RETROPORT.txt         RetroPort source licence

Upstream notices remain beside their programs. Mutable games, saves, private
machine data, and configuration are intentionally outside the static manifest.
