//! `htb ensure` — start the daemon if it isn't already running.
//!
//! The daemon is spawned from this very executable with null/file stdio, so
//! nothing ties its lifetime to the process that started it: the hook exits
//! immediately, the daemon reparents to init and keeps running. That is
//! deliberate, and matters most during `herdr update --handoff`: herdr's
//! server is replaced while its panes (and their transcripts) keep living —
//! a daemon that died with the old server would lose its in-memory cursors
//! and re-attach at EOF, silently dropping everything written during the
//! swap. The startup hook is only a bootstrap: a fresh herdr runs `ensure`,
//! finds the lock held, and the feed simply never hiccuped.
//! Its stderr is appended to <state>/log.jsonl; the daemon logs JSONL to
//! stderr and knows nothing about the file.

use crate::lock;
use crate::logging;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

/// A file past this size is truncated before the daemon takes it over — log
/// history is disposable, and an unbounded append would eat the disk.
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

pub fn ensure(state: &Path) -> ExitCode {
    if lock::is_running(state) {
        println!(
            "htb daemon already running pid={}",
            lock::read_pid(state).unwrap_or(0)
        );
        return ExitCode::SUCCESS;
    }
    if let Err(e) = fs::create_dir_all(state) {
        eprintln!("cannot create state dir {}: {e}", state.display());
        return ExitCode::FAILURE;
    }
    let log = state.join("log.jsonl");
    if let Ok(meta) = fs::metadata(&log)
        && meta.len() > LOG_MAX_BYTES
    {
        // truncate to empty (set_len keeps the inode)
        if let Ok(f) = fs::OpenOptions::new().write(true).open(&log) {
            let _ = f.set_len(0);
        }
    }
    let stderr = match OpenOptions::new().append(true).create(true).open(&log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot open {}: {e}", log.display());
            return ExitCode::FAILURE;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot resolve own executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    let child = Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn();
    match child {
        Ok(c) => {
            println!("htb daemon started pid={}", c.id());
            logging::log("daemon spawned", &[("pid", &c.id().to_string())]);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cannot spawn daemon: {e}");
            ExitCode::FAILURE
        }
    }
}
