#!/usr/bin/env bash
set -euo pipefail

project=$(cd "$(dirname "$0")/.." && pwd)
root=${1:-$project}
root=$(realpath -m "$root")
cache=${RETROPORT_DOWNLOAD_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/retroport/bootstrap}

for command in curl sha256sum 7z rsync realpath; do
    if ! command -v "$command" >/dev/null; then
        echo "missing required command: $command" >&2
        exit 1
    fi
done
if [[ ! -f "$root/RetroBat/RetroBat.exe" ]]; then
    echo "RetroBat base is missing; run tools/bootstrap_retrobat_base.sh first" >&2
    exit 1
fi

mkdir -p "$cache" "$root/Runtime/Linux/shadPS4"

fetch() {
    local name=$1 url=$2 expected=$3
    local output="$cache/$name"
    if [[ ! -f "$output" ]] ||
        ! echo "$expected  $output" | sha256sum --check --status
    then
        rm -f "$output"
        echo "Downloading $name" >&2
        curl --fail --location --retry 4 --retry-all-errors \
            --continue-at - --output "$output" "$url" >&2
    fi
    if ! echo "$expected  $output" | sha256sum --check >&2; then
        echo "refusing unverified download: $output" >&2
        return 1
    fi
    printf '%s\n' "$output"
}

install_archive() {
    local archive=$1 marker=$2 destination=$3
    local staging
    staging=$(mktemp -d "$root/.runtime-asset.XXXXXX")
    7z x -y -o"$staging" "$archive" >/dev/null
    local found
    found=$(find "$staging" -type f -iname "$marker" -print -quit)
    if [[ -z "$found" ]]; then
        echo "archive $(basename "$archive") does not contain $marker" >&2
        rm -rf "$staging"
        exit 1
    fi
    mkdir -p "$destination"
    rsync -a "$(dirname "$found")/" "$destination/"
    rm -rf "$staging"
}

# Play! ships as an NSIS installer; 7-Zip unpacks it without running it.
# The extracted emulator is pinned separately from the download.
install_nsis() {
    local installer=$1 destination=$2 marker=$3 expected=$4
    local staging
    staging=$(mktemp -d "$root/.runtime-asset.XXXXXX")
    7z x -y -o"$staging" "$installer" >/dev/null
    rm -rf "$staging/\$PLUGINSDIR" "$staging/uninstall.exe"
    if ! echo "$expected  $staging/$marker" | sha256sum --check --status; then
        echo "extracted $marker from $(basename "$installer") has an unexpected SHA-256" >&2
        rm -rf "$staging"
        exit 1
    fi
    mkdir -p "$destination"
    rsync -a "$staging/" "$destination/"
    rm -rf "$staging"
}

install_direct() {
    local source=$1 destination=$2 mode=${3:-755}
    mkdir -p "$(dirname "$destination")"
    install -m "$mode" "$source" "$destination"
}

# Extracts one archive member, verified by its own pin.
install_member() {
    local archive=$1 member=$2 destination=$3 expected=$4
    local staging
    staging=$(mktemp -d "$root/.runtime-asset.XXXXXX")
    7z e -y -o"$staging" "$archive" "$member" >/dev/null
    local extracted
    extracted="$staging/$(basename "$member")"
    if ! echo "$expected  $extracted" | sha256sum --check --status; then
        echo "$member from $(basename "$archive") is missing or has an unexpected SHA-256" >&2
        rm -rf "$staging"
        exit 1
    fi
    install_direct "$extracted" "$destination" 644
    rm -rf "$staging"
}

# Windows backends and cores. Libretro's nightly URLs change with every
# build, so cores come from the immutable stable bundle of RetroBat's
# RetroArch version.
retroarch_cores=$(fetch \
    RetroArch_cores-1.22.2-win64.7z \
    https://buildbot.libretro.com/stable/1.22.2/windows/x86_64/RetroArch_cores.7z \
    86b871e11b9b4772ac644b40a38f2c8e9449da1f355eae7da08aa061148547b0)
install_member "$retroarch_cores" RetroArch-Win64/cores/jaxe_libretro.dll \
    "$root/RetroBat/emulators/retroarch/cores/jaxe_libretro.dll" \
    fa056612cc58f987ae4d9074765e080897b763b52891f19f5f817c8677942b2a

xenia_win=$(fetch \
    xenia_canary_windows-6e5b832.7z \
    https://github.com/xenia-canary/xenia-canary/releases/download/6e5b832/xenia_canary_windows.7z \
    fe43847b26b73140bdf131259f540b12ed7edcb2bf18dd846dc5bc1cf7e293dd)
install_archive "$xenia_win" xenia_canary.exe "$root/RetroBat/emulators/xenia-canary"

rpcs3_win=$(fetch \
    rpcs3-v0.0.41-19564-700ca262_win64_msvc.7z \
    https://github.com/RPCS3/rpcs3-binaries-win/releases/download/build-700ca262f44fda57ba260283c3f0a4772db8a573/rpcs3-v0.0.41-19564-700ca262_win64_msvc.7z \
    3d0e7b796df5ec05fa2d9448d4c1203f97ae2f605bc16dad3bc175ed858c191e)
install_archive "$rpcs3_win" rpcs3.exe "$root/RetroBat/emulators/rpcs3"

cemu_win=$(fetch \
    cemu-2.6-windows-x64.zip \
    https://github.com/cemu-project/Cemu/releases/download/v2.6/cemu-2.6-windows-x64.zip \
    a6bcc2bc42a362d10213819948f3152fae7d47f70067f25939b51d3ddcfb0896)
install_archive "$cemu_win" Cemu.exe "$root/RetroBat/emulators/cemu"

shad_win=$(fetch \
    shadps4-win64-sdl-0.16.0.zip \
    https://github.com/shadps4-emu/shadPS4/releases/download/v.0.16.0/shadps4-win64-sdl-0.16.0.zip \
    f6cdcca82f239fb69b2f820ad9dec07f2f00b423273851b67a0e24bc783acf46)
install_archive "$shad_win" shadPS4.exe "$root/RetroBat/emulators/shadps4"

# The Qt launcher is only shadPS4's optional settings GUI, and its project
# deletes superseded builds; a missing pin is reported, not fatal.
if shad_qt=$(fetch \
    shadPS4QtLauncher-win64-qt-2026-10-02-4c4e109.zip \
    https://github.com/shadps4-emu/shadps4-qtlauncher/releases/download/shadPS4QtLauncher-2026-10-02-4c4e1090ea53dc1ec956fa9954acbf143d104b2e/shadPS4QtLauncher-win64-qt-2026-10-02-4c4e109.zip \
    7aaab50f623dfea987ed11e3fa6374c34f5fce988ab0673e4b0ab7216b7fab9f)
then
    install_archive "$shad_qt" shadPS4QtLauncher.exe "$root/RetroBat/emulators/shadps4"
else
    echo "warning: shadPS4's optional Qt launcher is no longer published at its pin; skipped" >&2
fi

eden_win=$(fetch \
    Eden-Windows-v0.2.1-amd64-msvc-standard.zip \
    https://stable.eden-emu.dev/v0.2.1/Eden-Windows-v0.2.1-amd64-msvc-standard.zip \
    ff498e5da9630216926ac3cbe9fb493b14930665c728a40a8f5b59507fdd7ebf)
install_archive "$eden_win" eden.exe "$root/RetroBat/emulators/eden"

cxbx=$(fetch \
    CxbxReloaded-CI-585c49a.zip \
    https://github.com/Cxbx-Reloaded/Cxbx-Reloaded/releases/download/CI-585c49a/CxbxReloaded-Release.zip \
    010d1e85bee9f82f05ae57ca483e7ae61fecba06c1637bf6b5a74ca09b03bf43)
install_archive "$cxbx" cxbx.exe "$root/RetroBat/emulators/cxbx-reloaded"

play=$(fetch \
    Play-x86-64-0.70.exe \
    https://www.purei.org/downloads/play/stable/0.70/Play-x86-64.exe \
    d4cd4583694d555771483526b87b7ff29ca42b0fab0693ae69d1189575a56883)
install_nsis "$play" "$root/RetroBat/emulators/play" Play.exe \
    ec4cc5ba8f0865c544f03b51e1c3534bfa1020f3cbdddedeac4f29dc824930b9

# PCSX2: the reference PS2 emulator once the owner's BIOS is present. Its
# self-updater is left out; RetroPort updates only through these pins.
pcsx2_win=$(fetch \
    pcsx2-v2.8.2-windows-x64-Qt.7z \
    https://github.com/PCSX2/pcsx2/releases/download/v2.8.2/pcsx2-v2.8.2-windows-x64-Qt.7z \
    7dfc829ca1994cc1045ac49f05e39b6cf968b72e6a374c40e05c2a2b4ac200b4)
install_archive "$pcsx2_win" pcsx2-qt.exe "$root/RetroBat/emulators/pcsx2"
rm -f "$root/RetroBat/emulators/pcsx2/updater.exe"

# xemu: the reference original-Xbox emulator once the owner's BIOS is present.
xemu_win=$(fetch \
    xemu-0.8.136-windows-x86_64.zip \
    https://github.com/xemu-project/xemu/releases/download/v0.8.136/xemu-0.8.136-windows-x86_64.zip \
    b25a6c24a2c2c36a0843a153cd9ee59ca6833ef87bdcd855ba0824930e4ddd1d)
install_archive "$xemu_win" xemu.exe "$root/RetroBat/emulators/xemu"
# xemu's official blank, formatted Xbox hard disk. RetroPort copies it into
# saves/xbox on first PLAY and never overwrites the copy games save onto.
xemu_hdd=$(fetch \
    xbox_hdd.qcow2-1.0.zip \
    https://github.com/xemu-project/xemu-hdd-image/releases/download/1.0/xbox_hdd.qcow2.zip \
    d9f5a4c1224ff24cf9066067bda70cc8b9c874ea22b9c542eb2edbfc4621bb39)
install_member "$xemu_hdd" xbox_hdd.qcow2 \
    "$root/RetroBat/emulators/xemu/xbox_hdd.blank.qcow2" \
    eb35d069715dfc0d37cfac086a0b73a89a02e8f12519cd8eda82ad479d1a4eed

# Vita3K publishes numbered builds alongside its rolling one.
vita3k_win=$(fetch \
    vita3k-4115-a366df69-windows-x86_64.7z \
    https://github.com/Vita3K/Vita3K-builds/releases/download/4115/vita3k-4115-a366df69-windows-x86_64.7z \
    35aead1c59a684f15b30e87cc18cdc34d535bca3873d711fa2ac92d9adb90897)
install_archive "$vita3k_win" Vita3K.exe "$root/RetroBat/emulators/vita3k"

# Native Linux routes.
xenia_linux=$(fetch \
    XeniaCanary-6e5b832.AppImage \
    https://github.com/xenia-canary/xenia-canary/releases/download/6e5b832/xenia_canary_linux.AppImage \
    6e0dba4e56fd5b48c0043be0879cb219c5dc9e2eed5e30f3df3cc0acb1177482)
install_direct "$xenia_linux" "$root/Runtime/Linux/XeniaCanary.AppImage"

rpcs3_linux=$(fetch \
    RPCS3-700ca262.AppImage \
    https://github.com/RPCS3/rpcs3-binaries-linux/releases/download/build-700ca262f44fda57ba260283c3f0a4772db8a573/rpcs3-v0.0.41-19564-700ca262_linux64.AppImage \
    190cb796ffce3cfb61f56f03ae44efa7fd8331d49fb37412ba5e6d950a2bd59b)
install_direct "$rpcs3_linux" "$root/Runtime/Linux/RPCS3.AppImage"

cemu_linux=$(fetch \
    Cemu-2.6-x86_64.AppImage \
    https://github.com/cemu-project/Cemu/releases/download/v2.6/Cemu-2.6-x86_64.AppImage \
    0c20c4aeb800bb13d9bab9474ef45a6f8fcde6402cad9b32ac2a1bbd03186313)
install_direct "$cemu_linux" "$root/Runtime/Linux/Cemu.AppImage"

shad_linux=$(fetch \
    shadps4-linux-sdl-0.16.0.zip \
    https://github.com/shadps4-emu/shadPS4/releases/download/v.0.16.0/shadps4-linux-sdl-0.16.0.zip \
    7cbb19fe8c909e04129d2431eef723d4710499b40d1aed0047681d14a1dfc79b)
install_archive "$shad_linux" '*.AppImage' "$root/Runtime/Linux/shadPS4"
shad_appimage=$(find "$root/Runtime/Linux/shadPS4" -maxdepth 1 -type f -iname '*.AppImage' -print -quit)
if [[ "$shad_appimage" != "$root/Runtime/Linux/shadPS4/Shadps4-sdl.AppImage" ]]; then
    mv -f "$shad_appimage" "$root/Runtime/Linux/shadPS4/Shadps4-sdl.AppImage"
fi
chmod +x "$root/Runtime/Linux/shadPS4/Shadps4-sdl.AppImage"

eden_linux=$(fetch \
    Eden-Linux-v0.2.1-amd64-gcc-standard.AppImage \
    https://stable.eden-emu.dev/v0.2.1/Eden-Linux-v0.2.1-amd64-gcc-standard.AppImage \
    2fae658397daf13c118082a3eb65d61a6519967b5e22e6667756baecf6000c5a)
install_direct "$eden_linux" "$root/Runtime/Linux/Eden.AppImage"

pcsx2_linux=$(fetch \
    pcsx2-v2.8.2-linux-appimage-x64-Qt.AppImage \
    https://github.com/PCSX2/pcsx2/releases/download/v2.8.2/pcsx2-v2.8.2-linux-appimage-x64-Qt.AppImage \
    0c46bb6a88aa2782b10853a7b07cf3387ba99cbef2b966372cd2315b8571abea)
install_direct "$pcsx2_linux" "$root/Runtime/Linux/PCSX2.AppImage"

xemu_linux=$(fetch \
    xemu-0.8.136-x86_64.AppImage \
    https://github.com/xemu-project/xemu/releases/download/v0.8.136/xemu-0.8.136-x86_64.AppImage \
    ac77363a599109194ba2af3caa695348d199f515cf1d0fde091beb629e0c3103)
install_direct "$xemu_linux" "$root/Runtime/Linux/xemu.AppImage"

vita3k_linux=$(fetch \
    Vita3K-4115-x86_64.AppImage \
    https://github.com/Vita3K/Vita3K-builds/releases/download/4115/Vita3K-x86_64.AppImage \
    ffce3720027ff8cee3505e25b6264eda12c6457f7ba31cecae26a90f87caf48e)
install_direct "$vita3k_linux" "$root/Runtime/Linux/Vita3K.AppImage"

echo "Installed and verified all pinned supplementary Windows and Linux backends."
