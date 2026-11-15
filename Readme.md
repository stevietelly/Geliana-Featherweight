# Geliana Featherweight

A screen recorder built for PCs that can barely run the game, let alone record it.

Geliana Featherweight is a Rust screen recorder with one job: capture gameplay while taking as little CPU, GPU and RAM from the game as possible. If your machine struggles to hit 60 FPS on its own, the recorder should not be the reason it hits 40.

> **Status: usable, still young (v0.2.1).** Capture, audio and encoding work end to end on Windows, with a real UI, tray app and installer. Overhead is engineered for, but published benchmark numbers against OBS are still pending — see [Roadmap](#roadmap).

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

## What it does today

- **Windows Graphics Capture** for monitors *and* application windows, with an automatic GDI fallback for monitor capture on systems where WGC misbehaves (toggle: `force_gdi`)
- **H.264 MP4 output** through Media Foundation's hardware encoder (via the [`windows-capture`](https://crates.io/crates/windows-capture) crate), sized from the target's real resolution on the first frame
- **Configurable capture**: FPS, bitrate, cursor visibility, optional recording-length limit — the output resolution follows the capture target itself
- **Audio**: WASAPI loopback for system sound and microphone capture, per-source volume, a mic gate threshold, and microphone noise suppression (RNNoise) that can be disabled or tuned by strength
- **Optional separate audio tracks**: mix system + mic into one track, or keep two tracks in the file for post-mixing
- **A real UI** (egui): a Recording tab with a live monitor — the first captured frame as a still, elapsed time, output size and rate, RAM use, and disk headroom with a time-left estimate — plus a past-recordings list that flexes into the space below
- **Settings tab**: capture target (monitor or open window), audio sources and levels, output folder and filename template, disk behavior, hotkeys, and more — persisted to disk and applied live
- **System tray app**: start/stop, open the recordings folder, quick preference toggles; closing the window hides to the tray instead of quitting, and the tray label mirrors session state
- **Global hotkeys**: start/stop recording from anywhere (default `Ctrl+F9` / `Ctrl+F10`), rebindable in the UI
- **Disk safety**: warns while space is running low and force-stops the session before the drive hits the floor, using the *measured* output rate to tell you how much recording time is left
- **Crash and session tolerance**: output is staged as `<name>.temp.mp4` and swapped into place on finalize; a session-end watcher finalizes a running recording on Windows shutdown/logoff; stop reasons are recorded so an automatic stop (hidden window, session end, disk floor) is explained afterwards
- **Single-instance**: launching twice surfaces the running window instead of starting a second recorder
- **Past-recordings list**: finished captures in the output folder, newest first, with date and size, open-file and open-folder shortcuts
- **Filename templates** with `{d}`, `{t}`, `{ms}`, `{monitor}` and friends, sanitized against path escapes and Windows reserved names
- **First-run hints and honest warnings**: which dialog failed, why the still is missing, what stopped the recording — surfaced in the window, not swallowed into a log
- **An About view** with the full license text of every dependency, embedded in the binary

## Install

Grab the latest release from the [releases page](https://github.com/stevietelly/Geliana-Featherweight/releases):

- **MSI installer** — installs to `Program Files\Geliana Featherweight` with Start Menu (and optional desktop) shortcuts, plus an optional system-PATH entry so `Featherweight` works from a terminal.
- **Portable zip** — run `Geliana Featherweight.exe` directly, no installer needed.

Requirements: Windows 10 (1903+) or Windows 11.

### Build from source

```bash
cargo run --release
```

A recent stable Rust toolchain is required. The binary is a GUI app — no console window.

## Basic use

1. Featherweight starts in the tray. Open the window from the tray icon (or just launch it again).
2. On the Recording tab, pick a monitor or window on the Settings tab — or keep the default monitor.
3. Hit **Record** (or `Ctrl+F9` anywhere), stop with **Stop** (`Ctrl+F10`), find finished files in `Videos\Featherweight` by default.
4. The Recording tab shows the first captured frame while recording; when idle it lists your past recordings below.

Most settings apply immediately and persist; advanced ones (performance preset, force-GDI, audio timestamps) live in `settings.json`.

## Roadmap

Phases are ordered by what de-risks the project fastest. Each phase should be benchmarked before moving to the next.

### Phase 0: Prove the capture path (done, minus the numbers)
- [x] Windows Graphics Capture via `windows-capture`
- [x] Encode to MP4
- [x] Real UI, tray, hotkeys, configuration (originally planned for later phases)
- [ ] **Baseline measurements**: CPU %, GPU %, RAM, dropped frames, and in-game FPS delta on a low-end machine
- [ ] Compare against OBS (same game, same settings) to set a real target

### Phase 1: Hardened core
- [ ] Split into a `featherweight-core` library crate with no UI dependencies
- [x] Start/stop controlled by API (UI, tray, hotkey), not by a timer
- [x] Configurable framerate, bitrate, target, audio sources
- [ ] Confirm the hardware encoder path (NVENC / AMF / Quick Sync via Media Foundation) is actually in use and that frames avoid CPU copies
- [ ] Fragmented MP4 (or MKV) output so interrupted recordings stay playable
- [ ] Graceful handling of capture-item loss (window closed, display change, GPU reset)

### Phase 2: Audio (shipped, needs polish)
- [x] System audio via WASAPI loopback
- [x] Microphone input with volume, gate threshold, and RNNoise suppression
- [x] Shared QPC-based clock, timestamped at capture time
- [x] Optional separate audio tracks for post-mixing
- [ ] End-to-end A/V sync verification on the recorders people actually use

### Phase 3: Low-end hardware support
- [ ] First-run hardware detection (GPU vendor, encoder availability, iGPU/dGPU layout) surfaced in the UI
- [x] Performance presets (Balanced default; Low impact via `settings.json`)
- [ ] Software encoder fallback for GPUs with no hardware encoder, with a clear warning in the UI
- [ ] Hybrid laptop handling: capture and encode on the correct adapter to avoid black frames and cross-GPU copies
- [ ] Adaptive quality: step resolution or framerate down automatically if frames start dropping

### Phase 4: Shell and controls (shipped)
- [x] System tray app with global hotkeys (start/stop)
- [x] Settings window; the window can be hidden entirely while recording
- [x] Recording stats: output size, rate, RAM, disk ETA
- [x] Output folder management and file naming

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

## Design principles

**The recorder core is headless.** Capture, encode and mux live in pipeline code with no UI dependency — the UI hands it a config and receives events over a channel. The UI can be closed entirely while recording.

**GPU in, GPU through, file out.** The target pipeline is: capture texture → hardware encoder → muxer. CPU involvement should be limited to scheduling and muxing.

**Bounded everything.** Queues and buffers have fixed upper bounds. If the encoder falls behind, drop frames deliberately and report it rather than ballooning memory.

**Measure, don't assume.** Claims about low overhead get backed by benchmarks on real low-end hardware, published in this repo.

**Fail safe.** Output is written incrementally to a staging file. Errors are surfaced in the UI, not swallowed.

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
| Windows 10 (1903+) / 11 | Working (WGC + GDI fallback, WASAPI audio, Media Foundation encode) |
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

MIT — see [LICENSE](LICENSE). Third-party license texts are embedded in the app's About view.

## Acknowledgements

- [`windows-capture`](https://crates.io/crates/windows-capture) for the capture and encoding foundation
- The Rust `windows` crate ecosystem
- [`wasapi`](https://crates.io/crates/wasapi) for system-audio loopback and microphone capture
- [`nnnoiseless`](https://crates.io/crates/nnnoiseless) (RNNoise) and the vendored SpeexDSP for microphone noise suppression
- [`egui`/`eframe`](https://github.com/emilk/egui) for the immediate-mode UI, and [`tray-icon`](https://crates.io/crates/tray-icon) for the system tray
