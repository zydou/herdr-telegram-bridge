//! bot.lock — the flock singleton.
//!
//! The lock lives in the kernel: the daemon holds it for its whole life and
//! death releases it (no signal handler exists on purpose — a daemon stuck
//! in long synchronous IO never runs one). Which is also why `stop` trusts
//! nothing but the lock itself: sending SIGTERM proves nothing, only
//! watching the lock actually release does. A daemon that fails to die within
//! the timeout is reported as a refusal, never as a fake success — "thought
//! it restarted but it didn't" was a real outage once.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn lock_path(state: &Path) -> PathBuf {
    state.join("bot.lock")
}

/// Open the lock file (creating it), read-write.
fn open_lock(state: &Path) -> Option<File> {
    fs::create_dir_all(state).ok()?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(state))
        .ok()
}

/// Acquire the singleton lock and stamp our pid. None when another daemon
/// already holds it — the second instance exits quietly.
pub fn acquire(state: &Path) -> Option<File> {
    let mut f = open_lock(state)?;
    f.try_lock().ok()?; // exclusive, non-blocking; released by close/death
    let _ = f.set_len(0);
    let _ = f.rewind();
    let _ = f.write_all(std::process::id().to_string().as_bytes());
    Some(f)
}

/// Whether a daemon holds the lock. The probe takes it and drops it — the
/// release rides on the fd's close.
pub fn is_running(state: &Path) -> bool {
    let Some(f) = open_lock(state) else {
        return false;
    };
    f.try_lock().is_err()
}

pub fn read_pid(state: &Path) -> Option<u32> {
    fs::read_to_string(lock_path(state))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// `htb stop`: send SIGTERM, then poll the lock as the only verdict.
pub fn stop_cli(state: &Path) -> ExitCode {
    if !is_running(state) || read_pid(state).is_none() {
        println!("htb daemon not running");
        return ExitCode::SUCCESS;
    }
    let pid = read_pid(state).expect("checked above");
    // /bin/kill via subprocess — std has no kill(2), and -15 (numeric) is the
    // most portable form across shells
    let _ = Command::new("/bin/kill")
        .arg("-15")
        .arg(pid.to_string())
        .status();

    let waited = Instant::now();
    while waited.elapsed() < STOP_TIMEOUT {
        if !is_running(state) {
            println!("htb daemon stopped (pid {pid})");
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    eprintln!(
        "htb daemon pid {pid} still holds the lock after {}s — refusing to claim success",
        STOP_TIMEOUT.as_secs()
    );
    ExitCode::FAILURE
}
