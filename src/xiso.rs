//! Disc images an emulator cannot read as they are. Cxbx-Reloaded starts an
//! original-Xbox game from its `default.xbe`; RetroBat can only hand it a
//! disc image (XDVDFS) by mounting it through the Dokan filesystem driver,
//! which a portable folder cannot install and Wine cannot provide, so
//! RetroPort unpacks the disc itself. Play! reads no gzip-compressed PS2
//! images, so RetroPort decompresses those.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use thiserror::Error;

const SECTOR: u64 = 2048;
const MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";
/// Where the game partition begins: rebuilt images at 0, then full dumps of
/// each disc generation (XGD1, XGD2, XGD3).
const PARTITION_OFFSETS: [u64; 4] = [0, 0x0FD9_0000, 0x1830_0000, 0x0208_0000];
/// Directory tables of real discs are a few sectors; this bounds the memory
/// a damaged or hostile image can claim.
const MAX_DIRECTORY_BYTES: u32 = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const COMPLETE: &str = ".retroport-unpacked";

#[derive(Debug, Error)]
pub enum XisoError {
    #[error("{0} is not an original-Xbox disc image (no XDVDFS volume found)")]
    NotXbox(PathBuf),
    #[error("the Xbox disc image {path} is damaged: {message}")]
    Damaged { path: PathBuf, message: String },
    #[error("the Xbox disc image has no default.xbe to start")]
    NoDefaultXbe,
    #[error("could not unpack the Xbox disc image: {0}")]
    Io(#[from] io::Error),
}

/// The byte offset of the game partition, if `image` is an Xbox disc.
pub fn game_partition(image: &Path) -> Result<Option<u64>, io::Error> {
    let mut file = File::open(image)?;
    let length = file.metadata()?.len();
    let mut magic = [0u8; 20];
    for base in PARTITION_OFFSETS {
        if base + 33 * SECTOR > length {
            continue;
        }
        file.seek(SeekFrom::Start(base + 32 * SECTOR))?;
        file.read_exact(&mut magic)?;
        if &magic == MAGIC {
            return Ok(Some(base));
        }
    }
    Ok(None)
}

/// Unpacks `image` into `destination` once and returns its `default.xbe`.
pub fn unpack_cached(image: &Path, destination: &Path) -> Result<PathBuf, XisoError> {
    unpack_once(image, destination, "default.xbe", |staging| {
        unpack(image, staging)?;
        if !staging.join("default.xbe").is_file() {
            return Err(XisoError::NoDefaultXbe);
        }
        Ok(())
    })
}

/// Decompresses a gzip-compressed disc image (PCSX2's `.gz`) into
/// `destination` once and returns the plain image, for emulators such as
/// Play! that read only uncompressed images.
pub fn gunzip_cached(image: &Path, destination: &Path) -> Result<PathBuf, XisoError> {
    unpack_once(image, destination, "disc.iso", |staging| {
        let input = io::BufReader::with_capacity(1024 * 1024, File::open(image)?);
        let mut decoder = flate2::read::MultiGzDecoder::new(input);
        let mut output = BufWriter::with_capacity(
            1024 * 1024,
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(staging.join("disc.iso"))?,
        );
        io::copy(&mut decoder, &mut output).map_err(|error| XisoError::Damaged {
            path: image.to_owned(),
            message: format!("it is not a complete gzip image ({error})"),
        })?;
        output
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        Ok(())
    })
}

/// Runs `unpack` into a staging folder and moves it to `destination`. A
/// completed unpack is recorded with the image's size and modification
/// time, so a replaced image is unpacked again and an interrupted unpack is
/// never mistaken for a complete one.
fn unpack_once(
    image: &Path,
    destination: &Path,
    launch: &str,
    unpack: impl FnOnce(&Path) -> Result<(), XisoError>,
) -> Result<PathBuf, XisoError> {
    let metadata = fs::metadata(image)?;
    let stamp = format!(
        "{} {}\n",
        metadata.len(),
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_nanos())
    );
    let launch = destination.join(launch);
    if fs::read_to_string(destination.join(COMPLETE)).is_ok_and(|recorded| recorded == stamp)
        && launch.is_file()
    {
        return Ok(launch);
    }
    let staging = destination.with_extension("unpacking");
    for stale in [&staging, &destination.to_path_buf()] {
        match fs::remove_dir_all(stale) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    }
    if let Some(parent) = staging.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&staging)?;
    let result = unpack(&staging).and_then(|()| {
        fs::write(staging.join(COMPLETE), &stamp)?;
        fs::rename(&staging, destination)?;
        Ok(())
    });
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(launch)
}

/// Writes every file of `image` under `destination`, which must exist.
pub fn unpack(image: &Path, destination: &Path) -> Result<(), XisoError> {
    let base = game_partition(image)?.ok_or_else(|| XisoError::NotXbox(image.to_owned()))?;
    let mut file = File::open(image)?;
    let length = file.metadata()?.len();
    let damaged = |message: String| XisoError::Damaged {
        path: image.to_owned(),
        message,
    };
    file.seek(SeekFrom::Start(base + 32 * SECTOR + 20))?;
    let mut root = [0u8; 8];
    file.read_exact(&mut root)?;
    let root_sector = u32::from_le_bytes(root[..4].try_into().unwrap());
    let root_size = u32::from_le_bytes(root[4..].try_into().unwrap());

    let mut pending = vec![(root_sector, root_size, destination.to_owned(), 0usize)];
    while let Some((sector, size, directory, depth)) = pending.pop() {
        if depth > MAX_DEPTH {
            return Err(damaged("directories nest too deeply".to_owned()));
        }
        if size == 0 {
            continue;
        }
        if size > MAX_DIRECTORY_BYTES {
            return Err(damaged(format!("a directory table claims {size} bytes")));
        }
        let start = base + u64::from(sector) * SECTOR;
        if start + u64::from(size) > length {
            return Err(damaged(
                "a directory lies beyond the end of the image".to_owned(),
            ));
        }
        let mut table = vec![0u8; size as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut table)?;
        for entry in directory_entries(&table).map_err(damaged)? {
            let path = directory.join(&entry.name);
            if entry.is_directory {
                fs::create_dir(&path)?;
                pending.push((entry.sector, entry.size, path, depth + 1));
                continue;
            }
            let offset = base + u64::from(entry.sector) * SECTOR;
            if offset + u64::from(entry.size) > length {
                return Err(damaged(format!(
                    "{} lies beyond the end of the image",
                    entry.name
                )));
            }
            file.seek(SeekFrom::Start(offset))?;
            let mut output = BufWriter::with_capacity(
                1024 * 1024,
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?,
            );
            let copied = io::copy(&mut (&mut file).take(u64::from(entry.size)), &mut output)?;
            if copied != u64::from(entry.size) {
                return Err(damaged(format!("{} is truncated", entry.name)));
            }
            output
                .into_inner()
                .map_err(io::IntoInnerError::into_error)?;
        }
    }
    Ok(())
}

struct Entry {
    name: String,
    sector: u32,
    size: u32,
    is_directory: bool,
}

/// A directory table is a binary search tree of 4-byte-aligned entries;
/// each names its left and right subtrees by offset in 4-byte units.
fn directory_entries(table: &[u8]) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut pending = vec![0usize];
    while let Some(offset) = pending.pop() {
        if !visited.insert(offset) {
            return Err("a directory table links back on itself".to_owned());
        }
        let header = table
            .get(offset..offset + 14)
            .ok_or("a directory entry lies outside its table")?;
        let left = u16::from_le_bytes([header[0], header[1]]);
        let right = u16::from_le_bytes([header[2], header[3]]);
        if left == 0xFFFF {
            // Unused space; an empty directory's table is nothing else.
            continue;
        }
        let sector = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let size = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let attributes = header[12];
        let name_length = usize::from(header[13]);
        let name = table
            .get(offset + 14..offset + 14 + name_length)
            .ok_or("a file name lies outside its directory table")?;
        // XDVDFS names are single-byte; Latin-1 maps each byte to a char.
        let name = name
            .iter()
            .map(|&byte| char::from(byte))
            .collect::<String>();
        if name.is_empty()
            || name == "."
            || name == ".."
            || name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':'))
        {
            return Err(format!("the file name {name:?} is not safe to create"));
        }
        entries.push(Entry {
            name,
            sector,
            size,
            is_directory: attributes & 0x10 != 0,
        });
        for child in [left, right] {
            if child != 0 {
                pending.push(usize::from(child) * 4);
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        left: u16,
        right: u16,
        sector: u32,
        size: u32,
        directory: bool,
        name: &str,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(left.to_le_bytes());
        bytes.extend(right.to_le_bytes());
        bytes.extend(sector.to_le_bytes());
        bytes.extend(size.to_le_bytes());
        bytes.push(if directory { 0x10 } else { 0x20 });
        bytes.push(name.len() as u8);
        bytes.extend(name.as_bytes());
        while bytes.len() % 4 != 0 {
            bytes.push(0xFF);
        }
        bytes
    }

    #[test]
    fn directory_tables_are_walked_as_trees_and_reject_loops_and_unsafe_names() {
        // default.xbe's entry is 25 bytes, padded to 28, so its right
        // subtree, media/, starts at unit 7.
        let mut table = entry(0, 7, 40, 100, false, "default.xbe");
        table.extend(entry(0, 0, 41, 2048, true, "media"));
        let names = directory_entries(&table)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.name, entry.is_directory))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("default.xbe".to_owned(), false),
                ("media".to_owned(), true)
            ]
        );

        // a.xbe's entry is 19 bytes, padded to 20 (unit 5); b.xbe naming
        // itself as its left subtree would loop forever.
        let mut cycle = entry(0, 5, 40, 1, false, "a.xbe");
        cycle.extend(entry(0, 0, 41, 1, false, "b.xbe"));
        cycle[20..22].copy_from_slice(&5u16.to_le_bytes());
        assert!(directory_entries(&cycle).is_err());

        assert!(directory_entries(&entry(0, 0, 40, 1, false, "..")).is_err());
        assert!(directory_entries(&entry(0, 0, 40, 1, false, "a/b")).is_err());
        assert!(directory_entries(&[0xFF; 2048]).unwrap().is_empty());
    }
}
