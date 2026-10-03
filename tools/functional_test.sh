#!/usr/bin/env bash
# Runs RetroPort's functional suite: the real binary against private copies
# of this assembled installation, real games from their publishers, and real
# emulators on a virtual display. Pass test names to run a subset.
set -euo pipefail

project=$(cd "$(dirname "$0")/.." && pwd)
work="$project/target/functional"
mkdir -p "$work"

for command in 7z ffmpeg curl wine wineboot wineserver; do
    if ! command -v "$command" >/dev/null; then
        echo "missing required command: $command" >&2
        exit 1
    fi
done
if ! command -v Xvfb >/dev/null && [[ ! -x "$work/tools/usr/bin/Xvfb" ]]; then
    echo "missing Xvfb (Debian/Ubuntu: xvfb, Arch: xorg-server-xvfb, Fedora: xorg-x11-server-Xvfb)" >&2
    exit 1
fi

# A Wine prefix for the tests alone, prepared unattended: Wine's Mono and
# Gecko download prompts cannot be answered on a virtual display.
export XDG_DATA_HOME="$work/xdg-data"
prefix="$XDG_DATA_HOME/retrobat-portable/wine-prefix"
if [[ ! -f "$prefix/.retroport-prefix-ready" ]]; then
    rm -rf "$prefix"
    mkdir -p "$prefix"
    WINEPREFIX="$prefix" WINEDLLOVERRIDES="mscoree,mshtml=" wineboot --init
    WINEPREFIX="$prefix" wineserver --wait
    mono=$(ls -1 "${XDG_CACHE_HOME:-$HOME/.cache}"/wine/wine-mono-*.msi 2>/dev/null | sort -V | tail -1 || true)
    if [[ -n "$mono" ]]; then
        WINEPREFIX="$prefix" wine msiexec /i "$mono" /qn
        WINEPREFIX="$prefix" wineserver --wait
    elif [[ ! -d /usr/share/wine/mono ]]; then
        echo "note: no Wine Mono found; PS2 and other EmulatorLauncher systems will report it" >&2
    fi
    echo "prepared by tools/functional_test.sh" > "$prefix/.retroport-prefix-ready"
fi

cargo build --release --manifest-path "$project/Cargo.toml"
cargo test --release --manifest-path "$project/Cargo.toml" --test functional \
    -- --ignored --test-threads=1 "$@"
