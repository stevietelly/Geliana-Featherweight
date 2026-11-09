use std::path::Path;

use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::core::HSTRING;

/// 1 GiB — recording stops if free space drops to or below this.
pub const HARD_FLOOR_BYTES: u64 = 1024 * 1024 * 1024;

/// 3 GiB — icon turns amber and status line is prefixed with "Low disk" below this.
pub const WARN_FLOOR_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// Returns the effective hard floor in bytes, respecting the
/// `FEATHERWEIGHT_MIN_FREE_MB` override environment variable.
pub fn hard_floor() -> u64 {
    if let Ok(val) = std::env::var("FEATHERWEIGHT_MIN_FREE_MB") {
        if let Ok(mb) = val.trim().parse::<u64>() {
            return mb * 1024 * 1024;
        }
    }
    HARD_FLOOR_BYTES
}

/// Queries free bytes available to the current user on the volume containing `path`.
/// Returns `None` on failure (path does not exist, API error, etc.).
pub fn free_bytes_for_path(path: &Path) -> Option<u64> {
    // We need a null-terminated UTF-16 path string pointing to the volume.
    // Using the directory of the file gives us the right volume.
    let dir = path.parent().unwrap_or(path);
    let wide = HSTRING::from(dir.as_os_str());

    let mut free_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_free: u64 = 0;

    let ok = unsafe {
        GetDiskFreeSpaceExW(
            &wide,
            Some(&mut free_bytes),
            Some(&mut total_bytes),
            Some(&mut total_free),
        )
    };

    if ok.is_ok() { Some(free_bytes) } else { None }
}

/// Returns a human-readable string like "41.2 GB".
pub fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

/// Rolling growth-rate estimator for the output file.
/// Sampled each tick; keeps a 10-sample window (~10 seconds at 1 Hz).
pub struct GrowthEstimator {
    samples: std::collections::VecDeque<(std::time::Instant, u64)>,
}

impl GrowthEstimator {
    pub fn new() -> Self {
        Self { samples: std::collections::VecDeque::with_capacity(11) }
    }

    /// Record the current file size. Returns estimated bytes-per-second, or `None` if
    /// the window is too small to estimate.
    pub fn push(&mut self, size: u64) -> Option<f64> {
        let now = std::time::Instant::now();
        self.samples.push_back((now, size));
        if self.samples.len() > 10 {
            self.samples.pop_front();
        }
        if self.samples.len() < 2 {
            return None;
        }
        let (t0, s0) = self.samples.front()?;
        let (t1, s1) = self.samples.back()?;
        let elapsed = t1.duration_since(*t0).as_secs_f64();
        if elapsed < 1.0 || s1 <= s0 {
            return None;
        }
        Some((s1 - s0) as f64 / elapsed)
    }
}
