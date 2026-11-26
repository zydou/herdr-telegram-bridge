//! The footer renderer closing every outbound message.
//!
//! One renderer for every message the daemon sends, so they all read alike.
//! The template may span several lines: each line is whitespace-collapsed on
//! its own, a line whose fields all render empty disappears, and an empty
//! field wrapped in literal 【】 brackets loses the pair. Token placeholders
//! get an extra rule: a zero/absent count carries its leading label segment
//! away with it — the label being the trailing run of non-space,
//! non-separator characters before the placeholder (a bare "I:" or an emoji),
//! together with the one separator glued to it.

use crate::config::Config;
use crate::render::{Target, Usage};
use std::collections::HashMap;

const TOKEN_FIELDS: [&str; 3] = ["input_tokens", "cache_read_input_tokens", "output_tokens"];
const SEP_CHARS: [char; 5] = [',', '，', '/', '·', '•'];

enum Part<'a> {
    Lit(&'a str),
    Token(&'a str),
}

/// Render the footer for one outbound message.
#[allow(clippy::too_many_arguments)]
pub fn render(
    cfg: &Config,
    target: &Target,
    emoji: &str,
    title: &str,
    cwd: &str,
    usage: Option<&Usage>,
) -> String {
    let mut fields: HashMap<&str, String> = HashMap::new();
    fields.insert("emoji", emoji.to_string());
    fields.insert(
        "folder",
        if target.folder.is_empty() {
            target.pane.clone()
        } else {
            target.folder.clone()
        },
    );
    fields.insert("pane", target.pane.clone());
    fields.insert("cwd", cwd.to_string());
    fields.insert(
        "title",
        if title.is_empty() {
            target.pane.clone()
        } else {
            title.to_string()
        },
    );
    fields.insert("device", cfg.device.clone());
    for f in TOKEN_FIELDS {
        let v = usage.and_then(|u| u.get(f)).unwrap_or(0);
        fields.insert(f, fmt_token(v));
    }

    let mut lines = Vec::new();
    for raw_line in cfg.footer_format.split('\n') {
        let line = if has_token_field(raw_line) {
            render_token_line(raw_line, &fields)
        } else {
            format_line(raw_line, &fields)
        };
        let line = line.replace("【】", "");
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let trimmed = collapsed.trim_matches(|c| c == ' ' || c == '·' || c == '•');
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
    }
    lines.join("\n")
}

/// True when the line mentions any token placeholder — it needs zero-hiding.
fn has_token_field(line: &str) -> bool {
    TOKEN_FIELDS
        .iter()
        .any(|f| line.contains(&format!("{{{f}}}")))
}

/// One token count: plain under 1000, K (one decimal) from 1000 on, empty at 0.
/// A K value with an all-zero decimal drops it: 43008 → "43K", not "43.0K".
fn fmt_token(v: u64) -> String {
    if v == 0 {
        return String::new();
    }
    if v >= 1000 {
        return format!("{:.1}K", v as f64 / 1000.0).replace(".0K", "K");
    }
    v.to_string()
}

/// A line holding token placeholders: [lit, token, lit, token, …, lit].
/// A token with an empty value drops its label segment from the preceding
/// literal (see module docs); everything else renders through the fields.
fn render_token_line(line: &str, fields: &HashMap<&str, String>) -> String {
    let parts = split_token_line(line);
    let mut out = String::new();
    let mut idx = 0;
    while idx < parts.len() {
        if let (Part::Lit(lit), Some(Part::Token(name))) = (&parts[idx], parts.get(idx + 1)) {
            let value = fields.get(name).cloned().unwrap_or_default();
            if value.is_empty() {
                out.push_str(&format_line(&drop_token_label(lit), fields));
            } else {
                out.push_str(&format_line(lit, fields));
                out.push_str(&value);
            }
            idx += 2;
            continue;
        }
        if let Part::Lit(lit) = &parts[idx] {
            out.push_str(&format_line(lit, fields));
        }
        idx += 1;
    }
    out.trim_matches(|c| SEP_CHARS.contains(&c) || c == ' ')
        .to_string()
}

/// Cut a dropped token's label and its separator off a literal run.
///
/// The label is the trailing run of non-space, non-separator characters —
/// "I:" in "I:{input_tokens}", "⬆️" in "⬆️{input_tokens}" — and when the value
/// is zero both go, along with the separator glued to the label (", C:" loses
/// its comma, " ⚡️" loses its space). Shared context to the left survives: in
/// "{device} I:" only "I:" drops, the device keeps its prefix.
fn drop_token_label(lit: &str) -> String {
    let mut end = lit.len();
    for (i, ch) in lit.char_indices().rev() {
        if ch == ' ' || ch == '\t' || SEP_CHARS.contains(&ch) {
            break;
        }
        end = i;
    }
    if end == lit.len() {
        return lit.to_string(); // all context, no trailing label
    }
    let context = &lit[..end];
    // the rightmost separator glued to the label goes with it; a context
    // without one keeps everything
    let cut = SEP_CHARS.iter().filter_map(|c| context.rfind(*c)).max();
    let context = match cut {
        Some(p) => &context[..p],
        None => context,
    };
    context.trim_end().to_string() // drop the pad so the next segment glues on
}

/// Split a line around token placeholders; anything else stays literal
/// (other placeholders included — `format_line` renders those later).
fn split_token_line(line: &str) -> Vec<Part<'_>> {
    let mut parts = Vec::new();
    let mut rest = line;
    'outer: while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        if let Some(end) = after.find('}') {
            let name = &after[..end];
            if TOKEN_FIELDS.contains(&name) {
                parts.push(Part::Lit(&rest[..start]));
                parts.push(Part::Token(name));
                rest = &after[end + 1..];
                continue 'outer;
            }
            // some other placeholder — keep the whole {…} literal so
            // format_line can still pair its braces
            let close = start + 1 + end + 1;
            parts.push(Part::Lit(&rest[..close]));
            rest = &rest[close..];
            continue 'outer;
        }
        // a lone '{' with no closing '}'
        parts.push(Part::Lit(&rest[..start + 1]));
        rest = &rest[start + 1..];
    }
    parts.push(Part::Lit(rest));
    parts
}

/// Lenient placeholder substitution: unknown fields render empty, a lone '{'
/// without a closing '}' stays literal.
fn format_line(s: &str, fields: &HashMap<&str, String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('}') {
            out.push_str(fields.get(&after[..end]).map(String::as_str).unwrap_or(""));
            rest = &after[end + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(footer_format: &str) -> Config {
        Config {
            chat_id: String::new(),
            relay_url: String::new(),
            relay_key: String::new(),
            device: "testbox".into(),
            topic_id: 3,
            proxy: String::new(),
            agent_details: vec![],
            footer_format: footer_format.into(),
        }
    }

    fn target() -> Target {
        Target {
            pane: "w1:p1".into(),
            agent: "claude".into(),
            folder: "~/proj".into(),
        }
    }

    fn usage(i: u64, c: u64, o: u64) -> Usage {
        Usage {
            input_tokens: Some(i),
            cache_read_input_tokens: Some(c),
            output_tokens: Some(o),
        }
    }

    fn default_footer() -> Config {
        cfg(crate::config::DEFAULT_FOOTER_FORMAT)
    }

    #[test]
    fn default_template_title_then_folder() {
        let f = default_footer();
        assert_eq!(
            render(
                &f,
                &target(),
                "✅",
                "fix bug",
                "",
                Some(&usage(1400, 25000, 345))
            ),
            "✅【fix bug】\n🖥testbox 📁~/proj\n⬆️1.4K ⚡️25K ⬇️345"
        );
    }

    #[test]
    fn empty_title_falls_back_to_pane() {
        let f = default_footer();
        assert_eq!(
            render(&f, &target(), "💭", "", "", None),
            "💭【w1:p1】\n🖥testbox 📁~/proj"
        );
    }

    #[test]
    fn empty_folder_falls_back_to_pane() {
        let t = Target {
            folder: String::new(),
            ..target()
        };
        assert_eq!(
            render(&default_footer(), &t, "💭", "", "", None),
            "💭【w1:p1】\n🖥testbox 📁w1:p1"
        );
    }

    #[test]
    fn custom_single_line_template() {
        let f = cfg("{emoji} {device} • {folder} • {title}");
        assert_eq!(
            render(&f, &target(), "💭", "fix bug", "", None),
            "💭 testbox • ~/proj • fix bug"
        );
        assert_eq!(
            render(&f, &target(), "✅", "", "", None),
            "✅ testbox • ~/proj • w1:p1"
        );
    }

    #[test]
    fn multiline_template_drops_empty_lines_and_unknown_fields() {
        let f = cfg("{emoji} {nope} · {folder}\n{title}\n{cwd}");
        assert_eq!(
            render(&f, &target(), "✅", "", "/home/d/proj deep", None),
            "✅ · ~/proj\nw1:p1\n/home/d/proj deep"
        );
    }

    #[test]
    fn unknown_field_renders_empty() {
        let f = cfg("{emoji} {nope} · {folder}");
        assert_eq!(render(&f, &target(), "✅", "", "", None), "✅ · ~/proj");
    }

    #[test]
    fn zero_token_hides_label_and_separator() {
        let f = default_footer();
        assert_eq!(
            render(&f, &target(), "💭", "", "", Some(&usage(1400, 0, 345))),
            "💭【w1:p1】\n🖥testbox 📁~/proj\n⬆️1.4K ⬇️345"
        );
    }

    #[test]
    fn first_token_zero_strips_leading_separator() {
        let f = default_footer();
        assert_eq!(
            render(&f, &target(), "✅", "t", "", Some(&usage(0, 999, 1))),
            "✅【t】\n🖥testbox 📁~/proj\n⚡️999 ⬇️1"
        );
    }

    #[test]
    fn last_token_zero_strips_trailing_separator() {
        let f = default_footer();
        assert_eq!(
            render(&f, &target(), "💭", "", "", Some(&usage(1400, 25000, 0))),
            "💭【w1:p1】\n🖥testbox 📁~/proj\n⬆️1.4K ⚡️25K"
        );
    }

    #[test]
    fn missing_usage_vanishes_the_token_line() {
        assert_eq!(
            render(&default_footer(), &target(), "💭", "", "", None),
            "💭【w1:p1】\n🖥testbox 📁~/proj"
        );
    }

    #[test]
    fn token_k_boundaries() {
        let f = default_footer();
        assert_eq!(
            render(&f, &target(), "🔧", "", "", Some(&usage(999, 1000, 20000))),
            "🔧【w1:p1】\n🖥testbox 📁~/proj\n⬆️999 ⚡️1K ⬇️20K"
        );
    }

    #[test]
    fn colon_label_context_survives_a_dropped_token() {
        let f = cfg(
            "{emoji} {device} I:{input_tokens}, C:{cache_read_input_tokens}, O:{output_tokens}",
        );
        assert_eq!(
            render(&f, &target(), "✅", "", "", Some(&usage(0, 25000, 345))),
            "✅ testbox, C:25K, O:345"
        );
    }
}
