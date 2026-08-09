//! Crash-safe file writes: stage the bytes in a sibling temp file, fsync, then replace
//! the target with a single `MoveFileExW` (write-through) so a reader never observes a
//! half-written file. Power loss or a kill mid-write leaves either the old file or the new
//! one — never a truncated mix. Shared by every persisted file (config, settings, drawer
//! categories, drawer cache).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Atomically write `bytes` to `path`, creating the parent directory if needed.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;

    let temp = parent.join(format!(
        ".featherdock-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    let from = wide_path(&temp);
    let to = wide_path(path);
    let result = unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        return Err(io::Error::other(error.to_string()));
    }
    let _ = File::open(parent).and_then(|dir| dir.sync_all());
    Ok(())
}

/// A stable 128-bit hex digest (FNV-1a over two seeds) of an arbitrary string —
/// used to derive collision-resistant, filesystem-safe file names from entry keys.
/// Deliberately dependency-free: the project ships no hash crate.
pub fn hash_hex(value: &str) -> String {
    let mut hi: u64 = 0xcbf29ce484222325;
    let mut lo: u64 = 0x84222325cbf29ce4;
    for byte in value.as_bytes() {
        hi ^= *byte as u64;
        hi = hi.wrapping_mul(0x100000001b3);
        lo ^= (*byte as u64).rotate_left(5) ^ 0x9e3779b97f4a7c15;
        lo = lo.wrapping_mul(0x100000001b3);
    }
    format!("{hi:016x}{lo:016x}")
}
