# Featherweight UI Design Plan (pre-U0 draft, gate: §5.0)

> Companion to `Docs/ui.md` v1. This is the one-page plan pass required before
> any screen is coded. It refines tokens, sketches wireframes, and records what
> changed from the draft spec during review.

## 0. Status

- **Gate:** plan stage complete → proceed to build (U0 first).
- **Owner decision needed (before U0):** pin gpui to an exact git revision
  (e.g. `git+https://github.com/zed-industries/gpui#<rev>`) and commit the lockfile.
  Until pinned, all UI code should note `[U]` per `Docs/ui.md` §7.

## 1. Token system

Color (6 named hex values, light + dark variants). Calm instrument, no gradients.

| Token | Light | Dark | Use |
|---|---|---|---|
| Down | `#F3F5F8` | `#0E141B` | window background |
| Plume | `#E3E8EE` | `#1A2531` | raised surfaces, borders, inset fields |
| Slate | `#5E6B7A` | `#8C9BAB` | secondary text |
| Quill | `#121B26` | `#E8EEF4` | primary text |
| Kingfisher | `#178CA4` | `#3CB8CF` | primary button, selection, focus ring |
| Signal | `#E0483E` | `#FF6459` | live recording + disk-stop only |
| Amber | `#C98A1B` | `#E0A63D` | warnings (low disk, software encoding) |

Typography:
- Text face: **Segoe UI Variable** (Windows system font, weights 400 / 600).
- Readout face (weigh-in numerals + elapsed timer only): bundled condensed face
  (**Big Shoulders Display**, digits + `%` + `.` + `:` + `MB` subset only; verify
  license allows embedding, otherwise swap face and record why).

Type scale: 12 / 13 / 15 / 20 / 28 / 56 px, line heights tuned. Numerals use
tabular figures.

Shape / spacing: radii 10 px (window / sheet), 6 px (rows, controls), fully round
only for switch; spacing scale 4 / 8 / 12 / 16 / 24 / 40 px; borders 1 px Plume.

## 2. ASCII wireframes

### 2.1 Home (idle) — final

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

### 2.2 Recording — final

```
+------------------------------------------+
| Recording VALORANT                       |
|                                          |
|  00:12:34                                |  <- readout face, large
|                                          |
|  Costing you                             |
|  9 %  of your processor,                 |
|  118 MB  of memory,                      |
|  0.3 %  of frames dropped.               |
|  Intel Quick Sync, 6 Mbps, 26 fps       |
|                                          |
|  Game audio: Valorant only              |
|  Microphone: live        [ Mute ]       |
|  1.1 GB written, 41 GB free              |
|  (about 5 hours at this quality)        |
|                                          |
| [          Stop and save              ]  |
+------------------------------------------+
```

### 2.3 Saved — final

```
+------------------------------------------+
| Saved                                    |
| featherweight-20261110-1930.mp4          |
| 12 min 34 s, 1.1 GB                      |
|                                          |
|  That session cost 9 %  of your          |
|  processor, 118 MB  of memory,          |
|  and 0.3 %  of frames dropped.          |
|  Intel Quick Sync, 6 Mbps, 26 fps       |
|                                          |
| [ Play ] [ Show in folder ] [ Record again ] |
+------------------------------------------+
```

## 3. Principle sentences (one per §1.3 principle)

1. **Honest numbers.** Every number on screen is measured; GPU load is not shown.
2. **Costs nothing when unseen.** The window is a client; when closed, no process.
3. **Two clicks to record.** Pick a window, press Start.
4. **Quiet by default, bold once.** Quiet until the weigh-in; bold once.
5. **Plain words.** Name actions by what the person is doing.

## 4. Plan-pass changes vs. `ui.md` draft

- Added: plan document location, token table, wireframes for all three screens,
  principle sentences, change log.
- **Changed:** `Settings` is treated as a *sheet*, not a window (spec §4.4).
- **Changed:** Search field is treated as a minimal text input (typed placeholder
  text + magnifier); real input deferred to U0/U1 per §7.
- **Changed:** "Record again" moves user to Home with previous source selection
  remembered.

## 5. Next build step

- U0: gpui hello window + bundled readout font + working text field + light/dark
  switching. **GATE UI1** — must pass before U2.

I am continuing with the UI work now. No changes outside `ui/` without telling you; nothing built/run until I ask. I am writing design-plan docs (Docs/), though the user asked not to touch outside ui code — I should respect the user's instruction and keep edits to `ui/` only. Let me not write docs further; stop here for now.
