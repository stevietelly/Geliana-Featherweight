//! Disk-space queries and the output-growth estimator.
//!
//! Everything here is UI-agnostic: the window renders the numbers, the tray
//! renders them again in its status line, and the session pump is what actually
//! produces them while the window is hidden.

use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::core::HSTRING;

/// 1 GiB — recording stops if free space drops to or below this.
pub const HARD_FLOOR_BYTES: u64 = 1024 * 1024 * 1024;

/// 3 GiB — icon turns amber and status line is prefixed with "Low disk" below this.
pub const WARN_FLOOR_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// Warn when the measured growth rate is fast enough to reach the hard floor
/// within this many seconds, even if absolute free space still looks healthy.
///
/// A fixed byte threshold alone makes the warning timing depend entirely on the
/// bitrate: 3 GiB is ~34 minutes at 12 Mbps but ~4 minutes at 100 Mbps.
pub const WARN_ETA_SECS: u64 = 5 * 60;

/// Number of samples the growth window keeps. At the recorder's 1 Hz tick this
/// is ~10 seconds, which smooths encoder flush bursts without lagging far
/// behind the current rate.
const SAMPLE_WINDOW: usize = 10;

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

/// Returns a human-readable string like "41.2 GB" or "86.4 MB".
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Returns a human-readable byte rate like "1.5 MB/s".
pub fn format_rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "—".to_owned();
    }
    format!("{}/s", format_bytes(bytes_per_sec.round() as u64))
}

/// Formats a remaining-time estimate: "~4 min left", "~1 h 20 min left".
///
/// Sub-minute estimates round up so a recording that is about to be stopped
/// never shows "0 min left".
pub fn format_eta(secs: u64) -> String {
    let minutes = (secs.saturating_add(59) / 60).max(1);
    if minutes >= 60 {
        format!("~{} h {} min left", minutes / 60, minutes % 60)
    } else {
        format!("~{minutes} min left")
    }
}

/// Seconds of recording left before `free` falls to `floor`, at the measured
/// `bytes_per_sec`. `None` when there is no usable rate to extrapolate from.
pub fn remaining_secs(free: u64, floor: u64, bytes_per_sec: f64) -> Option<u64> {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return None;
    }
    let headroom = free.saturating_sub(floor);
    Some((headroom as f64 / bytes_per_sec).round() as u64)
}

/// Rolling growth-rate estimator for the output file.
///
/// Sampled once per recorder tick (1 Hz); keeps a [`SAMPLE_WINDOW`]-sample
/// window and reports the average bytes-per-second across it. The whole thing
/// is ten `(Instant, u64)` pairs, so it can live for a whole session at no
/// meaningful cost.
pub struct GrowthEstimator {
    samples: VecDeque<(Instant, u64)>,
}

impl GrowthEstimator {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(SAMPLE_WINDOW + 1),
        }
    }

    /// Drops all samples. Called when a new session starts, and automatically
    /// when the file shrinks (see [`GrowthEstimator::push_at`]).
    pub fn reset(&mut self) {
        self.samples.clear();
    }

    /// Record the current file size sampled at `now`.
    ///
    /// The sample instant is passed in rather than read here so the caller can
    /// timestamp the *measurement*: the recorder reads the size and the clock
    /// on its own thread, and the event thread may handle the tick late on a
    /// busy machine. Timing at handling time would under-report the rate
    /// exactly when the machine is struggling.
    ///
    /// Returns estimated bytes-per-second, or `None` if the window is too small
    /// to estimate or shows no growth.
    pub fn push_at(&mut self, now: Instant, size: u64) -> Option<f64> {
        // A shrinking file means the path now refers to a different file (the
        // separate-track mux swaps <name>.temp.mp4 for the real output) or the
        // volume was changed underneath us. Samples that describe the old file
        // must not be averaged into the new one.
        if let Some((_, last)) = self.samples.back() {
            if size < *last {
                self.reset();
            }
        }

        self.samples.push_back((now, size));
        if self.samples.len() > SAMPLE_WINDOW {
            self.samples.pop_front();
        }

        let (t0, s0) = *self.samples.front()?;
        let (t1, s1) = *self.samples.back()?;
        let elapsed = t1.duration_since(t0).as_secs_f64();

        // Fewer than two useful samples, or no growth at all across the whole
        // window (encoder buffering at session start, or a stalled writer).
        // Reporting `None` here keeps the UI honest instead of extrapolating.
        if elapsed < 1.0 || s1 <= s0 {
            return None;
        }
        Some((s1 - s0) as f64 / elapsed)
    }
}

impl Default for GrowthEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Fixed base so sample intervals are exact. Calling `Instant::now()` per
    /// sample would add microseconds of drift and skew every rate.
    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn needs_two_samples() {
        let base = Instant::now();
        let mut growth = GrowthEstimator::new();
        assert_eq!(growth.push_at(at(base, 0), 0), None);
        // Same timestamp: no elapsed time, so still no estimate.
        assert_eq!(growth.push_at(at(base, 0), 2_000_000), None);
    }

    #[test]
    fn reports_average_rate_across_the_window() {
        let base = Instant::now();
        let mut growth = GrowthEstimator::new();
        // 2 MB/s for five seconds.
        for second in 0..=5 {
            let rate = growth.push_at(at(base, second), second * 2_000_000);
            if second == 0 {
                assert_eq!(rate, None, "a single sample cannot produce a rate");
            }
        }
        let rate = growth.push_at(at(base, 6), 12_000_000).expect("rate");
        assert!((rate - 2_000_000.0).abs() < 1.0, "got {rate}");
    }

    #[test]
    fn window_is_bounded_and_old_samples_age_out() {
        let base = Instant::now();
        let mut growth = GrowthEstimator::new();
        // 100 samples at 1 MB/s: the window must not grow past SAMPLE_WINDOW.
        for second in 0..100 {
            growth.push_at(at(base, second), second * 1_000_000);
        }
        assert_eq!(growth.samples.len(), SAMPLE_WINDOW);
        // The rate reflects the retained window (~9s), not the whole session.
        let rate = growth.push_at(at(base, 100), 100 * 1_000_000).expect("rate");
        assert!((rate - 1_000_000.0).abs() < 1.0, "got {rate}");
    }

    #[test]
    fn stalled_writer_reports_none() {
        let base = Instant::now();
        let mut growth = GrowthEstimator::new();
        for second in 0..3 {
            growth.push_at(at(base, second), 1_000_000);
        }
        // No growth across the whole window.
        assert_eq!(growth.push_at(at(base, 4), 1_000_000), None);
    }

    #[test]
    fn shrinking_file_resets_the_window() {
        let base = Instant::now();
        let mut growth = GrowthEstimator::new();
        growth.push_at(at(base, 0), 8_000_000);
        growth.push_at(at(base, 1), 10_000_000);
        // The temp file was replaced by the final one and is smaller.
        assert_eq!(growth.push_at(at(base, 2), 1_000), None);
        assert_eq!(growth.samples.len(), 1);
        // Growth after the reset is measured from the new file only.
        let rate = growth.push_at(at(base, 3), 2_001_000).expect("rate");
        assert!((rate - 2_000_000.0).abs() < 1.0, "got {rate}");
    }

    #[test]
    fn eta_counts_down_to_the_floor_not_to_zero() {
        let floor = HARD_FLOOR_BYTES;
        // 3 GiB free, 1 GiB floor, 1 MiB/s => 2048 s of headroom.
        let eta = remaining_secs(3 * 1024 * 1024 * 1024, floor, 1024.0 * 1024.0);
        assert_eq!(eta, Some(2048));
        // Already below the floor: zero seconds left, never negative.
        assert_eq!(remaining_secs(0, floor, 1024.0 * 1024.0), Some(0));
        // No usable rate.
        assert_eq!(remaining_secs(4 * 1024 * 1024 * 1024, floor, 0.0), None);
        assert_eq!(remaining_secs(4 * 1024 * 1024 * 1024, floor, f64::NAN), None);
    }

    #[test]
    fn formatting_is_stable() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(86_400_000), "82.4 MB");
        assert_eq!(format_rate(1_500_000.0), "1.4 MB/s");
        assert_eq!(format_rate(0.0), "—");
        assert_eq!(format_eta(59), "~1 min left");
        assert_eq!(format_eta(60), "~1 min left");
        assert_eq!(format_eta(3600), "~1 h 0 min left");
        assert_eq!(format_eta(4800), "~1 h 20 min left");
    }
}
