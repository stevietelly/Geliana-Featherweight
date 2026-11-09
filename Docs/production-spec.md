# Featherweight Production Spec v1: Encode Layer, Audio, Container

Audience: the engineer (or model) implementing this. Read the whole document, and `BACKGROUND_MODE_SPEC.md`, before writing code.

## 0. How to read this

- This spec **extends** `BACKGROUND_MODE_SPEC.md` (tray, lifecycle, disk guard, single-instance guard, logging, `mpsc` event boundary). Everything there still applies unless section 14 changes it.
- It **replaces** `windows-capture`'s `VideoEncoder` with our own `IMFSinkWriter` pipeline, adds audio (app, mic, mix) and adds fragmented MP4 output.
- Evidence tags used throughout:
  - **[V]** verified in the spike on the reference machine: Intel Core i5-8365U (4 cores / 8 logical, 15 W), Intel UHD 620, 1920x1080, Valorant running.
  - **[U]** expected to work, never tested. Test it before relying on it.
  - **[GATE]** an acceptance test in section 12 must pass before the item counts as done. If it fails, follow the stated fallback and report the failure. Do not paper over it.
- Rules:
  1. Verify every API against the pinned crate versions (`windows-capture` 2.0.1 and the matching `windows` crate; run `cargo tree -d` to make sure `windows` is not duplicated). Where this document's API names differ from the real ones, follow the real API and leave a one-line comment.
  2. Log exact `HRESULT`s on failure. No silent fallbacks: every fallback is a named, logged step.
  3. Complete, compilable files. No `...` and no `todo!()` in delivered code.
  4. Do not build anything listed in section 13 (out of scope).
  5. Keep the dependency boundary from the base spec: `recorder.rs`, `disk.rs`, `paths.rs`, `log.rs` and everything under `video/`, `audio/`, `mux/` must not import `tao`, `tray-icon` or `global-hotkey`.

---

## 1. Targets

Measured on the reference machine, capturing Valorant at 1080p with the 33 ms update-interval cap. These are targets, not guarantees; the numbers in brackets are what the spike measured.

| Target | Value | Notes |
|---|---|---|
| Process CPU | at most about 1 logical core average [spike: 0.86] | about 11% of the reference machine |
| Working set | at most 130 MB, flat over time [spike: 118 MB flat] | GPU memory is not counted here; also record it |
| Delivered frame rate | within 10% of the capture-only ceiling [spike after pool fix: 22.7 fps vs about 27 ceiling] | **[GATE] T2** |
| Pool-exhaustion drops | at most 1% of frames | **[GATE] T2** |
| Encoder | hardware (vendor MFT), never silent software | **[GATE] T3** |
| Audio timeline | no timestamp gaps; track durations within 100 ms of each other and of video | **[GATE] T5** |
| A/V offset | at most 40 ms at start and at end of a 5-minute recording | **[GATE] T6** |
| Crash recovery | file playable after `taskkill /f`, all tracks present | **[GATE] T9** |
| In-game FPS impact | measured and reported; proposed target at most 5% average loss | **[GATE] T12** |

---

## 2. Architecture

### 2.1 Threads

| Thread | Owner | Job |
|---|---|---|
| Main | `app.rs` (`tao` loop) | tray, hotkey, menu events, 1 s tick, state machine (unchanged from base spec) |
| Capture | `windows-capture` (`start_free_threaded`) | `on_frame_arrived`: convert and submit video |
| App-audio | `audio/loopback.rs` | WASAPI loopback capture into the app timeline accumulator |
| Mic | `audio/mic.rs` | WASAPI mic capture into the mic timeline accumulator (only when the mic track is enabled) |
| Audio clock | `audio/clock.rs` | 20 ms cadence: emit fixed-size chunks for all audio tracks to the muxer |
| Stop thread | `app.rs` | short-lived; joins capture and finalizes (as in base spec 7.3) |
| Bridge thread | `main.rs` | `mpsc` receiver to `EventLoopProxy` (as in base spec section 3) |

All `IMFSinkWriter` calls go through **one `Mutex`** in `mux/`. Video writes take about 20-80 µs and audio writes occur 150 times per second in total, so contention is negligible, and it removes any doubt about thread safety.

Every thread that uses COM calls `CoInitializeEx(MTA)` first and `CoUninitialize` on exit.

### 2.2 File layout

```
src/
  main.rs            entry, single-instance, panic hook, wiring       (base spec)
  app.rs             state machine, tray events, hotkey, tick          (base spec, extended)
  tray.rs            tray icon and menu                                (base spec, extended)
  settings.rs        tiny settings file                                (new, section 8)
  recorder.rs        orchestrates start/stop, owns pipelines           (rewritten)
  capture.rs         windows-capture handler                           (new, thin)
  video/
    mod.rs
    device.rs        D3D11 device access, multithread protection
    convert.rs       ID3D11VideoProcessor BGRA -> NV12, cached views
    pool.rs          NV12 texture pool with tracked samples
    encode.rs        sink-writer video stream setup, sample creation
  audio/
    mod.rs
    loopback.rs      app audio (process loopback, system fallback)
    mic.rs           microphone capture
    timeline.rs      per-source accumulator placed by QPC timestamp
    clock.rs         fixed-cadence chunk emission + mixing
    pid.rs           resolve the captured window's process ID
  mux/
    mod.rs           sink writer wrapper, Mutex, stream indices, finalize
    container.rs     fragmented/non-fragmented selection (section 5)
  disk.rs, paths.rs, log.rs                                            (base spec)
```

---

## 3. Video pipeline

### 3.1 Frame flow

```
windows-capture frame (BGRA D3D11 texture)
  -> VideoProcessorBlt on the capture device (BGRA -> NV12)       [V] ~0.5 ms
  -> NV12 texture from the tracked-sample pool
  -> IMFSample via MFCreateDXGISurfaceBuffer
  -> IMFSinkWriter::WriteSample (hardware H.264 MFT)               [V] ~20-80 µs
  -> MP4
```

Reference timings from the spike: Blt about 0.5 ms, Flush about 0.03 ms, sample creation about 0.02 ms, `WriteSample` about 0.02 ms per frame. The whole callback averages about 2 ms.

Use the **capture's own D3D11 device** (and its immediate context) for the video processor and pool [V]. Call `ID3D11Multithread::SetMultithreadProtected(true)` on it, since Media Foundation uses the device from its own threads.

### 3.2 Initialization order [V] (do not reorder)

Everything about the sink writer is created **lazily on the first frame**, because the D3D device comes from the first frame. In this order:

1. `MFStartup` (paired with `MFShutdown`).
2. `MFCreateDXGIDeviceManager`, then `ResetDevice` with the capture device.
3. Create attributes (3.5), create the sink writer, `AddStream` and `SetInputMediaType` for every stream.
4. **`BeginWriting` last.** Calling it before `ResetDevice` leaves the encoder without a D3D device and every `WriteSample` returns `E_INVALIDARG`.

Expect this first-frame init to stall the callback for **about 320-570 ms** [V] (measured idle and under Valorant). Frames arriving during that time are simply not delivered. Log the init duration. This is acceptable for v1; see section 12 T2 for how it is excluded from the frame-rate gate.

### 3.3 BGRA to NV12 conversion [V]

- Create the video processor enumerator and processor once. Create input and output views **once per texture** and cache them. Never create views per frame.
- Input color space: full-range RGB. Output: BT.709, 16-235 (studio range). The raw `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` bitfield values: **input `0x00`**, **output `0x14`** (`YCbCr_Matrix=1` at bit 2, `Nominal_Range=1` at bits 4-5). The wrong value gives a green tint or crushed contrast [V]; the spike confirmed `color_range=tv`, BT.709 in ffprobe.
- Also set matching `MF_MT_VIDEO_PRIMARIES`, `MF_MT_TRANSFER_FUNCTION`, `MF_MT_YUV_MATRIX`, `MF_MT_VIDEO_NOMINAL_RANGE` on the NV12 input type.
- Width and height must be even (NV12). Round down.
- `VideoProcessorBlt` only queues GPU work. Do not call `Flush` unless measurement shows a benefit; if kept, keep the comment accurate (it submits, it does not wait).

### 3.4 NV12 pool and sample lifetime [V]

- Samples are `IMFTrackedSample`s created over pool textures. A slot is **free only when the encoder has released its reference to the sample** (the tracked-sample callback fires), never immediately after `WriteSample`. The earlier immediate-release approach raced with the async encoder and produced corrupt frames.
- Re-arm each sample with `SetAllocator` before reuse (check the spike code for the exact pattern that worked).
- Pool size: start at **16** slots (a constant at the top of `pool.rs`). If no slot is free when a frame arrives, **drop that frame**, increment `dropped_pool`, and never block the capture thread.
- Create textures with at least `D3D11_BIND_RENDER_TARGET` (video-processor output); add the encoder-input bind flag only if the MFT rejects the texture. Use whatever flags worked in the spike.
- After `MFCreateDXGISurfaceBuffer`, **set the buffer's current length**: `QueryInterface` to `IMF2DBuffer`, `GetContiguousLength`, then `SetCurrentLength`. Otherwise the H.264 MFT rejects the sample as zero length [V].

**[GATE] T2, open issue:** after the tracked-sample fix, delivery under Valorant measured **22.7 fps** against about 26.8 fps with the old racy release. The likely cause is pool exhaustion (slots held by the encoder). Required work:
1. Log `dropped_pool` per second and report it.
2. Try pool sizes 8, 16, 24, 32 and report fps and drop rate for each.
3. Test `MF_LOW_LATENCY` (section 3.5) and report the difference.
4. Whatever combination wins becomes the default. If delivery stays more than 10% below the capture-only ceiling, report that; do not call it acceptable.

### 3.5 Sink writer configuration

Attributes when creating the writer:

- `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS = TRUE`
- `MF_SINK_WRITER_D3D_MANAGER = <device manager>`
- `MF_SINK_WRITER_DISABLE_THROTTLING = TRUE`
- `MF_LOW_LATENCY = TRUE` is **[U]**: make it a constant, default off, and measure both (T2).
- Container selection attributes: see section 5.

Video types:

- **Output:** `MFVideoFormat_H264`, frame size, frame rate 30/1, `MF_MT_AVG_BITRATE` from the preset, progressive, 1:1 pixel aspect, **`MF_MT_MPEG2_PROFILE = eAVEncH264VProfile_High` and a level hint (4.1 for 1080p30)**. Intel Quick Sync negotiated **Constrained Baseline** when no profile was set [V], which lowers quality per bit. Log the profile actually negotiated; if the encoder rejects High, fall back to Main, then Baseline, logging each step.
- **Input:** `MFVideoFormat_NV12`, same size and rate, plus the color attributes from 3.3.
- **Bitrate control [U]:** `MF_MT_AVG_BITRATE` on the output type is a request that a hardware MFT may ignore. After `BeginWriting`, also set, through `ICodecAPI` on the encoder (from `GetServiceForStream`), the rate-control mode (`CODECAPI_AVEncCommonRateControlMode`: start with CBR or peak-constrained VBR) and `CODECAPI_AVEncCommonMeanBitRate` (plus max bitrate if the mode needs it). Log the requested and the applied values, and log any `HRESULT` if the encoder refuses. Then measure the actual bitrate on moving content (T4). If the encoder ignores every request, report it, keep its default, and state the real figure in the README instead of the preset number.
- GOP/keyframe interval of about 2 seconds via `ICodecAPI` (`CODECAPI_AVEncMPVGOPSize`) **[U]** (it also sets the fragment boundaries for fragmented MP4).

Use `MinimumUpdateIntervalSettings::Custom(Duration::from_millis(33))` on the capture settings. `MinimumUpdateIntervalSettings::Milliseconds` does not exist in windows-capture 2.0.1 [V]. Capture as `ColorFormat::Bgra8` (WGC's native format).

### 3.6 Timestamps and the shared clock **[GATE] C1**

All times are in 100 ns units.

- `Frame::timestamp()` is `SystemRelativeTime`, which is QPC-derived **[U]**. Audio will be placed using WASAPI QPC positions. **First task before wiring audio:** log `QueryPerformanceCounter` (converted to 100 ns) next to `Frame::timestamp()` for the first 100 frames and again after 60 s. If the difference is stable (within about 2 ms) and small, both clocks are the same; use them interchangeably. If not, convert audio timestamps into the frame-timestamp clock using the measured offset, and report it.
- **Recording origin `T0` = the timestamp of the first video frame.** Video sample time = `frame_ts - T0`. Audio sample time = `audio_qpc - T0`; audio older than `T0` is discarded. (The spike's video started 97 ms after audio, which this removes.)
- Video sample duration is the delta to the next frame; the last frame reuses the previous delta. The output is variable frame rate when the source is static; that is expected.

### 3.7 Encoder identification and software warning

At first-frame init, after `BeginWriting`, obtain the encoder via `IMFSinkWriter::GetServiceForStream` and read its friendly name and whether it is D3D-aware. Log both. If the encoder is Microsoft's software encoder (name contains "Microsoft" / DLL `mfh264enc.dll`), log a **warning**, and set the tray status to "Software encoding: expect high CPU". Never fail the recording because of this. **[GATE] T3.**

### 3.8 Presets

| Preset | Output | Bitrate | Priority |
|---|---|---|---|
| Balanced (default) | native resolution (1080p), 30 fps cap | 6 Mbps for 1080p; scale by pixel count otherwise | P0 |
| Low impact | 720p, 30 fps cap | 3 Mbps | P1 **[U]** |

Low impact scales inside the video processor (output size smaller than input, set the stream and destination rectangles); the sink-writer input and output types use the scaled size. Do P0 first. Presets are a constant table in `video/encode.rs`; there is no preset UI in v1 (a line in `settings.rs`, section 8).

**Bitrates are requests, not verified results.** The spike never requested a bitrate: its ~5.6 Mbps (static screen) and ~5.8 Mbps (Valorant) were Quick Sync's own default, and nearly identical across very different content, which suggests a CBR-like default of about 5.7 Mbps. No run has shown the encoder honoring a requested value. 6 Mbps was chosen because it is close to the only configuration actually benchmarked (CPU, memory, fps numbers in section 1) and keeps files near 45 MB per minute; 8 Mbps would be about 60 MB per minute and may change the benchmarks. If T4 shows 6 Mbps looks poor under moving Valorant content in High profile, raise it and re-run T2; that is a product decision, not an implementation detail.

---

## 4. Audio

### 4.1 Tracks

Fixed layout when the mic track is enabled:

| Stream index | Track | Content | Format |
|---|---|---|---|
| 0 | video | H.264 | |
| 1 | audio 1 "mix" | app + mic | AAC LC stereo 48 kHz |
| 2 | audio 2 "app" | game/app audio only | AAC LC stereo 48 kHz |
| 3 | audio 3 "mic" | microphone only | AAC LC, mono 48 kHz |

If the mic track is **disabled** (the default), write only streams 0, 1 and 2 (mix = app).

Why mix first: most players and platforms (Discord, YouTube, built-in Windows players) play only the first audio track. Track 1 must always be the complete clip.

AAC settings: `MFAudioFormat_AAC`, 48 kHz, 16-bit, `MF_MT_AUDIO_AVG_BYTES_PER_SECOND` **16000** (128 kbps) for stereo; for the mono mic try **12000** and, if the encoder rejects it, use stereo. Input type to the writer is `MFAudioFormat_PCM`, 16-bit, 48 kHz. The sink writer inserts the AAC encoder itself [V] (three stereo tracks worked).

These are **requested values, not spike results.** The spike measured 163-166 kb/s per stereo track, which is what the encoder produced under the spike's own settings (about the 20000 bytes/s step, if it set one) and says nothing about whether 16000 is accepted or honored. As I recall the documented set for the Microsoft AAC encoder is 12000, 16000, 20000 and 24000 bytes/s for stereo; if the encoder rejects 16000, use the nearest accepted value and log it. At 128 kbps the saving versus the spike is about 0.3 MB per minute per track, so the exact number matters little; what matters is that T4 measures it.

### 4.2 App audio (`audio/loopback.rs`)

Goal: capture only the recorded program's audio, not the whole system.

**Process loopback [U]** (the spike's WASAPI loopback worked, but whether it was per-process is not recorded):

- `ActivateAudioInterfaceAsync` with `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`, activation parameters `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` and the target PID. Including the process tree matters because games and launchers use helper processes.
- You supply the capture format yourself (the client has no mix format): 48 kHz, 2 channels, and the sample format that works (16-bit PCM or 32-bit float; convert as needed).
- Initialize shared-mode, `AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK`.
- Requires a recent Windows 10 build (roughly 2004/21H1 or newer; verify the exact minimum). On older builds or if activation fails: fall back to endpoint loopback on the default render device, and set the tray status to "App audio: system-wide fallback".
- This is the hardest piece of the audio phase. Build it first as a **standalone micro-test** (`--audio-test <pid>`: capture 10 s from a PID to a WAV file) and prove it with Valorant before integrating **[GATE] T7a**. Also confirm teammates' voice chat is audible in the capture (it should be, since it plays through the game's own audio session).

**Resolving the PID (`audio/pid.rs`)**: `GraphicsCapturePicker` returns a capture item, not a process ID. For v1:
1. If the captured item is a **monitor**: use endpoint (system) loopback.
2. If it is a **window**: read the item's display name, enumerate top-level windows (`EnumWindows`), match by trimmed title, and read the PID with `GetWindowThreadProcessId`. If exactly one visible window matches, use it. If zero or several match, use system loopback and say so in the tray status.
3. Log which strategy was used and the PID.

**[GATE] T7a** includes proving this on Valorant (whose window title has trailing spaces).

Packet handling (both loopback and mic):
- Event-driven capture with a timeout; read packets in a loop.
- Use the packet's QPC position (`pu64QPCPosition`, 100 ns units) as the packet timestamp.
- Honor `AUDCLNT_BUFFERFLAGS_SILENT` (treat the data as zeros) and `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY` (log it; the timeline handles it).
- Loopback delivers **no packets while the app is silent**. That is normal; the timeline (4.3) fills the gap.

### 4.3 Timeline and fixed-cadence emission (`audio/timeline.rs`, `audio/clock.rs`)

This is the design decision that replaces "write whatever packets arrive". The spike's output had about **87% of the expected AAC packets** (1,203 packets x 1,024 samples is about 25.7 s of audio inside a 29.6 s track) because gaps were not filled. Every audio track must be a **continuous, gapless sequence of equal-sized chunks**.

**Per-source accumulator.** Each source (app, mic) owns a mutex-protected accumulator that stores 48 kHz samples, a write head (in samples, relative to `T0`) and a read head.

- On packet arrival, compute the packet's start position in samples from its QPC time relative to `T0`.
  - If it starts within **40 ms** of the write head (small drift or jitter), just append (trust the sample count, so a jittery timestamp does not cause artifacts).
  - If it starts **later** than that, fill the gap with zeros, then append.
  - If it starts **earlier** than that (overlap), trim the overlap.
  - Packets that start before `T0` are trimmed or discarded.
- The accumulator is bounded (for example 2 seconds of audio). If it is ever full, drop the oldest data and log a warning.

**Cadence.** The audio clock thread runs a high-resolution waitable timer (`CreateWaitableTimerExW` with the high-resolution flag) every **20 ms**. On each tick it computes `target = now - T0 - MARGIN` with `MARGIN` about **80 ms** (a constant, so late packets still arrive in time), and for every chunk of 960 samples (20 ms) up to `target`:

1. Take 960 samples from each source accumulator; if a source has not delivered up to that point, use zeros.
2. App track chunk = app samples. Mic track chunk = mic samples (zeros when muted or disabled). Mix chunk = app + mic.
3. Write each as an `IMFSample` with timestamp = chunk start (100 ns) and duration exactly 20 ms to its stream through the muxer.

Result: timestamps are contiguous by construction, so AAC packet count must equal `duration x 48000 / 1024` within about 0.5% **[GATE] T5**. The 20 ms timer costs about 50 wakeups per second, which is negligible, but report its CPU.

**This replaces** the spike's three gap strategies (do nothing / `SendStreamTick` / fill with silence). Fill-with-silence by construction is the chosen strategy. The spike never ran the dedicated gap test, so **[GATE] T7** confirms it. **Fallback** if T7 shows memory growth or sync loss: try `SendStreamTick` on the affected stream and report.

### 4.4 Mixing

Convert to float, `mix = app + mic`, hard-clamp to [-1, 1], convert to 16-bit. No automatic gain, no ducking. Mono mic is duplicated to both channels for the mix.

### 4.5 Mic track: opt-in, with a mute hotkey

Privacy and usefulness rules, since the mic is not always meant to be recorded:

- **Default off.** The tray has a checkbox **"Record microphone track (next recording)"**, persisted in the settings file (section 8). When off, the mic device is **never opened** (no Windows mic-in-use indicator) and there is no mic stream.
- When on, the mic device opens at recording start. Because `T0` is the first video frame and sink-writer init takes about 320-570 ms, the mic's initialization delay (294 ms in the spike) overlaps with it and mostly disappears. Any remaining late start is correct placement (leading silence), not desync [V: ffprobe showed `start_time=0.294`].
- A **mute hotkey** (default `Ctrl+Alt+M`, a constant in this version) toggles mute. While muted, the mic accumulator is fed nothing, so the mic track and its share of the mix are silent. The device stays open; the tray shows "Mic: live" or "Mic: muted". Muting must take effect in the next 20 ms chunk (**T8**).
- A tray item **"Mute microphone"** does the same.
- If hotkey registration fails (already taken), log it, set a tray status line, and continue; the tray item still works.

### 4.6 Errors and device changes

- **An audio failure never aborts a recording.** If the loopback or mic cannot start, log the `HRESULT`, write silence for that source, and show it in the tray status. Only video or muxer failures end a recording.
- On `AUDCLNT_E_DEVICE_INVALIDATED` (headset unplugged, default device changed): write silence, retry initialization every 2 seconds, and resume capture when it succeeds **[U], T11**.

---

## 5. Container and muxing (`mux/`)

### 5.1 Writer wrapper

One struct owns the `IMFSinkWriter` behind a `Mutex`. It exposes `write_video`, `write_audio(track, sample)` and `finalize`. Stream indices are fixed by the table in 4.1.

### 5.2 Fragmented MP4 with a fallback ladder

Why: a standard MP4 stores its index at the end, so a crash, freeze or full disk loses the file. Fragmented MP4 writes self-contained fragments continuously.

Spike status: fragmented sink creation works on this Windows build [V]; `AddStreamSink` for a fourth stream on the fMP4 media sink (Route B) was accepted [V]; **Route A with multiple audio streams was not tested, and no crash test has been run** [U].

Implement in this order and stop at the first rung that passes **T9** (1 video + 3 audio, then the kill test):

1. **Route A:** `MFCreateSinkWriterFromURL` with `MF_TRANSCODE_CONTAINERTYPE = MFTranscodeContainerType_FMPEG4`, and a fragment duration of about 2 s (`MF_MPEG4SINK_MIN_FRAGMENT_DURATION`, in 100 ns units, if present). Simplest integration.
2. **Route B:** `MFCreateFMPEG4MediaSink` over a file byte stream, then `MFCreateSinkWriterFromMediaSink`; add streams beyond those the sink pre-creates with `AddStreamSink` (the sink reports a stream count of 2 but pre-creates streams 0, 1 and 2; index 3 was accepted [V]).
3. **Fragmented, single mixed audio track:** loses the separate game and mic tracks; report it as a product decision for the owner.
4. **Non-fragmented multi-track MP4** (the verified path [V]) with the disk guard as the only protection. Document the limitation in the README.

`container.rs` exposes one enum, `ContainerMode`, set by a constant for now. **Log the mode actually in use at the start of every recording.** Never auto-switch rungs silently.

### 5.3 Finalization order

1. Stop video submission (end of capture).
2. The audio clock emits remaining chunks up to the last video timestamp (so audio tracks end with the video).
3. Stop audio sources; join their threads.
4. `Finalize()` the sink writer.
5. Only then report `CaptureEnded` (base spec 7.3). `finish` must run exactly once, via `Option::take()`, on every exit path including `Drop`.

---

## 6. Recorder orchestration (`recorder.rs`)

Start:
1. Resolve the audio strategy (4.2) and mic setting (4.5); log them.
2. Start the audio threads **before** the first video frame, so audio exists when `T0` is set.
3. Start capture (`start_free_threaded`) with the settings from 3.5.
4. On the first frame: set `T0`, run init (3.2), start the audio clock thread at `T0`.

`RecorderEvent` additions (still sent over `mpsc`, no UI types):

```rust
pub enum RecorderEvent {
    CaptureEnded { reason: StopReason, path: PathBuf },
    Status(StatusKind),          // AudioStrategy, EncoderName, SoftwareEncodingWarning, ContainerMode, MicLive, MicMuted, AudioUnavailable
}
```

Stats, logged to the file every 10 s and shown in the tray status: frames delivered, `dropped_pool`, writer errors, sink-writer queue health if available. Define "dropped" precisely: frames the app received but did not encode. Frames WGC never delivered are invisible to us; say so in the README.

---

## 7. Tray changes (`tray.rs`, `app.rs`)

Additions to the base-spec menu:

```
[disabled]  <status line>                      (adds: encoder name, container mode, drops)
-----------
Stop recording / New recording... / Open recordings folder
-----------
[checkbox]  Record microphone track (next recording)
[item]      Mute microphone  (Ctrl+Alt+M)       (enabled only while recording with the mic track on)
-----------
Quit
```

Status line examples: `Recording 00:12:34 | 3.2 GB | Quick Sync | fMP4 | mic muted`; the tooltip stays under 127 characters. When app audio fell back to system loopback or the encoder is software, say so in the status text.

Hotkey: use the `global-hotkey` crate with an event handler that forwards into `EventLoopProxy`, so it wakes the loop (the 1 s tick is too slow for a hotkey).

---

## 8. Settings file (`settings.rs`)

The base spec said "no config file". This version needs one tiny file for the mic-track checkbox and, later, presets.

- Path: `%LOCALAPPDATA%\Featherweight\settings.ini`.
- Plain `key=value` lines, hand-parsed (no `serde`). Keys: `mic_track=0|1`, `preset=balanced|low_impact`.
- Unknown keys and parse errors are ignored (use defaults) and logged. Written atomically (write to a temp file, then rename).

---

## 9. Dependencies

Base spec dependencies plus:

- `global-hotkey` (same family as `tray-icon`; hotkey only).
- `windows` crate features, in addition to the base set: `Win32_Media_Audio`, `Win32_Media_MediaFoundation`, `Win32_Graphics_Direct3D11`, `Win32_Graphics_Direct3D`, `Win32_Graphics_Dxgi`, `Win32_Graphics_Dxgi_Common`, `Win32_System_Com`, `Win32_System_Com_StructuredStorage` (activation `PROPVARIANT`), `Win32_System_Threading`, `Win32_System_Performance`, `Win32_UI_WindowsAndMessaging` (`EnumWindows`, `GetWindowThreadProcessId`), `Win32_Media`.

Do not add `serde`, `tokio`, `cpal`, `ffmpeg`, `rodio` or any audio crate. The sink writer encodes AAC; we only capture and place PCM.

---

## 10. Gotchas confirmed by the spike (keep these in the code comments)

1. Defer all sink-writer setup to the first frame; `ResetDevice` before `BeginWriting` [V].
2. `MFCreateDXGISurfaceBuffer` leaves `CurrentLength = 0`; fix via `IMF2DBuffer::GetContiguousLength` [V].
3. Release pool slots through tracked-sample callbacks, never right after `WriteSample` [V].
4. Color-space bitfield `0x14` for BT.709 16-235 output; `0x00` for full-range RGB input [V].
5. Intel Quick Sync picks Constrained Baseline unless a profile is requested [V].
6. `MinimumUpdateIntervalSettings::Custom(Duration)`; there is no `Milliseconds` in 2.0.1 [V].
7. `VideoSettingsBuilder` defaults to HEVC (not relevant to the production pipeline, but explains early baseline numbers) [V].
8. `MFCreateFMPEG4MediaSink` reports 2 streams but pre-creates 3 [V].
9. `BeginWriting` on a device-less encoder gives `E_INVALIDARG` on every `WriteSample` [V].

---

## 11. Dev tooling (not shipped behavior)

Keep these as hidden CLI flags on the same binary so measurements are repeatable:

- `--capture-only`: capture the primary monitor with a no-op handler for 180 s and print delivered fps per second. This produces the **ceiling** the T2 gate compares against.
- `--audio-test <pid>`: 10 s of app audio from a PID to a WAV (T7a).
- `--clock-check`: the C1 measurement from 3.6.
- A once-per-10 s stats line in the log, always on.

---

## 12. Acceptance tests

Run on a release build, on the reference machine unless stated. Report numbers, not just pass/fail.

| ID | Test | Pass criteria |
|---|---|---|
| C1 | Clock check (3.6) | QPC vs `Frame::timestamp()` difference stable within about 2 ms over 60 s, or a measured offset applied |
| T2 | 180 s Valorant recording, vs `--capture-only` ceiling run in the same scene (3 runs each); pool sizes 8/16/24/32; low-latency on/off | Delivered fps within 10% of ceiling (init second excluded); `dropped_pool` at most 1%; report CPU per frame and working set |
| T3 | Encoder identification (3.7) + Process Explorer thread stacks | Vendor MFT in use; log shows its name; software encoder triggers the warning |
| T4 | `ffprobe` on output | H.264 High (or negotiated fallback logged); `color_range=tv`, BT.709; video average bitrate measured on moving Valorant content (not a static screen) within +/-20% of the requested value, with requested, applied and measured figures all reported, and each audio track within +/-15% of its requested bitrate; 1 video + 3 audio (mic on) or 1 video + 2 audio (mic off); visually clean under moving Valorant content in 3 clips (no tearing, stale frames or color shift) |
| T5 | Audio packet accounting | For every audio track: packets x 1024 / 48000 within 0.5% of the track duration; track durations within 100 ms of each other and of the video |
| T6 | A/V sync | Clap-and-flash (or beep-and-flash) at start and at 5 minutes; offset at most 40 ms at both |
| T7a | Per-process loopback micro-test on Valorant | Valorant audio captured; other apps' audio (for example a media player) not captured; teammate voice chat present; tree capture works |
| T7 | Gap test | 30 s of app silence then a beep: sync preserved; working set flat (within 5 MB) during the gap; compare with `SendStreamTick` only if this fails |
| T8 | Mic behavior | Mic disabled: device never opened (log and Windows indicator confirm); enabled: audio present on track 3 and in the mix; mute produces digital silence within one 20 ms chunk |
| T9 | Fragmented container ladder (5.2) | The first rung where 1 video + 3 audio plays correctly in VLC and `ffprobe`; kill test (`taskkill /f` at about 30 s): file playable, recovers at least elapsed minus 5 s, all tracks present; non-fragmented control is unplayable |
| T10 | Base-spec tests 1-9 (tray, quit, window close, disk guard, new recording, single instance, picker cancel, memory flat, 1-hour run) | All pass, with the new encode and audio layers in place |
| T11 | Device-change test | Unplug/replug the headset mid-recording: recording continues, audio returns within about 3 s, no crash |
| T12 | In-game FPS impact | Valorant range (or fixed replay scene), with and without recording, 3 runs each, using an in-game FPS counter or PresentMon; report average and 1% lows. Proposed target at most 5% average loss |
| T13 | Weak machine | Repeat T2/T5/T12 on a dual-core machine if available; if not, state that clearly |

---

## 13. Out of scope for this version

Replay/instant-clip buffer, settings window or gpui UI (a separate process later), live streaming, HEVC/AV1, HDR, Linux/macOS, replacing the picker with a custom window list, voice-activity gating, mirroring Valorant's push-to-talk key, configurable hotkeys, toast notifications, auto-start with Windows, file segmenting, placeholder-file renaming.

---

## 14. Changes to BACKGROUND_MODE_SPEC.md

- Section 4 (dependencies): add `global-hotkey` and the extra `windows` features from section 9 here.
- Section 5 (file layout): replace with section 2.2 here.
- Section 6 (types): add `StatusKind` and the `Status` event; `RecordingFlags` gains the audio and container settings.
- Section 7.2 (handler): `VideoEncoder` is gone; the handler converts and submits through `video/` and `mux/`. The "no per-frame printing or logging" rule stands.
- Section 7.7 (disk guard): unchanged. Keep the 1 GiB hard floor even with fragmented MP4; it still protects the finalize step and the user's drive.
- Section 8 (settings knobs): the 33 ms cap is now `Custom(Duration::from_millis(33))`.
- Section 9 (known risks): items 1-4 are resolved by the spike [V]; fragmented MP4 is now section 5 here.
- Out-of-scope list: "audio" and "global hotkeys" are removed from it; they are in scope as defined here (hotkey: mic mute only).

---

## 15. Open items summary

| Item | Status | Resolved by |
|---|---|---|
| Delivery 22.7 fps vs ~27 ceiling after tracked-sample fix | open | T2 |
| Encoder profile (High vs Constrained Baseline) and bitrate target | open | T4 |
| Baseline `VideoEncoder` never identified, bitrate/profile differ from spike | open | not needed for production; note in the final report |
| Clock alignment (QPC vs `Frame::timestamp()`) | open | C1 |
| Per-process loopback and PID resolution | open | T7a |
| Audio gap strategy (design chosen, never tested) | open | T7 |
| A/V sync accuracy | open | T6 |
| Fragmented Route A with 3 audio + crash survival | open | T9 |
| `MF_LOW_LATENCY` effect | open | T2 |
| In-game FPS impact | open | T12 |
| Behavior on a dual-core machine | open | T13 |
| Mono AAC bitrate acceptance | open | T4 |
| Encoder honors requested video and audio bitrates (spike only saw defaults: ~5.7 Mbps video, 163-166 kb/s AAC) | open | T4 |
| Visual quality under moving content | open | T4 |