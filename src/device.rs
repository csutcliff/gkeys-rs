//! hidraw device discovery and I/O

use std::fs::{read_dir, read_to_string, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::events::{self, Event, G815};

pub struct Device {
    file: File,
    path: PathBuf,
}

impl Device {
    /// Open the G815 keyboard hidraw device
    pub fn open() -> Result<Self> {
        let path = find_hidraw_device()?;
        // Open with read+write for both receiving events and sending commands
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        log::info!("Opened device: {}", path.display());
        Ok(Self { file, path })
    }

    /// Get the device path
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Read and parse a HID event with timeout
    /// Returns Ok(None) if timeout expires without data
    pub fn read_event(&mut self) -> Result<Option<Event>> {
        self.read_event_timeout(Duration::from_millis(100))
    }

    /// Read and parse a HID event (blocking)
    pub fn read_event_blocking(&mut self) -> Result<Option<Event>> {
        let mut buf = [0u8; 20];
        match self.file.read(&mut buf) {
            Ok(n) if n > 0 => Ok(events::parse_report(&buf[..n])),
            Ok(_) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Read and parse a HID event with specified timeout
    pub fn read_event_timeout(&mut self, timeout: Duration) -> Result<Option<Event>> {
        let fd = self.file.as_raw_fd();
        let timeout_ms = timeout.as_millis() as i32;

        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };

        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };

        if ret < 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        if ret == 0 {
            // Timeout - no data available
            return Ok(None);
        }

        // Data available, read it
        let mut buf = [0u8; 20];
        match self.file.read(&mut buf) {
            Ok(n) if n > 0 => Ok(events::parse_report(&buf[..n])),
            Ok(_) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Send a HID report to the device (direct write, like g810-led)
    pub fn send_report(&mut self, data: &[u8; 20]) -> Result<()> {
        self.file
            .write_all(data)
            .context("Failed to write HID report")?;
        Ok(())
    }

    /// Set the profile LED (M1, M2, or M3)
    pub fn set_profile_led(&mut self, profile: u8) -> Result<()> {
        let cmd = events::led_command(profile);
        self.send_report(&cmd)
    }

    /// Set the MR (Memory Record) LED on or off
    pub fn set_mr_led(&mut self, on: bool) -> Result<()> {
        let cmd = events::mr_led_command(on);
        self.send_report(&cmd)
    }

    /// Set a single G-key LED color (1-5) without committing
    /// Call commit_leds() after setting all desired keys
    pub fn set_gkey_led_no_commit(&mut self, gkey: u8, r: u8, g: u8, b: u8) -> Result<()> {
        let cmd = events::gkey_led_command(gkey, r, g, b);
        self.send_report(&cmd)
    }

    /// Set all G-keys (1-5) to the same color
    pub fn set_all_gkeys_led(&mut self, r: u8, g: u8, b: u8) -> Result<()> {
        let cmd = events::all_gkeys_led_command(r, g, b);
        self.send_report(&cmd)?;
        self.commit_leds()
    }

    /// Set G-keys for recording: selected key red, others off
    pub fn set_gkeys_recording(&mut self, selected_gkey: u8) -> Result<()> {
        for g in 1..=5u8 {
            let (r, gv, b) = if g == selected_gkey {
                (255, 0, 0) // Red
            } else {
                (0, 0, 0) // Off
            };
            self.set_gkey_led_no_commit(g, r, gv, b)?;
        }
        self.commit_leds()
    }

    /// Turn off all G-key LEDs
    pub fn turn_off_gkeys(&mut self) -> Result<()> {
        self.set_all_gkeys_led(0, 0, 0)
    }

    /// Commit LED changes (required after setting colors)
    pub fn commit_leds(&mut self) -> Result<()> {
        let cmd = events::led_commit_command();
        self.send_report(&cmd)
    }
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
