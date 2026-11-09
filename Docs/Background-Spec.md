# Featherweight: Background Recording Mode (Implementation Spec)

Audience: the engineer (or model) implementing this. Read the whole document before writing code.

## 1. Goal

Turn the current 6-second console prototype into a background recorder:

1. User launches the app and picks a window or monitor (existing `GraphicsCapturePicker`).
2. Recording starts immediately. The app has **no visible window and no console**. Its only presence is a system tray icon.
3. Recording continues until one of these happens:
   - the user chooses **Stop** or **Quit** from the tray menu
   - the captured window is closed
   - free disk space on the output volume drops below a hard floor
   - the encoder or capture reports a fatal error
4. In every case the MP4 is **finalized and playable**, and the tray shows what happened.

Non-negotiable: low overhead. No UI rendering during recording, no per-frame logging or printing, no extra threads that spin.

## 2. Scope

**In scope:** tray icon and menu, background capture on a worker thread, stop conditions, disk-space guard, output file naming, single-instance guard, file logging, finalize-on-every-exit-path.

**Out of scope (do not build):** audio, global hotkeys, settings window, config file, replay buffer, file segmenting, toast notifications, auto-start with Windows, Linux/macOS.

## 3. Architecture

Two threads of interest:

| Thread | Responsibility |
|---|---|
| **Main thread** | `tao` event loop, tray icon, menu events, 1-second tick (tooltip and disk checks), state machine |
| **Capture thread** | Started by `windows-capture` via `start_free_threaded`. Runs the frame handler and owns the `VideoEncoder` |

The current code calls `Capture::start(settings)`, which **blocks the calling thread**. That is incompatible with a tray event loop. Use the free-threaded start, which returns a `CaptureControl` handle and runs capture on its own thread. Keep that handle in the app state.

Communication from capture thread to main thread goes through a plain `std::sync::mpsc::Sender<RecorderEvent>`. **`recorder.rs` must not depend on `tao` or `tray-icon`**; it only knows about the `Sender`. This keeps the recording logic UI-agnostic so it can be lifted into a separate core crate later with no rewrite.

Because an `mpsc::Receiver` cannot wake a `tao` event loop by itself, `main.rs` spawns one **bridge thread** at startup. It blocks on `receiver.recv()` (no polling, no spinning, zero CPU while idle) and forwards each message with `proxy.send_event(UserEvent::Recorder(ev))`. The bridge exits when all `Sender`s are dropped. Only `app.rs` and `main.rs` touch the `EventLoopProxy`.

Communication from main to capture is via `CaptureControl` (stop) only. Do not add shared mutable state unless needed.

```
main thread (tao loop)                     capture thread
-----------------------                    ---------------------
pick item  ───────────────► start_free_threaded(settings)
tray + 1s tick                              on_frame_arrived -> encoder.send_frame
menu click Stop ─► control.stop() ───────►  on_closed / Drop -> encoder.finish()
◄── RecorderEvent::CaptureEnded ─────────── (mpsc Sender -> bridge thread -> EventLoopProxy)
```

## 4. Dependencies

Add to `Cargo.toml` (versions are suggestions; run `cargo tree -d` and make sure the `windows` crate version does not end up duplicated against the one `windows-capture` uses):

```toml
tao = "0.30"            # event loop only; tray-icon is designed to pair with it
tray-icon = "0.19"      # tray icon + menu (re-exports muda)
dirs = "5"              # Videos folder
chrono = { version = "0.4", default-features = false, features = ["clock"] }  # filename timestamp
windows = { version = "<match windows-capture>", features = [
    "Win32_Foundation",
    "Win32_Storage_FileSystem",   # GetDiskFreeSpaceExW
    "Win32_System_Threading",     # CreateMutexW
] }
```

Do **not** add `image`, `serde`, `tokio`, `tracing`, or a GUI toolkit. Generate tray icons in code (see 7.5). Logging is a plain file append (see 7.9).

## 5. File layout

```
src/
  main.rs       entry, windows_subsystem attr, single-instance guard, panic hook, wiring
  app.rs        State enum, UserEvent, event loop, menu handling, tick logic
  recorder.rs   CaptureHandler (GraphicsCaptureApiHandler impl), start/stop helpers, StopReason
  disk.rs       free-space query, thresholds, growth-rate estimate
  tray.rs       icon generation, menu construction, tooltip/status formatting
  paths.rs      output dir + filename generation
  log.rs        append-to-file logger
```

**Dependency boundary (enforce this):** `recorder.rs`, `disk.rs`, `paths.rs` and `log.rs` must not import `tao`, `tray-icon` or any UI crate. Only `main.rs`, `app.rs` and `tray.rs` may. This is what lets those four files move into a `featherweight-core` crate later, and lets a separate gpui settings process (future work) reuse them without pulling in the tray stack.

## 6. Types

```rust
// recorder.rs
#[derive(Debug, Clone)]
pub enum StopReason {
    UserStopped,
    UserQuit,
    TargetClosed,
    DiskLow { free_bytes: u64 },
    EncoderError(String),
}

#[derive(Debug)]
pub enum RecorderEvent {
    CaptureEnded { reason: StopReason, path: std::path::PathBuf },
}

pub struct RecordingFlags {
    pub size: (u32, u32),
    pub output: std::path::PathBuf,
    pub events: std::sync::mpsc::Sender<RecorderEvent>,
}
```

```rust
// app.rs
pub enum UserEvent {
    Menu(tray_icon::menu::MenuEvent),
    Recorder(RecorderEvent),   // forwarded by the bridge thread
}

enum State {
    Recording { started: Instant, path: PathBuf, control: CaptureControl<..> },
    Stopping  { reason: StopReason, exit_after: bool },
    Stopped   { reason: StopReason, path: PathBuf },
}
```

`StopReason` for a stop requested from the tray is assigned by the main thread, since it initiated it. Reasons that originate on the capture thread (`TargetClosed`, `EncoderError`) are sent through `RecorderEvent::CaptureEnded`. The `StopReason` for a stop requested from the tray is carried by the same event, set by the main thread when it spawns the stop thread (7.3).

## 7. Behavior in detail

### 7.1 Startup

1. `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` at the top of `main.rs`. Release builds have no console. Debug builds keep it for development.
2. Install a panic hook that writes the panic message to the log file (a GUI-subsystem process otherwise dies silently).
3. Single-instance guard: `CreateMutexW` with a name like `Local\Featherweight-Recorder`. If it already exists, exit immediately. Keep the handle alive for the process lifetime.
4. Create the tao event loop with `EventLoopBuilder::<UserEvent>::with_user_event().build()`. Create an `mpsc::channel::<RecorderEvent>()`, keep the `Sender` in app state (clone it into each `RecordingFlags`), and spawn the bridge thread described in section 3 with the receiver and an `EventLoopProxy` clone.
5. Call `GraphicsCapturePicker::pick_item()`. If the user cancels, exit silently with no tray icon.
6. Create the output directory and path (7.6), check free space against the hard floor *before* starting. If already below it, show nothing, log it, and exit. (A tray-only app has no way to tell the user otherwise; acceptable for now.)
7. Build `Settings` as in the existing code, pass `RecordingFlags` as `Flags`, call `start_free_threaded`.
8. Build the tray icon and menu, enter the event loop.

### 7.2 The capture handler

Based on the existing `Capture` struct, with these changes:

- **Remove** the 6-second timer, the `print!` and the `stdout().flush()`. Per-frame I/O costs frames.
- `new()`: create `VideoEncoder` with `flags.output` instead of `"video.mp4"`. Store `encoder: Option<VideoEncoder>`.
- `on_frame_arrived()`: call `send_frame` and nothing else. On `Err`, send `CaptureEnded { EncoderError }`, attempt `finish()`, then `capture_control.stop()`.
- `on_closed()`: finalize the encoder (`take()` then `finish()`), then send `RecorderEvent::CaptureEnded { reason: TargetClosed, .. }` with `flags.events.send(..)`. Ignore a send error (the receiver is gone only during shutdown).
- Implement `Drop`: if `encoder` is still `Some`, call `finish()`. This is the safety net so every exit path finalizes the file.
- `finish()` must be called **at most once**: always go through `Option::take()`.

Important gotcha: Windows Graphics Capture only delivers a frame when the content changes. A static window may produce no frames for a long time. **Do not rely on checking a stop flag inside `on_frame_arrived`.** Stopping must work through `CaptureControl::stop()` from the main thread, with finalization in `on_closed` / `Drop`, so Quit works on an idle window.

### 7.3 Stopping

- Tray **Stop** or **Quit** moves state to `Stopping`. For Quit, set `exit_after = true`.
- Call `control.stop()` (check its exact signature in the pinned crate version; it likely consumes the handle and joins the capture thread). Because the join blocks during finalization, run it on a short-lived `std::thread` and send `RecorderEvent::CaptureEnded` (carrying the `StopReason` the main thread chose) through a clone of the same `Sender` when it returns. This keeps the tray responsive.
- On `CaptureEnded`:
  - if `exit_after`: exit the loop (`ControlFlow::Exit`)
  - otherwise: state becomes `Stopped`, icon goes gray, menu updates (7.4)
- **Never exit the process before finalization completes.** If Quit is chosen, the app waits for `CaptureEnded`. Add a safety timeout of about 10 seconds, after which it logs and exits anyway.

### 7.4 Tray menu and tooltip

Menu:

```
[disabled]  Recording: 00:12:34, 3.2 GB      <- status line, updated each tick
-----------
Stop recording                                <- enabled only while Recording
New recording...                              <- enabled only while Stopped
Open recordings folder
-----------
Quit
```

- Status line while recording: `Recording 00:12:34 | 3.2 GB | 41 GB free`.
- Status line when stopped: a one-line reason, for example `Stopped: disk space low` / `Stopped: window closed` / `Saved`.
- Tooltip mirrors the status line. Windows truncates tray tooltips at about 127 characters, so keep it short.
- **New recording...** runs the picker again on the main thread (it blocks the loop briefly, which is acceptable) and then repeats 7.1 steps 6-7. If the user cancels, stay in `Stopped`.
- **Open recordings folder** runs `explorer.exe <dir>`.

### 7.5 Tray icons

Generate two RGBA icons in code with `Icon::from_rgba`, 32x32: a filled circle, **red** while recording, **gray** otherwise. No asset files, no `image` crate.

### 7.6 Output path and naming

- Directory: `dirs::video_dir()` joined with `Featherweight`. Fall back to the current directory if unavailable. Create it with `create_dir_all`.
- Filename: `featherweight-YYYYMMDD-HHMMSS.mp4` using local time.
- Never overwrite an existing file. If the name exists, append `-1`, `-2`, and so on.

### 7.7 Disk-space guard

Run from the 1-second tick on the main thread. **No separate polling thread.** Drive the tick with `ControlFlow::WaitUntil(next_tick)` so the loop sleeps between ticks.

- Query free space on the output volume with `GetDiskFreeSpaceExW`, using `lpFreeBytesAvailableToCaller` so per-user quotas are respected.
- Constants in `disk.rs`:
  - `HARD_FLOOR = 1 GiB`: at or below this, stop with `StopReason::DiskLow`
  - `WARN_FLOOR = 3 GiB`: below this, switch the icon to an amber variant and prefix the status line with `Low disk`
- Override for testing: environment variable `FEATHERWEIGHT_MIN_FREE_MB` replaces `HARD_FLOOR`.
- **Why the floor exists and why it is large:** the current output is a standard (non-fragmented) MP4, where the index is written at the end during `finish()`. If the disk fills mid-write, the file is likely **unplayable**. The guard must stop recording while there is still room to finalize. Other programs (including the game) also write to disk during recording, which eats the margin. Do not lower the floor to save space.
- Growth estimate (best effort): sample the output file length each tick, compute bytes per second over a rolling 10-second window, and if the rate is above zero show `~N min left` in the status line using `(free - HARD_FLOOR) / rate`. Skip it if the size is not changing.

### 7.8 Captured window closes

`on_closed` fires when the capture item goes away. Finalize and report `TargetClosed`. Do not auto-restart.

### 7.9 Logging

`log.rs`: a function `log(msg: &str)` that appends a timestamped line to `%LOCALAPPDATA%\Featherweight\featherweight.log`. Log: startup, picked item size, output path, every state transition, every stop reason, errors, panics. Never log per frame. Truncate the file at startup if it exceeds about 1 MB.

## 8. Settings to apply

Use the existing `Settings::new(...)` call and consider these, with fallbacks:

- `DrawBorderSettings`: try the no-border variant so the yellow capture border does not appear. If starting capture fails (older Windows builds do not support it), retry with `Default`.
- `MinimumUpdateIntervalSettings`: this is the cheapest overhead lever on weak PCs. Cap at roughly 33 ms for ~30 FPS as the default. Make it a single constant at the top of `recorder.rs`.
- `ColorFormat::Rgba8` as in the prototype. Do not change it without benchmarking.

## 9. Known risks (verify against the pinned `windows-capture` version, do not assume)

1. **API names and signatures.** `start_free_threaded`, `CaptureControl::stop`/`wait`, and the `Settings::new` parameter list vary between crate versions. Read the crate docs for the version in `Cargo.toml` first.
2. **Fragmented MP4.** Check whether `ContainerSettingsBuilder` offers a fragmented-output option. If it does, enable it; it makes crashes survivable. If not, the disk guard in 7.7 is the only protection, and that limitation should be noted in the README.
3. **Window resize mid-recording.** The encoder is created with a fixed size. Test resizing the captured window. If the output breaks, stop gracefully with a clear status instead of writing garbage.
4. **Encoder finalization on thread exit.** Confirm by test that `Drop` or `on_closed` actually runs `finish()` when `control.stop()` is called.
5. **FAT32 output drives.** Single files cap at 4 GB there. Out of scope, but log a warning if the output volume's filesystem is FAT32.

## 10. Acceptance tests

Each must pass on a release build.

1. Launch, pick a window: no console, no taskbar entry, red tray icon.
2. Quit from tray while recording a **static** window (nothing moving): the app exits, `video.mp4`-style output is **playable** and has plausible duration.
3. Close the recorded window: tray goes gray, status says the window closed, file plays.
4. Disk guard: set `FEATHERWEIGHT_MIN_FREE_MB` slightly above current free space (or use a small VHD as the output volume): recording stops on its own, file plays.
5. Stop, then **New recording...** twice in a row: two separate playable files, no leaked threads or handles.
6. Launch a second instance while one is running: it exits immediately.
7. Cancel the picker at startup: exits silently, no tray icon left behind.
8. Idle overhead: with a recording running, process memory stays flat over 10 minutes (no growth), CPU use of the main thread is near zero between ticks.
9. A 1-hour recording produces a playable file and a log with no errors.

## 11. Rules for the implementer

- Deliver complete files, not snippets with "..." placeholders.
- Keep the dependency boundary from section 5: no `tao` / `tray-icon` imports in `recorder.rs`, `disk.rs`, `paths.rs` or `log.rs`.
- Do not add features from the out-of-scope list.
- Do not add dependencies beyond section 4 without a stated reason.
- Anything in section 9 that turns out different from what is written: follow the crate's real behavior, and leave a short comment explaining the deviation.