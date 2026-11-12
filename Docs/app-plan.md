# Plan: egui window + tray fixes (`src/app.rs`)

Status: **in progress.** See §2 "Fixed" for what landed on 2026-11-11.
Scope: the shipped binary `GelianaFeatherweight` (eframe/egui window + `tray-icon`
in one process). The `ui/` gpui crate is a gitignored mockup and is **out of scope**.

Line numbers refer to `src/app.rs` as of this writing. Re-check them before editing.

---

## 2. Fixed (2026-11-11)

### A0. Tray dead after hiding the window — ROOT CAUSE found and fixed

**Evidence (probe):** an eframe app that calls
`ViewportCommand::Visible(false)` **never receives `update()` again** —
eframe repaints via `window.request_redraw()`, and Windows does not deliver
`WM_PAINT` for hidden windows. The 100 ms poll loop, the 1 Hz ticker, and
every queued tray event died the moment the window was hidden. This was the
real cause of "tray menu events no longer occur": they were queued but never
drained. A minimized window keeps the loop alive (second probe), so minimize
is the correct hiding mechanism.

**Fixes applied:**

1. `set_window_hidden` → `set_window_minimized`: minimize instead of hide;
   restore via `Minimized(false)` + Focus. Tray text stays in sync with the
   real window state by reading `ViewportInfo.minimized` each frame (covers
   taskbar minimize/restore too).
2. Tray events now go through `MenuEvent::set_event_handler` +
   `TrayIconEvent::set_event_handler`, forwarded over an owned
   `mpsc::channel<UiEvent>` with `ctx.request_repaint()` — no more polling
   latency or dependence on the old 100 ms repaint. Hover noise
   (Enter/Move/Leave) is dropped in the handler. The app.rs NOTE claiming
   handlers "consume" events remains true (muda 0.21.2 `send`), which is why
   the handlers forward to our own channel.
3. `build_tray` sets `with_menu_on_left_click(false)`: left click =
   restore window, right click = menu. Previously a left click opened the
   menu AND fired the click handler, popping the window open on every
   menu open (the hidden→shown→shown flapping in the logs).
4. Heartbeat logging removed (was every 25 frames ≈ every 2.5 s, forever).
   State-change logging remains.
5. Start/stop moved off the UI thread into a `RecState` state machine
   (`Idle → Starting → Recording → Stopping → Idle`). Capture init and
   finalization block for seconds; previously the whole UI froze, which read
   as "record is delayed / dead click". The UI now shows "Starting
   recording…" / "Finalizing video…" immediately and stays responsive.
   Quit during start/stop defers the close until the worker finishes so the
   file is not truncated.

**Remaining:** A1, A2, A5, A6, A7 (below), plus the separation-of-concerns
refactor (AppCore shared state) once the bug list is clear.

---

## 1. Priority

| # | Issue | Effort | User-visible? | Status |
|---|---|---|---|---|
| A0 | Tray dead after hide; click flapping; UI freeze on start/stop; heartbeat flood | M | yes | **fixed 2026-11-11** |
| A1 | "Mute microphone" does nothing; Ctrl+Alt+M dead | M | yes | open |
| A2 | Quality preset not shown and not applied | M | yes | open |
| A3 | Idle window repaints at 10 fps | S–M | battery/CPU only | **fixed** (handlers + 1 s idle / 100 ms recording) |
| A4 | Heartbeat log writes forever | S | no | **fixed** (removed) |
| A5 | Only the primary monitor can be recorded | L | yes | open |
| A6 | Meter bar width can go negative | S | edge case | open |
| A7 | Disk-full message overwritten 1 s later | S | yes | open |
| A8 | Tray shows Idle after a failed finalize | S | rare | **fixed** (warn icon + error tooltip) |

Suggested order for the rest: **A1 → A2 → A7/A6 → A5**.

---

## A1. "Mute microphone" is an inert menu item

**Evidence**

- `app.rs:183` enables `mute_mic_item` while recording with the mic track on.
- `app.rs:232` disables it on stop.
- `poll_tray_events` (`app.rs:274`) matches show / quit / new / stop / folder /
  mic / separate-tracks / GDI. **There is no `mute_id` branch.**
- `Cargo.toml:10` declares `global-hotkey = "0.6"`; `grep` finds **zero**
  references anywhere in `src/`. No hotkey is registered.
- `settings.rs` no longer has a `mute_shortcut` field, so the spec's Ctrl+Alt+M
  has nowhere to live.

**Root cause:** mute was never plumbed past the menu layer. There is no muted
state in `ActiveRecording`/`audio.rs` for the UI to toggle.

**Fix (proposed)**

1. Add mutable state: `muted: bool` on `FeatherweightApp` **and** an
   `Arc<AtomicBool>` handed to the mic mixer in `recorder::start_recording`, so
   the audio thread observes it without a lock.
2. In `poll_tray_events`, add a `mute_id` branch: flip the flag, flip the item
   text between "Mute microphone" / "Unmute microphone", log the transition.
3. Reset text and flag in `start_recording`/`stop_recording`.
4. Register the hotkey in `app::run()` (where a long-lived manager can live) and
   forward presses through the same channel introduced in A3 — do **not** spawn
   a second polling loop.
5. Decide whether mute is sticky across recordings; simplest is per-recording
   (reset on start). Do not persist it unless asked.

**Acceptance**

- Recording with mic on: clicking the item removes mic audio from the saved file
  within ~200 ms; the item text flips; clicking again restores it.
- Ctrl+Alt+M does the same from any focus state, including when the window is
  hidden in the tray.
- Both are no-ops (item disabled) when idle or when the mic track is off.
- `FW_DIAG=1` logs one line per transition.

---

## A2. Quality preset is invisible and unimplemented

**Evidence**

- `settings.rs:35` `pub preset: Preset`, enum at `:39` (`Balanced`,
  `LowImpact`), load at `:135`, save at `:172`.
- `render_settings` (`app.rs:817`) renders backend, GDI toggle, system volume,
  mic toggle, separate tracks, mic volume. **No preset control.**
- `recorder.rs:546` `fps: 60, bitrate: 8_000_000` (WGC, hardcoded) and
  `recorder.rs:614` `30 / 6_000_000` (GDI, hardcoded). `grep preset
  src/recorder.rs` → no matches.

So the value round-trips through the ini file and affects nothing.

**Fix (proposed)**

1. Add a "Quality" card (or a row in the Video card) with two selectable rows:
   Balanced / Low impact, using the existing `toggle_row`-style layout. Persist
   on change and log it, same as the other settings.
2. Add `preset: settings::Preset` to the `recorder::start_recording` signature
   (it already takes `force_gdi`, volumes, etc.).
3. Map presets to concrete numbers in `recorder.rs` and use them for both
   backends. Numbers must be confirmed against `Docs/production-spec.md` §3.8;
   do not invent them. Current hardcoded values can serve as the Balanced
   baseline.
4. Surface the effective fps/bitrate in the tray status string so the difference
   is verifiable without a debugger.

**Acceptance**

- Switching preset changes the fps/bitrate logged at recording start, for both
  WGC and GDI.
- The choice survives a restart.
- `Low impact` is measurably cheaper (frame rate and/or bitrate), not just a
  label.

---

## A3. Idle CPU: unconditional 100 ms repaint

**Evidence**

- `app.rs:675` `ctx.request_repaint_after(Duration::from_millis(100))` runs on
  every frame, recording or not.
- `app.rs:110` comment explains why channel handlers were avoided.

**Verified fact (do not "fix" the comment):** `tray-icon 0.26.1` depends on
`muda 0.21.2` (`cargo tree -i muda`), and `muda-0.21.2/src/menu_event.rs:55`
sends each event **either** to the configured handler **or** to the channel —
never both. So installing a handler really does starve
`MenuEvent::receiver()`. The comment is correct.

**Fix (proposed)**

1. In `FeatherweightApp::new`, install handlers once:
   ```rust
   MenuEvent::set_event_handler(Some(move |ev| { let _ = tx.send(UiEvent::Menu(ev)); ctx.request_repaint(); }));
   TrayIconEvent::set_event_handler(Some(move |ev| { let _ = tx.send(UiEvent::Tray(ev)); ctx.request_repaint(); }));
   ```
   `cc.egui_ctx.clone()` is `Send + Sync`, so it can be captured directly.
2. Store the `Receiver<UiEvent>` on the struct; replace the two
   `::receiver().try_recv()` loops in `poll_tray_events` with one drain of that
   channel.
3. Schedule repaints deliberately:
   - recording → `request_repaint_after(100 ms)` (meters need it);
   - window hidden → nothing;
   - otherwise → nothing (eframe still repaints on input, tray events wake it
     explicitly).
   The 1 Hz ticker needs its own `request_repaint_after(1 s)` when visible.
4. Remove or rewrite the stale comment at `:110` and the "200ms repaint" note at
   `:501` (actual interval is 100 ms).

**Acceptance**

- Idle, window open: CPU in Task Manager is ~0%, not a steady few percent.
- Idle, window hidden: no repaint activity at all.
- Tray click, tray menu, window focus and the meters all still respond in
  <200 ms.
- One line in the log confirms handlers were installed (guards against silently
  having both mechanisms active).

---

## A4. Heartbeat log never stops

**Evidence:** `app.rs:501–509` — every 25th frame logs
`update() heartbeat #N`. At the current 10 fps that is every 2.5 s, forever,
mirrored to both `./featherweight.log` and
`%LOCALAPPDATA%\Featherweight\featherweight.log`.

**Fix:** gate it behind the existing `FW_DIAG=1` convention established in
`Docs/bugfix.md`, or delete it now that the app is stable. Do not leave a
per-frame-family log on by default.

**Acceptance:** a normal run writes only lifecycle lines (start, recording
start/stop, settings changes, errors). `FW_DIAG=1` restores the heartbeat.

---

## A5. Only the primary monitor can be recorded

**Evidence:** `recorder.rs:497` and `:779` use `Monitor::primary()`. The egui UI
has no picker; the window list in `Docs/ui.md` (and the mockup in `ui/`) was
never implemented against the real app.

**Fix (larger, needs its own plan):**

1. Enumerate monitors and top-level windows (skip tool windows, own process,
   zero-size) into an egui list with a search field.
2. Pass the selection into `recorder::start_recording` as a
   `CaptureTarget::{Monitor(index), Window(hwnd)}` instead of using
   `Monitor::primary()`.
3. Verify the window-capture entry point for the pinned `windows-capture 2.0.1`
   API before writing code — do not assume function names.
4. Reuse the existing `StopReason` machinery (`recorder.rs:19`) so a closed
   window ends the recording cleanly rather than hanging.

**Acceptance:** pick a window, record it alone; the file contains only that
window; closing it while recording finalizes the file and reports the reason;
falling back to a monitor still works.

---

## A6. Meter bar width can go negative

**Evidence:** `app.rs:1040` `let bar_width = ui.available_width() - 64.0;`
`allocate_exact_size` at `:1042` then receives a negative width in a narrow
window (min width is 360 px, so this is reachable with the dBFS label plus
indentation).

**Fix:** `let bar_width = (ui.available_width() - 64.0).max(40.0);`

**Acceptance:** at minimum window width with the mic meter visible, the bar
renders, nothing panics, nothing overlaps.

---

## A7. Disk-full stop message is overwritten within a second

**Evidence:** `app.rs:388–405` stops the recording and sets
`status_item`/tooltip to `Stopped: Disk Full (X left)`. The next ticker run
(fires every second, `:368`) sees `active_recording == None` and rewrites the
status to `Idle | X free` / `Low disk | X free`.

**Fix:** add a sticky `last_warning: Option<String>` (set on emergency stop,
cleared on the next successful `start_recording`) that the idle branch prefers
over the computed idle text.

**Acceptance:** after a forced disk-full stop (use `FEATHERWEIGHT_MIN_FREE_MB`
from `disk.rs` to trigger it), the window and the tray keep showing the reason
until the next recording starts.

---

## A8. Tray left inconsistent after a failed finalize

**Evidence:** `app.rs:221–225` sets the status to `Error finalizing video`, but
execution falls through to `:233–234` which restores the **stopped** icon and
the `Featherweight — Idle` tooltip.

**Fix:** on the `Err` arm, set `warn_icon()` and a tooltip that states the
failure; keep the status line until the next start.

**Acceptance:** simulate a finalize failure (e.g. output file locked by another
process); the tray icon stays amber and the tooltip still reports the error
after the next ticker run.

---

## 3. Explicitly out of scope here

- **Separate audio tracks require `ffmpeg` on PATH.** Real, and against
  `Docs/production-spec.md`, but it is an audio/mux workstream, not a UI fix.
- **Window-target capture** (A5) is listed for planning only; it deserves its own
  brief once the small fixes land.
- **`ui/` gpui crate**: gitignored, not in the build, not wired to the recorder.
  Leave it parked as a design reference unless we decide to ship a second
  process.
- **`settings.rs:5` header comment** lists only `mic_track`/`preset` while the
  file now stores five keys. Doc-only nit; fix when touching that file.

## 4. Rules for whoever implements this

1. One issue per commit; re-run `cargo check` after each.
2. Every behaviour change gets an `FW_DIAG`-gated log line, not a default one.
3. Do not change the recorder's threading or the mux path as part of a UI fix.
4. Keep the tray menu and the window in sync in both directions — every setting
   that exists in both places must update the other.
