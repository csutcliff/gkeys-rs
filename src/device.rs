//! hidraw device discovery and I/O

use std::fs::{read_dir, read_to_string, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

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

    /// Read and parse a HID event (blocking)
    pub fn read_event(&mut self) -> Result<Option<Event>> {
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
