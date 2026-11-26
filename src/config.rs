//! Plugin directories and config.toml loading.
//!
//! herdr injects HERDR_PLUGIN_{STATE,CONFIG}_DIR for hooks; outside herdr a bare
//! `htb` falls back to the same XDG layout herdr itself would use, keyed by the
//! plugin id `htb`. The config file is re-read every cycle of the daemon, so a
//! key rotation or topic change takes effect without a restart. Every key is
//! extracted leniently (a TOML integer chat_id is as valid as a string one);
//! anything the file fails to mention falls back to the default below — except
//! `device`, which is required: it is the relay-side audit label for this
//! machine, and an implicit hostname default proved too unstable to rely on.

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

        Ok(Config {
            chat_id: v.get("chat_id").and_then(loose_string).unwrap_or_default(),
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
            topic_id: v.get("topic_id").and_then(|x| x.as_integer()).unwrap_or(0),
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

    /// A device without its topic has nowhere to land its messages, and an
    /// unconfigured relay means every send would fail — the daemon naps until
    /// the config is fixed rather than spinning on guaranteed failures.
    pub fn usable(&self) -> bool {
        !self.chat_id.is_empty()
            && self.topic_id > 0
            && !self.relay_url.is_empty()
            && !self.relay_key.is_empty()
            && !self.agent_details.is_empty()
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
        write(tmp.path(), "device = \"d\"\nchat_id = 7");
        let c = Config::load(tmp.path()).unwrap();
        assert_eq!(c.proxy, "");
        assert!(c.agent_details.is_empty());
        assert_eq!(c.footer_format, DEFAULT_FOOTER_FORMAT);
        assert!(!c.usable()); // no topic, no relay, no details
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
            "device = \"d\"\nrelay_url = \"https://r.example/\"",
        );
        assert_eq!(
            Config::load(tmp.path()).unwrap().relay_url,
            "https://r.example"
        );
    }

    #[test]
    fn usable_needs_topic_relay_and_details() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "device = \"d\"\nchat_id = -1\ntopic_id = 3\nrelay_url = \"https://r\"\nrelay_key = \"k\"\nagent_details = [\"thinking\"]",
        );
        assert!(Config::load(tmp.path()).unwrap().usable());
    }
}
