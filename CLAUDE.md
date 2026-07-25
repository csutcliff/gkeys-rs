# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build --release

# Deploy to system path (service runs /usr/bin/gkeys-rs)
systemctl --user stop gkeys-rs.service
sudo cp target/release/gkeys-rs /usr/bin/gkeys-rs
systemctl --user start gkeys-rs.service

# Test manually with debug logging (without service)
systemctl --user stop gkeys-rs.service
RUST_LOG=debug /usr/bin/gkeys-rs

# Service management
systemctl --user status gkeys-rs.service
systemctl --user restart gkeys-rs.service
journalctl --user -u gkeys-rs.service -f
```

## Release Process

1. Bump version in `Cargo.toml`
2. Run `cargo update --workspace` to update `Cargo.lock` (the AUR package uses `--locked` and will fail if Cargo.lock is stale)
3. Commit and push

The AUR package repo is at `~/claude/gkeys-rs-aur/` (separate git, `ssh://aur@aur.archlinux.org/gkeys-rs-git.git`). It contains only `PKGBUILD` and `.SRCINFO`.

## Architecture

Rust 2024 edition, MSRV 1.85. Single-binary daemon using hidraw directly (not libusb - libusb detaches the kernel HID driver, breaking keyboard detection for other software).

### Source Layout

| File | Purpose |
|------|---------|
| `main.rs` | Entry point, device scan loop, reconnection with exponential backoff |
| `device.rs` | hidraw device discovery (scans `/sys/class/hidraw` for vendor `046d` product `c33f` interface 1) |
| `events.rs` | HID report parsing (20-byte reports, prefix `11 ff`, byte 2 = event type) |
| `macros.rs` | Macro execution (run, shortcut, typeout, uinput, sequence, nothing) |
| `recording.rs` | MR key macro recording |
| `config.rs` | JSON config loading from `~/.config/gkeys-rs/config.json` |
| `led.rs` | M-key LED control and keyboard RGB (direct hidraw writes, command `11 ff 0b 1c <mask>`) |
| `uinput.rs` | Virtual input device for key injection |

### Key Design Points

- **HID report format**: 20 bytes, prefix `11 ff`, byte 2 identifies event type (`0a`=G-key, `0b`=M-key, `0c`=MR)
- **LED control**: Direct write to hidraw device (command `11 ff 0b 1c <mask>`)
- **RGB color**: Applied to all keyboard zones on startup and reconnect via `rgb_color` config field
- **Profile switching**: M1/M2/M3 keys switch between `MEMORY_1`/`MEMORY_2`/`MEMORY_3` config sections
- **Auto-reconnection**: Survives KVM switches, monitor standby, USB resets
- **HID buffer drain**: After initialization and LED setup, the keyboard queues HID response reports in the hidraw buffer. These are drained before entering the event loop to prevent phantom G-key events (e.g., G1 firing on every boot). The MR LED has separate debounce logic (`is_mr_event_from_led`) for phantom events caused by LED writes during normal operation.

### Configuration Notes

- **Full paths required** in macro commands: systemd user services don't have `~/.local/bin` in PATH
- **GUI apps need systemd-run**: Use `systemd-run --user --scope /usr/bin/alacritty` for apps that should survive service restarts
- **hass-cli**: Requires `HASS_SERVER` and `HASS_TOKEN` env vars from `~/.config/environment.d/envvars.conf`
