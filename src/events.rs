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

/// G815 direct mode initialization sequence
/// Must be called before setting per-key colors for full keyboard control
/// Based on OpenRGB's InitializeDirect() method
pub fn direct_mode_init_commands() -> [[u8; 20]; 4] {
    let mut cmd1 = [0u8; 20];
    cmd1[0] = 0x11;
    cmd1[1] = 0xff;
    cmd1[2] = 0x08;
    cmd1[3] = 0x3e;

    let mut cmd2 = [0u8; 20];
    cmd2[0] = 0x11;
    cmd2[1] = 0xff;
    cmd2[2] = 0x08;
    cmd2[3] = 0x1e;

    let mut cmd3 = [0u8; 20];
    cmd3[0] = 0x11;
    cmd3[1] = 0xff;
    cmd3[2] = 0x0f;
    cmd3[3] = 0x1e;
    cmd3[0x10] = 0x01;

    let mut cmd4 = [0u8; 20];
    cmd4[0] = 0x11;
    cmd4[1] = 0xff;
    cmd4[2] = 0x0f;
    cmd4[3] = 0x1e;
    cmd4[4] = 0x01;

    [cmd1, cmd2, cmd3, cmd4]
}

/// All G815 key addresses for per-key LED control
/// Addresses need offset transformation per g810-led:
/// - Standard keys: HID code - 0x03
/// - Modifiers: HID code - 0x78
/// - G-keys: HID code + 0xB3
/// - Logo: HID code + 0xD1
const G815_ALL_KEYS: &[u8] = &[
    // Letters A-Z: HID 0x04-0x1D, offset -0x03 = 0x01-0x1A
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
    0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16,
    0x17, 0x18, 0x19, 0x1A,
    // Numbers 1-0: HID 0x1E-0x27, offset -0x03 = 0x1B-0x24
    0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24,
    // Enter through symbols: HID 0x28-0x38, offset -0x03 = 0x25-0x35
    0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x2B, 0x2C,
    0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35,
    // Caps + F1-F12: HID 0x39-0x45, offset -0x03 = 0x36-0x42
    0x36, 0x37, 0x38, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42,
    // Print/Scroll/Pause + Navigation + Arrows: HID 0x46-0x52, offset -0x03 = 0x43-0x4F
    0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E, 0x4F,
    // Numpad: HID 0x53-0x63, offset -0x03 = 0x50-0x60
    0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x5B, 0x5C,
    0x5D, 0x5E, 0x5F, 0x60,
    // ISO + Menu: HID 0x64-0x65, offset -0x03 = 0x61-0x62
    0x61, 0x62,
    // Modifiers left + right: HID 0xE0-0xE7, offset -0x78 = 0x68-0x6F
    0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x6F,
    // Media keys: HID 0x9B-0x9E, offset -0x03 = 0x98-0x9B
    0x98, 0x99, 0x9A, 0x9B,
    // Lighting indicator: HID 0x99, offset -0x03 = 0x96
    0x96,
    // Logo: HID 0x01 + 0xD1 = 0xD2
    0xD2,
    // G-keys: already at correct addresses 0xB4-0xB8
    0xB4, 0xB5, 0xB6, 0xB7, 0xB8,
];

/// Generate HID commands to set the entire keyboard to one color
/// Uses per-key format (0x1F frame type) for maximum compatibility
/// Returns a Vec of 20-byte commands that should be sent followed by led_commit_command()
pub fn full_keyboard_color_commands(r: u8, g: u8, b: u8) -> Vec<[u8; 20]> {
    G815_ALL_KEYS
        .iter()
        .map(|&key| {
            let mut cmd = [0u8; 20];
            cmd[0] = 0x11;
            cmd[1] = 0xff;
            cmd[2] = 0x10;
            cmd[3] = 0x1f; // Single-key frame type (LOGITECH_G815_ZONE_FRAME_TYPE_LITTLE)
            cmd[4] = key;
            cmd[5] = r;
            cmd[6] = g;
            cmd[7] = b;
            cmd
        })
        .collect()
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
