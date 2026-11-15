//! Past-recordings listing for the Recording view.
//!
//! Scans the configured output directory for finished captures and returns
//! them newest-first, so the window can show what has already been recorded
//! below the live monitor.
//!
//! Contents:
//! - [`RecordingEntry`]: one finished recording (path, name, size, mtime).
//! - [`scan_recordings`]: the directory scan and sort.

use std::path::{Path, PathBuf};

/// One finished recording found in the output directory.
#[derive(Debug, Clone)]
pub(crate) struct RecordingEntry {
    /// Full path to the `.mp4` file (ready to open or hand to a player).
    pub path: PathBuf,
    /// File name including extension, for display.
    pub name: String,
    /// Size on disk in bytes.
    pub size_bytes: u64,
    /// Last modification time; `None` when the filesystem did not report one.
    /// Newest first in the list, so this doubles as the sort key.
    pub modified: Option<std::time::SystemTime>,
}

/// Lists the finished recordings in `dir`, newest first.
///
/// Only regular `*.mp4` files count as recordings: the recorder stages its
/// output as `<name>.temp.mp4` and swaps it in on stop, so staging files are
/// skipped, as are directories and anything with another extension. A
/// missing or unreadable directory yields an empty list rather than an
/// error — the view shows an empty state, not a failure.
pub(crate) fn scan_recordings(dir: &Path) -> Vec<RecordingEntry> {
    let mut entries = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return entries;
    };

    for item in read_dir.flatten() {
        let path = item.path();
        // A plain metadata call follows symlinks; only regular files list.
        let Ok(meta) = item.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.to_ascii_lowercase().ends_with(".mp4") {
            continue;
        }
        // The recorder's staging file `<name>.temp.mp4` is not a recording.
        if name.to_ascii_lowercase().ends_with(".temp.mp4") {
            continue;
        }
        entries.push(RecordingEntry {
            path: path.clone(),
            name: name.to_owned(),
            size_bytes: meta.len(),
            modified: meta.modified().ok(),
        });
    }

    entries.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.name.cmp(&a.name))
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// Writes a one-byte file and backdates it, since scan order is mtime.
    fn touch(path: &Path, when: SystemTime) {
        std::fs::write(path, b"x").unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    #[test]
    fn lists_only_final_mp4_files_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        touch(&dir.path().join("Recording_old.mp4"), base);
        touch(&dir.path().join("Recording_new.mp4"), base + Duration::from_secs(60));
        touch(&dir.path().join("Recording_live.temp.mp4"), base + Duration::from_secs(120));
        touch(&dir.path().join("notes.txt"), base);
        std::fs::create_dir(dir.path().join("folder.mp4")).unwrap();

        let found = scan_recordings(dir.path());
        let names: Vec<&str> = found.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Recording_new.mp4", "Recording_old.mp4"]);
        assert_eq!(found[0].size_bytes, 1);
        assert!(found[0].modified.is_some());
    }

    #[test]
    fn missing_or_empty_dir_is_empty_list() {
        assert!(scan_recordings(Path::new("Z:\\no-such-dir-featherweight")).is_empty());

        let dir = tempfile::tempdir().unwrap();
        assert!(scan_recordings(dir.path()).is_empty());
    }
}
