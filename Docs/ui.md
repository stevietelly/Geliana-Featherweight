# Featherweight UI Spec v1 (gpui)

Audience: the engineer (or model) building the Featherweight window. Read this whole document first, then `BACKGROUND_MODE_SPEC.md` and `PRODUCTION_SPEC.md` (sections 2, 4.5, 6, 7, 8).

## 0. Purpose and rules

The recorder (tray process) already works without any window. This spec describes the optional **window**: choose what to record, see what the recorder costs, change a few settings, find recordings afterwards.

Rules:

1. **Before designing anything, list every design or UI skill available to you and read each one in full.** Owner-installed skills: `____ (owner: list the skill names here)`. Skills written for the web (HTML, CSS, React) are **design guidance only**. The output is native Rust using **gpui**. No WebView, no Tauri, no Electron, no HTML.
2. Follow the design process in section 5.0 (plan, review against this brief, build, critique with screenshots). Do not start coding screens before the plan exists.
3. Verify every gpui API against the exact git revision you pin. gpui is pre-1.0 and its API changes often. Where this document's names differ from the real ones, follow the real API and leave a one-line comment.
4. Complete, compilable files. No `...` and no `todo!()` in delivered code.
5. Do not build anything in section 10.
6. Evidence tags as in the production spec: **[U]** expected, never tested; **[GATE]** an acceptance test in section 9 must pass.

---

## 1. Design intent

**Subject.** Featherweight is a screen recorder for people whose PCs barely run their game.

**Audience.** PC gamers on modest laptops and older desktops. They are in a hurry, and suspicious of anything that might cost them frames.

**Primary jobs, in order:**
1. Start recording the right window in two clicks.
2. Trust that the recorder is light, because they can see what it costs.
3. Find the clip afterwards.

**Feeling.** Calm, light and precise, like a good instrument panel that shows only what it needs. It is the opposite of loud gamer chrome (no neon, no angular tech-HUD, no RGB gradients), and it also avoids a bland, templated settings app.

**The one memorable thing: the weigh-in.** An honest readout of what Featherweight itself costs: share of the processor, memory, frames dropped, which encoder. It appears live while the window is open during a recording, and as the closing card of every session. Spend all the boldness here; keep everything else quiet and disciplined.

**Principles:**
1. **Honest numbers.** Every number is measured, never decorative or promotional. If something is not measured (GPU load), it is not shown.
2. **Costs nothing when unseen.** The window is a client; when it is closed the process is gone.
3. **Two clicks to record.** Pick a window, press Start.
4. **Quiet by default, bold once.**
5. **Plain words.** Name things by what the person is doing, not by how the system works.

---

## 2. Process model and zero-cost rules

- The UI is a **separate binary**, `featherweight-ui`. The tray process (the recorder) is the only thing that is always running. The UI is started from the tray menu item **"Open Featherweight"** and **exits when its window closes**. It never auto-starts and never launches with a game.
- Recording must work with no UI at all. A UI crash or exit never affects a recording.
- **Single instance** via a named mutex (`Local\Featherweight-UI`). A second launch finds the first window (by its window class) and brings it to the front, then exits.
- **After "Start recording", the window closes itself** once the recorder reports `STATE recording`. The game must never be covered by our window. Reopen from the tray at any time.
- If the window is open during a recording:
  - redraw **only** on a state change or the once-per-second `STATS` message;
  - no looping animation of any kind;
  - no rendering while minimized or fully covered (verify gpui behavior; if gpui keeps rendering, stop requesting frames).
- **No overlays, no in-game HUD, no injection.** Anti-cheat safety is a product promise.
- If the UI is launched while the recorder is not running, it shows "Starting Featherweight" and starts `featherweight.exe`, then connects (retry for about 3 s).
- Budgets: **0% CPU when idle** (no timers, no redraw loop); first frame in under 1 second on the reference machine; report idle memory. These are measured in section 9.

---

## 3. IPC protocol

Transport: a named pipe, `\\.\pipe\featherweight-<username>`, **hosted by the recorder (tray) process** on one blocking server thread (zero CPU while no client is connected). One client at a time. Messages are UTF-8, one per line: `NAME key=value key=value`. Strings are double-quoted with `\"` and `\\` escapes. Unknown keys are ignored. A tiny shared crate, `featherweight-ipc`, holds the message types and a hand-written parser (no `serde` in the recorder), with unit tests for the parser.

UI to recorder:

```
HELLO ui=1
START kind=window hwnd=<u64> pid=<u32> title="<text>"
START kind=monitor index=<n>
STOP
MIC state=mute|unmute
SET key=<name> value=<text>          keys: mic_track (0|1), preset (balanced|low_impact), output_dir (path)
GET what=state|settings|recent
PROBE what=encoder
```

Recorder to UI:

```
HELLO recorder=1 version="<semver>"
STATE value=idle|starting|recording|stopping reason=<user|window_closed|disk_low|error>
STATS secs=<n> bytes=<n> free_bytes=<n> cpu_pct=<f> ws_mb=<n> fps=<f> dropped_pct=<f> mic=off|live|muted
INFO encoder="<name>" hw=0|1 container=fmp4|mp4 app_audio=process|system profile="<h264 profile>"
SAVED path="<path>" secs=<n> bytes=<n> avg_cpu_pct=<f> peak_ws_mb=<n> dropped_pct=<f> avg_fps=<f>
SETTINGS mic_track=<0|1> preset=<name> output_dir="<path>"
RECENT path="<path>" bytes=<n> modified=<unix>       (repeated, newest first, at most 10)
ERROR code=<id> text="<plain-language sentence>"
```

Rules:
- `STATS` is sent once per second **only while a client is connected and a recording is running**.
- `cpu_pct` is the recorder process's CPU as a share of **total machine capacity**, computed over a **rolling 10-second window**. Never per second: process-time granularity on Windows is coarse and per-second values are noisy.
- `SAVED` carries the session averages, which feed the closing weigh-in card. The recorder keeps running sums; it does not store per-second history.
- **Source selection moves into the UI.** The UI enumerates windows itself (so it knows `hwnd` and `pid`) and sends `START`. The recorder builds the capture item from the `hwnd`, not through `GraphicsCapturePicker`. This gives the audio code the target `pid` directly (this replaces the title-matching strategy in `PRODUCTION_SPEC.md` section 4.2 for the UI path). The tray's "New recording..." keeps using the system picker as a fallback.

---

## 4. Screens and flows

Single narrow column, about 400-440 px wide, fixed width, height follows content up to about 720 px. Left-aligned text; numerals right-aligned and tabular.

### 4.1 Home (idle)

```
+------------------------------------------+
| Featherweight                        gear|
|                                          |
| What do you want to record?              |
| [ Search windows                       ] |
|                                          |
|  [ico] VALORANT                  (sel.)  |
|  [ico] Discord                           |
|  [ico] Firefox  Docs                     |
|  Entire screen: Display 1                |
|                                          |
| Quality   [ Balanced | Low impact ]      |
| Microphone                        [ off ]|
|   Uses your microphone only while        |
|   recording.                             |
|                                          |
| [          Start recording            ]  |
|                                          |
| Hardware encoding with Intel Quick Sync. |
|                                          |
| Recent                                   |
|  featherweight-20261110-1930.mp4  1.1 GB |
+------------------------------------------+
```

- **Window list.** Enumerate top-level windows with `EnumWindows`. Include only windows that are visible, not cloaked (`DwmGetWindowAttribute` with `DWMWA_CLOAKED`), have a title, have no owner, are not `WS_EX_TOOLWINDOW`, and do not belong to our own processes. Keep the enumeration order, which approximates z-order and so "most recently used first". Show the executable's icon and the window title. Monitors appear as a separate "Entire screen" group.
- **Icons [U].** Open the process with `PROCESS_QUERY_LIMITED_INFORMATION`, read the image path with `QueryFullProcessImageNameW`, extract the icon, convert it to RGBA and cache it. If any step fails (protected processes such as an anti-cheat-guarded game may refuse), show a generic glyph and the title only. Never fail the list because an icon failed.
- **Refresh:** on window activation and when the user presses Refresh. **No polling timer.**
- **Selection then Start.** One selection at a time; Start is disabled until one is chosen. Enter starts recording, Esc closes.
- **Quality** and **Microphone** write immediately through `SET` and persist in the recorder's settings file (`PRODUCTION_SPEC.md` section 8).
- The footer line comes from `PROBE what=encoder` (see 4.5 for the software case).
- **Recent** shows up to 5 entries from `RECENT` (name, size). Shell thumbnails (`IShellItemImageFactory`) are an optional enhancement **[U]**: cheap because the shell caches them, and they make this list much nicer. Do not generate thumbnails yourself.

### 4.2 Recording (window reopened during a session)

```
+------------------------------------------+
| Recording VALORANT                       |
|                                          |
|  00:12:34                                |   <- readout face, large
|                                          |
|  Costing you                             |
|  9 % of your processor,                  |   <- the weigh-in (see 5.6)
|  118 MB of memory,                       |
|  0.3 % of frames dropped.                |
|  Intel Quick Sync, 6 Mbps, 26 fps        |
|                                          |
|  Game audio: Valorant only               |
|  Microphone: live        [ Mute ]        |
|  1.1 GB written, 41 GB free              |
|  (about 5 hours at this quality)         |
|                                          |
| [          Stop and save              ]  |
+------------------------------------------+
```

- The red **Signal** color appears only here (and in the tray icon): the live indicator next to the title.
- Disk estimate: free bytes minus the 1 GiB floor, divided by the recent write rate (from `STATS`).
- Audio line: "Game audio: <name> only" for process loopback, or "All system audio" with the reason when the recorder fell back (see section 6).

### 4.3 Saved (the closing weigh-in)

```
+------------------------------------------+
| Saved                                    |
| featherweight-20261110-1930.mp4          |
| 12 min 34 s, 1.1 GB                      |
|                                          |
|  That session cost 9 % of your           |
|  processor, 118 MB of memory,            |
|  and 0.3 % of frames dropped.            |
|  Intel Quick Sync, 6 Mbps, 26 fps        |
|                                          |
| [ Play ] [ Show in folder ] [ Record again ] |
+------------------------------------------+
```

The numbers come from `SAVED`. If the stop reason was disk-low or a closed window, a plain sentence above the card says so (section 6).

### 4.4 Settings

Reached from the gear on Home. A sheet, not a new window. Contents (nothing else):

- Output folder, with a Change button (writes `output_dir`).
- Quality (the same segmented control).
- Microphone track (the same switch), with the "only while recording" note.
- Mute shortcut: shown read-only (Ctrl+Alt+M).
- About: version, and "Open log folder".

### 4.5 States and errors

| State | Behavior |
|---|---|
| Recorder not running | "Starting Featherweight" with no controls; connect within about 3 s, else show the error row below |
| No hardware encoder (`hw=0`) | Footer becomes a banner (copy in section 6); recommend Low impact |
| Connection lost during a recording | Banner "Lost connection..."; the recording is unaffected |
| `ERROR` message | Banner with the `text` supplied by the recorder, plus a Dismiss button |
| No windows found | Empty state with Refresh |

---

## 5. Visual system

### 5.0 Process (mandatory)

1. **Plan first.** Write a one-page design plan before any code: a compact token system (color as 4-6 named hex values, type roles, radii and spacing scale), ASCII wireframes for Home, Recording and Saved, and one sentence per principle. Save it in the repo as `docs/ui-design-plan.md`.
2. **Review the plan against this brief and against the defaults list below.** If any part reads like what you would produce for any recorder or settings app, revise it and note what you changed and why.
3. **Build.**
4. **Critique with screenshots** of each screen in light and dark at 100% and 150% scaling. Before finishing, remove one decorative element ("take one accessory off"). Record what you removed.

**Defaults to avoid** (all legitimate in some briefs; here they would be defaults, not choices):
- a warm cream background with a serif display and a terracotta accent;
- near-black with a single bright neon accent (the gamer-tool cliché);
- a stack of identical rounded cards with the same shadow and gradient washes;
- tracked-out ALL-CAPS labels, eyebrow labels above headings, spaced em-dash labels, monospace small data labels, a trailing arrow on buttons;
- accenting a single word in a heading.

### 5.1 Direction: calm instrument

One column, generous vertical rhythm, thin borders instead of shadows, almost no decoration. Light theme is the home theme, with a matching dark theme; follow the Windows light/dark setting on start and when it changes **[U]** (verify gpui exposes appearance changes).

### 5.2 Color (proposal; refine in the plan pass, keep the names)

| Token | Light | Dark | Use |
|---|---|---|---|
| Down | `#F3F5F8` | `#0E141B` | window background |
| Plume | `#E3E8EE` | `#1A2531` | raised surfaces, borders, inset fields |
| Slate | `#5E6B7A` | `#8C9BAB` | secondary text |
| Quill | `#121B26` | `#E8EEF4` | primary text |
| Kingfisher | `#178CA4` | `#3CB8CF` | primary button, selection, focus ring |
| Signal | `#E0483E` | `#FF6459` | **live recording and disk-stop only** |
| Amber | `#C98A1B` | `#E0A63D` | warnings (low disk, software encoding) |

No gradients. One soft shadow, used only on the Settings sheet. Contrast: all text at least 4.5:1 against its background in both themes (check it, do not assume).

### 5.3 Typography

- **Text face: Segoe UI Variable** (the Windows system font). Zero bundled bytes, native feel. Weights 400 and 600.
- **Readout face: one bundled, openly licensed condensed display face** used **only** for the weigh-in numerals and the elapsed timer. Proposal: Big Shoulders Display (a condensed sports-lettering design that suits a weigh-in). Confirm the license permits embedding, include only the weights and glyphs needed (digits, percent, period, colon, "MB"), and test rendering in gpui. If it renders badly, choose another condensed face and say why.
- Scale: 12 / 13 / 15 / 20 / 28 / 56 px with matching line heights; tabular figures for all numerals. Line length under 80 characters.
- **Sentence case everywhere.** No ALL CAPS, no eyebrow labels, no monospace, no trailing arrows.

### 5.4 Shape and spacing

Radii follow hierarchy: **10 px** for the window's main surface and sheet, **6 px** for rows and controls, fully round only for the switch. Do not apply one radius to everything. Spacing scale 4 / 8 / 12 / 16 / 24 / 40 px. Borders are 1 px Plume.

### 5.5 Components (build once, reuse)

Button (primary, secondary, quiet), segmented control, switch, selectable list row with icon, search field, readout (numeral plus words inline), banner (info, warning), sheet. Draw icons as simple vector assets (gpui `svg`), at most about a dozen. Check whether `gpuikit` or `gpui-component` already provides a good version before writing your own, and prefer your own if theirs does not match the tokens. **A text input is required for the search field; gpui core may not have one** (section 7).

### 5.6 The weigh-in (signature element)

Not a row of KPI tiles. It is a **typeset sentence**: the words are set in the text face at 15 px and the numerals in the readout face at about 56 px, flowing together, so the type treatment is the design.

```
  9 %  of your processor,
  118 MB  of memory,
  0.3 %  of frames dropped.
  Intel Quick Sync, 6 Mbps, 26 fps
```

- Numerals right-aligned to a shared edge on the left of each line; words baseline-aligned to the numeral.
- Only measured values appear. **GPU load is not shown in v1.** (A GPU video-encode reading through the Windows performance counters is an optional later addition **[U]**.)
- Live version (4.2): updates once per second from `STATS`; numerals do not animate between values.
- Closing version (4.3): see motion below.
- If `hw=0`, the last line reads "Software encoding, 6 Mbps, 26 fps" and uses the Amber color.

### 5.7 Motion

- No ambient or looping animation anywhere.
- **Responses to actions only:** pressing Start changes the button state and then the window closes; pressing Stop replaces the Recording screen with Saved.
- **One orchestrated moment:** on the Saved screen, the weigh-in numerals settle from zero to their final values over about 600 ms. Respect the Windows "Show animations" setting (`SPI_GETCLIENTAREAANIMATION`); with it off, show final values immediately.

### 5.8 Accessibility and platform

- Full keyboard use: Tab order follows reading order; Enter activates the focused control or starts recording from Home; Esc closes.
- Visible focus ring: 2 px Kingfisher with a 2 px offset.
- DPI aware (100%, 125%, 150%, 200%); text and icons stay crisp.
- Native title bar for v1, themed to the app theme (the DWM dark-mode attribute). Do not spend the budget on custom window chrome unless gpui's custom title bar works first try on Windows.
- **Known limitation to document in the README:** gpui does not currently expose screen-reader accessibility **[U]**. The keyboard and contrast requirements above are the floor.

---

## 6. Copy

Plain verbs, sentence case, active voice, no filler. An action keeps the same name everywhere. Errors say what happened and what to do, and do not apologize.

| Moment | Text |
|---|---|
| Start button | Start recording |
| Stop button | Stop and save |
| Mic switch | Microphone, with the note "Uses your microphone only while recording." |
| Mic mute / unmute | Mute / Unmute |
| Saved screen title | Saved |
| Closing weigh-in lead | That session cost |
| No windows | No windows to record. Open the game, then press Refresh. |
| No hardware encoder | Your graphics card can't encode video, so Featherweight uses the processor instead. Expect more load. Low impact quality helps. |
| App audio fallback | Recording all system audio, because this window's sound couldn't be isolated. |
| Disk stop | Recording stopped when free space reached 1 GB, so the file stays playable. Free some space, then record again. |
| Window closed | The window closed, so the recording ended and was saved. |
| Mic unavailable | Your microphone isn't available. The recording continues without it. |
| Connection lost | Lost connection to the recorder. Recording continues from the tray. |
| Recorder starting | Starting Featherweight |

Keep every user-facing string in one `strings.rs` file (makes later localization possible without building it now).

---

## 7. gpui implementation notes and risks

- **Pin gpui to an exact git revision** and commit the lockfile. Expect API churn when upgrading; upgrades are a separate task.
- **U0 gate first** (section 8): confirm gpui builds and runs on Windows on the reference machine before designing around it.
- **Rendering discipline:** call `cx.notify()` only when state changes; request animation frames only during the 600 ms settle animation.
- **Text input:** verify whether gpui core, `gpuikit` or `gpui-component` offers a working single-line text field with clipboard and basic IME. If none does, implement a minimal one (typing, backspace, selection, paste) and document its limits.
- **Fonts:** register the bundled readout font through gpui's asset/text system. Keep the embedded font subset small.
- **Window list:** at most about 100 rows, so no virtualization needed.
- **Icons from executables:** convert `HICON` to RGBA through `GetIconInfo` and `GetDIBits`, cache by executable path, free GDI handles.
- **Threading:** the pipe client runs on its own thread and sends events into the gpui app through a channel; the UI thread never blocks on the pipe.
- **Fallback plan if U0 fails:** the same IPC and the same design tokens can sit on `egui` with less polish. That is a decision for the owner, not the implementer.

---

## 8. Build order and gates

| Step | Work | Gate |
|---|---|---|
| U0 | gpui hello window with the bundled font, a working text field and light/dark switching | **[GATE] UI1** |
| U1 | `featherweight-ipc` crate with parser tests; pipe server in the tray process; the UI against a mock server | parser tests pass |
| U2 | Home screen: window list, monitors, Quality and Microphone, Start and Stop wired to the real recorder | **UI2** |
| U3 | Recording and Saved screens, `STATS` pipeline with rolling CPU, weigh-in | **UI12** |
| U4 | Settings sheet, Recent list (shell thumbnails optional), error and empty states | UI5, UI8 |
| U5 | Polish pass per section 5.0, accessibility, performance checks | **UI3, UI9** |

Do the design plan (5.0) before U2.

---

## 9. Acceptance tests

| ID | Test | Pass criteria |
|---|---|---|
| UI1 | U0 results | Builds on Windows; first frame in under 1 s; bundled font renders; text input works; **report idle memory and idle CPU** |
| UI2 | Two clicks | From the tray "Open Featherweight": select a window, press Start. Window closes; tray icon turns red; recording file grows |
| UI3 | Zero idle cost | Window open and idle for 60 s: CPU at 0.0% apart from input events. With the window open during a recording: UI process under 1% of one core, redraw at most once per second |
| UI4 | Crash isolation | Kill the UI process mid-recording: recording continues and the file finalizes correctly |
| UI5 | Connection loss and reconnect | Kill and restart the UI mid-recording: it reconnects and shows the live state; banner shown while disconnected |
| UI6 | Scaling and themes | 100%, 150%, 200% scaling and light/dark switch: no clipped text, blurry icons or contrast failures |
| UI7 | Keyboard only | Home to Start to Stop to Saved without a mouse; focus always visible |
| UI8 | Copy review | Every error and empty-state string says what happened and what to do; every action name is consistent across screens |
| UI9 | Design critique | Screenshots of all screens in both themes attached; the defaults list (5.0) checked item by item; the removed "accessory" recorded |
| UI10 | Protected game | The window list shows Valorant with an icon or a graceful generic glyph; nothing crashes if the process cannot be opened |
| UI11 | Window list correctness | No cloaked windows, tool windows or our own windows; Refresh picks up new windows |
| UI12 | Weigh-in accuracy | The numbers shown equal the values in the recorder's log for the same session (CPU within rounding, drops and fps exactly) |

---

## 10. Out of scope

A custom hotkey editor, any overlay or in-game HUD, an in-app video player or editor, trimming, cloud upload, accounts, a theme picker, localization beyond keeping strings in one file, auto-update, an installer, replacing the native tray icon, GPU load display, a custom title bar.

---

## 11. Changes to other specs

- `PRODUCTION_SPEC.md` section 4.2: for the UI path the PID arrives in `START` (title matching remains only for the tray-picker fallback).
- `PRODUCTION_SPEC.md` section 7: add the tray item **"Open Featherweight"** (launches `featherweight-ui`).
- `PRODUCTION_SPEC.md` section 6: the recorder computes the rolling 10 s CPU figure and the session sums needed for `STATS` and `SAVED`, and hosts the pipe server thread.
- Workspace split becomes required: `featherweight-core` (library), `featherweight` (tray recorder), `featherweight-ui` (gpui), `featherweight-ipc` (shared messages, no dependencies).