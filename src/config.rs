//! Plugin directories and config.toml loading.
//!
//! herdr injects HERDR_PLUGIN_{STATE,CONFIG}_DIR for hooks; outside herdr a bare
//! `htb` falls back to the same XDG layout herdr itself would use, keyed by the
//! plugin id `htb`. The config file is re-read every cycle of the daemon, so a
//! key rotation or topic change takes effect without a restart. Most keys are
//! extracted leniently and default when unmentioned — except the ones a typo
//! would silently break: `device` is required, and `chat_id`/`topic_id` are
//! shape-validated (supergroup id / positive topic) with a hard error naming
//! the key, so a bad config is an error the operator sees, never a quiet
//! default. `device` is required because it is the relay-side audit label for
//! this machine, and a name you chose reads better in the audit log than a
//! hostname ever would.

use std::fs;
use std::path::{Path, PathBuf};

pub const PLUGIN_ID: &str = "htb";

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub chat_id: String,
    pub relay_url: String,
    pub relay_key: String,
    pub device: String,
    pub topic_id: i64,
    pub proxy: String,
    pub agent_details: Vec<String>,
    pub footer_format: String,
}

pub const DEFAULT_FOOTER_FORMAT: &str = "{emoji}【{title}】\n🖥{device} 📁{folder}\n⬆️{input_tokens} ⚡️{cache_read_input_tokens} ⬇️{output_tokens}";

/// The plugin's state directory (bot.lock, log.jsonl): herdr-injected when
/// running as a hook, the herdr XDG layout otherwise.
pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(d);
    }
    let xdg = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/state"));
    xdg.join("herdr/plugins").join(PLUGIN_ID)
}

/// The plugin's config directory (config.toml), same resolution rules as state.
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"));
    xdg.join("herdr/plugins/config").join(PLUGIN_ID)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A chat id may arrive as a TOML integer or string; both are accepted as-is.
fn loose_string(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

/// chat_id and topic_id are user-typed Telegram ids: both are read leniently
/// (a TOML integer, or a string that parses as one) but never defaulted — a
/// bad value is a hard error naming the key. A string topic_id used to
/// collapse to 0 here, and the daemon then napped forever with `htb status`
/// green and not a single log line to say why.
fn required_i64(v: &toml::Table, key: &str) -> Result<i64, String> {
    let raw = match v.get(key) {
        Some(toml::Value::Integer(i)) => return Ok(*i),
        Some(toml::Value::String(s)) => s.trim().to_string(),
        Some(other) => {
            return Err(format!(
                "config error: {key} must be an integer, got a {}",
                other.type_str()
            ));
        }
        None => return Err(format!("config error: {key} is required")),
    };
    raw.parse::<i64>()
        .map_err(|_| format!("config error: {key} must be an integer, got '{raw}'"))
}

impl Config {
    /// Parse config.toml over the defaults; re-read every daemon cycle.
    pub fn load(dir: &Path) -> Result<Config, String> {
        let path = dir.join("config.toml");
        let raw = fs::read_to_string(&path)
            .map_err(|e| format!("no usable config at {}: {e}", path.display()))?;
        let v: toml::Table = raw
            .parse()
            .map_err(|e| format!("bad config {}: {e}", path.display()))?;

        let device = v.get("device").and_then(loose_string).ok_or(
            "config error: device is required — it is this machine's label in the relay audit log",
        )?;

        let topic_id = required_i64(&v, "topic_id")?;
        if topic_id <= 0 {
            return Err(format!(
                "config error: topic_id must be a positive integer, got {topic_id}"
            ));
        }
        let chat_num = required_i64(&v, "chat_id")?;
        if !chat_num.to_string().starts_with("-100") {
            return Err(format!(
                "config error: chat_id must be a negative integer starting with -100 (a Telegram supergroup id), got {chat_num}"
            ));
        }

        Ok(Config {
            chat_id: chat_num.to_string(),
            relay_url: v
                .get("relay_url")
                .and_then(loose_string)
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string(),
            relay_key: v
                .get("relay_key")
                .and_then(loose_string)
                .unwrap_or_default(),
            device,
            topic_id,
            proxy: v.get("proxy").and_then(loose_string).unwrap_or_default(),
            agent_details: v
                .get("agent_details")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            footer_format: v
                .get("footer_format")
                .and_then(|x| x.as_str().map(str::to_string))
                .unwrap_or_else(|| DEFAULT_FOOTER_FORMAT.to_string()),
        })
    }

    /// The remaining soft gate: chat_id and topic_id are shape-validated at
    /// load time, so what is left to make the feed unusable is a missing relay
    /// (every send would fail) or an empty `agent_details` (no subscribers) —
    /// those are "not configured yet", which naps rather than exits.
    pub fn usable(&self) -> bool {
        !self.relay_url.is_empty() && !self.relay_key.is_empty() && !self.agent_details.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("config.toml"), body).unwrap();
    }

    #[test]
    fn missing_file_is_an_error_naming_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Config::load(tmp.path()).unwrap_err();
        assert!(err.contains("no usable config"), "{err}");
    }

    #[test]
    fn bad_toml_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "chat_id = [unclosed");
        assert!(Config::load(tmp.path()).is_err());
    }

    #[test]
    fn chat_id_may_be_an_integer() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "device = \"d\"\nchat_id = -1001234567890\ntopic_id = 3\nagent_details = [\"text\"]",
        );
        let c = Config::load(tmp.path()).unwrap();
        assert_eq!(c.chat_id, "-1001234567890");
        assert_eq!(c.topic_id, 3);
    }

    #[test]
    fn unmentioned_keys_fall_back_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "device = \"d\"\nchat_id = -1001234567890\ntopic_id = 3",
        );
        let c = Config::load(tmp.path()).unwrap();
        assert_eq!(c.proxy, "");
        assert!(c.agent_details.is_empty());
        assert_eq!(c.footer_format, DEFAULT_FOOTER_FORMAT);
        assert!(!c.usable()); // no relay, no details
    }

    #[test]
    fn device_is_required() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "chat_id = -10042");
        let err = Config::load(tmp.path()).unwrap_err();
        assert!(err.contains("device"), "{err}");
    }

    #[test]
    fn relay_url_trailing_slash_is_trimmed() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "device = \"d\"\nchat_id = -1001234567890\ntopic_id = 3\nrelay_url = \"https://r.example/\"",
        );
        assert_eq!(
            Config::load(tmp.path()).unwrap().relay_url,
            "https://r.example"
        );
    }

    #[test]
    fn usable_needs_relay_and_details() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "device = \"d\"\nchat_id = -1001234567890\ntopic_id = 3\nrelay_url = \"https://r\"\nrelay_key = \"k\"\nagent_details = [\"thinking\"]",
        );
        assert!(Config::load(tmp.path()).unwrap().usable());
    }

    #[test]
    fn id_keys_accept_string_spellings() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "device = \"d\"\nchat_id = \"-1001234567890\"\ntopic_id = \"3\"",
        );
        let c = Config::load(tmp.path()).unwrap();
        assert_eq!(c.chat_id, "-1001234567890");
        assert_eq!(c.topic_id, 3);
    }

    #[test]
    fn topic_id_must_be_a_positive_integer() {
        let tmp = tempfile::tempdir().unwrap();
        for body in [
            "device = \"d\"\nchat_id = -100123\ntopic_id = -5",
            "device = \"d\"\nchat_id = -100123\ntopic_id = 0",
            "device = \"d\"\nchat_id = -100123\ntopic_id = \"abc\"",
            "device = \"d\"\nchat_id = -100123\ntopic_id = 3.5",
            "device = \"d\"\nchat_id = -100123",
        ] {
            write(tmp.path(), body);
            let err = Config::load(tmp.path()).unwrap_err();
            assert!(err.contains("topic_id"), "{err}");
        }
    }

    #[test]
    fn chat_id_must_be_a_supergroup_id() {
        let tmp = tempfile::tempdir().unwrap();
        for body in [
            "device = \"d\"\ntopic_id = 3\nchat_id = 12345",
            "device = \"d\"\ntopic_id = 3\nchat_id = \"-123\"",
            "device = \"d\"\ntopic_id = 3\nchat_id = \"abc\"",
            "device = \"d\"\ntopic_id = 3",
        ] {
            write(tmp.path(), body);
            let err = Config::load(tmp.path()).unwrap_err();
            assert!(err.contains("chat_id"), "{err}");
        }
    }
}
