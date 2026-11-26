//! Transcript rows → outbound messages.
//!
//! The session jsonl's row shapes are modeled with serde so the filtering
//! rules are structural: only a top-level `"type": "assistant"` row
//! deserializes to `Row::Assistant` — Claude Code's "api-request-blob"
//! bookkeeping (which repeats a turn verbatim under role "assistant" with no
//! timestamp) lands in `Row::Ignored` by construction, and a `tool_result`
//! content block never becomes a `Block` at all. Any line that fails to
//! deserialize is skipped, never fatal.
//!
//! Each thinking and each text block becomes its own message; adjacent
//! tool_use blocks merge into one until the join would outgrow a rich message.
//! Oversized bodies are split, never silently clipped — a thinking/text block
//! is clipped at the detail cap, a tool_use's arguments ride in full.

use crate::config::Config;
use crate::footer;
use crate::resolve::Agent;
use serde::Deserialize;
use std::collections::HashSet;

/// One rich message fits 32768 UTF-8 chars; stay under and leave headroom.
pub const RICH_CHARS: usize = 32000;
/// The per-block detail cap — generous enough for the longest thinking blocks
/// Claude Code actually writes, while leaving room for the footer.
pub const DETAIL_BLOCK_CHARS: usize = RICH_CHARS - 500;
/// Bundles are joined under this; splitting happens with the same slack.
const JOIN_CHARS: usize = RICH_CHARS - 300;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    pub fn get(&self, field: &str) -> Option<u64> {
        match field {
            "input_tokens" => self.input_tokens,
            "cache_read_input_tokens" => self.cache_read_input_tokens,
            "output_tokens" => self.output_tokens,
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Row {
    /// A top-level `"type": "assistant"` line.
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(rename = "isSidechain", default)]
        sidechain: bool,
        timestamp: Option<String>,
        message: Option<AssistantMsg>,
    },
    /// api-request-blob, user rows and every bookkeeping type — by construction.
    #[serde(other)]
    Ignored,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct AssistantMsg {
    role: Option<String>,
    content: Option<serde_json::Value>,
    usage: Option<Usage>,
}

/// One content block: thinking, text, tool_use — anything else is `Ignored`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type")]
enum Block {
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        name: String,
        input: serde_json::Value,
    },
    #[serde(other)]
    Ignored,
}

/// One finished message, ready to send.
#[derive(Clone, Debug, PartialEq)]
pub struct Outbound {
    /// The message body (markdown); the caller appends the footer when sending.
    pub body: String,
    /// The rendered footer, reused verbatim for the plain-text fallback.
    pub footer: String,
    /// Which agent this is about — log fields, and the pane key for the
    /// per-pane ordered send lanes.
    pub target: Target,
    /// The block category, for the log's source label.
    pub kind: Kind,
}

/// Where a message lives: this device, its folder, the pane it came from.
/// Built from a herdr agent dict; `folder` is the ~-shortened cwd.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Target {
    pub pane: String,
    pub agent: String,
    pub folder: String,
}

impl Target {
    pub fn from_agent(a: &Agent, home: &str) -> Target {
        let folder = a
            .cwd_path()
            .map(|cwd| shorten_home(cwd, home))
            .unwrap_or_default();
        Target {
            pane: a.pane_id.clone(),
            agent: a.agent.clone(),
            folder,
        }
    }
}

fn shorten_home(path: &str, home: &str) -> String {
    if !home.is_empty()
        && let Some(rest) = path.strip_prefix(home)
    {
        return format!("~{rest}");
    }
    path.to_string()
}

/// Parse one jsonl line; None on unparseable input (skipped, never fatal).
pub fn parse_row(line: &str) -> Option<Row> {
    serde_json::from_str(line).ok()
}

/// The timestamp of the newest assistant text in a parsed row, if it speaks.
/// Freshness scoring in resolve.rs consumes this.
pub fn spoken_at(row: &Row) -> Option<f64> {
    match row {
        Row::Assistant {
            timestamp, message, ..
        } => {
            let ts = timestamp
                .as_deref()
                .and_then(crate::logging::parse_rfc3339_secs);
            match message {
                Some(m) if m.role.as_deref() == Some("assistant") && m.has_text() => ts,
                _ => None,
            }
        }
        Row::Ignored => None,
    }
}

impl AssistantMsg {
    fn has_text(&self) -> bool {
        match &self.content {
            Some(serde_json::Value::Array(items)) => items.iter().any(|v| {
                serde_json::from_value::<Block>(v.clone())
                    .map(|b| matches!(b, Block::Text { ref text } if !text.trim().is_empty()))
                    .unwrap_or(false)
            }),
            _ => false,
        }
    }
}

/// Which outbound stream a message belongs to; also the footer emoji and the
/// log label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Thinking,
    Text,
    ToolUse,
}

impl Kind {
    pub fn emoji(self) -> &'static str {
        match self {
            Kind::Thinking => "💭",
            Kind::Text => "⏳",
            Kind::ToolUse => "🔧",
        }
    }
}

/// Render one assistant row into (kind, body, usage) sections. Empty when the
/// row isn't a main-thread assistant turn or nothing passes the category
/// whitelist.
fn sections(row: &Row, enabled: &HashSet<String>) -> Vec<(Kind, String, Usage)> {
    let Row::Assistant {
        sidechain, message, ..
    } = row
    else {
        return vec![];
    };
    if *sidechain {
        return vec![];
    }
    let Some(msg) = message else { return vec![] };
    if msg.role.as_deref() != Some("assistant") {
        return vec![];
    }
    let Some(serde_json::Value::Array(items)) = &msg.content else {
        return vec![]; // a bare string here would iterate characters and silently drop
    };
    let usage = msg.usage.clone().unwrap_or_default();

    let mut out = Vec::new();
    for item in items {
        let Ok(block) = serde_json::from_value::<Block>(item.clone()) else {
            continue;
        };
        match block {
            Block::Thinking { thinking } => {
                let t = thinking.trim();
                if !t.is_empty() && enabled.contains("thinking") {
                    out.push((Kind::Thinking, clip(t, DETAIL_BLOCK_CHARS), usage.clone()));
                }
            }
            Block::Text { text } => {
                let t = text.trim();
                if !t.is_empty() && enabled.contains("text") {
                    out.push((Kind::Text, clip(t, DETAIL_BLOCK_CHARS), usage.clone()));
                }
            }
            Block::ToolUse { name, input } => {
                if enabled.contains("tool_use") {
                    let args = serde_json::to_string_pretty(&input).unwrap_or_default();
                    // quoted in full on purpose — the send path splits oversized
                    // bundles, so Write contents and Task prompts arrive complete
                    out.push((
                        Kind::ToolUse,
                        format!("🔧 **{name}**\n```json\n{args}\n```"),
                        usage.clone(),
                    ));
                }
            }
            Block::Ignored => {} // tool_result never leaves the machine
        }
    }
    out
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let cut: String = text.chars().take(limit).collect();
    format!("{} …", cut.trim_end())
}

/// Render a chunk of new jsonl lines into finished messages: sections across
/// lines, adjacent tool_use sections merged into one bundle, then split to
/// message-size pieces, each carrying its own footer.
pub fn render_chunk(
    chunk: &str,
    cfg: &Config,
    target: &Target,
    title: &str,
    cwd: &str,
    enabled: &HashSet<String>,
) -> Vec<Outbound> {
    // sections across all lines of the chunk, in transcript order
    let mut secs: Vec<(Kind, String, Usage)> = Vec::new();
    for line in chunk.lines() {
        if let Some(row) = parse_row(line) {
            secs.extend(sections(&row, enabled));
        }
    }

    // fold adjacent tool_use sections into bundles
    let mut bundles: Vec<(Kind, String, Usage)> = Vec::new();
    for (kind, body, usage) in secs {
        match bundles.last_mut() {
            Some((Kind::ToolUse, acc, last_usage))
                if kind == Kind::ToolUse
                    && acc.chars().count() + 2 + body.chars().count() <= JOIN_CHARS =>
            {
                acc.push_str("\n\n");
                acc.push_str(&body);
                *last_usage = usage; // a merged tool bundle reports its newest entry
            }
            _ => bundles.push((kind, body, usage)),
        }
    }

    let mut out = Vec::new();
    for (kind, body, usage) in bundles {
        let footer = footer::render(cfg, target, kind.emoji(), title, cwd, Some(&usage));
        for part in crate::telegram::split_text(&body, JOIN_CHARS) {
            out.push(Outbound {
                body: part,
                footer: footer.clone(),
                target: target.clone(),
                kind,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_row(content: serde_json::Value, ts: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "isSidechain": false,
            "timestamp": ts,
            "message": {"role": "assistant", "content": content, "usage": {"input_tokens": 7}}
        })
        .to_string()
    }

    fn all() -> HashSet<String> {
        ["thinking", "text", "tool_use"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn target() -> Target {
        Target {
            pane: "w1:p1".into(),
            agent: "claude".into(),
            folder: "~/proj".into(),
        }
    }

    fn cfg() -> Config {
        Config {
            chat_id: String::new(),
            relay_url: String::new(),
            relay_key: String::new(),
            device: "testbox".into(),
            topic_id: 3,
            proxy: String::new(),
            agent_details: vec![],
            footer_format: "{emoji}".into(),
        }
    }

    fn bodies(out: &[Outbound]) -> Vec<&str> {
        out.iter().map(|o| o.body.as_str()).collect()
    }

    #[test]
    fn thinking_and_text_become_separate_messages() {
        let chunk = assistant_row(
            serde_json::json!([{"type": "thinking", "thinking": " hush "}, {"type": "text", "text": " hello "}]),
            "2026-08-22T10:00:00Z",
        );
        let out = render_chunk(&chunk, &cfg(), &target(), "", "", &all());
        assert_eq!(out[0].kind, Kind::Thinking);
        assert_eq!(out[0].body, "hush");
        assert_eq!(out[1].kind, Kind::Text);
        assert_eq!(out[1].body, "hello");
        assert_eq!(out[0].footer, "💭");
        assert_eq!(out[1].footer, "⏳");
    }

    #[test]
    fn disabled_categories_dropped() {
        let chunk = assistant_row(
            serde_json::json!([{"type": "thinking", "thinking": "hush"}, {"type": "text", "text": "hello"}]),
            "2026-08-22T10:00:00Z",
        );
        let mut enabled = HashSet::new();
        enabled.insert("text".to_string());
        let out = render_chunk(&chunk, &cfg(), &target(), "", "", &enabled);
        assert_eq!(bodies(&out), ["hello"]);
    }

    #[test]
    fn tool_use_renders_name_and_pretty_args() {
        let chunk = assistant_row(
            serde_json::json!([{"type": "tool_use", "name": "Bash", "input": {"command": "ls"}}]),
            "2026-08-22T10:00:00Z",
        );
        let out = render_chunk(&chunk, &cfg(), &target(), "", "", &all());
        assert_eq!(
            bodies(&out),
            ["🔧 **Bash**\n```json\n{\n  \"command\": \"ls\"\n}\n```"]
        );
    }

    #[test]
    fn adjacent_tool_uses_share_one_message() {
        let chunk = format!(
            "{}\n{}\n",
            assistant_row(
                serde_json::json!([{"type": "tool_use", "name": "Bash", "input": {"command": "ls"}}]),
                "2026-08-22T10:00:00Z"
            ),
            assistant_row(
                serde_json::json!([{"type": "tool_use", "name": "Read", "input": {"path": "/x"}}]),
                "2026-08-22T10:00:01Z"
            ),
        );
        let out = render_chunk(&chunk, &cfg(), &target(), "", "", &all());
        assert_eq!(out.len(), 1);
        assert!(out[0].body.contains("Bash") && out[0].body.contains("Read"));
    }

    #[test]
    fn oversized_tool_bundle_splits_rather_than_clips() {
        // over JOIN_CHARS (31700): the bundle must split, not clip
        let long = "y".repeat(40_000);
        let chunk = assistant_row(
            serde_json::json!([{"type": "tool_use", "name": "Write", "input": {"content": long}}]),
            "2026-08-22T10:00:00Z",
        );
        let out = render_chunk(&chunk, &cfg(), &target(), "", "", &all());
        assert!(out.len() >= 2, "must split, not clip");
        assert!(
            out.iter().map(|o| o.body.chars().count()).sum::<usize>() >= 40_000,
            "every character ships"
        );
        assert_eq!(out[0].kind, Kind::ToolUse);
    }

    #[test]
    fn tool_result_never_sent() {
        let chunk = assistant_row(
            serde_json::json!([{"type": "tool_result", "content": "secret"}]),
            "2026-08-22T10:00:00Z",
        );
        let out = render_chunk(&chunk, &cfg(), &target(), "", "", &all());
        assert!(out.is_empty());
    }

    #[test]
    fn sidechain_rows_skipped() {
        let chunk = serde_json::json!({
            "type": "assistant", "isSidechain": true,
            "message": {"role": "assistant", "content": [{"type": "text", "text": "subagent talk"}]}
        })
        .to_string();
        assert!(render_chunk(&chunk, &cfg(), &target(), "", "", &all()).is_empty());
    }

    #[test]
    fn user_and_bookkeeping_rows_skipped() {
        let user = r#"{"type": "user", "message": {"role": "user", "content": []}}"#;
        let bookkeeping =
            r#"{"type": "queue-operation", "message": {"role": "assistant", "content": []}}"#;
        assert!(render_chunk(user, &cfg(), &target(), "", "", &all()).is_empty());
        assert!(render_chunk(bookkeeping, &cfg(), &target(), "", "", &all()).is_empty());
    }

    #[test]
    fn api_request_blob_skipped() {
        // repeats the turn verbatim under role "assistant" but under a foreign
        // top-level type — only Row::Ignored comes out of deserialization
        let blob = serde_json::json!({
            "type": "api-request-blob", "hash": "deadbeef",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "echo"}]}
        })
        .to_string();
        assert!(render_chunk(&blob, &cfg(), &target(), "", "", &all()).is_empty());
    }

    #[test]
    fn long_thinking_clipped_with_ellipsis() {
        let long = "x".repeat(40_000);
        let chunk = assistant_row(
            serde_json::json!([{"type": "thinking", "thinking": long}]),
            "2026-08-22T10:00:00Z",
        );
        let out = render_chunk(&chunk, &cfg(), &target(), "", "", &all());
        assert_eq!(out.len(), 1);
        assert!(out[0].body.chars().count() < 40_000);
        assert!(out[0].body.ends_with('…'));
    }

    #[test]
    fn prompt_tools_render_like_any_tool_use() {
        // the question/plan tools stream out like every other tool, which is
        // how a blocked pane shows itself in the feed
        let chunk = assistant_row(
            serde_json::json!([{"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "ship it?"}}]),
            "2026-08-22T10:00:00Z",
        );
        let out = render_chunk(&chunk, &cfg(), &target(), "", "", &all());
        assert_eq!(out.len(), 1);
        assert!(out[0].body.contains("AskUserQuestion"));
        assert!(out[0].body.contains("ship it?"));
    }

    #[test]
    fn unparseable_lines_are_skipped_not_fatal() {
        let chunk = "{not json\n".to_string()
            + &assistant_row(
                serde_json::json!([{"type": "text", "text": "ok"}]),
                "2026-08-22T10:00:00Z",
            );
        let out = render_chunk(&chunk, &cfg(), &target(), "", "", &all());
        assert_eq!(bodies(&out), ["ok"]);
    }

    #[test]
    fn footer_carries_the_row_usage() {
        let chunk = assistant_row(
            serde_json::json!([{"type": "text", "text": "hi"}]),
            "2026-08-22T10:00:00Z",
        );
        let mut c = cfg();
        c.footer_format = "{input_tokens}".into();
        let out = render_chunk(&chunk, &c, &target(), "", "", &all());
        assert_eq!(out[0].footer, "7");
    }

    #[test]
    fn target_folder_shortens_home() {
        let a = Agent {
            pane_id: "w1:p9".into(),
            agent: "claude".into(),
            cwd: Some("/home/someone/proj".into()),
            foreground_cwd: None,
            terminal_title_stripped: None,
            terminal_title: None,
            session: None,
        };
        assert_eq!(Target::from_agent(&a, "/home/someone").folder, "~/proj");
        assert_eq!(
            Target::from_agent(&a, "/other").folder,
            "/home/someone/proj"
        );
        // cwd missing: the foreground cwd stands in
        let fg = Agent {
            cwd: None,
            foreground_cwd: Some("/home/someone/other".into()),
            ..a
        };
        assert_eq!(Target::from_agent(&fg, "/home/someone").folder, "~/other");
    }
}
