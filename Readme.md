# Featherweight

A screen recorder built for PCs that can barely run the game, let alone record it.

Featherweight is a Rust screen recorder with one job: capture gameplay while taking as little CPU, GPU and RAM from the game as possible. If your machine struggles to hit 60 FPS on its own, the recorder should not be the reason it hits 40.

> **Status: early prototype.** Capture and encode work end to end on Windows. Everything under [Roadmap](#roadmap) is intent, not shipped behavior.

---

## Why this exists

Mainstream recorders are built for capable machines. On a low-end system they compete with the game for the same scarce resources, and the usual failure modes are:

- Software encoding (x264) eating CPU the game needs
- Frames copied from GPU to CPU and back, burning memory bandwidth
- A heavy UI process sitting in RAM while you play
- Game hooking that trips anti-cheat
- Configuration that assumes you know what a rate-control mode is

Featherweight treats overhead as the primary feature. Every design decision is judged against one question: *does this cost the game frames?*

## Goals

1. **Minimal impact on the game.** Target single-digit CPU overhead and no meaningful FPS loss on hardware where the game is already struggling.
2. **Small memory footprint.** Low idle RAM, flat memory during recording, nothing that grows with recording length.
3. **GPU-resident pipeline.** Captured frames stay on the GPU and go straight to a hardware encoder wherever possible. No CPU readback on the fast path.
4. **Safe with anti-cheat.** No injection, no hooking. Use OS-sanctioned capture APIs only.
5. **Crash-tolerant output.** A game freeze, driver reset or power loss should not leave you with a corrupt file.
6. **Zero-knowledge defaults.** First run detects the hardware and picks sane settings. Advanced controls exist but are never required.
7. **Honest.** Show dropped frames and measured overhead instead of pretending recording is free.

## Non-goals

- Live streaming (RTMP and friends). Possibly later, not a priority.
- Video editing, overlays, scenes, webcam compositing. Use OBS if you need those.
- Being the most feature-rich recorder. Being the lightest is the point.
- Game hooking / injection capture.

---

## Current state

The prototype is a single binary built on the [`windows-capture`](https://crates.io/crates/windows-capture) crate (Windows Graphics Capture API):

- Opens the system picker to choose a window or monitor
- Captures frames via Windows Graphics Capture
- Encodes through the crate's built-in video encoder to `video.mp4`
- Records for a fixed 6 seconds, then finalizes and exits
- No audio, no UI, no configuration

This is a spike to validate the capture path and see how the encoder behaves. It is not yet tuned for low-end hardware.

### Build and run

Requirements: Windows 10 (1903+) or Windows 11, a recent stable Rust toolchain.

```bash
cargo run --release
```

Pick a window or monitor in the dialog. After 6 seconds, `video.mp4` is written to the working directory.

---

## Roadmap

Phases are ordered by what de-risks the project fastest. Each phase should be benchmarked before moving to the next.

### Phase 0: Prove the capture path (current)
- [x] Windows Graphics Capture via `windows-capture`
- [x] Encode to MP4
- [ ] Baseline measurements: CPU %, GPU %, RAM, dropped frames, and in-game FPS delta on a low-end machine
- [ ] Compare against OBS (same game, same settings) to set a real target

### Phase 1: Headless core
- [ ] Split into a `featherweight-core` library crate with no UI dependencies
- [ ] Start/stop controlled by API, not by a timer
- [ ] Configurable resolution, framerate, bitrate
- [ ] Confirm the hardware encoder path (NVENC / AMF / Quick Sync via Media Foundation) is actually in use and that frames avoid CPU copies
- [ ] Fragmented MP4 (or MKV) output so interrupted recordings stay playable
- [ ] Graceful handling of capture-item loss (window closed, display change, GPU reset)

### Phase 2: Audio
- [ ] System audio via WASAPI loopback
- [ ] Microphone input
- [ ] Single shared clock (QPC-based) for audio and video, timestamped at capture time
- [ ] Separate audio tracks so game and mic can be mixed later

### Phase 3: Low-end hardware support
- [ ] First-run hardware detection (GPU vendor, encoder availability, iGPU/dGPU layout)
- [ ] Presets, e.g. *Low impact* (720p30), *Balanced* (1080p30), *Quality* (1080p60)
- [ ] Software encoder fallback (x264 ultrafast, capped threads) for GPUs with no hardware encoder, with a clear warning in the UI
- [ ] Hybrid laptop handling: capture and encode on the correct adapter to avoid black frames and cross-GPU copies
- [ ] Adaptive quality: step resolution or framerate down automatically if frames start dropping

### Phase 4: Shell and controls
- [ ] System tray app with global hotkeys (start/stop, save replay)
- [ ] Tiny settings window; nothing rendered while a game is running
- [ ] Recording stats: dropped frames, encode time, overhead estimate
- [ ] Output folder management and basic file naming

### Phase 5: Instant replay
- [ ] Ring buffer holding the last *N* seconds of encoded video
- [ ] Hotkey to save the buffer to disk
- [ ] Memory cap, since the buffer must stay bounded on low-RAM machines

### Later / maybe
- Linux: PipeWire portal capture with VAAPI encoding
- macOS: ScreenCaptureKit
- HDR handling and tone mapping
- AV1 encode on hardware that supports it
- Per-game profiles and auto-start on game launch
- Optional gpui front end, if its memory cost can be justified

---

## Design principles

**The recorder core is headless.** Capture, encode and mux live in a library with no UI dependency. Any front end (tray, settings window, CLI) is a thin client. The UI can be closed entirely while recording.

**GPU in, GPU through, file out.** The target pipeline is: capture texture → hardware encoder → muxer. CPU involvement should be limited to scheduling and muxing.

**Bounded everything.** Queues, buffers and the replay ring all have fixed upper bounds. If the encoder falls behind, drop frames deliberately and report it rather than ballooning memory.

**Measure, don't assume.** Claims about low overhead get backed by benchmarks on real low-end hardware, published in this repo.

**Fail safe.** Output is written incrementally. Errors are surfaced, not swallowed.

## Target hardware

Featherweight is designed around machines like:

- Older dual/quad-core CPUs
- Integrated graphics or entry-level discrete GPUs
- 4 to 8 GB of RAM
- Laptops with hybrid graphics

Development benchmarks should run on hardware in this class, not only on a modern dev machine.

## Platform support

| Platform | Status |
|---|---|
| Windows 10 (1903+) / 11 | Prototype working |
| Linux | Not started |
| macOS | Not started |

## Anti-cheat note

Featherweight uses Windows Graphics Capture, an OS-provided API, and does not inject into or hook game processes. That is the same approach built into Windows itself and is the least likely to be flagged. No capture method can promise zero risk with every anti-cheat system, so check your game's policy if you play competitively.

## Contributing

The project is at a stage where the most useful contributions are:

- **Benchmarks** on low-end hardware (CPU/GPU model, game, settings, measured FPS with and without recording)
- **Bug reports** with GPU model, driver version and game
- **Encoder and capture knowledge**, especially around hybrid GPUs and exclusive fullscreen quirks

Open an issue before starting large changes so the design stays coherent.

## License

To be decided.

## Acknowledgements

- [`windows-capture`](https://crates.io/crates/windows-capture) for the capture and encoding foundation
- The Rust `windows` crate ecosystem
