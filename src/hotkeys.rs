//! Global keyboard shortcuts for start/stop recording.
//!
//! This module owns the background hotkey polling thread, the shortcut
//! binding types that are persisted in `settings.json`, and the edge
//! detection that turns raw key state into clean start/stop events.
//!
//! Contents:
//! - [`KEYS`]: lookup table bridging egui `Key` variants, Win32 virtual-key
//!   codes, and human-readable labels.
//! - [`Shortcut`]: a single key + modifier combination (one binding).
//! - [`Shortcuts`]: the start/stop binding pair persisted with [`AppConfig`].
//! - [`HotkeyController`]: shared state between the UI thread and the polling
//!   thread (live bindings, suspend flag, swappable stop signal, shutdown).
//! - [`HotkeyEvent`]: events the polling thread sends to the UI via a channel.
//! - [`KeyEdges`]: rising-edge detector that fires once per key press and
//!   waits for all keys to be released after a rebind or resume.
//! - [`spawn`]: starts the 8 ms polling thread backed by `GetAsyncKeyState`.
//! - [`input_matches_polling`] / [`key_down`] / [`signal_stop`]: helpers used by
//!   both the UI (rebind verification) and the polling thread.
//! - `tests`: unit tests covering validation, edge detection, and stop-signal
//!   isolation between recording sessions.
//!
//! [`AppConfig`]: crate::config::AppConfig

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::log;
use eframe::egui::{Key, Modifiers};
use serde::{Deserialize, Serialize};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
};

/// Lookup table mapping an egui [`Key`] to its Win32 virtual-key code and a
/// short display label.
///
/// Only keys present here can be bound as a shortcut. The tuple layout is
/// `(egui key, virtual-key code, label)`.
const KEYS: &[(Key, u16, &str)] = &[
    (Key::A, 0x41, "A"),
    (Key::B, 0x42, "B"),
    (Key::C, 0x43, "C"),
    (Key::D, 0x44, "D"),
    (Key::E, 0x45, "E"),
    (Key::F, 0x46, "F"),
    (Key::G, 0x47, "G"),
    (Key::H, 0x48, "H"),
    (Key::I, 0x49, "I"),
    (Key::J, 0x4A, "J"),
    (Key::K, 0x4B, "K"),
    (Key::L, 0x4C, "L"),
    (Key::M, 0x4D, "M"),
    (Key::N, 0x4E, "N"),
    (Key::O, 0x4F, "O"),
    (Key::P, 0x50, "P"),
    (Key::Q, 0x51, "Q"),
    (Key::R, 0x52, "R"),
    (Key::S, 0x53, "S"),
    (Key::T, 0x54, "T"),
    (Key::U, 0x55, "U"),
    (Key::V, 0x56, "V"),
    (Key::W, 0x57, "W"),
    (Key::X, 0x58, "X"),
    (Key::Y, 0x59, "Y"),
    (Key::Z, 0x5A, "Z"),
    (Key::Num0, 0x30, "0"),
    (Key::Num1, 0x31, "1"),
    (Key::Num2, 0x32, "2"),
    (Key::Num3, 0x33, "3"),
    (Key::Num4, 0x34, "4"),
    (Key::Num5, 0x35, "5"),
    (Key::Num6, 0x36, "6"),
    (Key::Num7, 0x37, "7"),
    (Key::Num8, 0x38, "8"),
    (Key::Num9, 0x39, "9"),
    (Key::F1, 0x70, "F1"),
    (Key::F2, 0x71, "F2"),
    (Key::F3, 0x72, "F3"),
    (Key::F4, 0x73, "F4"),
    (Key::F5, 0x74, "F5"),
    (Key::F6, 0x75, "F6"),
    (Key::F7, 0x76, "F7"),
    (Key::F8, 0x77, "F8"),
    (Key::F9, 0x78, "F9"),
    (Key::F10, 0x79, "F10"),
    (Key::F11, 0x7A, "F11"),
    (Key::F12, 0x7B, "F12"),
    (Key::F13, 0x7C, "F13"),
    (Key::F14, 0x7D, "F14"),
    (Key::F15, 0x7E, "F15"),
    (Key::F16, 0x7F, "F16"),
    (Key::F17, 0x80, "F17"),
    (Key::F18, 0x81, "F18"),
    (Key::F19, 0x82, "F19"),
    (Key::F20, 0x83, "F20"),
    (Key::F21, 0x84, "F21"),
    (Key::F22, 0x85, "F22"),
    (Key::F23, 0x86, "F23"),
    (Key::F24, 0x87, "F24"),
    (Key::Space, 0x20, "Space"),
    (Key::Enter, 0x0D, "Enter"),
    (Key::Tab, 0x09, "Tab"),
    (Key::Backspace, 0x08, "Backspace"),
    (Key::Insert, 0x2D, "Insert"),
    (Key::Delete, 0x2E, "Delete"),
    (Key::Home, 0x24, "Home"),
    (Key::End, 0x23, "End"),
    (Key::PageUp, 0x21, "PageUp"),
    (Key::PageDown, 0x22, "PageDown"),
    (Key::ArrowLeft, 0x25, "Left"),
    (Key::ArrowUp, 0x26, "Up"),
    (Key::ArrowRight, 0x27, "Right"),
    (Key::ArrowDown, 0x28, "Down"),
];

/// A single keyboard shortcut: a Win32 virtual-key code plus the state of the
/// Ctrl, Alt, and Shift modifiers.
///
/// Stored as the raw virtual-key code (not the egui [`Key`]) so the binding is
/// independent of the UI framework and survives across sessions in
/// `settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    /// Win32 virtual-key code (e.g. `0x78` for F9).
    pub key: u16,
    /// Whether Ctrl must be held.
    pub ctrl: bool,
    /// Whether Alt must be held.
    pub alt: bool,
    /// Whether Shift must be held.
    pub shift: bool,
}

impl Shortcut {
    /// Converts a key press captured by the egui UI into a storable
    /// [`Shortcut`].
    ///
    /// Returns `None` when the key is not in [`KEYS`] or the resulting shortcut
    /// fails [`Shortcut::is_valid`] (e.g. a bare letter with no modifier).
    pub fn from_input(key: Key, modifiers: Modifiers) -> Option<Self> {
        // Look up the virtual-key code for this egui Key; bail out if unsupported.
        let key = KEYS.iter().find(|entry| entry.0 == key)?.1;
        let shortcut = Self {
            key,
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
        };
        shortcut.is_valid().then_some(shortcut)
    }

    /// Checks that a shortcut is usable as a global hotkey.
    ///
    /// Rules:
    /// - The key must be in [`KEYS`] (one we know how to poll and label).
    /// - It needs at least one modifier, *unless* the key is an F-key
    ///   (F1-F24, codes `0x70..=0x87`) which is safe to bind bare.
    /// - Alt+F4 (`alt && key == 0x73`) is rejected because it closes the window.
    fn is_valid(self) -> bool {
        KEYS.iter().any(|entry| entry.1 == self.key)
            && (self.ctrl || self.alt || self.shift || (0x70..=0x87).contains(&self.key))
            && !(self.alt && self.key == 0x73) // Alt+F4 closes the application.
    }

    /// Builds a human-readable label such as `"Ctrl+Shift+R"` for display in
    /// the UI. Unknown keys render as `"?"`.
    pub fn label(self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        // Append the key's display name from the KEYS table.
        parts.push(
            KEYS.iter()
                .find(|entry| entry.1 == self.key)
                .map_or("?", |entry| entry.2),
        );
        parts.join("+")
    }

    /// Exact match of the key-down flag and all three modifiers. Used by
    /// [`Shortcut::is_down`] to compare against live keyboard state.
    fn matches(self, key: bool, ctrl: bool, alt: bool, shift: bool) -> bool {
        key && self.ctrl == ctrl && self.alt == alt && self.shift == shift
    }

    /// Returns `true` when this shortcut's key and modifiers are all currently
    /// held, queried via [`GetAsyncKeyState`].
    fn is_down(self, ctrl: bool, alt: bool, shift: bool) -> bool {
        self.matches(key_down(i32::from(self.key)), ctrl, alt, shift)
    }
}

/// The start/stop hotkey pair, persisted as part of [`crate::config::AppConfig`].
///
/// `#[serde(default)]` lets older settings files that predate one of the fields
/// fall back to [`Shortcuts::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcuts {
    /// Hotkey that starts a recording.
    pub start: Shortcut,
    /// Hotkey that stops a recording.
    pub stop: Shortcut,
}

/// Default bindings: **Ctrl+F9** to start, **Ctrl+F10** to stop.
impl Default for Shortcuts {
    fn default() -> Self {
        Self {
            start: Shortcut {
                key: 0x78,
                ctrl: true,
                alt: false,
                shift: false,
            },
            stop: Shortcut {
                key: 0x79,
                ctrl: true,
                alt: false,
                shift: false,
            },
        }
    }
}

impl Shortcuts {
    /// Both shortcuts must individually be valid and they must differ, so the
    /// same key combo can't start and stop at the same time.
    pub fn is_valid(self) -> bool {
        self.start.is_valid() && self.stop.is_valid() && self.start != self.stop
    }
}

/// A cheap `Copy` snapshot of the controller's mutable state.
///
/// Copied out under the lock in [`HotkeyController::snapshot`] so the polling
/// thread can work on a consistent view without holding the mutex while it
/// queries the keyboard.
#[derive(Clone, Copy)]
struct ControlState {
    /// Current start/stop bindings.
    shortcuts: Shortcuts,
    /// While `true`, the polling thread suppresses all events.
    suspended: bool,
    /// Bumped (wrapping) whenever shortcuts or the suspend flag change, so
    /// [`KeyEdges`] can detect a rebind/resume and re-arm.
    revision: u64,
}

/// Shared state between the UI thread and the background polling thread.
///
/// The UI thread calls [`HotkeyController::update`], [`suspend`], and
/// [`set_stop_signal`]; the polling thread reads a snapshot each cycle via
/// [`snapshot`]. All fields are lock- or atomic-protected for cross-thread
/// access.
///
/// [`suspend`]: HotkeyController::suspend
/// [`set_stop_signal`]: HotkeyController::set_stop_signal
/// [`snapshot`]: HotkeyController::snapshot
pub struct HotkeyController {
    /// Live bindings + suspend flag + revision counter.
    state: Mutex<ControlState>,
    /// The recording stop signal, swappable per recording session. Held in a
    /// `Mutex<Arc<AtomicBool>>` so a new session can replace the signal without
    /// the polling thread racing on a stale reference.
    stop: Mutex<Arc<AtomicBool>>,
    /// Set by [`shutdown`] to tell the polling thread to exit its loop.
    ///
    /// [`shutdown`]: HotkeyController::shutdown
    shutdown: AtomicBool,
}

impl HotkeyController {
    /// Creates a controller with the given initial bindings and no stop signal.
    pub fn new(shortcuts: Shortcuts) -> Self {
        Self {
            stop: Mutex::new(Arc::new(AtomicBool::new(false))),
            shutdown: AtomicBool::new(false),
            state: Mutex::new(ControlState {
                shortcuts,
                suspended: false,
                revision: 0,
            }),
        }
    }

    /// Swaps the stop signal, so each recording session owns a fresh
    /// `AtomicBool` that the polling thread sets directly.
    pub fn set_stop_signal(&self, stop: Arc<AtomicBool>) {
        *self.stop.lock().unwrap_or_else(|error| error.into_inner()) = stop;
    }

    /// Signals the polling thread to stop and exit.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Pauses (`true`) or resumes (`false`) hotkey event generation. Bumps the
    /// revision so [`KeyEdges`] re-arms after resume.
    pub fn suspend(&self, suspended: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.suspended = suspended;
        state.revision = state.revision.wrapping_add(1);
    }

    /// Replaces the start/stop bindings. Bumps the revision so [`KeyEdges`]
    /// waits for all keys to be released before firing again.
    pub fn update(&self, shortcuts: Shortcuts) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.shortcuts = shortcuts;
        state.revision = state.revision.wrapping_add(1);
    }

    /// Copies the current control state out from under the lock. Safe because
    /// [`ControlState`] is `Copy`.
    fn snapshot(&self) -> ControlState {
        *self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// Events sent from the polling thread to the UI over an `mpsc` channel.
#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    /// The start hotkey was pressed.
    Start,
    /// The stop hotkey was pressed.
    Stop,
    /// One-shot signal sent at startup so the UI knows hotkeys are live.
    Availability {
        start: bool,
        stop: bool,
    },
}

/// Rising-edge detector for the start/stop hotkeys.
///
/// Ensures each key press fires at most once (not every poll cycle the key is
/// held) and that no event fires immediately after a rebind or resume: the
/// detector disarms until every relevant key has been released, preventing a
/// still-held modifier from retriggering the old binding.
#[derive(Default)]
struct KeyEdges {
    /// Last seen [`ControlState::revision`]; a mismatch re-arms the detector.
    revision: Option<u64>,
    /// Whether the detector is armed (all keys were released since the last
    /// rebind/resume).
    armed: bool,
    /// Whether the start key was down on the previous poll.
    start_down: bool,
    /// Whether the stop key was down on the previous poll.
    stop_down: bool,
}

impl KeyEdges {
    /// Evaluates one polling cycle and returns `(start_pressed, stop_pressed)`
    /// rising-edge events.
    ///
    /// On a revision change or while suspended, the detector disarms and
    /// emits nothing. It re-arms only once every key is released, so a
    /// still-held modifier after a rebind does not fire the new binding.
    fn poll(&mut self, state: ControlState, start: bool, stop: bool) -> (bool, bool) {
        // A rebind or a suspend/resume cycle invalidates the previous edge state.
        if self.revision != Some(state.revision) || state.suspended {
            self.revision = Some(state.revision);
            self.armed = false;
        }
        let events = if !self.armed || state.suspended {
            // Not armed yet: wait for all keys to be released before arming.
            if !state.suspended && !start && !stop {
                self.armed = true;
            }
            (false, false)
        } else {
            // Armed: fire only on the rising edge (key newly down this cycle).
            (start && !self.start_down, stop && !self.stop_down)
        };
        self.start_down = start;
        self.stop_down = stop;
        events
    }
}

/// Spawns the background hotkey polling thread.
///
/// The thread polls [`GetAsyncKeyState`] every 8 ms, converts key state into
/// rising-edge events via [`KeyEdges`], and forwards [`HotkeyEvent`]s over
/// `sender`. `wake` is called after each event so the egui UI repaints.
///
/// On the stop hotkey the recording stop signal is set *directly* (it does not
/// wait for a UI repaint or another captured frame) in addition to sending a
/// [`HotkeyEvent::Stop`].
///
/// Returns the thread's [`JoinHandle`] so the caller can `join` it on exit.
pub fn spawn(
    sender: Sender<HotkeyEvent>,
    controller: Arc<HotkeyController>,
    wake: impl Fn() + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("global-hotkeys".into())
        .spawn(move || {
            // Announce that hotkeys are active before the first poll.
            let _ = sender.send(HotkeyEvent::Availability {
                start: true,
                stop: true,
            });
            log::log("hotkey polling thread started (8 ms)");
            wake();
            let mut edges = KeyEdges::default();
            while !controller.shutdown.load(Ordering::Acquire) {
                let state = controller.snapshot();
                // Read live modifier state from the OS.
                let ctrl = key_down(VK_CONTROL.0 as i32);
                let alt = key_down(VK_MENU.0 as i32);
                let shift = key_down(VK_SHIFT.0 as i32);
                let (start, stop) = edges.poll(
                    state,
                    state.shortcuts.start.is_down(ctrl, alt, shift),
                    state.shortcuts.stop.is_down(ctrl, alt, shift),
                );
                if stop {
                    // Set the recording stop flag immediately, then notify the UI.
                    signal_stop(
                        &controller
                            .stop
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()),
                    );
                    if sender.send(HotkeyEvent::Stop).is_err() {
                        break;
                    }
                    log::log("stop shortcut detected");
                    wake();
                } else if start {
                    if sender.send(HotkeyEvent::Start).is_err() {
                        break;
                    }
                    log::log("start shortcut detected");
                    wake();
                }
                thread::sleep(Duration::from_millis(8));
            }
        })
}

/// egui merges keypad and top-row digits. Accept only the actual Win32 key
/// that our global polling will observe, also checking layout differences.
///
/// Used during rebinding in the UI to confirm the key egui reported is the
/// same physical key the polling thread would detect.
pub fn input_matches_polling(shortcut: Shortcut) -> bool {
    input_matches(shortcut, key_down)
}

/// Returns `true` when the shortcut's key is currently down *and* no
/// ambiguous key is also down.
///
/// Rejects the Windows/Super keys (`0x5B`/`0x5C`) and the entire numeric keypad
/// block (`0x60..=0x6F`). egui collapses keypad digits onto their top-row
/// equivalents, so without these exclusions a keypad press would be
/// indistinguishable from the real top-row key the polling thread watches.
fn input_matches(shortcut: Shortcut, is_down: impl Fn(i32) -> bool) -> bool {
    is_down(i32::from(shortcut.key))
        && !is_down(0x5B) // Left Windows/Super key.
        && !is_down(0x5C) // Right Windows/Super key.
        && !(0x60..=0x6F).any(is_down) // Entire numeric keypad block.
}

/// Returns `true` when the given Win32 virtual-key code is currently held.
///
/// `GetAsyncKeyState` sets the high bit (`0x8000`) when the key is down, so we
/// mask and test that single bit.
fn key_down(virtual_key: i32) -> bool {
    (unsafe { GetAsyncKeyState(virtual_key) } as u16 & 0x8000) != 0
}

/// Sets the recording stop flag directly.
///
/// Stopping does not depend on UI repainting or another captured frame, so the
/// polling thread sets the signal itself for the lowest-latency stop.
fn signal_stop(stop_signal: &AtomicBool) {
    stop_signal.store(true, Ordering::Release);
}

/// Unit tests for shortcut validation, edge detection, and stop-signal
/// isolation between recording sessions.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypad_input_cannot_be_saved_as_a_top_row_digit() {
        let shortcut = Shortcut::from_input(Key::Num0, Modifiers::CTRL).unwrap();
        assert!(input_matches(shortcut, |key| key == 0x30));
        assert!(!input_matches(shortcut, |key| key == 0x60));
        assert!(!input_matches(shortcut, |key| key == 0x30 || key == 0x60));
        assert!(!input_matches(shortcut, |key| key == 0x30 || key == 0x5B));
    }

    #[test]
    fn hotkey_worker_stops_while_idle() {
        let controller = Arc::new(HotkeyController::new(Shortcuts::default()));
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = spawn(sender, controller.clone(), || {}).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            HotkeyEvent::Availability { stop: true, .. }
        ));
        controller.shutdown();
        handle.join().unwrap();
    }

    #[test]
    fn rebinding_stop_signal_isolates_recording_sessions() {
        let controller = HotkeyController::new(Shortcuts::default());
        let old = Arc::new(AtomicBool::new(false));
        let current = Arc::new(AtomicBool::new(false));
        controller.set_stop_signal(old.clone());
        controller.set_stop_signal(current.clone());
        signal_stop(&old);
        assert!(!current.load(Ordering::Acquire));
        signal_stop(&controller.stop.lock().unwrap());
        assert!(current.load(Ordering::Acquire));
    }

    #[test]
    fn stop_hotkey_sets_direct_capture_signal() {
        let stop = AtomicBool::new(false);
        signal_stop(&stop);
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn shortcuts_validate_keys_and_conflicts() {
        let mut shortcuts = Shortcuts::default();
        assert!(shortcuts.is_valid());
        assert_eq!(shortcuts.start.label(), "Ctrl+F9");
        shortcuts.stop = shortcuts.start;
        assert!(!shortcuts.is_valid());
        assert!(Shortcut::from_input(Key::A, Modifiers::NONE).is_none());
        assert!(Shortcut::from_input(Key::F4, Modifiers::ALT).is_none());
        assert_eq!(
            Shortcut::from_input(Key::R, Modifiers::CTRL | Modifiers::SHIFT)
                .unwrap()
                .label(),
            "Ctrl+Shift+R"
        );
    }

    #[test]
    fn modifiers_match_exactly() {
        let shortcut = Shortcuts::default().start;
        assert!(shortcut.matches(true, true, false, false));
        assert!(!shortcut.matches(true, true, false, true));
        assert!(!shortcut.matches(false, true, false, false));
    }

    #[test]
    fn editing_and_rebinding_wait_for_key_release() {
        let controller = HotkeyController::new(Shortcuts::default());
        let mut edges = KeyEdges::default();
        edges.poll(controller.snapshot(), false, false);
        assert_eq!(
            edges.poll(controller.snapshot(), true, false),
            (true, false)
        );
        assert_eq!(
            edges.poll(controller.snapshot(), true, false),
            (false, false)
        );
        controller.suspend(true);
        assert_eq!(
            edges.poll(controller.snapshot(), false, true),
            (false, false)
        );
        controller.update(Shortcuts::default());
        controller.suspend(false);
        assert_eq!(
            edges.poll(controller.snapshot(), false, true),
            (false, false)
        );
        assert_eq!(
            edges.poll(controller.snapshot(), false, false),
            (false, false)
        );
        assert_eq!(
            edges.poll(controller.snapshot(), false, true),
            (false, true)
        );
    }
}
