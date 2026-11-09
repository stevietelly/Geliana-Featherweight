use std::path::PathBuf;

/// Returns the output directory (`<Videos>/Featherweight`), creating it if needed.
/// Falls back to the current directory if the Videos folder is unavailable.
pub fn output_dir() -> PathBuf {
    let base = dirs::video_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("Featherweight");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Returns a unique output path of the form `featherweight-YYYYMMDD-HHMMSS[-N].mp4`.
/// Increments the suffix until a non-existing filename is found.
pub fn output_path(dir: &std::path::Path) -> PathBuf {
    let now = chrono::Local::now();
    let stem = now.format("featherweight-%Y%m%d-%H%M%S").to_string();

    let candidate = dir.join(format!("{stem}.mp4"));
    if !candidate.exists() {
        return candidate;
    }

    for n in 1u32.. {
        let candidate = dir.join(format!("{stem}-{n}.mp4"));
        if !candidate.exists() {
            return candidate;
        }
    }

    // Unreachable in practice
    dir.join(format!("{stem}.mp4"))
}
