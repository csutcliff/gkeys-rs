//! Fire-and-forget process spawning that does not leak zombies.
//!
//! `std::process::Child` explicitly does *not* wait on the child when it is
//! dropped, so a bare `Command::new(..).spawn()` whose handle is discarded
//! leaves a zombie in the process table for as long as this daemon lives. Every
//! macro of type `run` and every desktop notification went through that path,
//! so a long-running daemon accumulated one dead PID per key press.
//!
//! Each spawn therefore gets a short-lived thread whose only job is to `wait()`
//! the child and exit. Spawns here are driven by human key presses, so the
//! thread-per-spawn cost is irrelevant, and unlike `signal(SIGCHLD, SIG_IGN)`
//! this leaves the rest of the process free to wait on children of its own.

use std::process::Command;

/// Stack for a reaper thread. It only blocks in `wait()`, so the 8 MiB default
/// reservation is pure waste when one of these exists per key press.
const REAPER_STACK_BYTES: usize = 64 * 1024;

/// Spawn `cmd` detached, and reap it in the background when it exits.
pub fn spawn_reaped(cmd: &mut Command) -> std::io::Result<()> {
    let mut child = cmd.spawn()?;
    let pid = child.id();

    let reaper = std::thread::Builder::new()
        .name(format!("reap-{}", pid))
        .stack_size(REAPER_STACK_BYTES)
        .spawn(move || match child.wait() {
            Ok(status) => log::debug!("Child {} exited: {}", pid, status),
            Err(e) => log::warn!("Failed to wait on child {}: {}", pid, e),
        });

    if let Err(e) = reaper {
        // The child is already running and nothing else will collect it, so say
        // so rather than letting a silent zombie accumulate.
        log::warn!("Could not start reaper thread for child {}: {}", pid, e);
    }
    Ok(())
}
