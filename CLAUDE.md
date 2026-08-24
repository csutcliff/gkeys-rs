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
| `main.rs` | Entry point, CLI arg dispatch, device scan loop, reconnection with exponential backoff |
| `device.rs` | hidraw device discovery (scans `/sys/class/hidraw` for vendor `046d` product `c33f` interface 1) |
| `events.rs` | HID report parsing (20-byte reports, prefix `11 ff`, byte 2 = event type) |
| `macros.rs` | Macro execution (run, shortcut, typeout, uinput, sequence, nothing) |
| `recording.rs` | MR key macro recording |
| `config.rs` | JSON config loading from `~/.config/gkeys-rs/config.json` |
| `led.rs` | M-key LED control and keyboard RGB (direct hidraw writes, command `11 ff 0b 1c <mask>`) |
| `uinput.rs` | Virtual input device for key injection |
| `control.rs` | Unix control socket + `--set-profile` client (see below) |

### Key Design Points

- **HID report format**: 20 bytes, prefix `11 ff`, byte 2 identifies event type (`0a`=G-key, `0b`=M-key, `0c`=MR)
- **LED control**: Direct write to hidraw device (command `11 ff 0b 1c <mask>`)
- **RGB color**: Applied to all keyboard zones on startup and reconnect via `rgb_color` config field
- **Profile switching**: M1/M2/M3 keys switch between `MEMORY_1`/`MEMORY_2`/`MEMORY_3` config sections
- **Auto-reconnection**: Survives KVM switches, monitor standby, USB resets
- **HID buffer drain**: After initialization and LED setup, the keyboard queues HID response reports in the hidraw buffer. These are drained before entering the event loop to prevent phantom G-key events (e.g., G1 firing on every boot). The MR LED has separate debounce logic (`is_mr_event_from_led`) for phantom events caused by LED writes during normal operation.
- **Control socket** (`control.rs`): a background thread (`ControlListener`) accepts connections on a Unix socket and forwards parsed requests to the main loop over an `mpsc` channel (`ControlRequest`), same pattern as the udev watcher's wake pipe. The socket path is resolved by the single shared `socket_path()` function, called by both `ControlListener::new` (daemon side) and `run_set_profile` (client side) so they cannot disagree about where it is: `$XDG_RUNTIME_DIR/gkeys-rs.sock` if that variable is set and non-empty, else `/run/user/<euid>/gkeys-rs.sock` if that directory exists, else `/tmp/gkeys-rs-<euid>.sock`. The middle step matters because `XDG_RUNTIME_DIR` is only set inside a logind session; a caller outside one (a udev rule via `systemd-run`, a cron job, a system unit, ssh with no session) would otherwise silently fall through to the `/tmp` path even though the daemon is listening under `/run/user`, which is exactly the bug this order fixes. The pure resolution logic lives in `resolve_socket_path`, kept separate from `socket_path`'s env/filesystem reads so it's unit testable without root. When the client can't connect, it also prints `describe_socket_resolution`'s explanation of what each step saw, so a path mismatch is diagnosable from the error message alone. Socket setup is best effort: a bind failure is logged as a warning and the daemon runs on without it. `ControlListener` also owns a self-pipe; the accept thread writes a byte after queuing a request, and `Device::open`'s `control_fd` parameter carries the read end into `poll_read` alongside the udev wake fd, so the inner loop's `read_event_blocking` (an indefinite `poll()`, unchanged) breaks out the instant a request arrives instead of waiting for the next keyboard event or a fixed poll interval - an idle daemon still blocks with zero periodic wakeups when the control socket goes unused. `poll_read` treats the udev wake fd firing as a disconnect (as before) but the control fd firing as "no HID data, go check the channel", so the two wake sources are distinguishable. The main loop drains the channel with `try_recv` at two points: once per outer-loop iteration between (re)connection attempts (no `LedController` available yet, so only the profile state updates), and once per inner-loop iteration while a device is connected; each point also drains the control-socket wake pipe (`ControlListener::drain`) so a burst of requests doesn't cause repeated spurious wakes. Applying a switch goes through `apply_profile`, the same helper `handle_event`'s `Event::MKey` arm calls, so the socket path and a physical M-key press can't drift apart. `gkeys-rs --set-profile <n>` is a thin client in the same binary: it connects to the socket, sends `profile <n>`, and prints the daemon's reply. Connections are handled one at a time on the accept thread, so `read_request_line` bounds each one with a 2s read timeout (`READ_TIMEOUT`) and a 256-byte cap on the request line (`MAX_LINE_BYTES`, enforced via `Read::take`); without both, a client that connects and never sends a newline would wedge the socket for every client after it.

### Configuration Notes

- **Full paths required** in macro commands: systemd user services don't have `~/.local/bin` in PATH
- **GUI apps need systemd-run**: Use `systemd-run --user --scope /usr/bin/alacritty` for apps that should survive service restarts
- **hass-cli**: Requires `HASS_SERVER` and `HASS_TOKEN` env vars from `~/.config/environment.d/envvars.conf`
