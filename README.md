# gkeys-rs

A Rust daemon for handling Logitech keyboard G-key macros on Linux via hidraw.

## Features

- **Coexists with OpenRGB**: Uses hidraw instead of libusb, so the kernel HID driver remains attached and OpenRGB can control keyboard lighting
- **Automatic reconnection**: Survives keyboard disconnection (KVM switches, monitor standby, USB reconnects) with exponential backoff retry
- **Profile switching**: M1/M2/M3 keys switch between profiles with LED feedback
- **Macro recording**: Record macros directly on the keyboard using the MR key
- **RGB color control**: Optional static color for the entire keyboard on daemon startup
- **Multiple macro types**: run, shortcut, typeout, uinput, sequence
- **Desktop notifications**: Optional notifications on profile switch and macro recording
- **Low resource usage**: Small Rust binary with minimal dependencies

## Compatibility

### Tested

| Keyboard | USB ID | G-Keys | Status |
|----------|--------|--------|--------|
| Logitech G815 | `046d:c33f` | 5 | **Tested & Working** |

### Probably Compatible (Untested)

These keyboards share similar HID interfaces and G-key layouts:

| Keyboard | USB ID | G-Keys | Notes |
|----------|--------|--------|-------|
| Logitech G915 | `046d:c33e` | 5 | Wireless version of G815 |
| Logitech G915 TKL | `046d:c343` | 5 | Tenkeyless wireless |

### May Require Modifications

These keyboards have different numbers of G-keys and may need code changes:

| Keyboard | USB ID | G-Keys | Notes |
|----------|--------|--------|-------|
| Logitech G910 Orion Spark | `046d:c32b` | 9 | Different G-key layout |
| Logitech G910 Orion Spectrum | `046d:c335` | 9 | Different G-key layout |

## Installation

### From AUR (Arch Linux)

```bash
yay -S gkeys-rs
```

### From Source

```bash
git clone https://github.com/csutcliff/gkeys-rs.git
cd gkeys-rs
cargo build --release
sudo cp target/release/gkeys-rs /usr/local/bin/
```

## Configuration

Create a configuration file at `~/.config/gkeys-rs/config.json`:

```json
{
  "notify": true,
  "profiles": {
    "MEMORY_1": {
      "MACRO_1": { "hotkey_type": "run", "do": "notify-send 'G1 pressed'" },
      "MACRO_2": { "hotkey_type": "shortcut", "do": "ctrl+shift+t" },
      "MACRO_3": { "hotkey_type": "typeout", "do": "Hello, World!" },
      "MACRO_4": { "hotkey_type": "nothing" },
      "MACRO_5": { "hotkey_type": "run", "do": "alacritty" }
    },
    "MEMORY_2": {
      "MACRO_1": { "hotkey_type": "run", "do": "firefox" }
    },
    "MEMORY_3": {
      "MACRO_1": { "hotkey_type": "run", "do": "code" }
    }
  }
}
```

### RGB Color (Optional)

Set a static color for the entire keyboard on daemon startup:

```json
{
  "notify": true,
  "rgb_color": { "r": 255, "g": 165, "b": 0 },
  "profiles": { ... }
}
```

- When set, applies the color to all 117 keyboard LEDs on startup
- G-keys are restored to this color after macro recording completes
- **Omit this field** to let external tools (like OpenRGB) manage keyboard lighting
- Values are 0-255 for each channel

### Macro Types

| Type | Description | Example |
|------|-------------|---------|
| `run` | Execute shell command | `"do": "notify-send 'Hello'"` |
| `shortcut` | Key combination | `"do": "ctrl+shift+t"` |
| `typeout` | Type text string | `"do": "my email@example.com"` |
| `uinput` | Raw key code | `"do": "28"` (Enter key) |
| `sequence` | Key sequence | `"do": "ctrl+a ctrl+c"` |
| `nothing` | Disable key | (no `do` field needed) |

## Usage

### Systemd Service (Recommended)

Create `~/.config/systemd/user/gkeys-rs.service`:

```ini
[Unit]
Description=G-Key Macro Daemon (Rust)
After=graphical-session.target

[Service]
Type=simple
ExecStart=/usr/local/bin/gkeys-rs
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=graphical-session.target
```

Enable and start:

```bash
systemctl --user enable --now gkeys-rs.service
```

### Manual

```bash
RUST_LOG=debug gkeys-rs
```

## Macro Recording

Record key sequences directly on the keyboard without editing the config file.

### How to Record

1. **Press MR** - MR LED turns on, all G-keys light up white
2. **Press a G-key** (G1-G5) - Selected G-key turns red, MR LED starts flashing, recording begins
3. **Type your key sequence** - All keystrokes are captured
4. **Press MR again** - Recording stops, macro is saved to config

### LED Feedback

| State | MR LED | G-Key LEDs |
|-------|--------|------------|
| Idle | Off | Normal/configured color |
| Awaiting G-key selection | On (solid) | All white |
| Recording | Flashing | Selected key red, others off |
| Save successful | Quick flash (4x) | Return to normal |

### Notes

- Recorded macros are saved as `sequence` type (e.g., `"do": "h, e, l, l, o"`)
- Macros are saved to the current profile (M1/M2/M3)
- A backup of the config is created before saving (`config.json.bak`)
- Press MR twice quickly (without selecting a G-key) to cancel
- Recording with no keys captured shows a cancellation notification

## Control Socket

The daemon listens on a Unix socket so an external program can request a
profile switch without touching the keyboard's physical M keys, for example
to reset to M1 from a script that runs when a KVM switch brings focus back
to this machine.

- **Path**: resolved the same way by the daemon and by `--set-profile`, in
  order:
  1. `$XDG_RUNTIME_DIR/gkeys-rs.sock`, if that variable is set and non-empty.
  2. `/run/user/<uid>/gkeys-rs.sock`, if that directory exists. A caller
     outside a logind session, such as a udev rule run via `systemd-run`, a
     cron job, a system unit, or a script run over ssh with no session,
     does not have `XDG_RUNTIME_DIR` set even though the directory (and the
     daemon's socket in it) already exists, so this fallback is what lets
     it still find the right socket.
  3. `/tmp/gkeys-rs-<uid>.sock`, if neither of the above is available.

  Mode `0600` (owner only) either way.
- **Setup is best effort**: if the socket can't be created, the daemon logs
  a warning and keeps running as normal. This feature is entirely optional.
- **Protocol**: one line in, one line out, one command per connection.

  | Request | Response |
  |---------|----------|
  | `profile <1\|2\|3>` | `ok`, or `err <reason>` |

Applying a profile switch over the socket does exactly what pressing the
corresponding M key does: it updates the active profile, the M-key LED, and
sends the desktop notification if `notify` is enabled.

### `--set-profile`

The same binary doubles as a client, so there is nothing extra to install:

```bash
gkeys-rs --set-profile 1
```

Prints the daemon's reply and exits `0` on `ok`, non-zero otherwise
(including when no daemon is running). This takes no other arguments and
does not start the daemon.

**Example** - reset to M1 whenever this machine regains focus from a KVM
switch, by calling it from whatever hook fires on that event:

```bash
gkeys-rs --set-profile 1
```

## Requirements

- User must be in `input` group (for hidraw access) or use appropriate udev rules
- `uinput` kernel module loaded

### udev Rules (Optional)

For non-root access, create `/etc/udev/rules.d/99-gkeys.rules`:

```udev
# Logitech G815
KERNEL=="hidraw*", ATTRS{idVendor}=="046d", ATTRS{idProduct}=="c33f", MODE="0660", GROUP="input"

# Logitech G915
KERNEL=="hidraw*", ATTRS{idVendor}=="046d", ATTRS{idProduct}=="c33e", MODE="0660", GROUP="input"
```

Then reload:

```bash
sudo udevadm control --reload-rules
sudo udevadm trigger
```

## Credits & Acknowledgments

This project was inspired by and references code from:

- **[g910-gkey-macro-support](https://github.com/JSubelj/g910-gkey-macro-support)** by JSubelj
  - Original Python implementation of G-key macro support
  - Configuration format compatibility
  - HID event parsing patterns

- **[g810-led](https://github.com/MatMoul/g810-led)** by MatMoul
  - LED control protocol reverse engineering
  - M-key LED command format
  - Key address offset calculations

- **[OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB)** by CalcProgrammer1
  - G815 per-key LED addresses
  - Direct mode initialization sequence

## Why gkeys-rs?

The original `g910-gkeys` uses libusb which detaches the kernel HID driver from USB interface 1. This prevents other tools like OpenRGB from detecting the keyboard for RGB control.

`gkeys-rs` uses hidraw directly, which allows the kernel HID driver to remain attached. This means:
- OpenRGB can control keyboard lighting
- G-key macros work simultaneously
- No conflicts between RGB and macro software

## License

GPL-3.0 - see [LICENSE](LICENSE)

## Contributing

Contributions welcome! If you have a different Logitech keyboard and can test compatibility, please open an issue with:
- Your keyboard model
- USB ID (`lsusb | grep Logitech`)
- Whether it works, and any modifications needed
