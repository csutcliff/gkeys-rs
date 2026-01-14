//! hidraw device discovery and I/O

use std::fs::{read_dir, read_to_string, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::events::{parse_report, Event, G815};

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

        let mut dev = Self { file, path };
        dev.initialize_gkeys()?;
        Ok(dev)
    }

    /// Initialize G-key software mode via HID++ 2.0
    /// This disables onboard profiles so G-keys only send vendor reports
    fn initialize_gkeys(&mut self) -> Result<()> {
        let mut cmd = [0u8; 20];
        let mut resp = [0u8; 20];
        cmd[0] = 0x11;
        cmd[1] = 0xff;

        // Query ONBOARD_PROFILES feature index (0x8100)
        cmd[2] = 0x00; // Root feature
        cmd[3] = 0x00; // getFeatureIndex function
        cmd[4] = 0x81; // Feature ID high byte
        cmd[5] = 0x00; // Feature ID low byte
        self.file.write_all(&cmd)?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = self.file.read(&mut resp);
        let onboard_idx = resp[4];

        if onboard_idx != 0 {
            log::debug!("ONBOARD_PROFILES feature at index 0x{:02x}", onboard_idx);
            // Set onboard mode to DISABLED (0x02) - disables onboard key bindings
            cmd[2] = onboard_idx;
            cmd[3] = 0x10; // setMode function
            cmd[4] = 0x02; // Disabled mode (no onboard profile)
            cmd[5] = 0x00;
            self.file.write_all(&cmd)?;
            std::thread::sleep(std::time::Duration::from_millis(20));
            log::info!("Onboard profiles disabled");
        } else {
            log::warn!("ONBOARD_PROFILES feature not found");
        }

        // Query GKEYS feature index (0x8010)
        cmd[2] = 0x00;
        cmd[3] = 0x00;
        cmd[4] = 0x80;
        cmd[5] = 0x10;
        self.file.write_all(&cmd)?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = self.file.read(&mut resp);
        let gkeys_idx = resp[4];

        if gkeys_idx != 0 {
            log::debug!("GKEYS feature at index 0x{:02x}", gkeys_idx);
            // Initialize GKEYS (getCount)
            cmd[2] = gkeys_idx;
            cmd[3] = 0x00;
            cmd[4] = 0x00;
            cmd[5] = 0x00;
            self.file.write_all(&cmd)?;
            std::thread::sleep(std::time::Duration::from_millis(20));
        } else {
            log::warn!("GKEYS feature not found");
        }

        log::info!("G-key software mode initialized");
        Ok(())
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
            Ok(n) if n > 0 => Ok(parse_report(&buf[..n])),
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
            Ok(n) if n > 0 => Ok(parse_report(&buf[..n])),
            Ok(_) => Ok(None),
            Err(e) => Err(e.into()),
        }
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
