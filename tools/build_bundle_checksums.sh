#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 BUNDLE_ROOT" >&2
    exit 2
fi

root=$(realpath "$1")
output="$root/SHA256SUMS"
temporary="$root/.SHA256SUMS.tmp.$$"

cd "$root"
runtime_files=(
    RetroPort-Linux \
    RetroPort.exe \
    RetroPort-Linux.desktop \
    README-FIRST.txt \
    THIRD-PARTY-ASSETS.txt \
    LICENSE-RETROPORT.txt \
    VERIFY-LINUX.sh \
    VERIFY-WINDOWS.cmd \
    VERIFY-WINDOWS.ps1 \
    Source/RetroPort-source.zip \
    RetroBat/emulators/retroarch/cores/jaxe_libretro.dll \
    RetroBat/emulators/xenia-canary/xenia_canary.exe \
    RetroBat/emulators/xenia-canary/LICENSE-XENIA.txt \
    RetroBat/RetroBat.exe \
    RetroBat/emulationstation/emulationstation.exe \
    RetroBat/emulationstation/emulatorLauncher.exe \
    RetroBat/emulators/play/Play.exe
)
for required in "${runtime_files[@]}"
do
    if [[ ! -f "$required" ]]; then
        echo "missing required bundle file: $required" >&2
        exit 1
    fi
done

static_list=$(mktemp)
trap 'rm -f "$static_list"' EXIT
printf '%s\0' "${runtime_files[@]}" > "$static_list"

for directory in \
    Runtime/Linux \
    RetroBat/emulators/play/platforms \
    RetroBat/emulators/play/imageformats \
    RetroBat/emulators/play/styles \
    RetroBat/emulators/play/arcadedefs \
    RetroBat/emulators/eden/LICENSES \
    RetroBat/emulators/cxbx-reloaded/hlsl \
    RetroBat/emulators/rpcs3/Icons \
    RetroBat/emulators/rpcs3/qt6 \
    RetroBat/emulators/rpcs3/test \
    RetroBat/emulators/cemu/gameProfiles \
    RetroBat/emulators/cemu/resources
do
    if [[ -d "$directory" ]]; then
        find "$directory" -type f -print0 >> "$static_list"
    fi
done

for directory in \
    RetroBat/emulators/play \
    RetroBat/emulators/eden \
    RetroBat/emulators/cxbx-reloaded \
    RetroBat/emulators/rpcs3 \
    RetroBat/emulators/cemu \
    RetroBat/emulators/shadps4
do
    if [[ -d "$directory" ]]; then
        find "$directory" -maxdepth 1 -type f \
            ! -name 'settings.ini' \
            ! -name 'RPCS3.buf' \
            ! -name 'portable.txt' \
            ! -name '*.installer-bak*' \
            -print0 >> "$static_list"
    fi
done

if [[ -d Artwork ]]; then
    find Artwork -type f -print0 >> "$static_list"
fi

sort -zu "$static_list" | xargs -0 -r sha256sum > "$temporary"

mv "$temporary" "$output"
echo "Wrote $(wc -l < "$output") verified checksums to $output"
