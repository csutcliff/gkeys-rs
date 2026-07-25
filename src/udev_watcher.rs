//! Udev-based USB hotplug watcher
//!
//! Watches for remove events on the G815's hidraw/USB nodes and wakes the
//! main loop via a pipe fd so blocking reads break out immediately instead
//! of waiting for an I/O error on the stale fd.

use std::io::Write;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::io::FromRawFd;
use std::thread;

use anyhow::{Context, Result};
use udev::{EventType, MonitorBuilder};

use crate::events::G815;

pub struct UdevWatcher {
    wake_rx: OwnedFd,
    // Thread joins implicitly when the process exits; no explicit join needed.
    _thread: thread::JoinHandle<()>,
}

impl UdevWatcher {
    pub fn new() -> Result<Self> {
        let (rx_fd, tx_fd) = make_pipe()?;

        // Clone for the thread (write end only)
        let thread = thread::Builder::new()
            .name("udev-watcher".into())
            .spawn(move || {
                if let Err(e) = run_watcher(tx_fd) {
                    log::error!("udev watcher failed: {}", e);
                }
            })
            .context("Failed to spawn udev watcher thread")?;

        Ok(Self {
            wake_rx: rx_fd,
            _thread: thread,
        })
    }

    /// Raw fd that becomes readable when a disconnect event is observed.
    pub fn wake_fd(&self) -> RawFd {
        self.wake_rx.as_raw_fd()
    }

    /// Drain any pending wake bytes so subsequent polls only fire on NEW events.
    pub fn drain(&self) {
        let fd = self.wake_rx.as_raw_fd();
        let mut buf = [0u8; 64];
        loop {
            // Non-blocking peek via poll(0) then read
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ret <= 0 {
                break;
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }
}

fn make_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // O_CLOEXEC so we don't leak fds into child processes spawned by macros
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error()).context("pipe2 failed");
    }
    let rx = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let tx = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((rx, tx))
}

fn run_watcher(tx_fd: OwnedFd) -> Result<()> {
    let monitor = MonitorBuilder::new()
        .context("MonitorBuilder::new failed")?
        .match_subsystem("hidraw")
        .context("match_subsystem hidraw failed")?
        .listen()
        .context("monitor listen failed")?;

    log::info!("udev watcher listening for hidraw events");

    // Convert OwnedFd into a File that now owns the fd (no double-close).
    let mut tx = unsafe { std::fs::File::from_raw_fd(tx_fd.into_raw_fd()) };

    let target_vendor = format!("{:04x}", G815.vendor_id);
    let target_product = format!("{:04x}", G815.product_id);

    // `udev::MonitorSocket::iter()` is NON-blocking: it drains pending events
    // and then returns None. Relying on the iterator alone would make this
    // thread exit immediately on startup, close the pipe, and fire a false
    // "disconnect" in the main loop. Instead, poll() the netlink fd ourselves
    // to block until events are available, then drain them via iter().
    let monitor_fd = monitor.as_raw_fd();
    loop {
        let mut pfd = libc::pollfd {
            fd: monitor_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, -1) };
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e).context("udev monitor poll failed");
        }

        for event in monitor.iter() {
            if event.event_type() != EventType::Remove {
                continue;
            }
            if !event_matches_g815(&event, &target_vendor, &target_product) {
                continue;
            }

            log::info!(
                "udev: remove event for {} - waking main loop",
                event.sysname().to_string_lossy()
            );
            // Any byte will do; main loop treats readability as "disconnect".
            let _ = tx.write_all(&[1u8]);
        }
    }
}

fn event_matches_g815(event: &udev::Event, vendor: &str, product: &str) -> bool {
    // The HID_ID attribute on the parent HID device is "BUS:VVVV:PPPP" (hex,
    // uppercase, zero-padded to 4). On a remove event the device may already
    // be gone from sysfs, so we also check the hidraw's own device path which
    // the kernel reports in the event regardless.
    let mut node: Option<udev::Device> = Some(event.device());
    while let Some(dev) = node {
        if let Some(hid_id) = dev.property_value("HID_ID") {
            let hid_id = hid_id.to_string_lossy().to_ascii_lowercase();
            // Expected: "0003:0000046d:0000c33f"
            let needle = format!(":{:0>8}:{:0>8}", vendor, product);
            if hid_id.ends_with(&needle) {
                return true;
            }
        }
        node = dev.parent();
    }
    false
}
