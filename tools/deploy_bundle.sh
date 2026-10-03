#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 BUNDLE_ROOT" >&2
    exit 2
fi

project=$(cd "$(dirname "$0")/.." && pwd)
bundle=$(realpath "$1")
linux_binary="$project/target/x86_64-unknown-linux-gnu/release/retrobat-portable"
windows_binary="$project/target/x86_64-pc-windows-msvc/release/retrobat-portable.exe"

for binary in "$linux_binary" "$windows_binary"; do
    if [[ ! -f "$binary" ]]; then
        echo "missing release build: $binary" >&2
        exit 1
    fi
done
for required in \
    "$project/RetroBat/RetroBat.exe" \
    "$project/RetroBat/emulationstation/emulatorLauncher.exe" \
    "$project/RetroBat/emulationstation/.emulationstation/es_systems.cfg"
do
    if [[ ! -f "$required" ]]; then
        echo "local source bundle is incomplete: missing $required" >&2
        exit 1
    fi
done

assemble_launchers_and_documents() {
    local root=$1
    mkdir -p "$root/Source" "$root/Artwork"
    python3 "$project/tools/build_source_snapshot.py" "$root/Source/RetroPort-source.zip"
    cp -f "$linux_binary" "$root/RetroPort-Linux"
    cp -f "$windows_binary" "$root/RetroPort.exe"
    cp -f "$project/packaging/RetroPort-Linux.desktop" "$root/RetroPort-Linux.desktop"
    cp -f "$project/packaging/README-FIRST.txt" "$root/README-FIRST.txt"
    cp -f "$project/packaging/THIRD-PARTY-ASSETS.txt" "$root/THIRD-PARTY-ASSETS.txt"
    cp -f "$project/packaging/VERIFY-LINUX.sh" "$root/VERIFY-LINUX.sh"
    cp -f "$project/packaging/VERIFY-WINDOWS.cmd" "$root/VERIFY-WINDOWS.cmd"
    cp -f "$project/packaging/VERIFY-WINDOWS.ps1" "$root/VERIFY-WINDOWS.ps1"
    cp -f "$project/LICENSE" "$root/LICENSE-RETROPORT.txt"
    rsync -a "$project/packaging/Artwork/" "$root/Artwork/"
    chmod +x "$root/RetroPort-Linux" "$root/VERIFY-LINUX.sh"
}

# The source checkout is itself the canonical, complete local bundle.
assemble_launchers_and_documents "$project"
"$project/tools/build_bundle_checksums.sh" "$project"

if [[ "$bundle" != "$project" ]]; then
    for directory in RetroBat Runtime Artwork; do
        if [[ ! -d "$project/$directory" ]]; then
            echo "local source bundle is incomplete: missing $project/$directory" >&2
            exit 1
        fi
    done
    mkdir -p "$bundle/.retrobat-portable"
    # 1. Static files: exactly those in the integrity manifest. They are
    #    pinned and verified, so the destination's copy is replaced.
    static_list=$(mktemp)
    trap 'rm -f "$static_list"' EXIT
    cut -c67- "$project/SHA256SUMS" > "$static_list"
    rsync -t --files-from="$static_list" "$project/" "$bundle/"
    # 2. Everything else in the runtime trees is added only where the
    #    destination lacks it. Games, saves, BIOS files, emulator state and
    #    .retrobat-portable at the destination are never deleted or
    #    overwritten.
    for directory in RetroBat Runtime Artwork; do
        rsync -rt --ignore-existing "$project/$directory/" "$bundle/$directory/"
    done
    chmod +x "$bundle/RetroPort-Linux" "$bundle/VERIFY-LINUX.sh"
    "$project/tools/build_bundle_checksums.sh" "$bundle"
fi

cp -f "$project/SHA256SUMS" "$project/packaging/SHA256SUMS"
echo "Assembled canonical local RetroPort at $project"
echo "Synchronized and checksummed RetroPort at $bundle"
