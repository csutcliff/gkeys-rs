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
use std::io::{self, BufRead, BufReader, Read, Write};
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

/// How long the socket thread waits for a client to finish sending its
/// request line. A local client sending a handful of bytes needs a fraction
/// of this; the point is bounding a client that connects and then never
/// writes a newline (killed at the wrong moment, or just probing the
/// socket), which would otherwise wedge the accept thread, and with it
/// every other client, forever (connections are handled one at a time).
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum bytes read while looking for the request line's newline.
/// Real commands are a handful of bytes; this just bounds a client that
/// streams data without ever sending one.
const MAX_LINE_BYTES: u64 = 256;

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

/// Where the control socket lives. Both the daemon (`ControlListener::new`)
/// and the client (`run_set_profile`) call this, so they can't disagree
/// about the path: resolution order, checked in order:
///
/// 1. `$XDG_RUNTIME_DIR/gkeys-rs.sock`, if the variable is set and
///    non-empty.
/// 2. `/run/user/<uid>/gkeys-rs.sock`, if that directory exists. This is
///    the directory a logind session's `XDG_RUNTIME_DIR` normally points
///    at, but the variable itself is only set inside a logind session. A
///    caller outside one, such as a udev rule invoked via `systemd-run`, a
///    cron job, a system unit, or a script run over ssh with no session,
///    does not have it set even though the directory (and the daemon's
///    socket in it) already exists, so this fallback is what lets such a
///    caller still find the right socket.
/// 3. `/tmp/gkeys-rs-<uid>.sock`, if neither of the above is available.
///
/// Uses the effective uid throughout, for both the `/run/user` path and
/// the final fallback filename.
pub fn socket_path() -> PathBuf {
    let uid = unsafe { libc::geteuid() };
    resolve_socket_path(env::var_os("XDG_RUNTIME_DIR").as_deref(), run_user_dir_exists(uid), uid)
}

/// Whether `/run/user/<uid>` exists. Split out from `resolve_socket_path`
/// so the resolution order itself can be unit tested without touching the
/// filesystem or requiring a real `/run/user` entry.
fn run_user_dir_exists(uid: libc::uid_t) -> bool {
    Path::new(&format!("/run/user/{}", uid)).is_dir()
}

/// Pure resolution logic behind `socket_path`. See its doc comment for the
/// order; kept separate so tests can drive it with values that would
/// otherwise need root or a specific logind/session state to produce.
fn resolve_socket_path(xdg_runtime_dir: Option<&std::ffi::OsStr>, run_user_dir_exists: bool, uid: libc::uid_t) -> PathBuf {
    if let Some(dir) = xdg_runtime_dir {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("gkeys-rs.sock");
        }
    }
    if run_user_dir_exists {
        return PathBuf::from(format!("/run/user/{}", uid)).join("gkeys-rs.sock");
    }
    PathBuf::from(format!("/tmp/gkeys-rs-{}.sock", uid))
}

/// One line per resolution step, for the client's "daemon does not appear
/// to be running" message: someone hitting a path mismatch again should be
/// able to see why from the message alone, without needing to already know
/// this function's internals.
fn describe_socket_resolution(uid: libc::uid_t) -> String {
    let xdg = env::var_os("XDG_RUNTIME_DIR");
    let xdg_desc = match xdg.as_deref() {
        Some(dir) if !dir.is_empty() => format!("$XDG_RUNTIME_DIR is set to {}", PathBuf::from(dir).display()),
        Some(_) => "$XDG_RUNTIME_DIR is set but empty".to_string(),
        None => "$XDG_RUNTIME_DIR is not set".to_string(),
    };
    let run_user = format!("/run/user/{}", uid);
    let run_user_desc = if run_user_dir_exists(uid) {
        format!("{} exists", run_user)
    } else {
        format!("{} does not exist", run_user)
    };
    format!(
        "{}; {}; falls back to /tmp/gkeys-rs-{}.sock",
        xdg_desc, run_user_desc, uid
    )
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

/// Outcome of trying to read one request line from a client connection.
#[derive(Debug)]
enum LineRead {
    /// A full line was read (including its trailing newline, when present).
    Line(String),
    /// The client didn't finish sending a line within the timeout.
    TimedOut,
    /// The client sent more than the allowed number of bytes without a
    /// newline in them.
    TooLong,
    /// The client closed the connection without sending anything.
    Empty,
}

/// Read one request line from `stream`, bounded by `timeout` and `max_len`.
/// Split out from `handle_connection` so the timeout and length-cap logic
/// can be exercised directly against a real socket pair in tests, with
/// smaller limits than the daemon uses in practice.
fn read_request_line(stream: &UnixStream, timeout: Duration, max_len: u64) -> io::Result<LineRead> {
    stream.set_read_timeout(Some(timeout))?;

    let mut reader = BufReader::new(stream).take(max_len);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(_) => {}
        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
            return Ok(LineRead::TimedOut);
        }
        Err(e) => return Err(e),
    }

    if line.is_empty() {
        return Ok(LineRead::Empty);
    }
    // Take reports EOF once its allowance is used up, whether or not the
    // client actually stopped sending, so read_line can return a line-sized
    // chunk with no trailing newline for either reason. Treating that as
    // "too long" only when the allowance is fully spent (rather than on
    // "no newline" alone) keeps a genuinely short, newline-less line - the
    // client closing right after a partial write - from being misreported.
    if reader.limit() == 0 && !line.ends_with('\n') {
        return Ok(LineRead::TooLong);
    }
    Ok(LineRead::Line(line))
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

    let line = match read_request_line(&stream, READ_TIMEOUT, MAX_LINE_BYTES) {
        Ok(LineRead::Line(line)) => line,
        Ok(LineRead::Empty) => return, // client disconnected without sending anything
        Ok(LineRead::TimedOut) => {
            log::debug!(
                "control socket: client took longer than {:?} to send a line, dropping connection",
                READ_TIMEOUT
            );
            return;
        }
        Ok(LineRead::TooLong) => {
            log::debug!(
                "control socket: line exceeded {} bytes without a newline, dropping connection",
                MAX_LINE_BYTES
            );
            let _ = writeln!(writer, "err line too long");
            return;
        }
        Err(e) => {
            log::warn!("control socket: read failed: {}", e);
            return;
        }
    };

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
            let uid = unsafe { libc::geteuid() };
            eprintln!(
                "gkeys-rs: daemon does not appear to be running (no control socket at {}): {}",
                path.display(),
                e
            );
            eprintln!("gkeys-rs: resolution order checked: {}", describe_socket_resolution(uid));
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
    use std::ffi::OsStr;

    // resolve_socket_path is pure (no env or filesystem access of its own),
    // so the resolution order is tested directly with values a real
    // XDG_RUNTIME_DIR / /run/user state would produce, without needing root
    // or creating anything under /run/user.

    #[test]
    fn resolve_uses_xdg_runtime_dir_when_set() {
        let path = resolve_socket_path(Some(OsStr::new("/run/user/1000")), true, 1000);
        assert_eq!(path, PathBuf::from("/run/user/1000/gkeys-rs.sock"));
    }

    #[test]
    fn resolve_prefers_xdg_runtime_dir_over_run_user_fallback() {
        // An explicit XDG_RUNTIME_DIR should win even when /run/user/<uid>
        // also exists: it may legitimately point somewhere else entirely
        // (a container, a sandbox).
        let path = resolve_socket_path(Some(OsStr::new("/custom/runtime")), true, 1000);
        assert_eq!(path, PathBuf::from("/custom/runtime/gkeys-rs.sock"));
    }

    #[test]
    fn resolve_falls_back_to_run_user_dir_when_unset() {
        let path = resolve_socket_path(None, true, 1000);
        assert_eq!(path, PathBuf::from("/run/user/1000/gkeys-rs.sock"));
    }

    #[test]
    fn resolve_falls_back_to_run_user_dir_when_empty() {
        let path = resolve_socket_path(Some(OsStr::new("")), true, 1000);
        assert_eq!(path, PathBuf::from("/run/user/1000/gkeys-rs.sock"));
    }

    #[test]
    fn resolve_falls_back_to_tmp_when_nothing_else_available() {
        let path = resolve_socket_path(None, false, 1000);
        assert_eq!(path, PathBuf::from("/tmp/gkeys-rs-1000.sock"));
    }

    #[test]
    fn resolve_empty_xdg_and_no_run_user_dir_falls_to_tmp() {
        // Guards against a regression producing an empty path or something
        // like "/gkeys-rs.sock" when XDG_RUNTIME_DIR is set-but-empty and
        // /run/user/<uid> isn't there either.
        let path = resolve_socket_path(Some(OsStr::new("")), false, 1000);
        assert_eq!(path, PathBuf::from("/tmp/gkeys-rs-1000.sock"));
    }

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

    // read_request_line's timeout and length-cap logic depends on real
    // socket behaviour (SO_RCVTIMEO, EOF-on-close), so these run against a
    // genuine UnixStream::pair() rather than a hand-rolled mock, with much
    // smaller limits than the daemon uses so the suite stays fast.

    #[test]
    fn read_request_line_returns_a_full_line() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            b.write_all(b"profile 1\n").unwrap();
        });

        let result = read_request_line(&a, Duration::from_secs(1), 256).unwrap();
        writer.join().unwrap();

        match result {
            LineRead::Line(line) => assert_eq!(line, "profile 1\n"),
            other => panic!("expected Line, got {:?}", other),
        }
    }

    #[test]
    fn read_request_line_times_out_when_client_sends_nothing() {
        let (a, _b) = UnixStream::pair().unwrap();
        // _b is kept alive (not dropped) so `a` sees an open connection with
        // no data, not EOF, and genuinely has to wait out the timeout.
        let result = read_request_line(&a, Duration::from_millis(50), 256).unwrap();
        assert!(matches!(result, LineRead::TimedOut));
    }

    #[test]
    fn read_request_line_rejects_a_line_past_the_cap() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            b.write_all(&vec![b'x'; 300]).unwrap();
        });

        let result = read_request_line(&a, Duration::from_secs(1), 256).unwrap();
        writer.join().unwrap();

        assert!(matches!(result, LineRead::TooLong));
    }

    #[test]
    fn read_request_line_reports_empty_on_immediate_close() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(b);
        let result = read_request_line(&a, Duration::from_secs(1), 256).unwrap();
        assert!(matches!(result, LineRead::Empty));
    }
}
