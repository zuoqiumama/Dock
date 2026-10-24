use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 256 * 1024;

pub fn write(context: &str, error: &dyn std::fmt::Display) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let entry = format!("[{timestamp}] {context}: {error}\n");
    let primary = crate::config::config_path()
        .parent()
        .map(|directory| directory.join("featherdock.log"));
    let fallback = std::env::current_exe().ok().and_then(|executable| {
        executable
            .parent()
            .map(|directory| directory.join("outputs").join("featherdock.log"))
    });
    write_to_paths(primary.into_iter().chain(fallback), &entry);
}

fn write_to_paths(paths: impl IntoIterator<Item = PathBuf>, entry: &str) {
    for path in paths {
        if append(&path, entry).is_ok() {
            return;
        }
    }
}

fn append(path: &Path, entry: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let truncate = fs::metadata(path)
        .map(|metadata| metadata.len() >= MAX_LOG_BYTES)
        .unwrap_or(false);
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(!truncate)
        .truncate(truncate)
        .open(path)?;
    file.write_all(entry.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_primary_log_falls_back_and_oversized_logs_are_truncated() {
        let directory = std::env::temp_dir().join(format!(
            "featherdock-error-log-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        // A file in place of the directory deterministically rejects creation,
        // including when the test process has administrator privileges.
        let blocked = directory.join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let primary = blocked.join("featherdock.log");
        let fallback = directory.join("outputs").join("featherdock.log");
        write_to_paths([primary, fallback.clone()], "first failure\n");
        assert_eq!(fs::read_to_string(&fallback).unwrap(), "first failure\n");

        fs::write(&fallback, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        append(&fallback, "new failure\n").unwrap();
        assert_eq!(fs::read_to_string(&fallback).unwrap(), "new failure\n");
        fs::remove_dir_all(directory).unwrap();
    }
}
