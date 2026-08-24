//! Control socket: lets an external program request state changes (e.g.
//! resetting to profile M1 when returning to this machine via a KVM
//! switch) without touching the keyboard's physical M keys.
//!
//! The wire protocol is deliberately minimal: one line in, one line out,
//! one command per connection.
//!
//!   profile <n>   ->   "ok"  |  "err <reason>"
//!
//! Setup is best effort. A user who never touches this feature must never
//! see the daemon fail to start because of it: if the socket can't be
//! created, `ControlListener::new` returns an error and the caller (in
//! main.rs) logs a warning and runs on without a control channel.

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

/// How long a client waits for the main loop to process its request before
/// giving up. Bounded so a wedged main loop can't hang a caller forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// A parsed control-socket command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    /// Switch to memory profile 1, 2 or 3, same as pressing the M key.
    SetProfile(u8),
}

/// One request handed from the socket thread to the main loop, carrying a
/// channel the main loop uses to deliver the reply back to the waiting
/// client connection.
pub struct ControlRequest {
    pub command: ControlCommand,
    reply: Sender<String>,
}

impl ControlRequest {
    /// Send the response line back to the client connection. Ignored if the
    /// client already gave up (reply timeout, or it disconnected).
    pub fn respond(&self, response: impl Into<String>) {
        let _ = self.reply.send(response.into());
    }
}

/// Parse one line of the wire protocol. Never panics on malformed input;
/// returns a short human-readable reason on failure so the caller can send
/// it straight back as `err <reason>`.
///
/// Kept as a plain match on the leading word so a second command can be
/// added later as another arm, without needing to redesign this function
/// or its caller.
pub fn parse_command(line: &str) -> Result<ControlCommand, String> {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("profile") => {
            let arg = parts.next().ok_or("missing profile number")?;
            if parts.next().is_some() {
                return Err("too many arguments".to_string());
            }
            let n: u8 = arg
                .parse()
                .map_err(|_| format!("invalid profile number '{}'", arg))?;
            if !(1..=3).contains(&n) {
                return Err(format!("profile number out of range (1-3): {}", n));
            }
            Ok(ControlCommand::SetProfile(n))
        }
        Some(other) => Err(format!("unknown command '{}'", other)),
        None => Err("empty command".to_string()),
    }
}

/// Where the control socket lives: `$XDG_RUNTIME_DIR/gkeys-rs.sock`, or
/// `/tmp/gkeys-rs-<uid>.sock` if `XDG_RUNTIME_DIR` isn't set.
pub fn socket_path() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("gkeys-rs.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/gkeys-rs-{}.sock", uid))
    }
}

/// A background thread accepting control-socket connections. Held for the
/// life of the daemon; dropping it (including via an early return during
/// shutdown) removes the socket file, same as a clean exit should.
///
/// Also owns the read end of a self-pipe: the accept thread writes a byte
/// to it after queuing a request, the same wake pattern `udev_watcher` uses
/// for disconnect events. `Device` polls this fd alongside the hidraw fd so
/// a blocking read breaks out the instant a control request arrives instead
/// of waiting on the next keyboard event.
pub struct ControlListener {
    path: PathBuf,
    wake_rx: OwnedFd,
    _thread: thread::JoinHandle<()>,
}

impl ControlListener {
    /// Bind the control socket and start accepting connections on a
    /// dedicated thread. Each parsed request is forwarded to `tx` and the
    /// wake pipe is nudged; the socket thread then waits (bounded by
    /// `REPLY_TIMEOUT`) for the main loop to send back a reply, and writes
    /// it to the client.
    pub fn new(tx: Sender<ControlRequest>) -> Result<Self> {
        let path = socket_path();

        // Remove a stale socket left behind by a previous run that didn't
        // exit cleanly (kill -9, crash). A leftover file here would
        // otherwise make bind() fail with "address already in use".
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("failed to bind control socket {}", path.display()))?;

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;

        let (wake_rx, wake_tx) = make_pipe()?;
        // Convert the write end into a File the accept thread can write to;
        // it now owns the fd (no double-close on drop).
        let wake_tx = unsafe { fs::File::from_raw_fd(wake_tx.into_raw_fd()) };

        let thread_path = path.clone();
        let thread = thread::Builder::new()
            .name("control-listener".into())
            .spawn(move || accept_loop(listener, tx, wake_tx, thread_path))
            .context("failed to spawn control-listener thread")?;

        Ok(Self { path, wake_rx, _thread: thread })
    }

    /// Path the socket is bound at, for logging.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Raw fd that becomes readable when a control request has been queued
    /// for the main loop. Passed into `Device` alongside the udev watcher's
    /// wake fd.
    pub fn wake_fd(&self) -> RawFd {
        self.wake_rx.as_raw_fd()
    }

    /// Drain any pending wake bytes so bursts of requests (or one that
    /// wasn't yet acted on) don't cause repeated spurious wakes once the
    /// corresponding channel messages have already been drained.
    pub fn drain(&self) {
        let fd = self.wake_rx.as_raw_fd();
        let mut buf = [0u8; 64];
        loop {
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
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

impl Drop for ControlListener {
    fn drop(&mut self) {
        // Best effort: a failed removal just leaves a stale file that the
        // next startup's remove-then-bind will clean up.
        let _ = fs::remove_file(&self.path);
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

/// Accept connections until the listener errors out unrecoverably. Not
/// explicitly joined or signalled to stop; it ends with the process on
/// shutdown, same as the udev watcher thread.
fn accept_loop(listener: UnixListener, tx: Sender<ControlRequest>, mut wake_tx: fs::File, path: PathBuf) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => handle_connection(stream, &tx, &mut wake_tx),
            Err(e) => log::warn!("control socket accept error: {}", e),
        }
    }
    log::debug!("control listener at {} stopped accepting", path.display());
}

/// Handle one client connection: read a single line, parse it, forward a
/// valid command to the main loop and wait for its reply, then write the
/// response line back. Malformed input never reaches the main loop at all,
/// so a client sending garbage can't affect the daemon.
fn handle_connection(stream: UnixStream, tx: &Sender<ControlRequest>, wake_tx: &mut fs::File) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            log::warn!("control socket: failed to clone stream: {}", e);
            return;
        }
    };
    let mut reader = BufReader::new(stream);

    let mut line = String::new();
    if let Err(e) = reader.read_line(&mut line) {
        log::warn!("control socket: read failed: {}", e);
        return;
    }
    if line.is_empty() {
        // Client connected and disconnected without sending anything.
        return;
    }

    let response = match parse_command(&line) {
        Ok(command) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            if tx.send(ControlRequest { command, reply: reply_tx }).is_err() {
                "err daemon shutting down".to_string()
            } else {
                // Wake the main loop's blocking read so it notices this
                // request immediately instead of waiting for the next
                // keyboard event or a disconnect.
                let _ = wake_tx.write_all(&[1u8]);
                match reply_rx.recv_timeout(REPLY_TIMEOUT) {
                    Ok(response) => response,
                    Err(_) => "err timed out waiting for daemon".to_string(),
                }
            }
        }
        Err(reason) => format!("err {}", reason),
    };

    if let Err(e) = writeln!(writer, "{}", response) {
        log::debug!("control socket: write failed: {}", e);
    }
}

/// Handle `--set-profile <n>` (or `-h`/`--help`) if present in `args`
/// (already stripped of argv[0]). Returns `Some(exit_code)` if client mode
/// ran and the process should exit immediately with that code, or `None` if
/// `args` doesn't request client mode, in which case the caller should
/// proceed to start the daemon exactly as it does today.
pub fn maybe_run_client(args: &[String]) -> Option<i32> {
    match args {
        [] => None,
        [flag, n] if flag.as_str() == "--set-profile" => Some(run_set_profile(n)),
        [flag] if flag.as_str() == "-h" || flag.as_str() == "--help" => {
            print_usage();
            Some(0)
        }
        _ => {
            eprintln!("gkeys-rs: unrecognised arguments: {}", args.join(" "));
            print_usage();
            Some(2)
        }
    }
}

fn print_usage() {
    eprintln!("usage: gkeys-rs                          run the macro daemon");
    eprintln!("       gkeys-rs --set-profile <1|2|3>     ask a running daemon to switch profile");
}

/// Connect to a running daemon's control socket and request a profile
/// switch. Prints the daemon's reply and returns a process exit code: 0 on
/// "ok", non-zero otherwise, including when no daemon is reachable.
fn run_set_profile(n: &str) -> i32 {
    let path = socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "gkeys-rs: daemon does not appear to be running (no control socket at {}): {}",
                path.display(),
                e
            );
            return 1;
        }
    };

    if let Err(e) = writeln!(stream, "profile {}", n) {
        eprintln!("gkeys-rs: failed to send request: {}", e);
        return 1;
    }

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    if let Err(e) = reader.read_line(&mut response) {
        eprintln!("gkeys-rs: failed to read reply: {}", e);
        return 1;
    }

    let response = response.trim();
    println!("{}", response);
    if response == "ok" {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_profile() {
        assert_eq!(parse_command("profile 1"), Ok(ControlCommand::SetProfile(1)));
        assert_eq!(parse_command("profile 2\n"), Ok(ControlCommand::SetProfile(2)));
        assert_eq!(parse_command("profile 3"), Ok(ControlCommand::SetProfile(3)));
    }

    #[test]
    fn rejects_out_of_range_profile() {
        assert!(parse_command("profile 9").is_err());
        assert!(parse_command("profile 0").is_err());
    }

    #[test]
    fn rejects_malformed_profile() {
        assert!(parse_command("profile").is_err());
        assert!(parse_command("profile abc").is_err());
        assert!(parse_command("profile 1 2").is_err());
    }

    #[test]
    fn rejects_unknown_command() {
        assert!(parse_command("frobnicate").is_err());
        assert!(parse_command("profile1").is_err());
    }

    #[test]
    fn rejects_empty_line() {
        assert!(parse_command("").is_err());
        assert!(parse_command("   ").is_err());
    }
}
