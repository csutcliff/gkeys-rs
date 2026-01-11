//! Dedicated LED control thread for G815 keyboard
//!
//! Handles all LED operations asynchronously to avoid blocking the main event loop
//! and to properly manage flashing patterns.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::events;

/// Commands that can be sent to the LED controller thread
#[derive(Debug)]
pub enum LedCommand {
    /// Set MR LED on or off
    SetMrLed(bool),
    /// Set profile LED (M1, M2, or M3)
    SetProfileLed(u8),
    /// Set all G-keys to the same color
    SetAllGKeysLed { r: u8, g: u8, b: u8 },
    /// Set G-keys for recording mode (selected key red, others off)
    SetGKeysRecording { selected_gkey: u8 },
    /// Start MR LED slow flashing (500ms on/off)
    StartMrFlashing,
    /// Stop MR LED flashing and turn it off
    StopMrFlashing,
    /// Quick flash MR LED (for successful save)
    QuickFlashMr { count: u8 },
    /// Turn off all G-key LEDs
    TurnOffGKeys,
    /// Set entire keyboard to a single color
    SetFullKeyboardColor { r: u8, g: u8, b: u8 },
    /// Restore G-keys to configured color (or turn off if None)
    RestoreGKeysColor { color: Option<(u8, u8, u8)> },
    /// Shutdown the LED controller thread
    Shutdown,
}

/// MR LED flash interval during recording (500ms on, 500ms off)
const MR_FLASH_INTERVAL: Duration = Duration::from_millis(500);

/// MR LED quick flash interval for success (125ms on, 125ms off)
const MR_QUICK_FLASH_INTERVAL: Duration = Duration::from_millis(125);

/// Time window (ms) to ignore MR events after LED write
const MR_LED_DEBOUNCE_MS: u64 = 30;

/// LED controller that runs operations in a dedicated thread
pub struct LedController {
    tx: Sender<LedCommand>,
    thread: Option<JoinHandle<()>>,
    /// Timestamp of last MR LED write (ms since UNIX epoch)
    /// Cleared after consuming one phantom event
    mr_led_write_time: Arc<AtomicU64>,
}

impl LedController {
    /// Create a new LED controller with its own thread
    pub fn new(device_path: PathBuf) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mr_led_write_time = Arc::new(AtomicU64::new(0));
        let mr_time_clone = mr_led_write_time.clone();

        let path_clone = device_path.clone();
        let thread = thread::Builder::new()
            .name("led-controller".into())
            .spawn(move || {
                if let Err(e) = led_worker(path_clone, rx, mr_time_clone) {
                    log::error!("LED worker error: {}", e);
                }
            })
            .context("Failed to spawn LED controller thread")?;

        log::debug!("LED controller started for {}", device_path.display());

        Ok(Self {
            tx,
            thread: Some(thread),
            mr_led_write_time,
        })
    }

    /// Check if MR event is from LED write (within debounce window)
    /// Consumes at most one event per LED write by clearing the timestamp
    pub fn is_mr_event_from_led(&self) -> bool {
        let write_time = self.mr_led_write_time.load(Ordering::SeqCst);
        if write_time == 0 {
            return false; // No pending LED write or already consumed
        }
        let now = current_time_ms();
        if now.saturating_sub(write_time) < MR_LED_DEBOUNCE_MS {
            // Within window - consume by clearing timestamp
            self.mr_led_write_time.store(0, Ordering::SeqCst);
            true
        } else {
            false // Outside window, real press
        }
    }

    /// Send a command to the LED controller
    fn send(&self, cmd: LedCommand) {
        if let Err(e) = self.tx.send(cmd) {
            log::warn!("Failed to send LED command: {}", e);
        }
    }

    /// Set MR LED on or off
    pub fn set_mr_led(&self, on: bool) {
        self.send(LedCommand::SetMrLed(on));
    }

    /// Set profile LED (M1, M2, or M3)
    pub fn set_profile_led(&self, profile: u8) {
        self.send(LedCommand::SetProfileLed(profile));
    }

    /// Set all G-keys to the same color
    pub fn set_all_gkeys_led(&self, r: u8, g: u8, b: u8) {
        self.send(LedCommand::SetAllGKeysLed { r, g, b });
    }

    /// Set G-keys for recording mode (selected key red, others off)
    pub fn set_gkeys_recording(&self, selected_gkey: u8) {
        self.send(LedCommand::SetGKeysRecording { selected_gkey });
    }

    /// Start MR LED slow flashing
    pub fn start_mr_flashing(&self) {
        self.send(LedCommand::StartMrFlashing);
    }

    /// Stop MR LED flashing
    pub fn stop_mr_flashing(&self) {
        self.send(LedCommand::StopMrFlashing);
    }

    /// Quick flash MR LED for successful save
    pub fn quick_flash_mr(&self, count: u8) {
        self.send(LedCommand::QuickFlashMr { count });
    }

    /// Turn off all G-key LEDs
    pub fn turn_off_gkeys(&self) {
        self.send(LedCommand::TurnOffGKeys);
    }

    /// Set entire keyboard to a single color
    pub fn set_full_keyboard_color(&self, r: u8, g: u8, b: u8) {
        self.send(LedCommand::SetFullKeyboardColor { r, g, b });
    }

    /// Restore G-keys to configured color (or turn off if None)
    pub fn restore_gkeys_color(&self, color: Option<(u8, u8, u8)>) {
        self.send(LedCommand::RestoreGKeysColor { color });
    }

    /// Shutdown the LED controller
    pub fn shutdown(&self) {
        self.send(LedCommand::Shutdown);
    }
}

impl Drop for LedController {
    fn drop(&mut self) {
        self.send(LedCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Get current time in milliseconds since UNIX epoch
fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// LED worker thread function
fn led_worker(
    device_path: PathBuf,
    rx: Receiver<LedCommand>,
    mr_led_write_time: Arc<AtomicU64>,
) -> Result<()> {
    // Open device for writing (separate handle from the reader)
    let mut file = OpenOptions::new()
        .write(true)
        .open(&device_path)
        .with_context(|| format!("LED worker failed to open {}", device_path.display()))?;

    log::debug!("LED worker opened {}", device_path.display());

    let mut flashing = false;
    let mut flash_on = false;
    let mut last_flash = Instant::now();

    loop {
        // Use timeout to handle flashing
        let timeout = if flashing {
            MR_FLASH_INTERVAL.saturating_sub(last_flash.elapsed())
        } else {
            Duration::from_secs(3600) // Long timeout when not flashing
        };

        match rx.recv_timeout(timeout) {
            Ok(cmd) => {
                log::trace!("LED command: {:?}", cmd);
                match cmd {
                    LedCommand::SetMrLed(on) => {
                        // Set flag before writing MR LED command
                        mr_led_write_time.store(current_time_ms(), Ordering::SeqCst);
                        write_report(&mut file, &events::mr_led_command(on));
                    }

                    LedCommand::SetProfileLed(profile) => {
                        write_report(&mut file, &events::led_command(profile));
                    }

                    LedCommand::SetAllGKeysLed { r, g, b } => {
                        write_report(&mut file, &events::all_gkeys_led_command(r, g, b));
                        write_report(&mut file, &events::led_commit_command());
                    }

                    LedCommand::SetGKeysRecording { selected_gkey } => {
                        for g in 1..=5u8 {
                            let (r, gv, b) = if g == selected_gkey {
                                (255, 0, 0) // Red for selected
                            } else {
                                (0, 0, 0) // Off for others
                            };
                            write_report(&mut file, &events::gkey_led_command(g, r, gv, b));
                        }
                        write_report(&mut file, &events::led_commit_command());
                    }

                    LedCommand::StartMrFlashing => {
                        flashing = true;
                        flash_on = true;
                        last_flash = Instant::now();
                        // Set flag before writing MR LED command
                        mr_led_write_time.store(current_time_ms(), Ordering::SeqCst);
                        write_report(&mut file, &events::mr_led_command(true));
                    }

                    LedCommand::StopMrFlashing => {
                        flashing = false;
                        // Set flag before writing MR LED command
                        mr_led_write_time.store(current_time_ms(), Ordering::SeqCst);
                        write_report(&mut file, &events::mr_led_command(false));
                    }

                    LedCommand::QuickFlashMr { count } => {
                        flashing = false;
                        for _ in 0..count {
                            // Set flag before each MR LED command
                            mr_led_write_time.store(current_time_ms(), Ordering::SeqCst);
                            write_report(&mut file, &events::mr_led_command(true));
                            thread::sleep(MR_QUICK_FLASH_INTERVAL);
                            mr_led_write_time.store(current_time_ms(), Ordering::SeqCst);
                            write_report(&mut file, &events::mr_led_command(false));
                            thread::sleep(MR_QUICK_FLASH_INTERVAL);
                        }
                    }

                    LedCommand::TurnOffGKeys => {
                        write_report(&mut file, &events::all_gkeys_led_command(0, 0, 0));
                        write_report(&mut file, &events::led_commit_command());
                    }

                    LedCommand::SetFullKeyboardColor { r, g, b } => {
                        // Send initialization sequence first (required for direct mode)
                        for cmd in events::direct_mode_init_commands() {
                            write_report(&mut file, &cmd);
                        }
                        // Set all keys to the specified color
                        for cmd in events::full_keyboard_color_commands(r, g, b) {
                            write_report(&mut file, &cmd);
                        }
                        write_report(&mut file, &events::led_commit_command());
                    }

                    LedCommand::RestoreGKeysColor { color } => {
                        match color {
                            Some((r, g, b)) => {
                                write_report(&mut file, &events::all_gkeys_led_command(r, g, b));
                            }
                            None => {
                                write_report(&mut file, &events::all_gkeys_led_command(0, 0, 0));
                            }
                        }
                        write_report(&mut file, &events::led_commit_command());
                    }

                    LedCommand::Shutdown => {
                        // Turn off LEDs before exiting
                        flashing = false;
                        mr_led_write_time.store(current_time_ms(), Ordering::SeqCst);
                        write_report(&mut file, &events::mr_led_command(false));
                        write_report(&mut file, &events::all_gkeys_led_command(0, 0, 0));
                        write_report(&mut file, &events::led_commit_command());
                        log::debug!("LED worker shutting down");
                        break;
                    }
                }
            }

            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Handle flash toggle on timeout
                if flashing && last_flash.elapsed() >= MR_FLASH_INTERVAL {
                    flash_on = !flash_on;
                    last_flash = Instant::now();
                    // Set flag before writing MR LED command
                    mr_led_write_time.store(current_time_ms(), Ordering::SeqCst);
                    write_report(&mut file, &events::mr_led_command(flash_on));
                }
            }

            Err(mpsc::RecvTimeoutError::Disconnected) => {
                log::debug!("LED worker channel disconnected");
                break;
            }
        }
    }

    Ok(())
}

/// Write a HID report to the device, logging errors
fn write_report(file: &mut std::fs::File, data: &[u8; 20]) {
    if let Err(e) = file.write_all(data) {
        log::warn!("Failed to write LED report: {}", e);
    }
}
