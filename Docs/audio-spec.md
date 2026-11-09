# Spike: Media Foundation Sink Writer for Featherweight

Audience: the engineer (or model) running this spike. Read the whole document before writing code.

This is an **experiment, not a feature**. The output is a set of measurements and pass/fail answers, plus the throwaway code that produced them. Do not integrate anything into the main Featherweight crate.

---

## 0. Why this spike exists

Featherweight is a screen recorder for weak Windows PCs. It currently encodes with `windows-capture`'s `VideoEncoder`. Three product needs push us toward owning the encode layer with our own `IMFSinkWriter`:

1. **Multiple audio tracks** in one MP4 (mixed, app-only, mic-only). `VideoEncoder` has one audio stream, built on `MediaStreamSource::CreateFromDescriptors(video, audio)`. That cannot be extended.
2. **Fragmented MP4**, so a crash, freeze or full disk leaves a playable file instead of a corrupt one.
3. **Control over the GPU path**, so frames never take a detour through the CPU on low-end hardware.

The risk: a hand-built sink-writer pipeline can be *slower* than the current encoder on weak machines (silent CPU color conversion, extra copies, bad sample lifetime handling). So this spike answers five questions, in this order of importance:

| # | Question |
|---|---|
| Q1 | Can a hand-built sink-writer pipeline match or beat the current `VideoEncoder` on CPU and memory, on the GPU path? |
| Q2 | Is a **hardware** H.264 encoder actually in use, with no CPU color conversion? |
| Q3 | Can one MP4 hold **1 video + 3 AAC tracks**, and do the tracks play correctly? |
| Q4 | How does the pipeline behave when an audio stream goes silent (no packets for seconds)? Does sync hold, and does memory grow? |
| Q5 | Does **fragmented MP4** work with the same setup, including multiple audio tracks, and does it survive `taskkill /f` mid-recording? |

The result decides whether Featherweight rewrites its encode layer. Report honestly. A negative result is a perfectly good outcome.

---

## 1. Rules for the implementer

1. **Verify every API against the `windows` crate version you pin.** Names, signatures and feature flags below are written from memory of the Win32 docs and may differ in `windows-rs`. When something doesn't match, follow the real API and add a one-line comment saying so.
2. **Never fake a result.** If a call returns an `HRESULT` error, log the exact code and stop that stage. Do not silently fall back to another path. Every fallback must be an explicit, named, logged step, and its numbers must be reported separately.
3. **Do not claim something works without evidence.** "The file plays" means you opened it in a player or ran `ffprobe` and recorded the output, not that `WriteSample` returned `S_OK`.
4. **Same conditions for every measurement** (section 3). A comparison with different settings is invalid.
5. **Complete, compilable files.** No `...` placeholders, no `todo!()` in the delivered code.
6. **Keep it small.** One crate, a handful of files, subcommands via `std::env::args` (no `clap`).
7. **Comment the *why*** at each non-obvious step, especially anything about bind flags, color spaces, threading and sample lifetime.

---

## 2. Setup

### 2.1 Project

Create a **standalone crate** at `spike/` (its own `Cargo.toml`, not a member of the main workspace).

```
spike/
  Cargo.toml
  src/
    main.rs        subcommand dispatch, run orchestration, CSV metrics output
    capture.rs     windows-capture handler that hands frames to a pipeline trait
    baseline.rs    pipeline A: windows-capture VideoEncoder (the control)
    mfgpu.rs       pipeline B: IMFSinkWriter, GPU path, NV12 via ID3D11VideoProcessor
    audio_synth.rs synthetic audio generators with QPC timestamps
    metrics.rs     process CPU time, working set, callback timing, frame counts
  RESULTS.md       you fill this in (section 9)
```

### 2.2 Dependencies

- `windows-capture`: same version the main project uses.
- `windows`: **must match the version `windows-capture` depends on.** Run `cargo tree -d` and confirm there is no duplicate `windows` crate. Enable features as needed. Expect at least:
  `Win32_Foundation`, `Win32_Media_MediaFoundation`, `Win32_Graphics_Direct3D`, `Win32_Graphics_Direct3D11`, `Win32_Graphics_Dxgi`, `Win32_Graphics_Dxgi_Common`, `Win32_System_Com`, `Win32_System_Performance`, `Win32_System_Threading`, `Win32_System_ProcessStatus`, `Win32_Media`.
- Nothing else unless unavoidable. No async runtime, no logging framework (use `println!` and a CSV file).

### 2.3 Tools to have installed (for verification, not as code dependencies)

- `ffprobe` (from FFmpeg) and VLC. `ffprobe -show_streams -show_format <file>` is the ground truth for tracks, codec, bitrate and duration.
- Task Manager (Details tab with the GPU engine columns enabled) or `typeperf` for GPU engine counters.

### 2.4 Machine

Run on the **weakest machine available** (old dual/quad-core CPU, iGPU or entry-level GPU). Record: CPU model, GPU model and driver version, RAM, Windows build number, display resolution and refresh rate. If you can only use a strong machine, say so prominently in the report; the numbers are then an upper bound on how good things look.

---

## 3. Fixed test conditions (apply to **all** runs)

- **Source:** primary monitor capture, no user interaction (use `Monitor::primary()` from `windows-capture`, not the picker).
- **Content:** the same looping, constantly-moving content for every run (for example a fullscreen looping video or a continuously animated page). A static desktop produces almost no frames and makes the test meaningless. Use the same content for every run and note what it was.
- **Duration:** 60 seconds per run. Run each configuration **3 times**; discard the first as warm-up and report the other two.
- **Video:** H.264, 30 fps, 8 Mbps if the capture is 1080p (scale linearly with pixel count otherwise), progressive, same resolution everywhere. Width and height must be **even** (NV12 requirement); round down.
- **Frame rate cap:** use `MinimumUpdateIntervalSettings` set to about 33 ms in **every** pipeline so the baseline and spike see the same frame delivery. Look up the correct variant in the pinned crate version.
- **Cursor:** off. **Capture border:** off (fall back to default if unsupported, and note it).
- **Color format:** capture as BGRA (`ColorFormat::Bgra8`), which is the native format of Windows Graphics Capture. Also run the baseline once with `Rgba8` (what the prototype uses) and note whether it changes CPU or GPU load. That comparison is a side result, not the main one.
- **Nothing else running** during a measurement: close browsers (other than the content source), updaters and indexers; plugged in, high-performance power plan; note the plan.
- **Audio:** none for stages 0 and 1 (so the video comparison is clean).

---

## 4. Stage 0: Baseline (the control)

Subcommand: `spike baseline`

1. Capture the primary monitor with `windows-capture` and feed each frame to `VideoEncoder::send_frame`, writing a non-fragmented `baseline.mp4`, with the fixed settings above and no audio.
2. Record metrics (section 8) for 60 s.
3. Run `ffprobe` on the output and record codec, resolution, frame count, duration, bitrate.
4. **Identify the encoder in use.** Check the GPU engine counters during the run: with a hardware encoder active you should see activity on the GPU's *Video Encode* engine. Record whether it is active. (This tells us whether the control itself is on the hardware path.)

This establishes the numbers that Stage 1 must match or beat.

---

## 5. Stage 1: GPU video path through `IMFSinkWriter`

Subcommand: `spike gpu`

Goal: reproduce Stage 0's output (video only, non-fragmented MP4) using your own sink writer, with frames staying on the GPU.

### 5.1 Pipeline

```
WGC frame (BGRA texture)
   -> ID3D11VideoProcessor Blt (BGRA -> NV12, on GPU)
   -> NV12 texture from a pool
   -> IMFSample via MFCreateDXGISurfaceBuffer
   -> IMFSinkWriter::WriteSample (hardware H.264 MFT)
   -> MP4
```

### 5.2 Getting the D3D11 texture and device from a frame

`windows-capture`'s `Frame` should expose the raw texture and the device or context (check for accessors such as `as_raw_texture`, `device`, `device_context`). Use **the capture's own D3D11 device** for the video processor and the NV12 pool. Doing everything on one device and one immediate context means GPU commands are ordered, so reading the capture frame inside the callback and letting it be reused after the callback returns is safe. (Confirm this reasoning by checking for visual corruption in the output, such as tearing or stale frames.)

If the capture device was **not** created with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`, `QueryInterface` for `ID3D11VideoDevice` will fail. In that case log it clearly, then try the explicit fallback: create your own device with `VIDEO_SUPPORT | BGRA_SUPPORT` and copy frames across with a shared texture. Report separately whether you had to do this, since it adds a copy.

Call `ID3D11Multithread::SetMultithreadProtected(true)` on the device. Media Foundation uses the device from its own threads.

### 5.3 BGRA to NV12 on the GPU

Hardware H.264 encoders take NV12. If you give the sink writer BGRA, it may insert a **CPU** color converter, which defeats the whole design. So convert yourself:

1. `ID3D11VideoDevice::CreateVideoProcessorEnumerator` with a content description (progressive, input and output size equal to the capture size).
2. `CreateVideoProcessor`.
3. For each pool texture, **create the input/output views once and cache them.** Do not create views per frame; that is real overhead.
4. Per frame: `ID3D11VideoContext::VideoProcessorBlt` from the frame's texture to an NV12 texture.
5. **Color space:** set the stream (input) color space to full-range RGB and the output to BT.709 studio-range YCbCr, and set matching `MF_MT_VIDEO_PRIMARIES`, `MF_MT_TRANSFER_FUNCTION`, `MF_MT_YUV_MATRIX` and `MF_MT_VIDEO_NOMINAL_RANGE` on the media types. If you skip this, colors will look washed out or shifted in players. **Check the output visually** against the screen and note the result.

### 5.4 NV12 texture pool and sample lifetime

The encoder reads textures asynchronously, so a texture must not be reused until the encoder is done with it.

Preferred approach: `MFCreateVideoSampleAllocatorEx` with `IMFVideoSampleAllocatorEx::InitializeSampleAllocatorEx`, giving it the DXGI device manager and a media type. It hands out `IMFSample`s backed by D3D11 textures and recycles them when released. Set the bind flags attribute so the textures can be a video-processor render target (at minimum `D3D11_BIND_RENDER_TARGET`; add the encoder-input bind flag if the MFT rejects the texture).

Fallback: a fixed ring of about 16 NV12 textures with a counter of in-flight samples. Log if the ring is ever about to wrap onto an in-flight slot. If you must use the fallback, say why.

When wrapping a texture: `MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)`, then `MFCreateSample`, `AddBuffer`. DXGI surface buffers commonly need the current length set (for example from `IMF2DBuffer::GetContiguousLength`); check whether the encoder needs it.

### 5.5 Sink writer setup

- `MFStartup` once (and `CoInitializeEx` on each thread that needs COM). Pair with `MFShutdown`.
- `MFCreateDXGIDeviceManager`, then `ResetDevice` with the capture device.
- `MFCreateAttributes` and set:
  - `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS = TRUE`
  - `MF_SINK_WRITER_D3D_MANAGER = <the device manager>`
  - `MF_SINK_WRITER_DISABLE_THROTTLING = TRUE` (otherwise the writer may throttle input)
  - `MF_LOW_LATENCY = TRUE` (test with and without; report the effect)
- `MFCreateSinkWriterFromURL(path, None, attrs)` for a non-fragmented `.mp4`.
- Output (encoded) type: `MFVideoFormat_H264`, frame size, frame rate 30/1, `MF_MT_AVG_BITRATE`, progressive, 1:1 pixel aspect, a sensible profile (try High, fall back to Main).
- Input type: `MFVideoFormat_NV12`, same size and rate, plus the color attributes from 5.3.
- `AddStream`, `SetInputMediaType`, `BeginWriting`.

### 5.6 Timestamps

Use `Frame::timestamp()` (a `TimeSpan`, 100 ns units, QPC-derived and relative to system start). Define the recording origin as the timestamp of the **first** frame; each sample time is `timestamp - origin`. Sample duration is the delta to the next frame (use the previous delta for the last frame).

**Verify the clock assumption:** at several frame arrivals, also read `QueryPerformanceCounter`, convert to 100 ns units, and log the difference against `Frame::timestamp()`. If the two clocks are the same, the difference is a small, stable latency. If it drifts or jumps, report it. Audio timestamps in Stage 2 depend on this.

### 5.7 Prove it is really on the GPU

Collect all of these, since no single one is conclusive:

1. Log the encoder MFT in use if you can get it (via `IMFSinkWriter::GetServiceForStream` and `IMFTransform` attributes, such as the friendly-name attribute and whether it is D3D-aware). Also enumerate H.264 encoders with `MFTEnumEx` and `MFT_ENUM_FLAG_HARDWARE` and log what is available.
2. GPU *Video Encode* engine utilization during the run (as in Stage 0).
3. Process CPU use compared with the baseline. A silent CPU conversion usually shows up as a clear CPU increase.
4. Whether the capture-callback time (section 8) is small and stable.

### 5.8 Output checks

`ffprobe` the file: codec, resolution, frame rate, frame count (expected about 1800 for 60 s at 30 fps), duration, bitrate. Play it in VLC and check smoothness, colors and that it seeks.

---

## 6. Stage 2: One video plus three AAC tracks

Subcommand: `spike multitrack`

Use the same GPU video path as Stage 1, plus synthetic audio so no real WASAPI work is needed.

### 6.1 Synthetic audio sources (`audio_synth.rs`)

Three generators, 48 kHz, 16-bit, stereo PCM, produced in about 10 ms chunks and timestamped from QPC against the same origin as the video:

| Track | Content |
|---|---|
| 1 "mix" | sum of tracks 2 and 3, clipped safely |
| 2 "app" | 440 Hz sine |
| 3 "mic" | 880 Hz sine |

Distinct pitches so you can confirm by ear, or with `ffmpeg` analysis, that each track holds the right content. Pace the generator threads by wall-clock time (QPC), not by `sleep` counts.

### 6.2 Streams

Four `AddStream` calls in total: one video, three audio. For each audio stream:

- Output type: `MFAudioFormat_AAC`, 48 kHz, stereo, 16-bit, `MF_MT_AUDIO_AVG_BYTES_PER_SECOND` set to a value the AAC encoder accepts (such as 16000 for 128 kbps, or 24000 for 192 kbps), AAC profile/level attributes as required.
- Input type: `MFAudioFormat_PCM`, 48 kHz, stereo, 16-bit.
- The sink writer inserts the AAC encoder itself.

Write audio from its own threads via `WriteSample(streamIndex, ...)`. `IMFSinkWriter` is thread-safe for writes to different streams, but confirm this against the docs and watch for any `HRESULT` errors.

### 6.3 Pass/fail checks

- **Q3 core check:** `ffprobe -show_streams` lists exactly one video and three audio streams, all with plausible duration (within about 0.1 s of each other).
- In VLC (Audio, Audio Track), the three tracks are selectable, and each one has the correct tone.
- If `AddStream` for the second or third audio stream fails, or the MPEG-4 sink rejects it, record the exact `HRESULT` and **stop this stage**. That is a key finding. Do not attempt workarounds beyond what is listed in 8.
- Repeat the Stage 1 measurements with audio on, and report the CPU and memory cost of audio on top of video.
- Note which tracks Windows' built-in players expose (they usually play only the first). Informational only.

---

## 7. Stage 3: Audio gaps and sync

Subcommand: `spike gap`

WASAPI loopback delivers **no data when nothing is playing**. The pipeline must stay in sync through gaps. Using the Stage 2 setup:

1. Let all three tracks run for 10 s.
2. **Stop producing track 2 for 3 s** (no `WriteSample` calls at all), then resume with correctly timestamped samples. Tracks 1, 3 and video continue throughout.
3. At the end, check (with `ffprobe` and by playing the file) whether track 2's audio after the gap is still aligned with the video and the other tracks. Use the tones: for example, add a distinct short beep on track 2 at a known time after the gap and confirm it lands at that time.
4. During the gap, watch **process memory**. The MP4 sink interleaves streams; a stalled stream can make it buffer the others unboundedly. Record peak working set during and after the gap.
5. Repeat the test with `IMFSinkWriter::SendStreamTick(streamIndex, timestamp)` called on track 2 during the gap. Report whether it changes memory use, interleaving or sync.
6. Repeat with **explicit silence** written to fill the gap. Report which of the three strategies (do nothing, tick, fill with silence) keeps sync and bounds memory.
7. Do the same for a 30 s gap if the 3 s result is ambiguous.

Report the winning strategy and the evidence. This directly shapes the real WASAPI code later.

---

## 8. Stage 4: Fragmented MP4 and crash survival

Subcommand: `spike fmp4`

Try both routes. Each must be tested with (a) video plus one audio track, and (b) video plus three audio tracks.

**Route A:** `MFCreateSinkWriterFromURL` with the attribute `MF_TRANSCODE_CONTAINERTYPE = MFTranscodeContainerType_FMPEG4` (check it exists in the pinned crate and on the target Windows version). Set a fragment duration attribute if one exists (for example `MF_MPEG4SINK_MIN_FRAGMENT_DURATION`, which is in 100 ns units) to about 2 seconds.

**Route B:** `MFCreateFMPEG4MediaSink` with a byte stream (`MFCreateFile`), a video type and an audio type, then `MFCreateSinkWriterFromMediaSink`. Note that this function takes **one** audio type. After creating the sink, try adding further audio stream sinks (`IMFMediaSink::AddStreamSink`) and report whether it works.

For each working combination:

1. Record 60 s normally and `ffprobe` the result (tracks, duration).
2. **Crash test:** start a recording, and at about 30 s kill the process with `taskkill /f /pid <pid>`. Then run `ffprobe` and try playing the partial file. Record: playable or not, how much footage was recovered (duration), and whether audio tracks survived.
3. Run the same crash test on the **non-fragmented** MP4 from Stage 1 as the control (expected: unplayable).

Also measure CPU and memory for the fragmented output compared with non-fragmented.

If multi-audio does not work in fragmented mode, the report must state the exact failure, and list which of these fallbacks is available (do not build them, just say whether they look feasible):

- fragmented MP4 with a single mixed audio track
- non-fragmented MP4 with multiple tracks
- per-source audio written to sidecar files and muxed on stop

---

## 9. Measurement method

Implement in `metrics.rs` and write one CSV per run (`run_<stage>_<n>.csv`) with one row per second.

| Metric | How |
|---|---|
| Process CPU % | `GetProcessTimes` (kernel + user) delta over wall-clock delta, divided by logical core count. Also report as "% of one core". |
| Working set (MB) and peak | `GetProcessMemoryInfo` (`WorkingSetSize`, `PeakWorkingSetSize`) |
| Frames delivered | count in the capture callback |
| Frames written | count of successful `WriteSample` calls |
| Frames dropped | expected (about 1800) minus frames written, and any failed writes |
| Callback time | QPC time spent inside the capture callback: average, p95, max, in microseconds |
| `WriteSample` time | same stats for the sink-writer call |
| GPU use | read manually from Task Manager or `typeperf` per run: 3D engine, Video Encode engine, Video Processing engine, for the **spike process** and for the whole GPU. Record the values in `RESULTS.md`. (Reading GPU engine counters programmatically is out of scope.) |
| Output file | size in MB, then `ffprobe` details |

Report the mean and worst second for CPU, plus the peak working set. Do not report a single average only.

---

## 10. Deliverables

1. The `spike/` crate, compiling cleanly with `cargo build --release`, with the five subcommands: `baseline`, `gpu`, `multitrack`, `gap`, `fmp4`.
2. The CSV files from your runs.
3. `RESULTS.md` using the template below, with actual numbers and evidence. Mark anything you did not run as **NOT RUN** with the reason.

### RESULTS.md template

```
# Spike results

## Machine
CPU / GPU + driver / RAM / Windows build / resolution + refresh / power plan / test content

## Q1/Q2: GPU video path vs baseline (video only, non-fragmented)
| Metric | Baseline (VideoEncoder) | Baseline (Rgba8) | Spike (sink writer) |
| CPU % mean / worst |
| Working set peak |
| Callback time avg / p95 / max |
| Dropped frames |
| GPU 3D % / Video Encode % |
| Output size, bitrate |
Encoder identified: ...
Hardware encoder active (evidence): ...
CPU color conversion detected? ...
Fallbacks used (own device, ring instead of allocator, etc.): ...
Visual check (colors, tearing, smoothness): ...
Clock check (QPC vs Frame::timestamp): ...

## Q3: Multi-track audio
AddStream results (exact HRESULTs on failure):
ffprobe stream list:
VLC track switching + tone check:
Extra CPU/memory for audio:

## Q4: Gaps
| Strategy | Sync after gap | Peak memory during gap | Notes |
| do nothing |
| SendStreamTick |
| fill with silence |
Recommended strategy:

## Q5: Fragmented MP4
| Route | 1 audio ok? | 3 audio ok? | Crash test playable? | Recovered duration | CPU/memory vs non-fragmented |
| A |
| B |
| control (non-fragmented) |
Available fallbacks if multi-audio fails:

## Conclusions
Answer Q1 to Q5 each in one sentence, with a clear recommendation.
Unexpected findings:
Things I could not verify:
```

---

## 11. How the results will be used (so you know what matters)

| Outcome | Decision it drives |
|---|---|
| Spike CPU at or below baseline (within measurement noise), hardware encoder confirmed, tracks play | Rewrite the encode layer around `IMFSinkWriter` and build the audio phase on it |
| Spike CPU clearly higher than baseline (for example 20% or more) | Do **not** adopt it as is. Investigate the cause (CPU conversion, copies, throttling) before any decision |
| Multi-audio fails in the MPEG-4 sink | Choose a fallback from section 8 and re-scope the audio phase |
| Fragmented works only with one audio track | Decide between crash-safety and separate tracks |
| Gaps blow up memory or break sync under "do nothing" | The real audio code must implement whichever strategy won in Stage 3 |

Your job is accurate measurements and honest uncertainty, not a particular answer.

---

## 12. Known unknowns (explicitly check; do not assume)

1. Whether `Frame` exposes the raw texture and device, and whether the capture device has video support enabled.
2. Whether `ColorFormat::Bgra8` versus `Rgba8` changes load in the capture pipeline.
3. Whether `Frame::timestamp()` shares a clock with `QueryPerformanceCounter` (section 5.6).
4. The exact bind flags the hardware encoder accepts for NV12 input textures.
5. Whether the MPEG-4 sink accepts more than one audio stream, in both fragmented and non-fragmented modes.
6. Whether `MF_TRANSCODE_CONTAINERTYPE` with FMPEG4 is honored by `MFCreateSinkWriterFromURL` on the test machine's Windows build.
7. Whether a stalled stream makes the MP4 sink buffer other streams without bound.
8. Whether sink-writer input throttling or low-latency settings change CPU use on the target hardware.