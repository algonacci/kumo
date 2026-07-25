//! Background process management for `kumo start`/`stop`/`restart`. Rather than a real Unix
//! `fork()`+`setsid()` (which has no Windows equivalent), Kumo re-spawns itself as a detached child
//! process running `kumo run` — the existing foreground gateway — with its own stdout/stderr
//! redirected to a log file, then the parent (the `start` command) exits immediately. This is the
//! same "respawn detached" pattern used by tools like Docker Desktop's CLI, and it works
//! identically on Linux, macOS, and Windows without any OS-specific process APIs of our own; the
//! only OS-specific part left is checking whether a PID is still alive and asking it to stop,
//! which `sysinfo` already abstracts.

use std::{
    fs,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use sysinfo::{Pid, Signal, System};

use crate::storage;

const PID_FILE: &str = "kumo.pid";
const LOG_FILE: &str = "kumo.log";
/// How long `stop` waits for a graceful shutdown (Kumo's own Ctrl+C handler clears pending
/// approvals and asks the Telegram dispatcher to shut down) before escalating to a hard kill.
const STOP_GRACE_PERIOD: Duration = Duration::from_secs(10);

fn pid_file_path() -> Result<PathBuf> {
    Ok(storage::data_dir()?.join(PID_FILE))
}

fn log_file_path() -> Result<PathBuf> {
    Ok(storage::data_dir()?.join(LOG_FILE))
}

/// The PID of a running background instance, whether it was started by `kumo start` (tracked via
/// the PID file) or by a service manager (`kumo enable`'s systemd/launchd unit, which does not go
/// through `start` and so never writes one). Checks the PID file first — cheap, and the common
/// case for `kumo start`/`stop` — then falls back to scanning every process for one running this
/// same executable with `run` as an argument, which catches a service-managed instance too.
pub fn running_pid() -> Result<Option<u32>> {
    let mut system = System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let path = pid_file_path()?;
    if let Ok(contents) = fs::read_to_string(&path) {
        match contents.trim().parse::<u32>() {
            Ok(pid) if system.process(Pid::from_u32(pid)).is_some() => return Ok(Some(pid)),
            _ => {
                // Either unparsable or the process is gone (stale after a crash); either way the
                // file no longer reflects reality, so drop it rather than keep checking it again.
                let _ = fs::remove_file(&path);
            }
        }
    }

    Ok(find_running_instance(&system))
}

/// Scan every process for one running this same `kumo` executable — this is how a service-managed
/// instance (started directly by systemd/launchd, not by `kumo start`) is found, since nothing
/// wrote a PID file for it. Matching on the executable path alone (not also checking for `run`
/// among the process's arguments, which would be the more precise check) is a deliberate
/// concession to macOS: reading another process's argument list needs elevated privileges there,
/// so `Process::cmd()` silently comes back empty for anything not spawned by this same process,
/// even though `Process::exe()` still resolves. In practice this is safe regardless: every other
/// `kumo` subcommand (`status`, `doctor`, `stop`, `enable`, ...) runs to completion in well under a
/// second, so any other `kumo`-executable process still alive at the moment of this scan is, for
/// all practical purposes, the long-running gateway (`run`) — never a second `run` (`start` refuses
/// to launch one while another is already tracked, and running two would both try to long-poll the
/// same Telegram bot token and fail anyway).
fn find_running_instance(system: &System) -> Option<u32> {
    let this_exe = std::env::current_exe().ok()?;
    let this_exe = this_exe.canonicalize().unwrap_or(this_exe);
    let own_pid = std::process::id();

    system.processes().values().find_map(|process| {
        if process.pid().as_u32() == own_pid {
            return None;
        }
        let exe = process.exe()?;
        let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
        (exe == this_exe).then(|| process.pid().as_u32())
    })
}

/// `kumo start`: spawn a detached `kumo run` and return immediately, leaving it running in the
/// background. Refuses if an instance is already running rather than starting a second one, since
/// two Kumo processes would both try to long-poll the same Telegram bot token.
pub fn start() -> Result<()> {
    if let Some(pid) = running_pid()? {
        bail!("Kumo is already running (pid {pid}). Use `kumo stop` first, or `kumo restart`.");
    }

    let exe = std::env::current_exe().context("could not determine the kumo executable path")?;
    let log_path = log_file_path()?;
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open log file {}", log_path.display()))?;
    let log_file_for_stderr = log_file
        .try_clone()
        .context("failed to duplicate the log file handle")?;

    let mut command = std::process::Command::new(exe);
    command
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_for_stderr));
    detach(&mut command);

    let child = command
        .spawn()
        .context("failed to start kumo in the background")?;
    fs::write(pid_file_path()?, child.id().to_string()).context("failed to write the PID file")?;

    println!("Kumo started in the background (pid {}).", child.id());
    println!("Logs: {}", log_path.display());
    println!("Use `kumo status` to check on it, or `kumo stop` to stop it.");
    Ok(())
}

/// `kumo stop`: ask the background instance to shut down gracefully (same signal `Ctrl+C` sends
/// in the foreground), waiting up to `STOP_GRACE_PERIOD` before force-killing it.
pub fn stop() -> Result<()> {
    let Some(pid) = running_pid()? else {
        println!("Kumo is not running.");
        return Ok(());
    };

    let mut system = System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let Some(process) = system.process(Pid::from_u32(pid)) else {
        // Exited between running_pid()'s check and here; nothing left to do.
        let _ = fs::remove_file(pid_file_path()?);
        println!("Kumo is not running.");
        return Ok(());
    };

    let sent_graceful = process.kill_with(Signal::Term).unwrap_or(false);
    if !sent_graceful {
        // Platforms without a SIGTERM equivalent (e.g. Windows) fall back to a hard kill directly.
        process.kill();
    }

    let deadline = Instant::now() + STOP_GRACE_PERIOD;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        let mut system = System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        if system.process(Pid::from_u32(pid)).is_none() {
            let _ = fs::remove_file(pid_file_path()?);
            println!("Kumo stopped.");
            return Ok(());
        }
    }

    // Still alive after the grace period: escalate to a hard kill.
    let mut system = System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    if let Some(process) = system.process(Pid::from_u32(pid)) {
        process.kill();
    }
    let _ = fs::remove_file(pid_file_path()?);
    println!("Kumo did not stop gracefully within {STOP_GRACE_PERIOD:?}; it was force-killed.");
    Ok(())
}

/// Platform-specific step to fully detach the child so it survives the parent (this `start`
/// invocation) exiting and is not tied to the launching terminal.
#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // Start a new session so the child has no controlling terminal: closing the terminal that
    // ran `kumo start` (or that terminal's own shell exiting) sends no SIGHUP to it.
    unsafe {
        command.pre_exec(|| {
            libc_setsid();
            Ok(())
        });
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

#[cfg(unix)]
fn libc_setsid() {
    // SAFETY: setsid() has no preconditions beyond being called in a freshly forked child (which
    // `pre_exec` guarantees) and this process not already being a session leader (a just-forked
    // child never is).
    unsafe {
        setsid();
    }
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS: the child gets no console at all, so it is not tied to the parent's
    // console window and is unaffected by that console closing.
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_pid_returns_none_when_no_pid_file_exists() {
        // Uses the real data_dir(), but only ever reads — safe as long as no other test in this
        // process writes a PID file. Kept minimal since this crosses into environment-dependent
        // territory (the OS's own process table) that isn't worth mocking for one check.
        if pid_file_path().unwrap().exists() {
            return; // Some other kumo instance's PID file happens to exist; skip rather than flake.
        }
        assert!(running_pid().unwrap().is_none());
    }
}
