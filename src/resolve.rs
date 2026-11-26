//! Transcript resolution: pane → the session jsonl worth tailing.
//!
//! The session id herdr reports can go stale — Claude Code rotates the file on
//! compaction/resume while herdr keeps pointing at the old one, and the old
//! file's mtime stays alive with housekeeping writes. Candidates are therefore
//! ranked by when an assistant last actually *spoke* in them, not by mtime.
//! When herdr has no ref at all (its SessionStart hook reports once, so a pane
//! re-created under a live process never receives one) transcripts are still
//! findable by cwd, which Claude Code encodes into its store directory name.

use crate::render::{parse_row, spoken_at};
use serde::Deserialize;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// A transcript whose last reply is older than this isn't the active one.
pub const FRESH_SECONDS: f64 = 300.0;
const TAIL_BYTES: u64 = 2_000_000;

/// One pane as herdr's `agent list` reports it.
#[derive(Clone, Debug, Deserialize)]
pub struct Agent {
    pub pane_id: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(rename = "agent_session", default)]
    pub session: Option<AgentSession>,
    // herdr reports both forms; a serde alias would make the two collide as
    // a duplicate field when the JSON carries both, so they stay two fields
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub terminal_title: Option<String>,
}

impl Agent {
    /// The pane's terminal title, as herdr reports it; may be empty.
    pub fn display_title(&self) -> String {
        self.terminal_title_stripped
            .as_deref()
            .or(self.terminal_title.as_deref())
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// The pane's working directory: the focused cwd when herdr reports one,
    /// the foreground cwd otherwise.
    pub fn cwd_path(&self) -> Option<&str> {
        self.cwd
            .as_deref()
            .filter(|c| !c.is_empty())
            .or(self.foreground_cwd.as_deref().filter(|c| !c.is_empty()))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentSession {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

/// Claude Code encodes a project path into its store directory name by
/// replacing every non-alphanumeric *character* (code point, not byte) with
/// '-', matching the JS/Python string replace the original naming used.
fn encode_store_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// File mtime in nanoseconds since the epoch — fine-grained enough to rank
/// candidates written within the same second.
fn mtime(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn jsonl_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return vec![];
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect()
}

/// When the newest assistant text in a transcript's tail was written.
///
/// Only top-level `type: assistant` rows with a parseable timestamp count:
/// api-request-blob repeats a turn's content under role "assistant" with no
/// timestamp, which would make a frozen reply look brand new. None when the
/// tail holds no timestamped assistant text — the candidate sits out the
/// scoring.
fn last_reply_at(path: &Path) -> Option<f64> {
    let mut f = fs::File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes);
    for line in tail.lines().rev() {
        if let Some(row) = parse_row(line)
            && let Some(t) = spoken_at(&row)
        {
            return Some(t);
        }
    }
    None
}

/// The freshest candidate when any of them spoke recently enough; else the
/// id-matched file. Two concurrent sessions in one store can still be
/// confused, but a frozen one is worse.
fn pick_freshest(pool: Vec<PathBuf>, matched: &Path, now: f64) -> PathBuf {
    let mut best = matched.to_path_buf();
    let mut best_at = 0.0;
    for p in pool {
        if let Some(t) = last_reply_at(&p)
            && t > best_at
        {
            best_at = t;
            best = p;
        }
    }
    if best_at > now - FRESH_SECONDS {
        best
    } else {
        matched.to_path_buf()
    }
}

/// Resolve the transcript path for one pane.
///
/// The id glob wins when it hits; the cwd-encoded store backs it up when the
/// ref is missing or dead. Scoring is scoped to the matched file's project
/// directory — the successor session lives in the same store.
pub fn session_file(
    home: &Path,
    session: Option<&AgentSession>,
    cwd: Option<&str>,
    now: f64,
) -> Option<PathBuf> {
    let projects = home.join(".claude").join("projects");

    let mut hits: Vec<PathBuf> = Vec::new();
    if let Some(s) = session
        && s.kind.as_deref() == Some("id")
        && let Some(v) = s.value.as_deref().filter(|v| !v.is_empty())
    {
        // glob: any project directory directly under projects/ may
        // hold <id>.jsonl
        hits = fs::read_dir(&projects)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join(format!("{v}.jsonl")))
            .filter(|p| p.is_file())
            .collect();
    }
    if hits.is_empty()
        && let Some(cwd) = cwd.filter(|c| !c.is_empty())
    {
        hits = jsonl_in(&projects.join(encode_store_dir(cwd)));
    }
    if hits.is_empty() {
        return None;
    }
    let matched = hits.into_iter().max_by_key(|p| mtime(p))?;
    // the successor session lives in the same store as the matched file
    let pool = jsonl_in(matched.parent()?);
    Some(pick_freshest(pool, &matched, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn now() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    fn write_jsonl(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    fn assistant_text(ts: &str, text: &str) -> String {
        serde_json::json!({
            "type": "assistant", "isSidechain": false, "timestamp": ts,
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        })
        .to_string()
    }

    #[test]
    fn store_dir_encoding_matches_claude_code() {
        assert_eq!(encode_store_dir("/home/u/My Proj_2"), "-home-u-My-Proj-2");
        // non-ASCII code points count as one character each, like the original
        assert_eq!(encode_store_dir("/home/u/中文"), "-home-u---");
    }

    #[test]
    fn id_glob_hit_returns_matched_even_when_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude/projects/-home-u-proj");
        let m = write_jsonl(
            &proj,
            "deadbeef.jsonl",
            &[assistant_text("2020-01-01T00:00:00Z", "old")],
        );
        let got = session_file(
            tmp.path(),
            Some(&AgentSession {
                kind: Some("id".into()),
                value: Some("deadbeef".into()),
            }),
            None,
            now(),
        )
        .unwrap();
        assert_eq!(got, m);
    }

    #[test]
    fn fresh_successor_outranks_stale_matched_id() {
        // herdr still points at the pre-compaction file; the successor speaks now
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude/projects/-home-u-proj");
        let _old = write_jsonl(
            &proj,
            "aaaa.jsonl",
            &[assistant_text("2020-01-01T00:00:00Z", "frozen")],
        );
        let ts = crate::logging::now_rfc3339_millis();
        let _new = write_jsonl(&proj, "bbbb.jsonl", &[assistant_text(&ts, "alive")]);
        let got = session_file(
            tmp.path(),
            Some(&AgentSession {
                kind: Some("id".into()),
                value: Some("aaaa".into()),
            }),
            None,
            now(),
        )
        .unwrap();
        assert!(got.ends_with("bbbb.jsonl"), "{got:?}");
    }

    #[test]
    fn cwd_store_backs_up_a_missing_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join(".claude/projects/-home-u-My-Proj");
        let p = write_jsonl(
            &store,
            "only.jsonl",
            &[assistant_text("2020-01-01T00:00:00Z", "hi")],
        );
        let got = session_file(tmp.path(), None, Some("/home/u/My Proj"), now()).unwrap();
        assert_eq!(got, p);
    }

    #[test]
    fn no_ref_no_store_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(session_file(tmp.path(), None, None, now()).is_none());
        assert!(session_file(tmp.path(), None, Some("/nowhere"), now()).is_none());
    }

    #[test]
    fn all_stale_falls_back_to_newest_mtime() {
        // nothing spoke within the window: the newest mtime wins, as herdr's ref implies
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join(".claude/projects/-home-u-proj");
        let a = write_jsonl(
            &store,
            "a.jsonl",
            &[assistant_text("2020-01-01T00:00:00Z", "a")],
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = write_jsonl(
            &store,
            "b.jsonl",
            &[assistant_text("2020-01-02T00:00:00Z", "b")],
        );
        let got = session_file(tmp.path(), None, Some("/home/u/proj"), now()).unwrap();
        assert_eq!(got, b);
        assert_ne!(got, a);
    }

    #[test]
    fn blob_rows_do_not_count_as_speech() {
        // a candidate whose tail is only api-request-blob (no timestamp, replayed
        // content) must not look alive
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join(".claude/projects/-home-u-proj");
        let blob = serde_json::json!({
            "type": "api-request-blob", "hash": "x",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "replayed"}]}
        })
        .to_string();
        let blobfile = write_jsonl(&store, "blob.jsonl", std::slice::from_ref(&blob));
        let ts = crate::logging::now_rfc3339_millis();
        let alive = write_jsonl(&store, "alive.jsonl", &[assistant_text(&ts, "real")]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        // blob file has the newest mtime — freshness scoring must ignore it
        fs::write(&blobfile, blob + "\n").unwrap();
        let got = session_file(tmp.path(), None, Some("/home/u/proj"), now()).unwrap();
        assert_eq!(got, alive);
    }

    #[test]
    fn broken_lines_are_skipped_in_the_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude/projects/-home-u-proj");
        let ts = crate::logging::now_rfc3339_millis();
        let m = write_jsonl(
            &proj,
            "id.jsonl",
            &["{broken".into(), assistant_text(&ts, "ok")],
        );
        let got = session_file(
            tmp.path(),
            Some(&AgentSession {
                kind: Some("id".into()),
                value: Some("id".into()),
            }),
            None,
            now(),
        )
        .unwrap();
        assert_eq!(got, m);
    }
}
