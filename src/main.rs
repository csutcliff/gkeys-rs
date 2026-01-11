mod config;
mod device;
mod events;
mod led;
mod macros;
mod recording;
mod uinput;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;

use config::{Config, HotkeyType, Macro};
use device::Device;
use events::Event;
use led::LedController;
use macros::MacroExecutor;
use recording::{Recorder, RecordingAction};

/// Number of quick flashes on successful recording
const MR_QUICK_FLASH_COUNT: u8 = 4;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("gkeys-rs starting");

    // Load config
    let config_path = Config::config_path()?;
    let mut config = match Config::load() {
        Ok(c) => {
            log::info!("Loaded config from {}", config_path.display());
            c
        }
        Err(e) => {
            log::error!("Failed to load config: {}", e);
            log::error!("Expected config at: {}", config_path.display());
            return Err(e);
        }
    };

    // Create macro executor
    let mut executor = MacroExecutor::new()?;
    log::info!("Virtual keyboard created");

    // Create macro recorder
    let mut recorder = Recorder::new();

    // Current profile (preserved across reconnections)
    let mut current_profile = String::from("MEMORY_1");

    // LED controller (created per device connection)
    let mut led_controller: Option<LedController> = None;

    // Setup signal handling for clean shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    // Outer loop handles device reconnection
    let mut reconnect_delay = Duration::from_secs(1);
    let max_reconnect_delay = Duration::from_secs(30);

    while running.load(Ordering::SeqCst) {
        // Try to open device
        let mut device = match Device::open() {
            Ok(d) => {
                log::info!("Opened device: {}", d.path().display());
                reconnect_delay = Duration::from_secs(1); // Reset delay on success
                d
            }
            Err(e) => {
                log::warn!("Device not found: {} - retrying in {:?}", e, reconnect_delay);
                thread::sleep(reconnect_delay);
                reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
                continue;
            }
        };

        // Create LED controller for this device
        let led_ctrl = match LedController::new(device.path().clone()) {
            Ok(ctrl) => ctrl,
            Err(e) => {
                log::error!("Failed to create LED controller: {}", e);
                thread::sleep(reconnect_delay);
                reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
                continue;
            }
        };
        led_controller = Some(led_ctrl);
        let led = led_controller.as_ref().unwrap();

        // Set profile LED to match current state
        let profile_num = current_profile
            .strip_prefix("MEMORY_")
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(1);
        led.set_profile_led(profile_num);

        log::info!("Ready. Listening for G-key events...");

        // Inner event loop - runs until device disconnects or shutdown
        loop {
            if !running.load(Ordering::SeqCst) {
                log::info!("Shutting down");
                return Ok(());
            }

            // Poll for captured keys during recording
            if recorder.is_recording() {
                recorder.poll_captured_keys();
            }

            // Use timeout read so we can poll captured keys during recording
            let event_result = if recorder.is_recording() {
                device.read_event() // 100ms timeout
            } else {
                device.read_event_blocking() // Blocking read when not recording
            };

            match event_result {
                Ok(Some(event)) => {
                    // Check if recorder should handle this event
                    if let Some(action) = handle_event_for_recording(
                        &event,
                        &mut recorder,
                        &current_profile,
                        led,
                    ) {
                        handle_recording_action(action, &mut config, led);
                    } else if !recorder.is_recording() && !recorder.is_awaiting() {
                        // Normal macro execution only when not recording
                        handle_event(
                            &event,
                            &config,
                            &mut current_profile,
                            &mut executor,
                            led,
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    // Check if it's an interrupted system call (from signal)
                    if running.load(Ordering::SeqCst) {
                        log::warn!("Device disconnected: {} - will attempt reconnection", e);
                        break; // Break inner loop to try reconnection
                    }
                }
            }
        }
    }

    log::info!("Shutting down");
    Ok(())
}

fn handle_event(
    event: &Event,
    config: &Config,
    current_profile: &mut String,
    executor: &mut MacroExecutor,
    led: &LedController,
) {
    match event {
        Event::GKey(n) => {
            let macro_name = format!("MACRO_{}", n);
            log::debug!("G{} pressed (profile: {})", n, current_profile);

            if let Some(macro_def) = config.get_macro(current_profile, &macro_name) {
                if let Err(e) = executor.execute(macro_def) {
                    log::error!("Failed to execute macro: {}", e);
                }
            } else {
                log::debug!("No macro defined for {} in {}", macro_name, current_profile);
            }
        }
        Event::GKeyRelease => {
            log::trace!("G-key released");
        }
        Event::MKey(n) => {
            let new_profile = format!("MEMORY_{}", n);
            log::debug!("M{} pressed, current='{}', new='{}'", n, current_profile, new_profile);
            // Only switch if different (prevents feedback loop from LED response)
            if *current_profile != new_profile {
                log::info!("Switching to profile M{}", n);
                *current_profile = new_profile.clone();

                led.set_profile_led(*n);

                if config.notify.0 {
                    // Send desktop notification
                    let _ = std::process::Command::new("notify-send")
                        .arg("-a")
                        .arg("gkeys-rs")
                        .arg(format!("Profile M{}", n))
                        .spawn();
                }
            }
        }
        Event::MKeyRelease => {
            log::trace!("M-key released");
        }
        Event::MRKey => {
            // Handled by recording state machine
            log::trace!("MR pressed (handled by recorder)");
        }
        Event::MRKeyRelease => {
            log::trace!("MR released");
        }
    }
}

/// Check if an event should be handled by the recorder
fn handle_event_for_recording(
    event: &Event,
    recorder: &mut Recorder,
    current_profile: &str,
    led: &LedController,
) -> Option<RecordingAction> {
    match event {
        Event::MRKey => {
            // Check if this MR event was generated by an LED write
            if led.is_mr_event_from_led() {
                log::debug!("MR event from LED write, ignoring");
                return None;
            }
            let action = recorder.on_mr_press(current_profile);
            // Filter out None actions
            if matches!(action, RecordingAction::None) {
                None
            } else {
                Some(action)
            }
        }
        Event::GKey(n) if recorder.is_awaiting() => Some(recorder.on_gkey_press(*n)),
        _ => None,
    }
}

/// Execute a recording action with LED control
fn handle_recording_action(action: RecordingAction, config: &mut Config, led: &LedController) {
    match action {
        RecordingAction::None => {}

        RecordingAction::EnterAwaiting => {
            // MR LED on, all G-keys white
            led.set_mr_led(true);
            led.set_all_gkeys_led(255, 255, 255);
            log::debug!("Awaiting G-key selection - G-keys white, MR on");
        }

        RecordingAction::StartedRecording { gkey } => {
            // Selected G-key red, others off
            led.set_gkeys_recording(gkey);
            // Start MR flashing (handled by LED thread)
            led.start_mr_flashing();

            log::debug!("Recording G{} - G-key red, MR flashing", gkey);
            let _ = std::process::Command::new("notify-send")
                .args([
                    "-a",
                    "gkeys-rs",
                    &format!("Recording G{}", gkey),
                    "Press keys, then MR to stop",
                ])
                .spawn();
        }

        RecordingAction::SaveMacro {
            profile,
            gkey,
            sequence,
        } => {
            // Quick flash MR LED (handled by LED thread)
            led.quick_flash_mr(MR_QUICK_FLASH_COUNT);
            // Turn off G-key LEDs
            led.turn_off_gkeys();

            // Save the macro
            let macro_name = format!("MACRO_{}", gkey);
            config.set_macro(
                &profile,
                &macro_name,
                Macro {
                    hotkey_type: HotkeyType::Sequence,
                    action: sequence.clone(),
                },
            );

            if let Err(e) = config.save() {
                log::error!("Failed to save config: {}", e);
                let _ = std::process::Command::new("notify-send")
                    .args([
                        "-a",
                        "gkeys-rs",
                        "Recording failed",
                        &format!("Could not save: {}", e),
                    ])
                    .spawn();
                return;
            }

            log::info!("Saved macro G{} = {}", gkey, sequence);
            let _ = std::process::Command::new("notify-send")
                .args(["-a", "gkeys-rs", &format!("Recorded G{}", gkey), &sequence])
                .spawn();
        }

        RecordingAction::CancelledEmpty => {
            // No keys captured - just turn off LEDs, no flash
            led.stop_mr_flashing();
            led.turn_off_gkeys();
            log::info!("Recording cancelled - no keys captured");
            let _ = std::process::Command::new("notify-send")
                .args(["-a", "gkeys-rs", "Recording cancelled", "No keys were captured"])
                .spawn();
        }

        RecordingAction::CancelledNoGKey => {
            // MR pressed without G-key - just turn off LEDs, no flash
            led.set_mr_led(false);
            led.turn_off_gkeys();
            log::debug!("Recording cancelled - no G-key selected");
        }

        RecordingAction::Error(msg) => {
            // Error - turn off LEDs
            led.stop_mr_flashing();
            led.turn_off_gkeys();
            log::error!("Recording error: {}", msg);
            let _ = std::process::Command::new("notify-send")
                .args(["-a", "gkeys-rs", "Recording error", &msg])
                .spawn();
        }
    }
}
