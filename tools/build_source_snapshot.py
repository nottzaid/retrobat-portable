#!/usr/bin/env python3
"""Build the source snapshot shipped beside the portable executables."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import zipfile


PROJECT_ROOT = Path(__file__).resolve().parent.parent
SOURCE_TREES = (".cargo", "catalog", "src", "tests", "tools")
ROOT_FILES = (
    ".gitignore",
    "Cargo.lock",
    "Cargo.toml",
    "LICENSE",
    "README.md",
    "rust-toolchain.toml",
)


def source_files() -> list[Path]:
    files: set[Path] = set()
    for tree in SOURCE_TREES:
        for path in (PROJECT_ROOT / tree).rglob("*"):
            if (
                path.is_file()
                and "__pycache__" not in path.parts
                and path.suffix != ".pyc"
            ):
                files.add(path)
    files.update(path for path in (PROJECT_ROOT / name for name in ROOT_FILES) if path.is_file())
    files.update(
        path
        for path in (PROJECT_ROOT / "packaging").iterdir()
        if path.is_file() and path.name != "SHA256SUMS"
    )
    return sorted(files, key=lambda path: path.relative_to(PROJECT_ROOT).as_posix())


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_name(f".{output.name}.tmp.{os.getpid()}")

    files = source_files()
    try:
        with zipfile.ZipFile(
            temporary,
            mode="w",
            compression=zipfile.ZIP_DEFLATED,
            compresslevel=9,
        ) as archive:
            for path in files:
                archive.write(path, path.relative_to(PROJECT_ROOT).as_posix())
        os.replace(temporary, output)
    finally:
        temporary.unlink(missing_ok=True)
    print(f"Wrote {len(files)} source files to {output}")


if __name__ == "__main__":
    main()
