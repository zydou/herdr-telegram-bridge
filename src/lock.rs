//! bot.lock — the flock singleton.
//!
//! The lock lives in the kernel: the daemon holds it for its whole life and
//! death releases it (no signal handler exists on purpose — a daemon stuck
//! in long synchronous IO never runs one). Which is also why `stop` trusts
//! nothing but the lock itself: sending SIGTERM proves nothing, only
//! watching the lock actually release does. A daemon that fails to die within
//! the timeout is reported as a refusal, never as a fake success — a caller
//! that believes a restart happened when it did not is worse than one that
//! knows it failed.

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
///
/// The try_lock is retried a few times over ~150ms: `htb status` probes the
/// lock by taking and dropping it, and a fresh daemon whose single attempt
/// lands inside one of those microsecond-long probe holds would exit for no
/// reason — the classic lost race against an observer. A genuinely held lock
/// outlasts the retries, so real exclusion is unaffected.
pub fn acquire(state: &Path) -> Option<File> {
    for attempt in 0..4 {
        let mut f = open_lock(state)?;
        if f.try_lock().is_ok() {
            let _ = f.set_len(0);
            let _ = f.rewind();
            let _ = f.write_all(std::process::id().to_string().as_bytes());
            return Some(f);
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    None
}

/// Whether a daemon holds the lock. The probe takes it and drops it — the
/// release rides on the fd's close. The flock is the whole verdict: an
/// acquire window (lock taken, pid not yet stamped) is invisible here and
/// harmless, since the next acquirer's try_lock simply fails; a dead daemon
/// releases the lock with its last breath.
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
