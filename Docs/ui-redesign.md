# UI Redesign: View-Based Navigation with Recording Monitor and About/Licenses

## Context
The app is a single scrolling settings window (`render_idle` in `src/app/window.rs`): status bar + recording bar + banners + a two-column settings grid, all in one place. The user wants a proper multi-view UI: a **Recording view** that surfaces live session details including a **still image of the first captured frame**, and an **About view** with developer, app version, and full third-party license texts. User decisions: **top tab bar** navigation, **Recording is the landing view with auto-switch on start**, **full embedded license texts**.

Work happens on a new branch `feature/ui-views` off `development`.

## 1. View framework — `src/app/window.rs`
- Add `enum View { Recording, Settings, About }` + `view: View` field (default `Recording`).
- In `fn ui`: keep the existing prelude (hide-on-first-frame, tray events, close interception, `update_window_and_audio`, `tray.sync_session`); render an `egui::TopBottomPanel::top("tabs")` strip with three tab buttons using the existing `Button::selected` idiom (matching the Monitor/Window toggle chips), theme tokens only. The Recording tab label carries a red ● + live `mm:ss` while a session runs.
- `CentralPanel` dispatches: `render_recording` | `render_settings` | `render_about`.
- Move the repaint scheduling (1 s; 50 ms when audio preview runs) from `render_idle` up into `fn ui` so it applies to all views.
- Auto-switch: UI `start_recording()` sets `view = Recording`; plus a `was_active` flag in `fn ui` detects Idle→active transitions (covers tray "New recording" while window visible). No auto-show of a hidden window — unchanged behavior.

## 2. Recording view — `render_recording` (new, `src/app/window.rs`)
Reuses the pieces of today's `render_idle` (title row + disk chip logic, banners, warning/hint/error boxes — moved here):
- **Waiting**: existing amber waiting banner + explanation + Stop.
- **Recording**: thumbnail on the left; right column: ● `Recording mm:ss` (monospace), target title, stat chips (size · rate · RAM via existing `stats_readout`), resolution (from thumbnail texture size when loaded), output path (truncated, hover = full) + **Open folder** button (reuse `open_folder` in `src/tray/menu.rs:226`, made `pub(crate)`), Stop button.
- **Idle**: "No recording running" + Record button; if a thumbnail from the just-finished session is still in memory, show a dimmed "Last recording" card with its path.
- Disk critical/warn banners, `last_error`/`stop_reason` notices, first-run hint, and `self.warning` render here (as today).
- Idle disk-space polling (once per second, `crate::disk::free_bytes_for_path`) lives here since the chip is here.

**Thumbnail loading**: new fields `thumb: Option<egui::TextureHandle>`, `thumb_for: Option<PathBuf>` (session output path the texture belongs to). While a session is active and `thumb_for` doesn't match: read `pipeline::first_frame_png_path()`, decode with existing `theme::decode_png_rgba` (`src/app/theme.rs:48`), upload via `ctx.load_texture(..., ColorImage::from_rgba_unmultiplied, TextureOptions::LINEAR)`. Retry cadence: request a 200 ms repaint until loaded. Display sized to fit available width/height preserving aspect; before load (and in simulated mode) show a bordered placeholder explaining why.

## 3. Settings view — refactor `render_idle` → `render_settings`
Same file. Strip out everything that moved to the Recording view; keep the two-column settings grid (`Video & Audio` / `App & Capture` + Shortcuts) with its `ScrollArea` unchanged, including rebind capture, preview meters, config-change detection/save at end of `fn ui` (untouched).

## 4. About view — new `src/app/about.rs`
- `pub struct AboutView { icon: Option<TextureHandle> }` owned by `FeatherweightApp`; `render(&mut self, ui, ctx)` loads the app icon lazily (expose `APP_ICON` decode from `theme.rs`).
- Identity card: icon, **Featherweight**, version chip `env!("CARGO_PKG_VERSION")`, description `env!("CARGO_PKG_DESCRIPTION")`, developer line from `env!("CARGO_PKG_AUTHORS")` ("Stephen Telian · Geliana"), homepage hyperlink `env!("CARGO_PKG_HOMEPAGE")`, license MIT.
- Third-party licenses: `CollapsingHeader` per license group, monospace full text in a bounded `ScrollArea`:
  - **MIT** — chrono, dirs, directories, eframe/egui, tray-icon, png, serde, serde_json, tempfile, windows, windows-sys, windows-capture, wasapi, rfd + build deps (winresource, cc); plus exact LICENSE texts of the MIT-only crates (windows-capture, wasapi, rfd).
  - **Apache-2.0** — eframe/egui, winit + the dual-license list; full text once.
  - **BSD-3-Clause** — nnnoiseless (RNNoise), exact LICENSE from the cargo registry cache.
  - **SpeexDSP** (vendored) — `include_str!("../../vendor/speexdsp/COPYING")`.
- New files `src/assets/licenses/*.txt` created by copying the actual texts from the local cargo registry (`~/.cargo/registry/src/...`), embedded with `include_str!`. Add `pub mod about;` to `src/app/mod.rs`.

## 5. First-frame thumbnail capture — `src/capture/pipeline.rs`
- `pub(crate) fn first_frame_png_path() -> Option<PathBuf>` → `%LOCALAPPDATA%\Featherweight\first-frame.png` (same dir pattern as `log.rs:8`), overwritten per session.
- `WgcFlags` gains `thumbnail: Option<PathBuf>`; in `on_frame_arrived`'s first-frame branch (`pipeline.rs:235-269`), after encoder creation, grab the frame via the existing `frame.buffer()` / `as_nopadding_buffer` pattern and write a downscaled PNG. Non-fatal on failure (log once).
- GDI fallback: encode the first successful `grab()` the same way — its DIB is bottom-up, so the helper takes a `bottom_up: bool` flag.
- Pure helpers + unit tests (following the `letterbox_*` test style): `downscale_bgra` (nearest-neighbor, max 960×540, aspect-preserving), BGRA→RGBA swap, vertical flip for the GDI path, and a PNG encode/decode roundtrip (encode to `Cursor<Vec<u8>>` via `png::Encoder`, decode via `theme::decode_png_rgba`).

## 6. Target title plumbing — `src/capture/recorder.rs` + `src/app/session.rs`
- `RecorderEvent::Started(PathBuf)` → `Started { path: PathBuf, target: String }`. The worker builds `target` at start: window title, or monitor label via `enumerate_monitors()` lookup (fallback `Monitor {index}`). Update both emit sites (`recorder.rs:331`, `recorder.rs:375`) and the simulated worker.
- `SessionStats` gains `target_title: Option<String>` and `output_path: Option<PathBuf>`; the pump sets them on `Waiting`/`Started`, clears on finish. Update pump match arms and session tests.

## Files changed
`src/app/window.rs` (views + tab bar + recording view + thumbnail), `src/app/about.rs` (new), `src/app/mod.rs`, `src/app/theme.rs` (icon exposure), `src/app/session.rs`, `src/capture/recorder.rs`, `src/capture/pipeline.rs`, `src/tray/menu.rs` (`open_folder` visibility), `src/assets/licenses/*.txt` (new).

## Verification
1. `cargo test --release` — existing 62 tests plus new pipeline (downscale/flip/roundtrip) and session tests; all must pass.
2. `cargo build --release` clean.
3. Manual run: tabs render; default view = Recording; Record from Settings auto-switches; thumbnail appears after the first frame (test minimized-window start → placeholder during Waiting); Open folder works; About shows v0.2.0, developer, collapsible full license texts; tray/hotkey start still switches the tab when visible.
4. `FEATHERWEIGHT_SIMULATE=1 FEATHERWEIGHT_AUTOSTART=1 cargo run` to inspect the recording layout without real capture (thumbnail placeholder path).