//! hidraw device discovery and I/O

use std::fs::{read_dir, read_to_string, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::events::{parse_report, Event, G815};

/// How long to wait for a HID++ response before retrying
const HIDPP_RESPONSE_TIMEOUT: Duration = Duration::from_millis(200);

/// Maximum time to spend waiting for the keyboard firmware to respond to
/// its first HID++ query after the hidraw node appears. On a KVM-mediated
/// re-enumeration the firmware can take a second or two before it answers.
const HIDPP_READY_TIMEOUT: Duration = Duration::from_secs(3);

/// How many times to retry the full init sequence before giving up
const INIT_MAX_ATTEMPTS: u32 = 3;

pub struct Device {
    file: File,
    path: PathBuf,
    /// Optional external fd (e.g. udev watcher pipe) that breaks blocking
    /// reads when it becomes readable, signalling a disconnect.
    wake_fd: Option<std::os::fd::RawFd>,
    /// Optional external fd (control-socket listener pipe) that breaks
    /// blocking reads when a control request has been queued. Unlike
    /// `wake_fd`, this does not signal a disconnect.
    control_fd: Option<std::os::fd::RawFd>,
}

impl Device {
    /// Open the G815 keyboard hidraw device
    pub fn open(wake_fd: Option<std::os::fd::RawFd>, control_fd: Option<std::os::fd::RawFd>) -> Result<Self> {
        let path = find_hidraw_device()?;
        // Open with read+write for both receiving events and sending commands
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        log::info!("Opened device: {}", path.display());

        let mut dev = Self { file, path, wake_fd, control_fd };

        // Retry the full init sequence. On a KVM-mediated reconnect the
        // keyboard firmware is sometimes not yet responsive to HID++, and
        // an earlier failed exchange can leave stale reports in the queue.
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=INIT_MAX_ATTEMPTS {
            match dev.initialize_gkeys() {
                Ok(()) => {
                    if attempt > 1 {
                        log::info!("HID++ init succeeded on attempt {}", attempt);
                    }
                    return Ok(dev);
                }
                Err(e) => {
                    log::warn!("HID++ init attempt {}/{} failed: {}", attempt, INIT_MAX_ATTEMPTS, e);
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("HID++ init failed with no error recorded")))
    }

    /// Initialize G-key software mode via HID++ 2.0.
    ///
    /// Disables onboard profiles and enables G-key diversion so presses arrive
    /// as vendor reports on interface 1 instead of being handled by the
    /// keyboard's onboard macro engine. Each HID++ exchange verifies that the
    /// response actually matches the query — the previous implementation
    /// blindly consumed whatever byte was next in the hidraw queue, which on
    /// a KVM re-enumeration was often a stale buffered report.
    fn initialize_gkeys(&mut self) -> Result<()> {
        // Discard anything left in the hidraw queue from the previous session
        // before we start trying to match request/response pairs.
        self.drain_buffer();

        // Wait until the keyboard responds to a trivial HID++ ping. This
        // handles the race where the hidraw node exists but the firmware is
        // still booting after a USB reset.
        self.wait_for_hidpp_ready()?;

        // Query ONBOARD_PROFILES feature index (0x8100)
        let onboard_idx = self.query_feature_index(0x8100)
            .context("querying ONBOARD_PROFILES feature index")?;

        if onboard_idx != 0 {
            log::debug!("ONBOARD_PROFILES feature at index 0x{:02x}", onboard_idx);
            // Set onboard mode to DISABLED (0x02) - disables onboard key bindings
            self.hidpp_call(onboard_idx, 0x10, &[0x02, 0x00])
                .context("setMode(disabled) for ONBOARD_PROFILES")?;
            log::info!("Onboard profiles disabled");
        } else {
            log::warn!("ONBOARD_PROFILES feature not found");
        }

        // Query GKEYS feature index (0x8010)
        let gkeys_idx = self.query_feature_index(0x8010)
            .context("querying GKEYS feature index")?;

        if gkeys_idx == 0 {
            bail!("GKEYS feature not found - keyboard won't deliver G-key events");
        }
        log::debug!("GKEYS feature at index 0x{:02x}", gkeys_idx);
        // Enable G-key diversion so G-key presses generate HID++ vendor
        // reports instead of their default onboard behavior.
        self.hidpp_call(gkeys_idx, 0x20, &[0x01, 0x00])
            .context("enableDiversion for GKEYS")?;
        log::info!("G-key diversion enabled");

        log::info!("G-key software mode initialized");
        Ok(())
    }

    /// Poll the root feature (index 0, function getProtocolVersion = 0x10)
    /// until it answers, confirming the firmware is awake.
    fn wait_for_hidpp_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + HIDPP_READY_TIMEOUT;
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            match self.hidpp_call(0x00, 0x10, &[0x00, 0x00, 0x00]) {
                Ok(_) => {
                    if attempts > 1 {
                        log::debug!("HID++ ready after {} ping(s)", attempts);
                    }
                    return Ok(());
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e).context("keyboard firmware never responded to HID++ ping");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    /// HID++ root.getFeatureIndex(feature_id): returns the feature's index,
    /// or 0 if the feature isn't supported.
    fn query_feature_index(&mut self, feature_id: u16) -> Result<u8> {
        let hi = (feature_id >> 8) as u8;
        let lo = feature_id as u8;
        let resp = self.hidpp_call(0x00, 0x00, &[hi, lo])?;
        Ok(resp[4])
    }

    /// Send a HID++ 2.0 short request and return the matching response.
    /// `feature_idx` is byte 2 of the report; `function` is byte 3 (high
    /// nibble = function id, low nibble = software id which we leave at 0).
    /// `params` fills bytes 4..20, zero-padded.
    fn hidpp_call(&mut self, feature_idx: u8, function: u8, params: &[u8]) -> Result<[u8; 20]> {
        // Drain any unrelated reports first so the response we match is
        // genuinely ours, not a stale notification still in the queue.
        self.drain_buffer();

        let mut cmd = [0u8; 20];
        cmd[0] = 0x11;
        cmd[1] = 0xff;
        cmd[2] = feature_idx;
        cmd[3] = function;
        let n = params.len().min(16);
        cmd[4..4 + n].copy_from_slice(&params[..n]);

        self.file.write_all(&cmd).context("write HID++ request")?;

        // Poll for a response that actually matches what we asked for. If
        // the keyboard interleaves an unrelated notification we want to skip
        // it, not mistake it for our reply.
        let deadline = Instant::now() + HIDPP_RESPONSE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("HID++ response timeout for feature 0x{:02x} fn 0x{:02x}", feature_idx, function);
            }
            let mut resp = [0u8; 20];
            if !poll_read(self.file.as_raw_fd(), None, None, &mut resp, remaining)? {
                continue;
            }
            // HID++ 2.0 error: header 0x11 0xff then feature_idx=0xff, the
            // original feature is in resp[3]'s high nibble... We're
            // conservative: require our feature+function to match exactly.
            if resp[0] == 0x11 && resp[1] == 0xff && resp[2] == feature_idx && resp[3] == function {
                return Ok(resp);
            }
            // Error report echoing back our request: feature_idx=0x8f means
            // "error response" in HID++ 2.0. Surface as an error.
            if resp[0] == 0x11 && resp[1] == 0xff && resp[2] == 0x8f
                && resp[3] == feature_idx && resp[4] == function
            {
                bail!(
                    "HID++ error for feature 0x{:02x} fn 0x{:02x}: err=0x{:02x}",
                    feature_idx, function, resp[5]
                );
            }
            // Unrelated report (e.g. buffered G-key notification). Ignore
            // and keep waiting for our reply.
            log::trace!("HID++ ignoring unrelated report while waiting: {:02x?}", &resp[..8]);
        }
    }

    /// Get the device path
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Drain any buffered HID reports from the device
    ///
    /// After initialization and LED setup, the keyboard may have queued
    /// response/notification reports that would be misinterpreted as key
    /// events. This reads and discards all pending reports.
    pub fn drain_buffer(&mut self) {
        let fd = self.file.as_raw_fd();
        let mut buf = [0u8; 20];
        let mut count = 0;

        loop {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            // poll with 0 timeout = non-blocking check
            let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ret <= 0 {
                break;
            }

            match self.file.read(&mut buf) {
                Ok(n) if n > 0 => {
                    count += 1;
                    log::debug!(
                        "Drained buffered report #{}: {:02x?}",
                        count,
                        &buf[..n]
                    );
                }
                _ => break,
            }
        }

        if count > 0 {
            log::info!("Drained {} buffered report(s)", count);
        }
    }

    /// Read and parse a HID event with a short timeout (for recording-mode
    /// poll loop).
    pub fn read_event(&mut self) -> Result<Option<Event>> {
        self.read_event_timeout(Duration::from_millis(100))
    }

    /// Read and parse a HID event, blocking until one arrives, the wake fd
    /// becomes readable, the control fd becomes readable, or an I/O error
    /// occurs.
    ///
    /// - Wake fd fires -> returns a disconnect error so the caller
    ///   reconnects.
    /// - Control fd fires -> returns `Ok(None)`; there's no HID event to
    ///   report, but a control request is now waiting on the channel for
    ///   the caller to pick up. The caller should re-check that channel and
    ///   then call this again.
    pub fn read_event_blocking(&mut self) -> Result<Option<Event>> {
        let mut buf = [0u8; 20];
        if !poll_read(self.file.as_raw_fd(), self.wake_fd, self.control_fd, &mut buf, Duration::MAX)? {
            return Ok(None);
        }
        Ok(parse_report(&buf))
    }

    /// Read and parse a HID event with specified timeout
    pub fn read_event_timeout(&mut self, timeout: Duration) -> Result<Option<Event>> {
        let mut buf = [0u8; 20];
        if !poll_read(self.file.as_raw_fd(), self.wake_fd, self.control_fd, &mut buf, timeout)? {
            return Ok(None);
        }
        Ok(parse_report(&buf))
    }

}

/// Poll the device fd (and optional wake/control fds) for readability, then
/// read one report. Returns:
/// - `Ok(true)` if `buf` was filled from the device fd
/// - `Ok(false)` if the timeout expired, or the control fd fired (there is
///   no HID data to report either way; on an infinite timeout, reaching
///   `Ok(false)` means the control fd fired, since the timeout itself can't
///   expire)
/// - `Err(_)` if the wake fd fired (disconnect), the device EOFed, or a
///   real I/O error occurred
fn poll_read(
    dev_fd: std::os::fd::RawFd,
    wake_fd: Option<std::os::fd::RawFd>,
    control_fd: Option<std::os::fd::RawFd>,
    buf: &mut [u8; 20],
    timeout: Duration,
) -> Result<bool> {
    let mut pfds = [
        libc::pollfd { fd: dev_fd, events: libc::POLLIN, revents: 0 },
        libc::pollfd {
            fd: wake_fd.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: control_fd.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // Only ask poll() to look at as many entries as are actually in use;
    // entries beyond that stay at fd -1 and are never touched.
    let nfds = if control_fd.is_some() {
        3
    } else if wake_fd.is_some() {
        2
    } else {
        1
    };

    // poll() takes milliseconds as int. Duration::MAX would overflow, so
    // cap to -1 (infinite wait) for effectively-infinite timeouts.
    let timeout_ms: i32 = if timeout == Duration::MAX {
        -1
    } else {
        timeout.as_millis().min(i32::MAX as u128) as i32
    };

    let ret = unsafe { libc::poll(pfds.as_mut_ptr(), nfds as libc::nfds_t, timeout_ms) };
    if ret < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EINTR) {
            return Ok(false);
        }
        return Err(e.into());
    }
    if ret == 0 {
        return Ok(false);
    }

    // Wake fd fired - treat as disconnect. Caller handles the reconnect.
    if wake_fd.is_some() && pfds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
        bail!("wake fd signalled disconnect");
    }

    // Control fd fired - a request is waiting on the control channel. This
    // is not a disconnect, so just report "no data" and let the caller
    // check the channel.
    if control_fd.is_some() && pfds[2].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
        return Ok(false);
    }

    // Device fd error/hangup - stale fd after USB removal
    if pfds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        bail!("device fd hangup/error (revents=0x{:x})", pfds[0].revents);
    }

    if pfds[0].revents & libc::POLLIN == 0 {
        return Ok(false);
    }

    // SAFETY: valid fd, valid buffer, len fits in size_t.
    let n = unsafe { libc::read(dev_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if n == 0 {
        // EOF on a hidraw fd means the device was unplugged - this used to
        // silently return Ok(None) and the main loop would busy-spin.
        bail!("device returned EOF");
    }
    Ok(true)
}

/// Find the hidraw device for the G815 keyboard interface 1
fn find_hidraw_device() -> Result<PathBuf> {
    let target_vendor = format!("{:04X}", G815.vendor_id).to_uppercase();
    let target_product = format!("{:04X}", G815.product_id).to_uppercase();

    for entry in read_dir("/sys/class/hidraw")? {
        let entry = entry?;
        let hidraw_name = entry.file_name();
        let device_path = entry.path().join("device");

        // Read uevent to get HID_ID
        let uevent_path = device_path.join("uevent");
        if let Ok(uevent) = read_to_string(&uevent_path) {
            // Look for HID_ID=0003:0000046D:0000C33F
            for line in uevent.lines() {
                if line.starts_with("HID_ID=") {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.len() >= 3 {
                        let vendor = parts[1].trim_start_matches("0000");
                        let product = parts[2].trim_start_matches("0000");
                        if vendor == target_vendor && product == target_product {
                            // Check if this is interface 1 by looking at full device path
                            let real_path = std::fs::canonicalize(&device_path)?;
                            let path_str = real_path.to_string_lossy();
                            // Interface 1 has :1.1/ in the path
                            if path_str.contains(":1.1/") {
                                return Ok(PathBuf::from(format!(
                                    "/dev/{}",
                                    hidraw_name.to_string_lossy()
                                )));
                            }
                        }
                    }
                }
            }
        }
    }

    Err(anyhow!(
        "G815 interface 1 not found. Is the keyboard connected and not claimed by another program?"
    ))
}
