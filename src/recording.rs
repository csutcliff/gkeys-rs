//! Macro recording state machine and evdev input capture

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::Result;
use evdev::{Device as EvdevDevice, InputEventKind, Key};

use crate::events::G815;

/// Recording state machine states
#[derive(Debug)]
pub enum RecordingState {
    /// Normal operation, not recording
    Idle,
    /// MR pressed, waiting for G-key selection (1-5)
    AwaitingGKey { profile: String },
    /// Recording active, capturing keyboard input
    Recording {
        profile: String,
        gkey: u8,
        captured_keys: Vec<String>,
        stop_flag: Arc<AtomicBool>,
        receiver: Receiver<String>,
        _capture_thread: JoinHandle<()>,
    },
}

/// Actions returned by recorder for main loop to execute
#[derive(Debug)]
pub enum RecordingAction {
    /// No action needed
    None,
    /// Entering awaiting state: MR LED on, all G-keys white
    EnterAwaiting,
    /// Started recording: selected G-key red, others off, start MR slow flash
    StartedRecording { gkey: u8 },
    /// Recording complete with keys captured: save macro, MR quick flash, then LEDs off
    SaveMacro {
        profile: String,
        gkey: u8,
        sequence: String,
    },
    /// Cancelled with no keys: just turn off LEDs (no flash)
    CancelledEmpty,
    /// Cancelled (MR pressed without G-key): just turn off LEDs (no flash)
    CancelledNoGKey,
    /// Error occurred: turn off LEDs
    Error(String),
}

/// Recorder manages the macro recording state machine
pub struct Recorder {
    state: RecordingState,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            state: RecordingState::Idle,
        }
    }

    /// Check if currently recording
    pub fn is_recording(&self) -> bool {
        matches!(self.state, RecordingState::Recording { .. })
    }

    /// Check if awaiting G-key selection
    pub fn is_awaiting(&self) -> bool {
        matches!(self.state, RecordingState::AwaitingGKey { .. })
    }

    /// Handle MR key press - transitions state
    pub fn on_mr_press(&mut self, current_profile: &str) -> RecordingAction {
        log::debug!("MR press - current state: {:?}", std::mem::discriminant(&self.state));

        // Take ownership of current state
        let old_state = std::mem::replace(&mut self.state, RecordingState::Idle);

        match old_state {
            RecordingState::Idle => {
                // Start awaiting G-key selection
                self.state = RecordingState::AwaitingGKey {
                    profile: current_profile.to_string(),
                };
                log::info!("Recording: awaiting G-key selection");
                RecordingAction::EnterAwaiting
            }
            RecordingState::AwaitingGKey { .. } => {
                // Cancel - MR pressed without selecting G-key
                log::info!("Recording: cancelled (no G-key selected)");
                RecordingAction::CancelledNoGKey
            }
            RecordingState::Recording {
                profile,
                gkey,
                mut captured_keys,
                stop_flag,
                receiver,
                _capture_thread,
            } => {
                // Stop recording - signal thread to stop
                stop_flag.store(true, Ordering::SeqCst);

                // Drain any remaining keys from the receiver
                while let Ok(key) = receiver.try_recv() {
                    captured_keys.push(key);
                }

                // Build the sequence string
                let sequence = captured_keys.join(", ");
                log::info!(
                    "Recording: finished G{} with {} keys: {}",
                    gkey,
                    captured_keys.len(),
                    sequence
                );

                // If no keys captured, return CancelledEmpty instead of SaveMacro
                if captured_keys.is_empty() {
                    RecordingAction::CancelledEmpty
                } else {
                    RecordingAction::SaveMacro {
                        profile,
                        gkey,
                        sequence,
                    }
                }
            }
        }
    }

    /// Handle G-key press during awaiting state
    pub fn on_gkey_press(&mut self, gkey: u8) -> RecordingAction {
        // Only handle if in AwaitingGKey state
        let old_state = std::mem::replace(&mut self.state, RecordingState::Idle);

        if let RecordingState::AwaitingGKey { profile } = old_state {
            // Try to find and open the keyboard evdev device
            match find_keyboard_evdev() {
                Some(path) => match start_capture_thread(&path) {
                    Ok((receiver, stop_flag, handle)) => {
                        self.state = RecordingState::Recording {
                            profile: profile.clone(),
                            gkey,
                            captured_keys: Vec::new(),
                            stop_flag,
                            receiver,
                            _capture_thread: handle,
                        };
                        log::info!("Recording: started for G{} on profile {}", gkey, profile);
                        return RecordingAction::StartedRecording { gkey };
                    }
                    Err(e) => {
                        log::error!("Recording: failed to start capture thread: {}", e);
                        return RecordingAction::Error(format!("Failed to start capture: {}", e));
                    }
                },
                None => {
                    log::error!("Recording: keyboard evdev device not found");
                    return RecordingAction::Error("Keyboard device not found".to_string());
                }
            }
        } else {
            // Restore state if not awaiting
            self.state = old_state;
        }

        RecordingAction::None
    }

    /// Poll for captured keys during recording (non-blocking)
    pub fn poll_captured_keys(&mut self) {
        if let RecordingState::Recording {
            ref receiver,
            ref mut captured_keys,
            ..
        } = self.state
        {
            // Non-blocking receive of captured keys
            while let Ok(key) = receiver.try_recv() {
                log::debug!("Recording: captured key '{}'", key);
                captured_keys.push(key);
            }
        }
    }
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the G815 keyboard evdev device (interface 0, standard keyboard)
fn find_keyboard_evdev() -> Option<PathBuf> {
    let devices = evdev::enumerate();
    for (path, device) in devices {
        let id = device.input_id();
        log::trace!(
            "Checking evdev {:?}: vendor={:04x} product={:04x} name={:?}",
            path,
            id.vendor(),
            id.product(),
            device.name()
        );
        if id.vendor() == G815.vendor_id && id.product() == G815.product_id {
            // Check if this device has regular keyboard keys AND LED support
            // Interface 0 (main keyboard) has LEDs, interface 1 (G-keys) does not
            let has_keys = device
                .supported_keys()
                .map(|k| k.contains(Key::KEY_A))
                .unwrap_or(false);
            let has_leds = device.supported_leds().is_some();

            if has_keys && has_leds {
                log::info!(
                    "Found keyboard evdev at {:?} (name: {:?})",
                    path,
                    device.name()
                );
                return Some(path);
            }
        }
    }
    log::warn!("G815 keyboard evdev device not found");
    None
}

/// Start the keyboard capture thread
fn start_capture_thread(
    path: &PathBuf,
) -> Result<(Receiver<String>, Arc<AtomicBool>, JoinHandle<()>)> {
    let device = EvdevDevice::open(path)?;
    let (tx, rx): (Sender<String>, Receiver<String>) = mpsc::channel();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_clone = stop_flag.clone();

    let handle = thread::spawn(move || {
        capture_loop(device, tx, stop_flag_clone);
    });

    Ok((rx, stop_flag, handle))
}

/// Main capture loop running in separate thread
fn capture_loop(mut device: EvdevDevice, tx: Sender<String>, stop_flag: Arc<AtomicBool>) {
    let mut modifier_state = ModifierState::default();
    log::debug!("Recording: capture thread started");

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            log::debug!("Recording: capture thread stopping (flag set)");
            break;
        }

        // Fetch events with a short timeout
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    log::trace!("Recording: raw event {:?}", event);
                    if let InputEventKind::Key(key) = event.kind() {
                        let pressed = event.value() == 1; // 1 = press, 0 = release, 2 = repeat
                        log::debug!(
                            "Recording: key event {:?} pressed={}",
                            key,
                            pressed
                        );

                        // Update modifier state
                        if modifier_state.update(key, pressed) {
                            continue; // Don't emit modifier keys themselves
                        }

                        // Only capture key presses, not releases or repeats
                        if pressed {
                            if let Some(key_str) = modifier_state.format_with_key(key) {
                                log::info!("Recording: captured '{}'", key_str);
                                if tx.send(key_str).is_err() {
                                    // Receiver dropped, exit
                                    log::debug!("Recording: receiver dropped, exiting");
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                if stop_flag.load(Ordering::SeqCst) {
                    log::debug!("Recording: capture thread stopping (flag set after error)");
                    break;
                }
                // EAGAIN is normal for non-blocking, other errors we should log
                if e.raw_os_error() != Some(libc::EAGAIN) {
                    log::warn!("Recording: evdev read error: {}", e);
                }
                // Small sleep to avoid busy loop on errors
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    log::debug!("Recording: capture thread exited");
}

/// Track modifier key state for building key chords
#[derive(Default)]
struct ModifierState {
    ctrl: bool,
    shift: bool,
    alt: bool,
    meta: bool,
}

impl ModifierState {
    /// Update modifier state, returns true if this was a modifier key
    fn update(&mut self, key: Key, pressed: bool) -> bool {
        match key {
            Key::KEY_LEFTCTRL | Key::KEY_RIGHTCTRL => {
                self.ctrl = pressed;
                true
            }
            Key::KEY_LEFTSHIFT | Key::KEY_RIGHTSHIFT => {
                self.shift = pressed;
                true
            }
            Key::KEY_LEFTALT | Key::KEY_RIGHTALT => {
                self.alt = pressed;
                true
            }
            Key::KEY_LEFTMETA | Key::KEY_RIGHTMETA => {
                self.meta = pressed;
                true
            }
            _ => false,
        }
    }

    /// Format a key with current modifiers as "ctrl+shift+a" etc.
    fn format_with_key(&self, key: Key) -> Option<String> {
        let key_name = key_to_name(key)?;

        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("ctrl");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.shift {
            parts.push("shift");
        }
        if self.meta {
            parts.push("super");
        }
        parts.push(key_name);

        Some(parts.join("+"))
    }
}

/// Convert evdev Key to key name string
fn key_to_name(key: Key) -> Option<&'static str> {
    // Map common keys to names compatible with our shortcut format
    Some(match key {
        // Letters
        Key::KEY_A => "a",
        Key::KEY_B => "b",
        Key::KEY_C => "c",
        Key::KEY_D => "d",
        Key::KEY_E => "e",
        Key::KEY_F => "f",
        Key::KEY_G => "g",
        Key::KEY_H => "h",
        Key::KEY_I => "i",
        Key::KEY_J => "j",
        Key::KEY_K => "k",
        Key::KEY_L => "l",
        Key::KEY_M => "m",
        Key::KEY_N => "n",
        Key::KEY_O => "o",
        Key::KEY_P => "p",
        Key::KEY_Q => "q",
        Key::KEY_R => "r",
        Key::KEY_S => "s",
        Key::KEY_T => "t",
        Key::KEY_U => "u",
        Key::KEY_V => "v",
        Key::KEY_W => "w",
        Key::KEY_X => "x",
        Key::KEY_Y => "y",
        Key::KEY_Z => "z",
        // Numbers
        Key::KEY_1 => "1",
        Key::KEY_2 => "2",
        Key::KEY_3 => "3",
        Key::KEY_4 => "4",
        Key::KEY_5 => "5",
        Key::KEY_6 => "6",
        Key::KEY_7 => "7",
        Key::KEY_8 => "8",
        Key::KEY_9 => "9",
        Key::KEY_0 => "0",
        // Function keys
        Key::KEY_F1 => "f1",
        Key::KEY_F2 => "f2",
        Key::KEY_F3 => "f3",
        Key::KEY_F4 => "f4",
        Key::KEY_F5 => "f5",
        Key::KEY_F6 => "f6",
        Key::KEY_F7 => "f7",
        Key::KEY_F8 => "f8",
        Key::KEY_F9 => "f9",
        Key::KEY_F10 => "f10",
        Key::KEY_F11 => "f11",
        Key::KEY_F12 => "f12",
        // Special keys
        Key::KEY_ESC => "esc",
        Key::KEY_TAB => "tab",
        Key::KEY_CAPSLOCK => "capslock",
        Key::KEY_SPACE => "space",
        Key::KEY_ENTER => "enter",
        Key::KEY_BACKSPACE => "backspace",
        Key::KEY_DELETE => "delete",
        Key::KEY_INSERT => "insert",
        Key::KEY_HOME => "home",
        Key::KEY_END => "end",
        Key::KEY_PAGEUP => "pageup",
        Key::KEY_PAGEDOWN => "pagedown",
        Key::KEY_UP => "up",
        Key::KEY_DOWN => "down",
        Key::KEY_LEFT => "left",
        Key::KEY_RIGHT => "right",
        // Punctuation
        Key::KEY_MINUS => "minus",
        Key::KEY_EQUAL => "equal",
        Key::KEY_LEFTBRACE => "leftbrace",
        Key::KEY_RIGHTBRACE => "rightbrace",
        Key::KEY_SEMICOLON => "semicolon",
        Key::KEY_APOSTROPHE => "apostrophe",
        Key::KEY_GRAVE => "grave",
        Key::KEY_BACKSLASH => "backslash",
        Key::KEY_COMMA => "comma",
        Key::KEY_DOT => "dot",
        Key::KEY_SLASH => "slash",
        // Numpad
        Key::KEY_KP0 => "kp0",
        Key::KEY_KP1 => "kp1",
        Key::KEY_KP2 => "kp2",
        Key::KEY_KP3 => "kp3",
        Key::KEY_KP4 => "kp4",
        Key::KEY_KP5 => "kp5",
        Key::KEY_KP6 => "kp6",
        Key::KEY_KP7 => "kp7",
        Key::KEY_KP8 => "kp8",
        Key::KEY_KP9 => "kp9",
        Key::KEY_KPMINUS => "kpminus",
        Key::KEY_KPPLUS => "kpplus",
        Key::KEY_KPENTER => "kpenter",
        Key::KEY_KPDOT => "kpdot",
        Key::KEY_KPSLASH => "kpslash",
        Key::KEY_KPASTERISK => "kpasterisk",
        // Print screen, scroll lock, pause
        Key::KEY_SYSRQ => "printscreen",
        Key::KEY_SCROLLLOCK => "scrolllock",
        Key::KEY_PAUSE => "pause",
        // Skip modifiers - they're handled separately
        Key::KEY_LEFTCTRL
        | Key::KEY_RIGHTCTRL
        | Key::KEY_LEFTSHIFT
        | Key::KEY_RIGHTSHIFT
        | Key::KEY_LEFTALT
        | Key::KEY_RIGHTALT
        | Key::KEY_LEFTMETA
        | Key::KEY_RIGHTMETA => return None,
        // Unknown key
        _ => {
            log::trace!("Unknown key: {:?}", key);
            return None;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modifier_state() {
        let mut state = ModifierState::default();

        // Press ctrl
        assert!(state.update(Key::KEY_LEFTCTRL, true));
        assert!(state.ctrl);

        // Press 'a' with ctrl held
        let result = state.format_with_key(Key::KEY_A);
        assert_eq!(result, Some("ctrl+a".to_string()));

        // Release ctrl
        assert!(state.update(Key::KEY_LEFTCTRL, false));
        assert!(!state.ctrl);

        // Press 'a' without modifiers
        let result = state.format_with_key(Key::KEY_A);
        assert_eq!(result, Some("a".to_string()));
    }

    #[test]
    fn test_key_to_name() {
        assert_eq!(key_to_name(Key::KEY_A), Some("a"));
        assert_eq!(key_to_name(Key::KEY_ENTER), Some("enter"));
        assert_eq!(key_to_name(Key::KEY_F1), Some("f1"));
        assert_eq!(key_to_name(Key::KEY_LEFTCTRL), None); // Modifiers return None
    }
}
