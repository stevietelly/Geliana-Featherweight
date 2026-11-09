/// audio/timeline.rs — per-source PCM accumulator placed by QPC timestamp.
///
/// Production spec section 4.3: every audio track must be a continuous,
/// gapless sequence of equal-sized chunks. Each source (app, mic) owns a
/// mutex-protected accumulator:
///
/// - On packet arrival the packet's start position (in frames, relative to
///   `T0`) is computed from its QPC time.
///   - Within 40 ms of the write head (small drift or jitter): just append,
///     trusting the sample count so a jittery timestamp does not cause
///     artifacts.
///   - Later than that: fill the gap with zeros, then append.
///   - Earlier (overlap): trim the overlapping samples.
///   - Starting before `T0`: trim or discard.
/// - The accumulator is bounded (2 seconds of audio). When full, the oldest
///   data is dropped with a warning.
///
/// Sample format inside the timeline: interleaved 16-bit PCM, 48 kHz, with a
/// fixed channel count per timeline (app = stereo, mic = mono). One "frame"
/// means one sample per channel; a 960-frame chunk is 20 ms.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use super::{ClockMap, SAMPLE_RATE};

/// Accumulator capacity: 2 seconds of audio (spec 4.3).
const MAX_FRAMES: i64 = 2 * SAMPLE_RATE as i64;

/// Drift window (spec 4.3): packet starts within 40 ms of the write head are
/// appended without age/overlap analysis.
const JITTER_FRAMES: i64 = 40 * SAMPLE_RATE as i64 / 1000;

/// A packet that arrived before `T0` was set. Buffered until the origin
/// arrives, then placed by its QPC position (packets arrive in QPC order, so
/// the pending list stays time-sorted by construction).
struct PendingPacket {
    qpc_100ns: i64,
    samples: Vec<i16>,
}

pub(super) struct Timeline {
    channels: u32,
    map: ClockMap,
    /// Interleaved samples; `buf[0]` is the first sample at `start_frame`.
    buf: Vec<i16>,
    /// Absolute frame index (relative to T0) of `buf[0]`. Meaningless while
    /// the buffer is empty.
    start_frame: i64,
    pending: Vec<PendingPacket>,
    warned_pending: bool,
    warned_full: bool,
}

/// Shared, thread-safe handle for one source timeline.
pub(super) type SharedTimeline = Arc<Mutex<Timeline>>;

impl Timeline {
    pub(super) fn new(channels: u32, map: ClockMap) -> Self {
        Self {
            channels,
            map,
            buf: Vec::new(),
            start_frame: 0,
            pending: Vec::new(),
            warned_pending: false,
            warned_full: false,
        }
    }

    /// Place a packet delivered at the given QPC position (already converted
    /// to 100 ns units on the same epoch as the video frame timestamps).
    pub(super) fn place(&mut self, qpc_100ns: i64, samples: &[i16]) {
        match self.map.video_clock_100ns(qpc_100ns) {
            None => {
                // T0 not set yet (between capture start and the first video
                // frame). Keep the packet; the clock drains it after T0.
                self.pending.push(PendingPacket {
                    qpc_100ns,
                    samples: samples.to_vec(),
                });
                // Bounded: more than ~1 s pending means the clock never
                // started; drop the oldest and warn once.
                let pending_frames: i64 = self
                    .pending
                    .iter()
                    .map(|p| (p.samples.len() / self.channels as usize) as i64)
                    .sum();
                if pending_frames > SAMPLE_RATE as i64 {
                    self.pending.remove(0);
                    if !self.warned_pending {
                        self.warned_pending = true;
                        let _ = crate::log::log(
                            "audio: timeline holds more than 1 s of packets from before T0; dropping pending packets",
                        );
                    }
                }
            }
            Some(rel_100ns) => {
                // The first placement after T0 also drains the pending list.
                while let Some(p) = self.pending.first() {
                    if let Some(rel) = self.map.video_clock_100ns(p.qpc_100ns) {
                        self.place_relative(rel, &p.samples);
                        self.pending.remove(0);
                    } else {
                        break;
                    }
                }
                self.place_relative(rel_100ns, samples);
            }
        }
    }

    /// Place a packet whose position relative to `T0` is known as a
    /// 100 ns span, trimming any portion that starts before `T0`.
    fn place_relative(&mut self, rel_100ns: i64, samples: &[i16]) {
        let ch = self.channels as usize;
        if samples.is_empty() {
            return;
        }
        let total_frames = (samples.len() / ch) as i64;

        if rel_100ns < 0 {
            // Packet starts before T0. Fully inside: discard; straddling the
            // origin: trim the lead and place the remainder (spec 4.3).
            let lead_frames = ((-rel_100ns as i128 * SAMPLE_RATE as i128 + 9_999_999)
                / 10_000_000i128) as i64;
            if lead_frames >= total_frames {
                return;
            }
            let rest = &samples[(lead_frames as usize) * ch..];
            self.extend(0, rest);
            return;
        }

        // floor: no negative jitter reaches here.
        let frames = (((rel_100ns as i64 as i128) * SAMPLE_RATE as i128) / 10_000_000i128) as i64;
        self.extend(frames, samples);
    }

    /// Append/merge a packet at the given absolute frame position, filling
    /// gaps with zeros and trimming overlaps (spec 4.3).
    fn extend(&mut self, at_frame: i64, samples: &[i16]) {
        let ch = self.channels as usize;
        if samples.is_empty() {
            return;
        }

        if self.buf.is_empty() {
            self.start_frame = at_frame;
            self.buf = samples.to_vec();
            self.enforce_capacity();
            return;
        }

        let buf_end = self.start_frame + (self.buf.len() / ch) as i64;
        let drift_frames = at_frame - buf_end;

        if drift_frames.abs() <= JITTER_FRAMES {
            // Small drift or jitter: just append at the write head.
            self.buf.extend_from_slice(samples);
        } else if drift_frames > 0 {
            // Clear hole: fill with zeros first.
            let gap = drift_frames as usize * ch;
            self.buf.resize(self.buf.len() + gap, 0);
            self.buf.extend_from_slice(samples);
        } else {
            // Overlap: trim the overlapping lead samples (spec 4.3).
            let swallowed = (-drift_frames) as usize; // frames of the packet the buffer already holds
            let packet_frames = (samples.len() / ch) as usize;
            if swallowed < packet_frames {
                self.buf.extend_from_slice(&samples[swallowed * ch..]);
            }
            // Fully swallowed packets contribute nothing.
        }
        self.enforce_capacity();
    }

    /// Trim the oldest samples so the buffer stays within capacity.
    fn enforce_capacity(&mut self) {
        let ch = self.channels as usize;
        let len_frames = (self.buf.len() / ch) as i64;
        if len_frames > MAX_FRAMES {
            let drop = (len_frames - MAX_FRAMES) as usize * ch;
            let new_start = self.start_frame + (drop / ch) as i64;
            self.buf.drain(..drop);
            self.start_frame = new_start;
            if !self.warned_full {
                self.warned_full = true;
                let _ =
                    crate::log::log("audio: source accumulator over 2 s; dropping oldest data");
            }
        }
    }

    /// Copy `[from, from + frames)`; missing samples come back as zeros.
    /// Returns interleaved samples with length exactly `frames * channels`,
    /// then trims everything the buffer holds up to `from`. The clock calls
    /// this with monotonically increasing positions.
    pub(super) fn take(&mut self, from_frame: i64, frames: u64) -> Vec<i16> {
        let ch = self.channels as usize;
        let mut out = vec![0i16; frames as usize * ch];

        if !self.buf.is_empty() {
            let buf_end = self.start_frame + (self.buf.len() / ch) as i64;
            let until = from_frame + frames as i64;
            if buf_end > from_frame && self.start_frame < until {
                // Overlap between the buffer and the requested window.
                let src_lo = (from_frame.max(self.start_frame) - self.start_frame) as usize * ch;
                let src_hi = (buf_end.min(until) - self.start_frame) as usize * ch;
                let dst_lo = (from_frame.max(self.start_frame) - from_frame) as usize * ch;
                let n = src_hi - src_lo;
                out[dst_lo..dst_lo + n].copy_from_slice(&self.buf[src_lo..src_hi]);
            }
        }

        // Trim everything up to (but not including) from_frame.
        if !self.buf.is_empty() {
            let buf_end = self.start_frame + (self.buf.len() / ch) as i64;
            if buf_end <= from_frame {
                self.buf.clear();
            } else if self.start_frame < from_frame {
                let drop = (from_frame - self.start_frame) as usize * ch;
                self.buf.drain(..drop);
            }
        }
        self.start_frame = from_frame;

        out
    }
}

// Ordering is used in ClockMap (imported here to keep the module API tidy).
#[allow(unused_imports)]
use std::sync::atomic::AtomicI64;

#[allow(dead_code)]
fn _compile_check(_: &AtomicI64, _: Ordering) {}
