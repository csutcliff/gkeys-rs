mod config;
mod control;
mod device;
mod events;
mod led;
mod macros;
mod proc;
mod recording;
mod udev_watcher;
mod uinput;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;

use config::{Config, HotkeyType, Macro};
use control::{ControlCommand, ControlListener, ControlRequest};
use device::Device;
use events::Event;
use led::LedController;
use macros::MacroExecutor;
use proc::spawn_reaped;
use recording::{Recorder, RecordingAction};
use udev_watcher::UdevWatcher;

/// Number of quick flashes on successful recording
const MR_QUICK_FLASH_COUNT: u8 = 4;

fn main() -> Result<()> {
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(exit_code) = control::maybe_run_client(&cli_args) {
        std::process::exit(exit_code);
    }

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
    let mut led_controller: Option<LedController>;

    // Setup signal handling for clean shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    // Start udev watcher: its wake fd becomes readable on G815 remove events
    // so blocking reads break out immediately rather than waiting for the
    // stale fd to error. If the watcher fails to start we degrade to the
    // old behaviour (poll-on-error) rather than crashing.
    let udev = match UdevWatcher::new() {
        Ok(w) => Some(w),
        Err(e) => {
            log::warn!("udev watcher unavailable: {} - falling back to read-error reconnect", e);
            None
        }
    };
    let wake_fd = udev.as_ref().map(|w| w.wake_fd());

    // Control socket: lets an external program (e.g. a KVM-switch focus
    // hook) request a profile switch without touching the keyboard. Best
    // effort, like the udev watcher above - a user with no interest in this
    // stays entirely unaffected if it can't be set up.
    let (control_tx, control_rx) = mpsc::channel::<ControlRequest>();
    let control_listener = match ControlListener::new(control_tx) {
        Ok(listener) => {
            log::info!("Control socket listening at {}", listener.path().display());
            Some(listener)
        }
        Err(e) => {
            log::warn!("Control socket unavailable: {} - continuing without it", e);
            None
        }
    };
    let control_wake_fd = control_listener.as_ref().map(|c| c.wake_fd());

    // Outer loop handles device reconnection
    let mut reconnect_delay = Duration::from_secs(1);
    let max_reconnect_delay = Duration::from_secs(30);

    while running.load(Ordering::SeqCst) {
        // Swallow any pending wake bytes so they don't immediately trip the
        // fresh device. We only want wakes from here on out.
        if let Some(ref w) = udev {
            w.drain();
        }
        if let Some(ref c) = control_listener {
            c.drain();
        }

        // Apply any profile-switch requests that arrived over the control
        // socket while no keyboard was attached. There's no LedController
        // to update yet, but the state change takes effect immediately and
        // the LED catches up via the profile sync below once a device
        // connects.
        drain_control_messages(&control_rx, &mut current_profile, &config, None);

        // Try to open device
        let mut device = match Device::open(wake_fd, control_wake_fd) {
            Ok(d) => {
                log::info!("Opened device: {}", d.path().display());
                reconnect_delay = Duration::from_secs(1); // Reset delay on success
                d
            }
            Err(e) => {
                log::warn!("Device open/init failed: {} - retrying in {:?}", e, reconnect_delay);
                wait_for_retry(
                    reconnect_delay,
                    wake_fd,
                    control_wake_fd,
                    &control_rx,
                    &mut current_profile,
                    &config,
                    control_listener.as_ref(),
                );
                reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
                continue;
            }
        };

        // Create LED controller for this device
        let led_ctrl = match LedController::new(device.path().clone()) {
            Ok(ctrl) => ctrl,
            Err(e) => {
                log::error!("Failed to create LED controller: {}", e);
                wait_for_retry(
                    reconnect_delay,
                    wake_fd,
                    control_wake_fd,
                    &control_rx,
                    &mut current_profile,
                    &config,
                    control_listener.as_ref(),
                );
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

        // Apply configured RGB color to entire keyboard if set
        if let Some(ref color) = config.rgb_color {
            log::info!("Setting keyboard color to RGB({}, {}, {})", color.r, color.g, color.b);
            led.set_full_keyboard_color(color.r, color.g, color.b);
        }

        // After initialization and LED setup, the keyboard generates phantom
        // HID reports (responses to LED commands, state notifications after
        // diversion enable) that get misinterpreted as key events.
        thread::sleep(Duration::from_millis(100));
        device.drain_buffer();

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

            // Apply any profile-switch requests that arrived over the
            // control socket. try_recv is non-blocking so this never stalls
            // the loop.
            drain_control_messages(&control_rx, &mut current_profile, &config, Some(led));
            if let Some(ref c) = control_listener {
                c.drain();
            }

            // Use timeout read so we can poll captured keys during recording
            let event_result = if recorder.is_recording() {
                device.read_event() // 100ms timeout
            } else {
                device.read_event_blocking() // blocking read, woken by keyboard I/O, disconnect, or a queued control request
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

/// Why a reconnect backoff wait ended.
#[derive(Debug, PartialEq, Eq)]
enum Wake {
    /// udev says a device appeared or vanished: stop waiting and try again.
    Udev,
    /// A control request is queued and needs answering now.
    Control,
    /// Nothing happened before the deadline.
    Elapsed,
    /// A signal cut the poll short. Not an event; the caller works out how
    /// much of the wait is left and goes back to waiting.
    Interrupted,
}

/// Wait up to `timeout` for either watcher to have something to say.
///
/// Same `poll()` shape as `device::poll_read`, minus the device fd: this runs
/// when there is no device to read from.
fn poll_wake(
    udev_fd: Option<std::os::fd::RawFd>,
    control_fd: Option<std::os::fd::RawFd>,
    timeout: Duration,
) -> Wake {
    let mut pfds = [
        libc::pollfd { fd: udev_fd.unwrap_or(-1), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: control_fd.unwrap_or(-1), events: libc::POLLIN, revents: 0 },
    ];
    let timeout_ms: i32 = timeout.as_millis().min(i32::MAX as u128) as i32;

    let ret = unsafe { libc::poll(pfds.as_mut_ptr(), 2, timeout_ms) };
    if ret < 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => Wake::Interrupted,
            // Anything else here is a programming error (a bad fd), not a
            // transient. Report it as elapsed so the caller retries the device
            // on its normal schedule rather than spinning on poll().
            _ => Wake::Elapsed,
        };
    }
    if ret == 0 {
        return Wake::Elapsed;
    }
    // udev first: a device that may be back is a better reason to stop waiting
    // than a queued request, and the request is drained at the top of the outer
    // loop anyway, before the next open attempt.
    if udev_fd.is_some() && pfds[0].revents != 0 {
        return Wake::Udev;
    }
    if control_fd.is_some() && pfds[1].revents != 0 {
        return Wake::Control;
    }
    Wake::Elapsed
}

/// Sit out the reconnect backoff without going deaf for the duration.
///
/// This used to be a plain `thread::sleep(reconnect_delay)`, and the delay
/// doubles to 30s. Two things went wrong in that window, both seen on
/// 2026-09-10 when the monitor's USB hub power-cycled (the G815 hangs off it,
/// at 5-3.4.1, so the panel powering down takes the keyboard with it):
///
/// - A `--set-profile` request arriving mid-sleep was not read until the sleep
///   ended, and the daemon's own `REPLY_TIMEOUT` is 5s, so the client was told
///   `err timed out waiting for daemon` by a daemon that was perfectly healthy
///   and merely asleep. That is what the panel's colour-profile udev hook hit,
///   4s after the hub came back.
/// - The keyboard itself stayed dead for the rest of the sleep after it had
///   physically returned -- up to 30s of no macros, no G-keys -- because the
///   udev "device added" wake was not being watched either.
///
/// So wait on both fds instead: return early when udev suggests the device is
/// back, and answer control requests as they arrive without giving up the rest
/// of the wait.
#[allow(clippy::too_many_arguments)]
fn wait_for_retry(
    delay: Duration,
    udev_fd: Option<std::os::fd::RawFd>,
    control_fd: Option<std::os::fd::RawFd>,
    control_rx: &mpsc::Receiver<ControlRequest>,
    current_profile: &mut String,
    config: &Config,
    listener: Option<&ControlListener>,
) {
    let deadline = std::time::Instant::now() + delay;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        match poll_wake(udev_fd, control_fd, remaining) {
            Wake::Udev => return,
            Wake::Control => {
                if let Some(c) = listener {
                    c.drain();
                }
                // No LedController: there is no device attached, which is the
                // whole reason we are in the backoff. The profile state still
                // updates and the LED catches up on reconnect.
                drain_control_messages(control_rx, current_profile, config, None);
            }
            Wake::Interrupted => continue,
            Wake::Elapsed => return,
        }
    }
}

/// Drain and apply any pending control-socket requests. Uses `try_recv` so
/// it never blocks the caller; called once per main-loop iteration both
/// while a device is connected and while the outer loop is between
/// (re)connection attempts, so a request isn't lost or left waiting behind
/// a stalled device open.
fn drain_control_messages(
    rx: &mpsc::Receiver<ControlRequest>,
    current_profile: &mut String,
    config: &Config,
    led: Option<&LedController>,
) {
    while let Ok(request) = rx.try_recv() {
        match request.command {
            ControlCommand::SetProfile(n) => {
                apply_profile(n, current_profile, config, led);
                request.respond("ok");
            }
        }
    }
}

/// Switch to the given memory profile: updates `current_profile`, the M-key
/// LED (if a controller is available) and sends the desktop notification if
/// enabled. This is exactly what happens on a physical M-key press
/// (`handle_event`'s `Event::MKey` arm below) and a control-socket
/// `profile <n>` request; both call this so the two paths can't drift.
///
/// `led` is `None` when no keyboard is currently attached. The profile
/// state still updates in that case (and is picked up by the LED the next
/// time the keyboard reconnects, via the profile sync in `main`), but there
/// is obviously no LED to write to in the meantime.
fn apply_profile(n: u8, current_profile: &mut String, config: &Config, led: Option<&LedController>) {
    let new_profile = format!("MEMORY_{}", n);
    // Only switch if different (prevents a feedback loop from the LED
    // response on a physical press, and makes a repeat request for the
    // already-active profile a harmless no-op).
    if *current_profile == new_profile {
        return;
    }
    log::info!("Switching to profile M{}", n);
    *current_profile = new_profile;

    if let Some(led) = led {
        led.set_profile_led(n);
    }

    if config.notify.0 {
        // Send desktop notification
        let _ = spawn_reaped(
            std::process::Command::new("notify-send")
                .arg("-a")
                .arg("gkeys-rs")
                .arg(format!("Profile M{}", n)),
        );
    }
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
            log::debug!("M{} pressed, current='{}'", n, current_profile);
            apply_profile(*n, current_profile, config, Some(led));
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
            let _ = spawn_reaped(std::process::Command::new("notify-send").args([
                "-a",
                "gkeys-rs",
                &format!("Recording G{}", gkey),
                "Press keys, then MR to stop",
            ]));
        }

        RecordingAction::SaveMacro {
            profile,
            gkey,
            sequence,
        } => {
            // Quick flash MR LED (handled by LED thread)
            led.quick_flash_mr(MR_QUICK_FLASH_COUNT);
            // Restore G-key LEDs to configured color (or off if not set)
            let gkey_color = config.rgb_color.as_ref().map(|c| (c.r, c.g, c.b));
            led.restore_gkeys_color(gkey_color);

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
                let _ = spawn_reaped(std::process::Command::new("notify-send").args([
                    "-a",
                    "gkeys-rs",
                    "Recording failed",
                    &format!("Could not save: {}", e),
                ]));
                return;
            }

            log::info!("Saved macro G{} = {}", gkey, sequence);
            let _ = spawn_reaped(
                std::process::Command::new("notify-send")
                    .args(["-a", "gkeys-rs", &format!("Recorded G{}", gkey), &sequence]),
            );
        }

        RecordingAction::CancelledEmpty => {
            // No keys captured - just restore LEDs, no flash
            led.stop_mr_flashing();
            let gkey_color = config.rgb_color.as_ref().map(|c| (c.r, c.g, c.b));
            led.restore_gkeys_color(gkey_color);
            log::info!("Recording cancelled - no keys captured");
            let _ = spawn_reaped(std::process::Command::new("notify-send").args([
                "-a",
                "gkeys-rs",
                "Recording cancelled",
                "No keys were captured",
            ]));
        }

        RecordingAction::CancelledNoGKey => {
            // MR pressed without G-key - just restore LEDs, no flash
            led.set_mr_led(false);
            let gkey_color = config.rgb_color.as_ref().map(|c| (c.r, c.g, c.b));
            led.restore_gkeys_color(gkey_color);
            log::debug!("Recording cancelled - no G-key selected");
        }

        RecordingAction::Error(msg) => {
            // Error - restore LEDs
            led.stop_mr_flashing();
            let gkey_color = config.rgb_color.as_ref().map(|c| (c.r, c.g, c.b));
            led.restore_gkeys_color(gkey_color);
            log::error!("Recording error: {}", msg);
            let _ = spawn_reaped(
                std::process::Command::new("notify-send").args(["-a", "gkeys-rs", "Recording error", &msg]),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    /// A pipe stands in for either watcher's wake fd: both are pipes whose
    /// read end becomes readable when a byte is written, which is the only
    /// property `poll_wake` cares about.
    fn pipe() -> (std::fs::File, std::fs::File) {
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
        use std::os::fd::FromRawFd;
        unsafe { (std::fs::File::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1])) }
    }

    /// The bug this whole function exists for: a profile request arriving
    /// during the backoff has to be noticed straight away, not after up to
    /// 30s, because the daemon answers its own clients within 5s or they are
    /// told it timed out.
    #[test]
    fn a_control_request_ends_the_wait_immediately() {
        let (rx, mut tx) = pipe();
        tx.write_all(&[1]).unwrap();

        let started = Instant::now();
        let woke = poll_wake(None, Some(rx.as_raw_fd()), Duration::from_secs(30));

        assert_eq!(woke, Wake::Control);
        assert!(started.elapsed() < Duration::from_secs(1), "waited {:?}", started.elapsed());
    }

    /// The other half: the keyboard coming back must cut the backoff short,
    /// or it stays dead for the rest of a sleep that can be 30s long.
    #[test]
    fn a_udev_event_ends_the_wait_immediately() {
        let (rx, mut tx) = pipe();
        tx.write_all(&[1]).unwrap();

        let started = Instant::now();
        let woke = poll_wake(Some(rx.as_raw_fd()), None, Duration::from_secs(30));

        assert_eq!(woke, Wake::Udev);
        assert!(started.elapsed() < Duration::from_secs(1), "waited {:?}", started.elapsed());
    }

    /// Both at once: udev wins, because a device that may be back is the
    /// better reason to stop waiting. The request is not lost -- the outer
    /// loop drains it before the next open attempt.
    #[test]
    fn a_udev_event_takes_precedence_over_a_control_request() {
        let (udev_rx, mut udev_tx) = pipe();
        let (ctl_rx, mut ctl_tx) = pipe();
        udev_tx.write_all(&[1]).unwrap();
        ctl_tx.write_all(&[1]).unwrap();

        let woke = poll_wake(Some(udev_rx.as_raw_fd()), Some(ctl_rx.as_raw_fd()), Duration::from_secs(30));

        assert_eq!(woke, Wake::Udev);
    }

    /// Nothing to report: the wait still has to end on its own, or the
    /// backoff would never expire and the device would never be retried.
    #[test]
    fn an_idle_wait_ends_at_the_timeout() {
        let (udev_rx, _udev_tx) = pipe();
        let (ctl_rx, _ctl_tx) = pipe();

        let started = Instant::now();
        let woke = poll_wake(Some(udev_rx.as_raw_fd()), Some(ctl_rx.as_raw_fd()), Duration::from_millis(50));

        assert_eq!(woke, Wake::Elapsed);
        assert!(started.elapsed() >= Duration::from_millis(45), "returned early: {:?}", started.elapsed());
    }

    /// With neither watcher available (both failed to start) the wait must
    /// still be a wait, not a busy spin returning instantly.
    #[test]
    fn with_no_watchers_at_all_it_is_still_a_timed_wait() {
        let started = Instant::now();
        let woke = poll_wake(None, None, Duration::from_millis(50));

        assert_eq!(woke, Wake::Elapsed);
        assert!(started.elapsed() >= Duration::from_millis(45), "returned early: {:?}", started.elapsed());
    }
}
