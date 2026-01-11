mod config;
mod device;
mod events;
mod macros;
mod uinput;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;

use config::Config;
use device::Device;
use events::Event;
use macros::MacroExecutor;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("gkeys-rs starting");

    // Load config
    let config_path = Config::config_path()?;
    let config = match Config::load() {
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

    // Current profile (preserved across reconnections)
    let mut current_profile = String::from("MEMORY_1");

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

        // Set profile LED to match current state
        let profile_num = current_profile
            .strip_prefix("MEMORY_")
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(1);
        if let Err(e) = device.set_profile_led(profile_num) {
            log::warn!("Failed to set profile LED: {}", e);
        }

        log::info!("Ready. Listening for G-key events...");

        // Inner event loop - runs until device disconnects or shutdown
        loop {
            if !running.load(Ordering::SeqCst) {
                log::info!("Shutting down");
                return Ok(());
            }

            match device.read_event() {
                Ok(Some(event)) => {
                    handle_event(
                        &event,
                        &config,
                        &mut current_profile,
                        &mut executor,
                        &mut device,
                    );
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
    device: &mut Device,
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

                if let Err(e) = device.set_profile_led(*n) {
                    log::warn!("Failed to set profile LED: {}", e);
                }

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
            log::debug!("MR pressed (macro record - not implemented)");
        }
        Event::MRKeyRelease => {
            log::trace!("MR released");
        }
    }
}
