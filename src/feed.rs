//! The daemon's single-threaded feed loop.
//!
//! One cycle every POLL_SECONDS: re-read the config, refresh herdr's pane
//! listing when it has gone stale, tail each claude pane's transcript, render
//! the new lines, then hand the messages to one worker thread per pane —
//! ordered within a pane (Telegram arrival order is the reading order),
//! parallel across panes (one slow lane never holds up the others, which a
//! single serial sender measurably did at 6–8 concurrent sessions).
//!
//! There is no signal handler and no shutdown path: SIGTERM kills the
//! process, the kernel releases the flock, and the in-memory cursors die
//! with it — a restart re-attaches at EOF by design. Waiting one sweep for
//! that handover is the accepted cost.

use crate::config::{self, Config};
use crate::herdr;
use crate::lock;
use crate::logging;
use crate::render::{self, Outbound, Target};
use crate::resolve;
use crate::tailer::Tails;
use crate::telegram;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const POLL_SECONDS: Duration = Duration::from_secs(5);
/// Nap while there is nothing configured to do: no topic, no relay, or an
/// empty `agent_details` means the detail stream has no subscribers.
const NAP_UNUSABLE: Duration = Duration::from_secs(30);
/// A listing older than this, still frozen, is logged once per window.
const STALE_LOG_SECONDS: f64 = 600.0;

struct Feed {
    tails: Tails,
    listing: Vec<resolve::Agent>,
    listing_at: Option<Instant>,
    stale_logged_at: Option<Instant>,
    home: PathBuf,
}

pub fn run_daemon(state: &Path) -> ExitCode {
    // keep the guard alive: dropping the File would release the flock
    let _guard = match lock::acquire(state) {
        Some(f) => f,
        None => {
            logging::log("another daemon already holds the lock", &[]);
            return ExitCode::FAILURE;
        }
    };
    logging::log(
        "daemon started",
        &[("pid", &std::process::id().to_string())],
    );

    let mut feed = Feed {
        tails: Tails::default(),
        listing: Vec::new(),
        listing_at: None,
        stale_logged_at: None,
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
    };
    loop {
        let started = Instant::now();
        let nap = feed.cycle();
        let left = nap.saturating_sub(started.elapsed());
        std::thread::sleep(left);
    }
}

impl Feed {
    /// One cycle; returns how long to sleep afterwards.
    fn cycle(&mut self) -> Duration {
        let cfg = match Config::load(&config::config_dir()) {
            Ok(c) => c,
            Err(e) => {
                // a broken config must not spin the loop hot
                logging::log("config error", &[("error", &e)]);
                return NAP_UNUSABLE;
            }
        };
        if !cfg.usable() {
            return NAP_UNUSABLE;
        }

        self.refresh_listing();

        let enabled: HashSet<String> = cfg.agent_details.iter().cloned().collect();
        let now = now_secs();
        let mut by_pane: HashMap<String, Vec<Outbound>> = HashMap::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for agent in &self.listing {
            if agent.agent != "claude" {
                continue; // claude-only: no other agent's transcripts are read
            }
            let Some(tf) =
                resolve::session_file(&self.home, agent.session.as_ref(), agent.cwd_path(), now)
            else {
                continue;
            };
            // two panes can resolve to one file (compaction keeps parent and
            // successor both alive): the first pane to reach it owns the batch
            if !seen.insert(tf.clone()) {
                continue;
            }
            let Some(chunk) = self.tails.drain(&tf) else {
                continue;
            };
            let target = Target::from_agent(agent, &self.home.to_string_lossy());
            let msgs = render::render_chunk(
                &chunk,
                &cfg,
                &target,
                &agent.display_title(),
                agent.cwd_path().unwrap_or(""),
                &enabled,
            );
            by_pane.entry(target.pane.clone()).or_default().extend(msgs);
        }
        if !by_pane.is_empty() {
            let chat = cfg.chat_id.clone();
            std::thread::scope(|s| {
                for msgs in by_pane.values() {
                    let cfg = &cfg;
                    let chat = &chat;
                    s.spawn(move || {
                        for m in msgs {
                            send_outbound(cfg, chat, m);
                        }
                    });
                }
            });
        }
        POLL_SECONDS
    }

    /// Refresh the pane listing when it has gone stale (≥ one poll cycle).
    /// A failed refresh keeps the frozen listing sweeping — herdr being down
    /// must not silence panes it already reported — but a listing frozen
    /// past STALE_LOG_SECONDS gets one log line per window, so panes created
    /// during the outage aren't silently missing.
    fn refresh_listing(&mut self) {
        let fresh = self.listing_at.is_some_and(|t| t.elapsed() < POLL_SECONDS);
        if fresh {
            return;
        }
        match herdr::agent_list() {
            Ok(listing) => {
                self.listing = listing;
                self.listing_at = Some(Instant::now());
                self.stale_logged_at = None;
            }
            Err(e) => {
                if self.listing.is_empty() {
                    logging::log("listing refresh failed", &[("error", &e)]);
                } else {
                    let frozen_for = self
                        .listing_at
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(0.0);
                    let unlogged = self
                        .stale_logged_at
                        .is_none_or(|t| t.elapsed().as_secs_f64() >= STALE_LOG_SECONDS);
                    if frozen_for >= STALE_LOG_SECONDS && unlogged {
                        logging::log(
                            "listing stale, sweeping frozen panes",
                            &[("frozen_s", &format!("{frozen_for:.0}")), ("error", &e)],
                        );
                        self.stale_logged_at = Some(Instant::now());
                    }
                }
            }
        }
    }
}

/// Send one message: rich first, plain on rejection — the caller's own
/// fallback is the one retry that is safe against double-posting.
fn send_outbound(cfg: &Config, chat: &str, m: &Outbound) {
    let foot_html = m.footer.replace('\n', "<br/>"); // raw newlines collapse inside an HTML block
    let rich = format!("{}\n\n<footer>{foot_html}</footer>", m.body);
    if telegram::send_rich(cfg, chat, &rich, Some(&m.target), "sweep").is_err() {
        let plain = format!("{}\n\n{}", m.body, m.footer);
        let _ = telegram::send_text(cfg, chat, &plain, Some(&m.target), "sweep");
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
