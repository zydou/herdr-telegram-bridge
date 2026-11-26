//! Every Telegram call the plugin makes, over ureq.
//!
//! The relay speaks the standard Bot API — RELAY_KEY in the token position,
//! the device name in a path prefix — so the client is nothing but "POST
//! JSON, read the answer". An HTTP proxy rides a CONNECT tunnel whose DNS
//! happens proxy-side (the socks5h semantics); `timeout_global` bounds the
//! whole exchange including resolution, so a wedged resolver cannot hang a
//! send lane forever.
//!
//! Error classes, in order:
//! - transport trouble and 5xx — retried with a growing pause
//! - 429 — sleeps the window Telegram itself asks for; this branch must be
//!   matched *before* the ok:false fail-fast or a rate limit reads as a parse
//!   failure and the caller hammers the API forever
//! - other 4xx — an API-level rejection; fail fast, retrying is futile
//! - 200 without `result` — Telegram already accepted the request, so a retry
//!   would post it a second time; fail fast (the caller's own rich→plain
//!   fallback is the one retry that is safe)

use crate::config::Config;
use crate::logging;
use crate::render::Target;
use serde_json::{Value, json};
use std::time::Duration;

/// Cloudflare's bot protection bans well-known client signatures (error 1010)
/// — a bare "htb" is neither recognizable nor deceptive.
const USER_AGENT: &str = "htb";
/// One attempt's whole-exchange budget: connect, tunnel, resolve, read.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(40);
const RETRIES: usize = 3;
/// Plain sends; leaves room for our own additions.
pub const TEXT_LIMIT: usize = 3500;
const LOG_CONTENT_CHARS: usize = 500;

/// Percent-encode one path segment (the device name); the relay
/// decodeURIComponent's it back before validating.
fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Blank the relay key out of a diagnostic string — the key rides in every
/// request URL, so transport error text would otherwise leak it.
fn redact(text: &str, cfg: &Config) -> String {
    if cfg.relay_key.is_empty() {
        text.to_string()
    } else {
        text.replace(&cfg.relay_key, "<relay-key>")
    }
}

/// One transport exchange. Err carries transport-level trouble only —
/// 4xx/5xx are answers from the relay and come back as (status, body).
fn post_once(url: &str, params: &Value, proxy: &str) -> Result<(u16, String), String> {
    let mut builder = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false);
    if !proxy.is_empty() {
        let p = ureq::Proxy::new(proxy).map_err(|e| format!("bad proxy: {e}"))?;
        builder = builder.proxy(Some(p));
    }
    let agent: ureq::Agent = builder.build().into();
    let resp = agent
        .post(url)
        .header("user-agent", USER_AGENT)
        .send_json(params.clone())
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let body = resp
        .into_body()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    Ok((status, body))
}

/// One Bot API call through the relay.
pub fn call(cfg: &Config, method: &str, params: &Value) -> Result<Value, String> {
    if cfg.relay_url.is_empty() || cfg.relay_key.is_empty() {
        return Err("relay not configured".into());
    }
    let url = format!(
        "{}/d/{}/bot{}/{}",
        cfg.relay_url,
        encode_segment(&cfg.device),
        cfg.relay_key,
        method
    );
    let params = params.clone();
    let proxy = cfg.proxy.clone();
    classify(
        cfg,
        method,
        || post_once(&url, &params, &proxy),
        |secs| std::thread::sleep(Duration::from_secs_f64(secs)),
    )
}

/// The retry/fail-fast classification loop, parameterized on the transport and
/// the pause so tests can drive it without network or wall-clock sleeping.
fn classify<F, S>(cfg: &Config, method: &str, mut post: F, mut sleep: S) -> Result<Value, String>
where
    F: FnMut() -> Result<(u16, String), String>,
    S: FnMut(f64),
{
    let mut err = String::new();
    for attempt in 0..RETRIES {
        let (status, body) = match post() {
            Ok(v) => v,
            Err(e) => {
                err = redact(&e, cfg);
                if attempt + 1 < RETRIES {
                    sleep(1.0 + attempt as f64);
                }
                continue;
            }
        };
        let parsed: Option<Value> = serde_json::from_str(&body).ok();

        if status == 429 {
            // must be matched before the ok:false fail-fast below
            let desc = parsed
                .as_ref()
                .and_then(|b| b.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("flood control");
            err = redact(desc, cfg);
            if attempt + 1 < RETRIES {
                let pause = parsed
                    .as_ref()
                    .and_then(|b| b.pointer("/parameters/retry_after"))
                    .and_then(Value::as_f64);
                match pause {
                    Some(p) if p >= 0.0 => {
                        logging::log(
                            "flood control",
                            &[("method", method), ("wait_s", &format!("{p:.0}"))],
                        );
                        sleep(p + 1.0);
                    }
                    _ => sleep(1.0 + attempt as f64),
                }
                continue;
            }
        } else if status >= 500 {
            err = format!("relay answered HTTP {status}"); // transient server trouble, same class as transport
        } else if let Some(b) = parsed {
            if !b.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                let desc = b
                    .get("description")
                    .map(Value::to_string)
                    .unwrap_or_else(|| b.to_string());
                return Err(redact(&desc, cfg)); // API-level rejection: fail fast
            }
            return match b.get("result") {
                Some(r @ (Value::Object(_) | Value::Array(_))) => Ok(r.clone()),
                _ => Err("reply without a result".into()), // retrying would double-send
            };
        } else {
            err = format!("unparseable reply (HTTP {status})");
        }
        if attempt + 1 < RETRIES {
            sleep(1.0 + attempt as f64);
        }
    }
    Err(err)
}

/// The pane/agent pair as log fields; each "" when absent.
fn target_who(target: Option<&Target>) -> (String, String) {
    match target {
        Some(t) => (t.pane.clone(), t.agent.clone()),
        None => (String::new(), String::new()),
    }
}

/// sendMessage parameter assembly — the single place it happens.
fn text_params(chat: &str, text: &str, topic_id: i64) -> Value {
    let clipped: String = text.chars().take(TEXT_LIMIT + 500).collect();
    let mut params = json!({
        "chat_id": chat,
        "text": clipped,
        "link_preview_options": {"is_disabled": true},
    });
    if topic_id > 0 {
        params["message_thread_id"] = json!(topic_id);
    }
    params
}

/// Send one plain message; the send log lands in stderr JSONL either way.
pub fn send_text(
    cfg: &Config,
    chat: &str,
    text: &str,
    target: Option<&Target>,
    source: &str,
) -> Result<(), String> {
    let params = text_params(chat, text, cfg.topic_id);
    match call(cfg, "sendMessage", &params) {
        Err(e) => {
            let head: String = text.chars().take(120).collect();
            logging::log(
                "send failed",
                &[
                    ("error", &e),
                    ("content", &format!("{head:?}")),
                    ("source", source),
                ],
            );
            Err(e)
        }
        Ok(_) => {
            let sent = params["text"].as_str().unwrap_or_default().to_string();
            let (pane, agent) = target_who(target);
            logging::log(
                "sent text",
                &[
                    ("chars", &sent.chars().count().to_string()),
                    (
                        "content",
                        sent.chars()
                            .take(LOG_CONTENT_CHARS)
                            .collect::<String>()
                            .as_str(),
                    ),
                    ("pane", &pane),
                    ("agent", &agent),
                    ("source", source),
                ],
            );
            Ok(())
        }
    }
}

/// sendRichMessage parameter assembly.
fn rich_params(chat: &str, markdown: &str, topic_id: i64) -> Value {
    let mut params = json!({
        "chat_id": chat,
        "rich_message": {"markdown": markdown},
    });
    if topic_id > 0 {
        params["message_thread_id"] = json!(topic_id);
    }
    params
}

/// Send one rich message (server-rendered markdown). Err means Telegram
/// rejected it — the caller's rich→plain fallback is the one safe retry.
pub fn send_rich(
    cfg: &Config,
    chat: &str,
    markdown: &str,
    target: Option<&Target>,
    source: &str,
) -> Result<(), String> {
    let params = rich_params(chat, markdown, cfg.topic_id);
    match call(cfg, "sendRichMessage", &params) {
        Err(e) => {
            let tail: String = e
                .chars()
                .skip(e.chars().count().saturating_sub(160))
                .collect();
            logging::log(
                "rich send rejected",
                &[("error", tail.as_str()), ("source", source)],
            );
            Err(e)
        }
        Ok(_) => {
            let (pane, agent) = target_who(target);
            logging::log(
                "sent rich",
                &[
                    ("chars", &markdown.chars().count().to_string()),
                    (
                        "content",
                        markdown
                            .chars()
                            .take(LOG_CONTENT_CHARS)
                            .collect::<String>()
                            .as_str(),
                    ),
                    ("pane", &pane),
                    ("agent", &agent),
                    ("source", source),
                ],
            );
            Ok(())
        }
    }
}

/// Chunk text so each piece fits one Telegram message, cut on a line boundary
/// when one fits within the limit and mid-line otherwise.
pub fn split_text(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut chunk: String = rest.chars().take(limit).collect();
        let mut tail = &rest[chunk.len()..];
        if !tail.is_empty()
            && let Some(cut) = chunk.rfind('\n').filter(|&c| c > 0)
        {
            tail = &rest[cut..]; // the newline rides with the tail, trimmed away below
            chunk.truncate(cut);
        }
        chunks.push(chunk);
        rest = tail.trim();
    }
    chunks
        .into_iter()
        .filter(|c| !c.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            chat_id: "-10042".into(),
            relay_url: "https://relay.example".into(),
            relay_key: "sekret".into(),
            device: "box".into(),
            topic_id: 7,
            proxy: String::new(),
            agent_details: vec![],
            footer_format: String::new(),
        }
    }

    /// scripted replies: Ok((status, body)) or Err(transport text)
    fn classify_scripted(
        replies: Vec<Result<(u16, String), String>>,
    ) -> (Result<Value, String>, Vec<f64>, usize) {
        let calls = std::cell::RefCell::new(0usize);
        let queue = std::cell::RefCell::new(replies);
        let post = || {
            let mut c = calls.borrow_mut();
            *c += 1;
            let mut q = queue.borrow_mut();
            q.remove(0)
        };
        let sleeps = std::cell::RefCell::new(Vec::new());
        let sleep = |s: f64| sleeps.borrow_mut().push(s);
        (
            classify(&cfg(), "sendMessage", post, sleep),
            sleeps.into_inner(),
            calls.into_inner(),
        )
    }

    #[test]
    fn success_returns_result() {
        let (r, _, calls) = classify_scripted(vec![Ok((
            200,
            r#"{"ok":true,"result":{"message_id":1}}"#.into(),
        ))]);
        assert_eq!(r.unwrap()["message_id"], 1);
        assert_eq!(calls, 1);
    }

    #[test]
    fn transport_trouble_is_retried_then_succeeds() {
        let (r, sleeps, calls) = classify_scripted(vec![
            Err("dns broke".into()),
            Ok((200, r#"{"ok":true,"result":{"message_id":1}}"#.into())),
        ]);
        assert!(r.is_ok());
        assert_eq!(calls, 2);
        assert_eq!(sleeps, vec![1.0]);
    }

    #[test]
    fn transport_trouble_exhausts_with_redacted_error() {
        let (r, sleeps, calls) = classify_scripted(vec![
            Err(
                "connect to https://relay.example/d/box/botsekret/sendMessage failed".into()
            );
            3
        ]);
        assert_eq!(calls, 3);
        assert_eq!(sleeps, vec![1.0, 2.0]);
        let e = r.unwrap_err();
        assert!(e.contains("<relay-key>"), "{e}");
        assert!(!e.contains("sekret"), "{e}");
    }

    #[test]
    fn five_hundred_is_retried_like_transport() {
        let (r, _, calls) = classify_scripted(vec![
            Ok((502, "bad gateway".into())),
            Ok((200, r#"{"ok":true,"result":[]}"#.into())),
        ]);
        assert!(r.is_ok());
        assert_eq!(calls, 2);
    }

    #[test]
    fn five_hundred_exhausts_with_status_error() {
        let (r, _, calls) = classify_scripted(vec![Ok((502, "bad gateway".into())); 3]);
        assert_eq!(r.unwrap_err(), "relay answered HTTP 502");
        assert_eq!(calls, 3);
    }

    #[test]
    fn four_twenty_nine_sleeps_retry_after_plus_one() {
        let body = r#"{"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":17}}"#.to_string();
        let (r, sleeps, calls) = classify_scripted(vec![
            Ok((429, body)),
            Ok((200, r#"{"ok":true,"result":{}}"#.into())),
        ]);
        assert!(r.is_ok());
        assert_eq!(calls, 2);
        assert_eq!(sleeps, vec![18.0]);
    }

    #[test]
    fn four_twenty_nine_without_retry_after_uses_backoff() {
        let (r, sleeps, calls) = classify_scripted(vec![
            Ok((
                429,
                r#"{"ok":false,"description":"Too Many Requests"}"#.into(),
            )),
            Ok((200, r#"{"ok":true,"result":{}}"#.into())),
        ]);
        assert!(r.is_ok());
        assert_eq!(calls, 2);
        assert_eq!(sleeps, vec![1.0]);
    }

    #[test]
    fn four_twenty_nine_beats_the_ok_false_fail_fast() {
        // the 429 body carries ok:false — the branch order must not let the
        // fail-fast swallow it and hammer the API through the limit window
        let (r, _, calls) = classify_scripted(vec![
            Ok((
                429,
                r#"{"ok":false,"description":"Too Many Requests","parameters":{"retry_after":0}}"#
                    .into(),
            )),
            Ok((
                429,
                r#"{"ok":false,"description":"Too Many Requests","parameters":{"retry_after":0}}"#
                    .into(),
            )),
            Ok((200, r#"{"ok":true,"result":{}}"#.into())),
        ]);
        assert!(r.is_ok());
        assert_eq!(calls, 3);
    }

    #[test]
    fn other_four_hundred_fails_fast_without_retry() {
        let (r, sleeps, calls) = classify_scripted(vec![Ok((
            400,
            r#"{"ok":false,"description":"Bad Request: chat not found"}"#.into(),
        ))]);
        assert!(r.unwrap_err().contains("chat not found"));
        assert_eq!(calls, 1);
        assert!(sleeps.is_empty());
    }

    #[test]
    fn ok_true_without_result_fails_fast() {
        // Telegram already accepted it — a retry would double-send
        let (r, sleeps, calls) = classify_scripted(vec![Ok((200, r#"{"ok":true}"#.into()))]);
        assert_eq!(r.unwrap_err(), "reply without a result");
        assert_eq!(calls, 1);
        assert!(sleeps.is_empty());
    }

    #[test]
    fn unparseable_body_is_an_error_not_a_panic() {
        // unparseable replies are retry-class: three bad answers, three attempts
        let (r, sleeps, calls) = classify_scripted(vec![Ok((200, "html garbage".into())); 3]);
        assert!(r.unwrap_err().contains("unparseable"));
        assert_eq!(calls, 3);
        assert_eq!(sleeps, vec![1.0, 2.0]);
    }

    #[test]
    fn text_params_clip_thread_and_preview() {
        let long = "x".repeat(5000);
        let p = text_params("-10042", &long, 7);
        assert_eq!(p["chat_id"], "-10042");
        assert_eq!(p["text"].as_str().unwrap().chars().count(), 4000);
        assert_eq!(p["message_thread_id"], 7);
        assert_eq!(p["link_preview_options"]["is_disabled"], true);

        let p = text_params("-10042", "hi", 0);
        assert!(p.get("message_thread_id").is_none());
    }

    #[test]
    fn rich_params_carry_markdown_and_thread() {
        let p = rich_params("-10042", "**bold**", 3);
        assert_eq!(p["rich_message"]["markdown"], "**bold**");
        assert_eq!(p["message_thread_id"], 3);
    }

    #[test]
    fn split_text_cuts_on_line_boundaries() {
        let text = "l1\nl2\nl3\nl4";
        assert_eq!(split_text(text, 8), ["l1\nl2", "l3\nl4"]);
        // no fitting line boundary: hard cut
        assert_eq!(split_text("aaaaaaaaaa\nb", 5), ["aaaaa", "aaaaa", "b"]);
        // oversize single line
        assert_eq!(split_text("xxxxxxxxxx", 4), ["xxxx", "xxxx", "xx"]);
    }

    #[test]
    fn split_text_never_returns_empty_chunks() {
        assert!(split_text("", 10).is_empty());
        assert!(split_text("   \n  ", 10).is_empty());
        assert_eq!(split_text("a\n\n\nb", 4), ["a\n\n", "b"]);
    }

    #[test]
    fn device_name_is_percent_encoded_for_the_relay_path() {
        assert_eq!(encode_segment("4090-box"), "4090-box");
        assert_eq!(encode_segment("小盒子"), "%E5%B0%8F%E7%9B%92%E5%AD%90");
    }
}
