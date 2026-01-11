//! G-key HID event definitions and parsing

/// Events from the keyboard
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// G-key pressed (1-5 for G815)
    GKey(u8),
    /// G-key released
    GKeyRelease,
    /// Memory/profile key pressed (1-3)
    MKey(u8),
    /// Memory key released
    MKeyRelease,
    /// Memory Record key pressed
    MRKey,
    /// Memory Record key released
    MRKeyRelease,
}

/// Keyboard definition with HID patterns
pub struct KeyboardDef {
    pub name: &'static str,
    pub vendor_id: u16,
    pub product_id: u16,
    pub interface: u8,
    pub num_gkeys: u8,
    pub num_mkeys: u8,
}

/// G815 keyboard definition
pub const G815: KeyboardDef = KeyboardDef {
    name: "Logitech G815",
    vendor_id: 0x046d,
    product_id: 0xc33f,
    interface: 1,
    num_gkeys: 5,
    num_mkeys: 3,
};

/// Parse a 20-byte HID report into an Event
///
/// G815 report format:
/// - G-keys: [0x11, 0xff, 0x0a, 0x00, bitmask, ...]
/// - M-keys: [0x11, 0xff, 0x0b, 0x00, bitmask, ...]
/// - MR key: [0x11, 0xff, 0x0c, 0x00, bitmask, ...]
pub fn parse_report(data: &[u8]) -> Option<Event> {
    if data.len() < 5 {
        return None;
    }

    // Check for Logitech vendor prefix
    if data[0] != 0x11 || data[1] != 0xff {
        return None;
    }

    match data[2] {
        // G-keys
        0x0a => {
            let mask = data[4];
            match mask {
                0x01 => Some(Event::GKey(1)),
                0x02 => Some(Event::GKey(2)),
                0x04 => Some(Event::GKey(3)),
                0x08 => Some(Event::GKey(4)),
                0x10 => Some(Event::GKey(5)),
                0x00 => Some(Event::GKeyRelease),
                _ => None, // Multiple keys or unknown
            }
        }
        // M-keys (profile select)
        0x0b => {
            let mask = data[4];
            match mask {
                0x01 => Some(Event::MKey(1)),
                0x02 => Some(Event::MKey(2)),
                0x04 => Some(Event::MKey(3)),
                0x00 => Some(Event::MKeyRelease),
                _ => None,
            }
        }
        // MR key (record)
        0x0c => {
            let mask = data[4];
            match mask {
                0x01 => Some(Event::MRKey),
                0x00 => Some(Event::MRKeyRelease),
                _ => None,
            }
        }
        _ => None,
    }
}

/// LED control command for setting active profile indicator
/// Returns a 20-byte HID report (from g810-led project)
/// Command: [0x11, 0xff, 0x0b, 0x1c, mask]
/// where mask is 0x01=M1, 0x02=M2, 0x04=M3
pub fn led_command(profile: u8) -> [u8; 20] {
    let mut cmd = [0u8; 20];
    cmd[0] = 0x11;
    cmd[1] = 0xff;
    cmd[2] = 0x0b; // MN key command
    cmd[3] = 0x1c; // LED set subcommand (was incorrectly 0x1a)
    cmd[4] = 1 << (profile.saturating_sub(1)); // Bitmask for M1/M2/M3
    cmd
}

/// MR (Memory Record) key LED control command
/// Returns a 20-byte HID report
/// Command: [0x11, 0xff, 0x0c, 0x0c, value]
/// where value is 0x00=off, 0x01=on
pub fn mr_led_command(on: bool) -> [u8; 20] {
    let mut cmd = [0u8; 20];
    cmd[0] = 0x11;
    cmd[1] = 0xff;
    cmd[2] = 0x0c;
    cmd[3] = 0x0c;
    cmd[4] = if on { 0x01 } else { 0x00 };
    cmd
}

/// G-key LED color command
/// Sets the RGB color for a single G-key (1-5)
/// Command: [0x11, 0xff, 0x10, 0x6c, r, g, b, key_addr, 0xff, ...]
/// G-key addresses: G1=0xb4, G2=0xb5, G3=0xb6, G4=0xb7, G5=0xb8
pub fn gkey_led_command(gkey: u8, r: u8, g: u8, b: u8) -> [u8; 20] {
    let mut cmd = [0u8; 20];
    cmd[0] = 0x11;
    cmd[1] = 0xff;
    cmd[2] = 0x10;
    cmd[3] = 0x6c;
    cmd[4] = r;
    cmd[5] = g;
    cmd[6] = b;
    // G-key address = gkey + 0xb3 (so G1=0xb4, G2=0xb5, etc.)
    cmd[7] = gkey.saturating_add(0xb3);
    cmd[8] = 0xff; // Terminator
    cmd
}

/// Set all G-keys (1-5) to the same color in a single command
/// Command: [0x11, 0xff, 0x10, 0x6c, r, g, b, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xff, ...]
pub fn all_gkeys_led_command(r: u8, g: u8, b: u8) -> [u8; 20] {
    let mut cmd = [0u8; 20];
    cmd[0] = 0x11;
    cmd[1] = 0xff;
    cmd[2] = 0x10;
    cmd[3] = 0x6c;
    cmd[4] = r;
    cmd[5] = g;
    cmd[6] = b;
    // All 5 G-key addresses
    cmd[7] = 0xb4;  // G1
    cmd[8] = 0xb5;  // G2
    cmd[9] = 0xb6;  // G3
    cmd[10] = 0xb7; // G4
    cmd[11] = 0xb8; // G5
    cmd[12] = 0xff; // Terminator
    cmd
}

/// Commit LED changes
/// Must be sent after setting key colors for changes to take effect
/// Command: [0x11, 0xff, 0x10, 0x7f, ...]
pub fn led_commit_command() -> [u8; 20] {
    let mut cmd = [0u8; 20];
    cmd[0] = 0x11;
    cmd[1] = 0xff;
    cmd[2] = 0x10;
    cmd[3] = 0x7f;
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gkeys() {
        let g1 = [0x11, 0xff, 0x0a, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_report(&g1), Some(Event::GKey(1)));

        let g5 = [0x11, 0xff, 0x0a, 0x00, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_report(&g5), Some(Event::GKey(5)));

        let release = [0x11, 0xff, 0x0a, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_report(&release), Some(Event::GKeyRelease));
    }

    #[test]
    fn test_parse_mkeys() {
        let m1 = [0x11, 0xff, 0x0b, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_report(&m1), Some(Event::MKey(1)));

        let m3 = [0x11, 0xff, 0x0b, 0x00, 0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_report(&m3), Some(Event::MKey(3)));
    }

    #[test]
    fn test_parse_mr() {
        let mr = [0x11, 0xff, 0x0c, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_report(&mr), Some(Event::MRKey));
    }
}
