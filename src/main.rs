//! htb — herdr → Telegram, one-way and read-only.
//!
//! Tails every claude pane's session transcript and streams what it finds into
//! a Telegram forum topic through the herdr-relay worker. Nothing comes back:
//! Telegram is a pure output channel for this plugin.

mod config;
mod feed;
mod footer;
mod herdr;
mod lock;
mod logging;
mod render;
mod resolve;
mod spawn;
mod tailer;
mod telegram;

use std::process::ExitCode;

fn main() -> ExitCode {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let state = config::state_dir();

    match cmd.as_str() {
        "daemon" => feed::run_daemon(&state),
        "ensure" => spawn::ensure(&state),
        "stop" => lock::stop_cli(&state),
        "restart" => {
            let _ = lock::stop_cli(&state);
            spawn::ensure(&state)
        }
        "status" => {
            if lock::is_running(&state) {
                println!(
                    "htb daemon running pid={}",
                    lock::read_pid(&state).unwrap_or(0)
                );
            } else {
                println!("htb daemon not running");
            }
            ExitCode::SUCCESS
        }
        "test" => test_message(),
        _ => {
            eprintln!(
                "htb — stream herdr's claude panes into Telegram\n\n\
                 usage: htb <command>\n\n\
                 ensure    start the daemon if it isn't already running (the [[startup]] hook)\n\
                 daemon    run the feed loop in the foreground\n\
                 stop      stop the running daemon (refuses to lie: the flock is the verdict)\n\
                 restart   stop it and start a fresh one\n\
                 status    report whether the daemon is running\n\
                 test      send a test message through the relay"
            );
            ExitCode::FAILURE
        }
    }
}

fn test_message() -> ExitCode {
    let cfg = match config::Config::load(&config::config_dir()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if cfg.relay_url.is_empty() || cfg.relay_key.is_empty() {
        eprintln!("relay not configured — see config.example.toml");
        return ExitCode::FAILURE;
    }
    match telegram::send_text(
        &cfg,
        &cfg.chat_id,
        "htb test message from herdr 🔔",
        None,
        "test",
    ) {
        Ok(_) => {
            println!("test message sent");
            let _ = spawn::ensure(&config::state_dir());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("send failed: {e}");
            ExitCode::FAILURE
        }
    }
}
