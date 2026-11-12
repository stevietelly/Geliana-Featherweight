# Bug brief: video freeze (pool exhaustion) and silent audio

Audience: the engineer (or model) investigating and fixing these two bugs. This is a **recon-first** brief: gather evidence with the experiments in sections 2 and 3, then apply only the fix the evidence points to (section 4). Do not start by changing architecture.

## 0. Symptoms and rules

Observed by the owner, on the reference machine (i5-8365U, UHD 620) recording Valorant:

- After about 30 s the **video freezes**. The log shows `dropped_pool=1012` in a 104 s recording. Pool size is currently **64** NV12 slots.
- **No audible audio** in playback, although the log shows `chunks_written=5201, write_errors=0` (5201 chunks at 50 per second is 104 s).
- The warning `audio: source accumulator over 2 s; dropping oldest data` appears about 2 s into **every** recording.

Rules:

1. **Evidence before fixes.** Run the experiments, record the numbers, then choose the fix. One change at a time, re-measuring after each.
2. Keep all diagnostics behind an env var (`FW_DIAG=1`). Nothing per-frame goes to the log; aggregate once per second.
3. Report exact numbers and the logs. Say "not verified" where something was not run.
4. The three "fixes" in section 1 are **rejected**. Do not apply them.
5. A healthy run must show **zero** "dropping oldest data" warnings. Treat that warning as a test failure, not noise.

---

## 1. A previous analysis, and why not to build on it

An earlier write-up proposed: (a) copy frames to a CPU-side staging copy and release the slot immediately, (b) drop the pool and wrap WGC's own texture directly, (c) remove `IMFTrackedSample`. It also concluded the audio was fine and the problem was playback. Problems with that analysis:

1. **Its own arithmetic contradicts its conclusion.** It says the encoder holds 20-30 frames, but the pool has 64 slots. Holding 30 of 64 never exhausts the pool. Exhaustion at about 30 s means the number of held slots is **growing** (a leak, or a queue that keeps filling), not sitting at a steady hold depth.
2. **Fix (a) destroys the architecture.** A GPU-to-CPU copy of every frame (about 3 MB at 1080p, roughly 80 MB/s at 26 fps) adds a GPU sync per frame and moves the cost onto the CPU, which is exactly what the NV12 GPU path was built to avoid.
3. **Fix (b) reintroduces the original bug.** WGC recycles its frame textures after the callback returns. Handing that texture to an asynchronous encoder is the same race that produced corrupt frames before `IMFTrackedSample` was added.
4. **Fix (c) removes the thing that makes reuse safe**, so it can only trade a freeze for corruption.
5. **"The release callback needs the mux mutex" is inconsistent with its own flow**, which says the callback takes the pool mutex. The real hazard is different (section 4.1: lock hazards).
6. **`chunks_written=5201` proves nothing about audio content.** It counts chunks emitted, which may all be silence. The audio claim ("the file has 104 s of AAC, so it's a player selection problem") was never tested by measuring the tracks.
7. **The negative-timestamp explanation does not fit total silence.** Even if the first 2 s were discarded, audio would be audible after that. Persistent silence plus a 2 s overflow warning on every run points at something that keeps going wrong, such as a clock mismatch or a read position that never advances (section 4.2).

Also: 64 NV12 slots at 1080p is about **200 MB** of GPU memory (shared system RAM on this iGPU), which undoes the memory advantage the GPU path was chosen for. Do not raise the pool size to hide the symptom.

---

## 2. Instrumentation to add first (`FW_DIAG=1`)

### 2.1 Video and pool

Counters (atomics), logged **once per second**:

- `delivered`: frames received in the capture callback
- `acquired`: slots taken
- `released`: times the tracked-sample callback (`PoolAllocator::Invoke` or equivalent) fired
- `in_flight`: `acquired - released`, sampled each second, with the maximum seen
- `dropped_pool`: frames dropped because no slot was free
- `released_on_error`: slots released on failure paths (see below)
- `write_video_ms`: average and maximum time spent in the video `WriteSample` call, **including time waiting for the mux lock** (measure lock wait separately from the call itself)
- `invoke_thread_ms`: time `Invoke` spends inside, including any lock wait

Log line example: `diag video: delivered=26 acquired=26 released=24 in_flight=38 max_in_flight=38 dropped_pool=0 write_ms_max=1.3 lock_wait_ms_max=0.2`.

Also log once at startup: pool size, slot count and estimated GPU memory (width x height x 1.5 x slots).

**Failure-path audit.** After a slot is acquired, every path must release it or hand it to a sample that will: a failed `VideoProcessorBlt`, a failed `MFCreateDXGISurfaceBuffer`, a failed `SetCurrentLength`, a failed sample or tracked-sample creation, and a failed `WriteSample`. Add an explicit release on each, counted in `released_on_error`. Any path that returns early without releasing is a leak.

**Reference audit.** The tracked-sample callback only fires when the **last** reference to the sample drops. Search for anything that keeps an extra reference alive: a cached `IMFSample` or `IMFMediaBuffer` per slot, a "last frame" field, a `Vec` of wrappers, a clone moved into a struct and never dropped, a sample captured by a closure or logged. Any of these makes `released` fall behind `acquired` forever.

### 2.2 Audio

At the first video frame, log: `T0` (100 ns), `QueryPerformanceCounter` converted to 100 ns, and `Frame::timestamp()` for the next 5 frames with the QPC value at each. This is the C1 clock check from the production spec. Do not skip it.

For the **first 20 loopback packets**, log: the packet QPC position, flags (`SILENT`, `DATA_DISCONTINUITY`), frame count, peak amplitude, and the placement position in samples relative to `T0` (negative if before `T0`).

**Once per second**, for each accumulator (app, mic): write head, read head, fill, count of dropped-oldest events. For the clock thread: its `target` (the position it is emitting up to). For each emitted track (app, mic, mix): the **peak and RMS of the chunks emitted that second**. This is the single most useful audio line, because it shows whether non-silent audio ever leaves the timeline.

Also log the strategy in use (process or system loopback), the target PID, and the fraction of packets flagged `SILENT`.

---

## 3. Experiments (in order)

Use the same content for every run (Valorant, or a continuously moving window if Valorant is unavailable), 180 s each, and keep the logs.

### 3.1 Video

| Run | Setup | Look at |
|---|---|---|
| V1 | **Video only**: no audio threads, no audio streams | `dropped_pool`, `in_flight` curve over time |
| V2 | Video plus audio (current configuration) | same; compare with V1 |
| V3 | Video only, pool size 12 / 16 / 24 / 32 / 64, and `MF_LOW_LATENCY` on and off | fps, `dropped_pool`, `max_in_flight`; one row per combination |
| V4 | Any run that freezes: plot `in_flight` per second | **growing steadily** (leak) or **flat at some value** (hold depth)? |
| V5 | Any run: compare `released` to `acquired` at the end | `released` far below `acquired` means callbacks are not firing |

V1 versus V2 is the key bisect: it tells us whether the video freeze is caused by the audio and muxer side or exists on its own.

### 3.2 Audio

| Run | How | Question answered |
|---|---|---|
| A1 | `ffmpeg -i out.mp4 -map 0:a:0 -vn -af volumedetect -f null NUL`, then again with `-map 0:a:1` (and `0:a:2` if a mic track exists). Also `ffprobe -show_streams` to confirm which track is which | **Is there actually audio in each track?** Report `mean_volume` and `max_volume` |
| A2 | The `--audio-test <pid>` micro-test: 10 s of app audio from the target PID to a WAV, while sound plays. Listen to it | Does capture work, independent of the timeline? |
| A3 | Same as A2 with **system** loopback | Is the problem process targeting? |
| A4 | The clock check from 2.2 | Are the frame and QPC clocks the same? |
| A5 | A debug flag that writes the **emitted chunks** (post-timeline, pre-encoder) per track to WAV files | Does non-silent audio leave the timeline? |

---

## 4. Decision tables and fixes

### 4.1 Video

| Observation | Meaning | Fix direction |
|---|---|---|
| V1 clean (drops near 0, `in_flight` bounded), V2 bad | The audio or muxer side is backing up the video: the MP4 sink interleaves streams, and a stalled or mis-timestamped audio stream makes it buffer video, so the encoder stops asking for input and slots stay held | Fix the audio emission and timestamps (4.2) first, and measure lock wait on the mux lock. Re-run V2 |
| V1 and V2 both bad, `in_flight` grows steadily | A **slot leak**: `acquired` outruns `released` | Do the failure-path and reference audits (2.1). Check that each reused slot's tracked sample is **re-armed** (`SetAllocator`) before reuse, and that `Invoke` fires (V5) |
| Both bad, `in_flight` rises then plateaus at the pool size | The encoder is holding many frames (lookahead or rate-control buffering) | Test `MF_LOW_LATENCY`; check the rate-control mode set through `ICodecAPI` (an unconstrained or quality VBR mode can enable lookahead on Intel; try CBR or peak-constrained VBR); set pool size to the measured hold depth plus about 4 and keep it small |
| `released` much lower than `acquired` and the callback never fires for some slots | Tracked-sample misuse or an extra reference (see the reference audit) | Fix the misuse; never work around it by enlarging the pool |

**Lock hazards (do this regardless of the outcome).** The release callback runs on an MF thread, often while the encoder holds its own internal locks. If `Invoke` can block on a lock that the capture thread holds **across** `WriteSample` (and `WriteSample` can block waiting for the encoder), you get a stall that looks like exhaustion. Rule: `Invoke` takes **no mutex**. For a pool of at most 64 slots, use a lock-free free-mask:

```rust
// Sketch: verify against the real pool.rs. Bit set = slot free. Max 64 slots.
pub struct SlotMask(AtomicU64);
impl SlotMask {
    pub fn new(n: u32) -> Self {
        Self(AtomicU64::new(if n >= 64 { !0 } else { (1u64 << n) - 1 }))
    }
    pub fn acquire(&self) -> Option<u32> {
        let mut cur = self.0.load(Ordering::Acquire);
        loop {
            if cur == 0 { return None; }
            let idx = cur.trailing_zeros();
            match self.0.compare_exchange_weak(
                cur, cur & !(1u64 << idx), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(idx),
                Err(c) => cur = c,
            }
        }
    }
    pub fn release(&self, idx: u32) { self.0.fetch_or(1u64 << idx, Ordering::AcqRel); }
    pub fn in_flight(&self, n: u32) -> u32 { n - self.0.load(Ordering::Relaxed).count_ones() }
}
```

The textures live in an immutable `Vec`; the callback state carries only the slot **index**. Also make sure `WriteSample` is never called while any pool lock is held.

### 4.2 Audio

Read A1 first.

| A1 result | Meaning | Next |
|---|---|---|
| Every track exactly silent (`max_volume` about -91 dB or `-inf`) | Zeros reach the encoder | Use A5 and the per-second chunk peak/RMS to find where the audio is lost |
| Very low but non-zero (about -70 to -90 dB) | A **scaling bug**: samples converted from float to 16-bit without multiplying by 32767 (or double-attenuated) | Fix the float-to-i16 conversion in the mix and track paths and clamp after scaling |
| App track audible, **mix** silent | A bug in the mix path | Fix the mix; confirm stream order against `ffprobe` |
| Tracks audible in `volumedetect` but "silent" in the player | Player track selection | Check which track the player defaulted to; confirm track 1 is the mix |

If A1 shows silence, locate the stage:

| Evidence | Meaning | Fix direction |
|---|---|---|
| A2 WAV silent | Capture or targeting problem | Wrong PID or process tree; check the `SILENT` flag fraction; compare with A3 (system loopback) |
| A2 fine, A5 chunk WAVs silent | The timeline or clock loses the audio | See the clock note below |
| A5 fine, MP4 silent | Encoder input or stream wiring | Check the PCM input type (16-bit vs float), buffer sizes, and that each chunk goes to the intended stream index |

**Clock note (most likely suspect).** The persistent "dropping oldest data" warning at about 2 s means the accumulator fills and nothing drains it. That happens if the read position never reaches where the data was placed, for example if packet times (WASAPI QPC position) and the clock's `target` (derived from `T0`, taken from `Frame::timestamp()`) are on **different bases**, so every packet is placed far from the read head. If A4 shows `Frame::timestamp()` and QPC differ by a large or drifting amount, stop using `Frame::timestamp()` for audio alignment: take `T0 = QueryPerformanceCounter()` (in 100 ns) at the first-frame callback, and map video times with the measured offset `offset = qpc_at_first_frame - first_frame_ts`.

**Packets before `T0`.** The audio threads start before the first video frame, so packets arrive before `T0` exists. They must go into a **separate bounded pre-roll buffer** (about 3 s) that does not count towards the 2 s overflow limit. When `T0` is set: trim everything older than `T0`, move the rest onto the timeline, and only then start the clock thread. The "dropping oldest" path should be unreachable in a healthy run.

---

## 5. Acceptance (re-run after the fixes)

- 180 s Valorant, video plus audio, 3 runs: `dropped_pool` at most 1% of frames; `in_flight` bounded; no "dropping oldest data" warnings.
- `volumedetect` on the app and mix tracks shows audible levels (mean at least about -40 dB while the game is playing).
- AAC packet count x 1024 / 48000 within 0.5% of track duration (T5).
- Delivered fps within 10% of the capture-only ceiling (T2).
- No frozen or stale video in any 3-minute clip (visual check).
- Idle memory unchanged or lower than before; pool size reported with its GPU-memory estimate.

## 6. Do not

- Apply fixes (a), (b) or (c) from section 1.
- Raise the pool size to hide the symptom.
- Declare the audio "fine" on the basis of `chunks_written` or `write_errors`.
- Hold any mutex that the release callback can take across a `WriteSample` call.

## 7. Files that would let the investigator (or the reviewer) pinpoint the cause directly

`src/video/pool.rs`; `src/video/encode.rs` (`make_video_sample`); `src/mux/mod.rs` (where the mutex is held); the capture callback and `recorder.rs` init order; `src/audio/timeline.rs` (`place`); `src/audio/clock.rs` (`emit_chunk` and how `target` is computed); `src/audio/loopback.rs` (packet handling and the origin handshake); and one full log from a failing run.