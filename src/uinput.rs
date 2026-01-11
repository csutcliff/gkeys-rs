//! Virtual keyboard using uinput for key emission

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::sync::LazyLock;

use anyhow::{Context, Result};

// Linux input event types and codes
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0x00;

// uinput ioctl commands
const UI_SET_EVBIT: libc::c_ulong = 0x40045564;
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565;
const UI_DEV_SETUP: libc::c_ulong = 0x405c5503;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;

const BUS_USB: u16 = 0x03;

/// input_event structure for writing events
#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

/// uinput_setup structure for device setup
#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// Key code definitions (subset of linux/input-event-codes.h)
pub mod keys {
    pub const KEY_ESC: u16 = 1;
    pub const KEY_1: u16 = 2;
    pub const KEY_2: u16 = 3;
    pub const KEY_3: u16 = 4;
    pub const KEY_4: u16 = 5;
    pub const KEY_5: u16 = 6;
    pub const KEY_6: u16 = 7;
    pub const KEY_7: u16 = 8;
    pub const KEY_8: u16 = 9;
    pub const KEY_9: u16 = 10;
    pub const KEY_0: u16 = 11;
    pub const KEY_MINUS: u16 = 12;
    pub const KEY_EQUAL: u16 = 13;
    pub const KEY_BACKSPACE: u16 = 14;
    pub const KEY_TAB: u16 = 15;
    pub const KEY_Q: u16 = 16;
    pub const KEY_W: u16 = 17;
    pub const KEY_E: u16 = 18;
    pub const KEY_R: u16 = 19;
    pub const KEY_T: u16 = 20;
    pub const KEY_Y: u16 = 21;
    pub const KEY_U: u16 = 22;
    pub const KEY_I: u16 = 23;
    pub const KEY_O: u16 = 24;
    pub const KEY_P: u16 = 25;
    pub const KEY_LEFTBRACE: u16 = 26;
    pub const KEY_RIGHTBRACE: u16 = 27;
    pub const KEY_ENTER: u16 = 28;
    pub const KEY_LEFTCTRL: u16 = 29;
    pub const KEY_A: u16 = 30;
    pub const KEY_S: u16 = 31;
    pub const KEY_D: u16 = 32;
    pub const KEY_F: u16 = 33;
    pub const KEY_G: u16 = 34;
    pub const KEY_H: u16 = 35;
    pub const KEY_J: u16 = 36;
    pub const KEY_K: u16 = 37;
    pub const KEY_L: u16 = 38;
    pub const KEY_SEMICOLON: u16 = 39;
    pub const KEY_APOSTROPHE: u16 = 40;
    pub const KEY_GRAVE: u16 = 41;
    pub const KEY_LEFTSHIFT: u16 = 42;
    pub const KEY_BACKSLASH: u16 = 43;
    pub const KEY_Z: u16 = 44;
    pub const KEY_X: u16 = 45;
    pub const KEY_C: u16 = 46;
    pub const KEY_V: u16 = 47;
    pub const KEY_B: u16 = 48;
    pub const KEY_N: u16 = 49;
    pub const KEY_M: u16 = 50;
    pub const KEY_COMMA: u16 = 51;
    pub const KEY_DOT: u16 = 52;
    pub const KEY_SLASH: u16 = 53;
    pub const KEY_RIGHTSHIFT: u16 = 54;
    pub const KEY_LEFTALT: u16 = 56;
    pub const KEY_SPACE: u16 = 57;
    pub const KEY_CAPSLOCK: u16 = 58;
    pub const KEY_F1: u16 = 59;
    pub const KEY_F2: u16 = 60;
    pub const KEY_F3: u16 = 61;
    pub const KEY_F4: u16 = 62;
    pub const KEY_F5: u16 = 63;
    pub const KEY_F6: u16 = 64;
    pub const KEY_F7: u16 = 65;
    pub const KEY_F8: u16 = 66;
    pub const KEY_F9: u16 = 67;
    pub const KEY_F10: u16 = 68;
    pub const KEY_F11: u16 = 87;
    pub const KEY_F12: u16 = 88;
    pub const KEY_F13: u16 = 183;
    pub const KEY_F14: u16 = 184;
    pub const KEY_F15: u16 = 185;
    pub const KEY_F16: u16 = 186;
    pub const KEY_F17: u16 = 187;
    pub const KEY_F18: u16 = 188;
    pub const KEY_F19: u16 = 189;
    pub const KEY_F20: u16 = 190;
    pub const KEY_RIGHTCTRL: u16 = 97;
    pub const KEY_RIGHTALT: u16 = 100;
    pub const KEY_HOME: u16 = 102;
    pub const KEY_UP: u16 = 103;
    pub const KEY_PAGEUP: u16 = 104;
    pub const KEY_LEFT: u16 = 105;
    pub const KEY_RIGHT: u16 = 106;
    pub const KEY_END: u16 = 107;
    pub const KEY_DOWN: u16 = 108;
    pub const KEY_PAGEDOWN: u16 = 109;
    pub const KEY_INSERT: u16 = 110;
    pub const KEY_DELETE: u16 = 111;
    pub const KEY_LEFTMETA: u16 = 125;
    pub const KEY_RIGHTMETA: u16 = 126;
    pub const KEY_MAX: u16 = 0x2ff;
}

/// Map of key names to key codes
static KEY_MAP: LazyLock<HashMap<&'static str, u16>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    // Modifiers
    m.insert("ctrl", keys::KEY_LEFTCTRL);
    m.insert("control", keys::KEY_LEFTCTRL);
    m.insert("shift", keys::KEY_LEFTSHIFT);
    m.insert("alt", keys::KEY_LEFTALT);
    m.insert("super", keys::KEY_LEFTMETA);
    m.insert("meta", keys::KEY_LEFTMETA);
    m.insert("win", keys::KEY_LEFTMETA);

    // Letters - must use QWERTY layout key codes, not alphabetical
    m.insert("a", keys::KEY_A);
    m.insert("b", keys::KEY_B);
    m.insert("c", keys::KEY_C);
    m.insert("d", keys::KEY_D);
    m.insert("e", keys::KEY_E);
    m.insert("f", keys::KEY_F);
    m.insert("g", keys::KEY_G);
    m.insert("h", keys::KEY_H);
    m.insert("i", keys::KEY_I);
    m.insert("j", keys::KEY_J);
    m.insert("k", keys::KEY_K);
    m.insert("l", keys::KEY_L);
    m.insert("m", keys::KEY_M);
    m.insert("n", keys::KEY_N);
    m.insert("o", keys::KEY_O);
    m.insert("p", keys::KEY_P);
    m.insert("q", keys::KEY_Q);
    m.insert("r", keys::KEY_R);
    m.insert("s", keys::KEY_S);
    m.insert("t", keys::KEY_T);
    m.insert("u", keys::KEY_U);
    m.insert("v", keys::KEY_V);
    m.insert("w", keys::KEY_W);
    m.insert("x", keys::KEY_X);
    m.insert("y", keys::KEY_Y);
    m.insert("z", keys::KEY_Z);

    // Numbers
    m.insert("0", keys::KEY_0);
    m.insert("1", keys::KEY_1);
    m.insert("2", keys::KEY_2);
    m.insert("3", keys::KEY_3);
    m.insert("4", keys::KEY_4);
    m.insert("5", keys::KEY_5);
    m.insert("6", keys::KEY_6);
    m.insert("7", keys::KEY_7);
    m.insert("8", keys::KEY_8);
    m.insert("9", keys::KEY_9);

    // Function keys
    m.insert("f1", keys::KEY_F1);
    m.insert("f2", keys::KEY_F2);
    m.insert("f3", keys::KEY_F3);
    m.insert("f4", keys::KEY_F4);
    m.insert("f5", keys::KEY_F5);
    m.insert("f6", keys::KEY_F6);
    m.insert("f7", keys::KEY_F7);
    m.insert("f8", keys::KEY_F8);
    m.insert("f9", keys::KEY_F9);
    m.insert("f10", keys::KEY_F10);
    m.insert("f11", keys::KEY_F11);
    m.insert("f12", keys::KEY_F12);
    m.insert("f13", keys::KEY_F13);
    m.insert("f14", keys::KEY_F14);
    m.insert("f15", keys::KEY_F15);
    m.insert("f16", keys::KEY_F16);
    m.insert("f17", keys::KEY_F17);
    m.insert("f18", keys::KEY_F18);
    m.insert("f19", keys::KEY_F19);
    m.insert("f20", keys::KEY_F20);

    // Special keys
    m.insert("esc", keys::KEY_ESC);
    m.insert("escape", keys::KEY_ESC);
    m.insert("tab", keys::KEY_TAB);
    m.insert("space", keys::KEY_SPACE);
    m.insert("enter", keys::KEY_ENTER);
    m.insert("return", keys::KEY_ENTER);
    m.insert("backspace", keys::KEY_BACKSPACE);
    m.insert("delete", keys::KEY_DELETE);
    m.insert("insert", keys::KEY_INSERT);
    m.insert("home", keys::KEY_HOME);
    m.insert("end", keys::KEY_END);
    m.insert("pageup", keys::KEY_PAGEUP);
    m.insert("pagedown", keys::KEY_PAGEDOWN);
    m.insert("up", keys::KEY_UP);
    m.insert("down", keys::KEY_DOWN);
    m.insert("left", keys::KEY_LEFT);
    m.insert("right", keys::KEY_RIGHT);
    m.insert("capslock", keys::KEY_CAPSLOCK);

    // Punctuation
    m.insert("minus", keys::KEY_MINUS);
    m.insert("equal", keys::KEY_EQUAL);
    m.insert("leftbrace", keys::KEY_LEFTBRACE);
    m.insert("rightbrace", keys::KEY_RIGHTBRACE);
    m.insert("semicolon", keys::KEY_SEMICOLON);
    m.insert("apostrophe", keys::KEY_APOSTROPHE);
    m.insert("grave", keys::KEY_GRAVE);
    m.insert("backslash", keys::KEY_BACKSLASH);
    m.insert("comma", keys::KEY_COMMA);
    m.insert("dot", keys::KEY_DOT);
    m.insert("slash", keys::KEY_SLASH);

    m
});

/// Character to key mapping for typeout
static CHAR_MAP: LazyLock<HashMap<char, (u16, bool)>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    // Lowercase letters (no shift)
    for (i, c) in ('a'..='z').enumerate() {
        m.insert(c, (keys::KEY_A + i as u16, false));
    }
    // Uppercase letters (with shift)
    for (i, c) in ('A'..='Z').enumerate() {
        m.insert(c, (keys::KEY_A + i as u16, true));
    }
    // Numbers
    m.insert('0', (keys::KEY_0, false));
    m.insert('1', (keys::KEY_1, false));
    m.insert('2', (keys::KEY_2, false));
    m.insert('3', (keys::KEY_3, false));
    m.insert('4', (keys::KEY_4, false));
    m.insert('5', (keys::KEY_5, false));
    m.insert('6', (keys::KEY_6, false));
    m.insert('7', (keys::KEY_7, false));
    m.insert('8', (keys::KEY_8, false));
    m.insert('9', (keys::KEY_9, false));
    // Shifted number row
    m.insert('!', (keys::KEY_1, true));
    m.insert('@', (keys::KEY_2, true));
    m.insert('#', (keys::KEY_3, true));
    m.insert('$', (keys::KEY_4, true));
    m.insert('%', (keys::KEY_5, true));
    m.insert('^', (keys::KEY_6, true));
    m.insert('&', (keys::KEY_7, true));
    m.insert('*', (keys::KEY_8, true));
    m.insert('(', (keys::KEY_9, true));
    m.insert(')', (keys::KEY_0, true));
    // Special characters
    m.insert(' ', (keys::KEY_SPACE, false));
    m.insert('\n', (keys::KEY_ENTER, false));
    m.insert('\t', (keys::KEY_TAB, false));
    m.insert('-', (keys::KEY_MINUS, false));
    m.insert('_', (keys::KEY_MINUS, true));
    m.insert('=', (keys::KEY_EQUAL, false));
    m.insert('+', (keys::KEY_EQUAL, true));
    m.insert('[', (keys::KEY_LEFTBRACE, false));
    m.insert('{', (keys::KEY_LEFTBRACE, true));
    m.insert(']', (keys::KEY_RIGHTBRACE, false));
    m.insert('}', (keys::KEY_RIGHTBRACE, true));
    m.insert(';', (keys::KEY_SEMICOLON, false));
    m.insert(':', (keys::KEY_SEMICOLON, true));
    m.insert('\'', (keys::KEY_APOSTROPHE, false));
    m.insert('"', (keys::KEY_APOSTROPHE, true));
    m.insert('`', (keys::KEY_GRAVE, false));
    m.insert('~', (keys::KEY_GRAVE, true));
    m.insert('\\', (keys::KEY_BACKSLASH, false));
    m.insert('|', (keys::KEY_BACKSLASH, true));
    m.insert(',', (keys::KEY_COMMA, false));
    m.insert('<', (keys::KEY_COMMA, true));
    m.insert('.', (keys::KEY_DOT, false));
    m.insert('>', (keys::KEY_DOT, true));
    m.insert('/', (keys::KEY_SLASH, false));
    m.insert('?', (keys::KEY_SLASH, true));
    m
});

pub struct VirtualKeyboard {
    file: File,
}

impl VirtualKeyboard {
    /// Create a new virtual keyboard device
    pub fn new() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("Failed to open /dev/uinput")?;

        let fd = file.as_raw_fd();

        unsafe {
            // Enable key events
            if libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int) < 0 {
                anyhow::bail!("Failed to set EV_KEY");
            }

            // Enable all key codes we might use
            for key in 1..=keys::KEY_MAX {
                libc::ioctl(fd, UI_SET_KEYBIT, key as libc::c_int);
            }

            // Setup device info
            let mut setup: UinputSetup = mem::zeroed();
            setup.id.bustype = BUS_USB;
            setup.id.vendor = 0x1234;
            setup.id.product = 0x5678;
            setup.id.version = 1;
            let name = b"gkeys-rs virtual keyboard";
            setup.name[..name.len()].copy_from_slice(name);

            if libc::ioctl(fd, UI_DEV_SETUP, &setup) < 0 {
                anyhow::bail!("Failed to setup uinput device");
            }

            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                anyhow::bail!("Failed to create uinput device");
            }
        }

        // Give udev time to create the device node
        std::thread::sleep(std::time::Duration::from_millis(100));

        Ok(Self { file })
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) -> Result<()> {
        let event = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };

        let bytes = unsafe {
            std::slice::from_raw_parts(
                &event as *const InputEvent as *const u8,
                mem::size_of::<InputEvent>(),
            )
        };

        self.file.write_all(bytes)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    /// Press a key (key down)
    pub fn press(&mut self, key: u16) -> Result<()> {
        self.emit(EV_KEY, key, 1)?;
        self.sync()
    }

    /// Release a key (key up)
    pub fn release(&mut self, key: u16) -> Result<()> {
        self.emit(EV_KEY, key, 0)?;
        self.sync()
    }

    /// Click a key (press and release)
    pub fn click(&mut self, key: u16) -> Result<()> {
        self.press(key)?;
        self.release(key)
    }

    /// Parse a key name to a key code
    pub fn parse_key(name: &str) -> Option<u16> {
        let name = name.trim().to_lowercase();
        // Handle KEY_XXX format
        let name = name.strip_prefix("key_").unwrap_or(&name);
        KEY_MAP.get(name).copied()
    }

    /// Type a string character by character
    pub fn typeout(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            if let Some(&(key, shift)) = CHAR_MAP.get(&c) {
                if shift {
                    self.press(keys::KEY_LEFTSHIFT)?;
                }
                self.click(key)?;
                if shift {
                    self.release(keys::KEY_LEFTSHIFT)?;
                }
                // Small delay between characters
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        Ok(())
    }

    /// Execute a keyboard shortcut like "ctrl+shift+t"
    pub fn shortcut(&mut self, shortcut: &str) -> Result<()> {
        let parts: Vec<&str> = shortcut.split('+').map(|s| s.trim()).collect();
        let mut pressed = Vec::new();

        // Press all keys
        for part in &parts {
            if let Some(key) = Self::parse_key(part) {
                self.press(key)?;
                pressed.push(key);
            } else {
                log::warn!("Unknown key in shortcut: {}", part);
            }
        }

        // Release in reverse order
        for key in pressed.into_iter().rev() {
            self.release(key)?;
        }

        Ok(())
    }

    /// Execute a sequence of shortcuts like "ctrl+a, ctrl+c"
    pub fn sequence(&mut self, seq: &str) -> Result<()> {
        for part in seq.split(',') {
            let part = part.trim();
            if !part.is_empty() {
                self.shortcut(part)?;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        Ok(())
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}
