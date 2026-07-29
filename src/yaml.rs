//! A small block-style YAML writer that matches what `yq .` prints.
//!
//! libyaml — and so `serde_yaml_ng::to_string` — writes block sequences at the
//! indentation of the key that owns them:
//!
//! ```yaml
//! organizations:
//! - one
//! ```
//!
//! `yq` indents them under the key instead, which is what people expect a
//! pretty-printed file to look like:
//!
//! ```yaml
//! organizations:
//!   - one
//! ```
//!
//! Rather than emit through libyaml and re-indent the text afterwards — which
//! cannot be done safely once a scalar spans lines — this walks the serialized
//! value and writes it directly, choosing quoting the way `yq` does. Running
//! `yq .` over the result is a no-op.
//!
//! One deliberate difference: `yq` writes a string containing newlines as a
//! literal block (`|-`), while this writes it double-quoted with `\n`. Both
//! parse back to the same string, and no field GitHub returns here spans
//! lines, so it does not come up in practice.

use std::fmt::Write as _;

use serde::Serialize;
use serde_yaml_ng::{Mapping, Value};

const INDENT: usize = 2;

/// Serialize `value` as block YAML in `yq`'s layout.
pub fn to_string<T: Serialize>(value: &T) -> Result<String, serde_yaml_ng::Error> {
    let value = serde_yaml_ng::to_value(value)?;
    let mut out = String::new();
    match &value {
        Value::Mapping(map) if !map.is_empty() => write_mapping(map, 0, &mut out),
        Value::Sequence(seq) if !seq.is_empty() => write_sequence(seq, 0, &mut out),
        other => {
            out.push_str(&flow(other));
            out.push('\n');
        }
    }
    Ok(out)
}

fn write_mapping(map: &Mapping, indent: usize, out: &mut String) {
    for (key, value) in map {
        pad(indent, out);
        out.push_str(&flow(key));
        out.push(':');
        match value {
            Value::Mapping(child) if !child.is_empty() => {
                out.push('\n');
                write_mapping(child, indent + INDENT, out);
            }
            Value::Sequence(child) if !child.is_empty() => {
                out.push('\n');
                write_sequence(child, indent + INDENT, out);
            }
            scalar => {
                out.push(' ');
                out.push_str(&flow(scalar));
                out.push('\n');
            }
        }
    }
}

fn write_sequence(seq: &[Value], indent: usize, out: &mut String) {
    for item in seq {
        match item {
            Value::Mapping(child) if !child.is_empty() => {
                let mut buf = String::new();
                write_mapping(child, indent + INDENT, &mut buf);
                push_dashed(&buf, indent, out);
            }
            Value::Sequence(child) if !child.is_empty() => {
                let mut buf = String::new();
                write_sequence(child, indent + INDENT, &mut buf);
                push_dashed(&buf, indent, out);
            }
            scalar => {
                pad(indent, out);
                out.push_str("- ");
                out.push_str(&flow(scalar));
                out.push('\n');
            }
        }
    }
}

/// Write an already-rendered block at `indent + INDENT`, replacing the padding
/// on its first line with `- ` so the item's first key sits on the dash line.
fn push_dashed(block: &str, indent: usize, out: &mut String) {
    pad(indent, out);
    out.push_str("- ");
    // The block was rendered at `indent + INDENT`, so its first line starts
    // with exactly that many ASCII spaces; the dash takes their place.
    out.push_str(&block[indent + INDENT..]);
}

fn pad(indent: usize, out: &mut String) {
    for _ in 0..indent {
        out.push(' ');
    }
}

/// One-line rendering of a value: a scalar, or a flow collection for the empty
/// and complex-key cases that block style cannot express inline.
fn flow(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => string_scalar(s),
        Value::Sequence(seq) => {
            let items: Vec<String> = seq.iter().map(flow).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Mapping(map) => {
            let items: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", flow(k), flow(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        Value::Tagged(tagged) => {
            let mut rendered = String::new();
            let _ = write!(rendered, "{} {}", tagged.tag, flow(&tagged.value));
            rendered
        }
    }
}

/// Pick a scalar style the way `yq` does: double quotes when the text would
/// otherwise read back as some other type or needs escapes, single quotes when
/// only YAML syntax is in the way, and no quotes at all when neither applies.
fn string_scalar(s: &str) -> String {
    if needs_escaping(s) || resolves_to_non_string(s) {
        double_quoted(s)
    } else if is_plain_safe(s) {
        s.to_string()
    } else {
        single_quoted(s)
    }
}

/// Characters that cannot survive in an unescaped scalar: control characters,
/// the separators YAML treats as line breaks, and anything above the basic
/// plane, which `yq` escapes rather than emitting raw.
fn needs_escaping(s: &str) -> bool {
    s.chars().any(|c| {
        c.is_control() || c as u32 > 0xFFFF || matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}')
    })
}

/// Whether a string can be written without quotes and read back unchanged.
/// Callers rule out escapes and type collisions first; this covers syntax.
fn is_plain_safe(s: &str) -> bool {
    if s.is_empty() || s.trim() != s {
        return false;
    }
    let first = s.chars().next().expect("non-empty");
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    // `: ` opens a mapping and ` #` opens a comment, anywhere in the scalar.
    !(s.contains(": ") || s.contains(" #") || s.ends_with(':'))
}

fn single_quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\u{1b}' => out.push_str("\\e"),
            '\u{85}' => out.push_str("\\N"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            c if c.is_control() => {
                let _ = write!(out, "\\x{:02X}", c as u32);
            }
            c if c as u32 > 0xFFFF => {
                let _ = write!(out, "\\U{:08X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Whether a plain scalar would come back as a bool, null or number rather
/// than a string. Mirrors what a YAML 1.2 core parser resolves, which is also
/// what `yq` quotes against: `TRUE` and `0755` are numbers-in-waiting, while
/// `yes`, `On`, `inf` and `1:30` are ordinary strings.
fn resolves_to_non_string(s: &str) -> bool {
    if matches!(
        s,
        "" | "~"
            | "null"
            | "Null"
            | "NULL"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
    ) {
        return true;
    }
    is_number_like(s)
}

fn is_number_like(s: &str) -> bool {
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    if digits.is_empty() {
        return false;
    }
    if matches!(digits, ".inf" | ".Inf" | ".INF" | ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    // Radix-prefixed integers, with the underscore separators Go's parser —
    // and so `yq`'s resolver — accepts.
    let lower = digits.to_ascii_lowercase();
    for (prefix, radix) in [("0x", 16u32), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let rest = rest.replace('_', "");
            return !rest.is_empty() && rest.chars().all(|c| c.is_digit(radix));
        }
    }
    // Decimal integers and floats: at least one digit, and nothing but digits,
    // one dot and one exponent.
    let plain = lower.replace('_', "");
    if plain.is_empty() || !plain.contains(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let (mantissa, exponent) = match plain.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (plain.as_str(), None),
    };
    if let Some(exponent) = exponent {
        let exponent = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
        if exponent.is_empty() || !exponent.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    let mut parts = mantissa.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    parts.next().is_none()
        && whole.chars().all(|c| c.is_ascii_digit())
        && fraction.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml_ng::Value;

    fn emit(yaml: &str) -> String {
        let value: Value = serde_yaml_ng::from_str(yaml).expect("input parses");
        to_string(&value).expect("emits")
    }

    /// Emitting and re-parsing must give back exactly what went in.
    fn round_trips(yaml: &str) -> String {
        let original: Value = serde_yaml_ng::from_str(yaml).expect("input parses");
        let emitted = to_string(&original).expect("emits");
        let reparsed: Value = serde_yaml_ng::from_str(&emitted).expect("output parses");
        assert_eq!(
            reparsed, original,
            "round trip changed the data:\n{emitted}"
        );
        emitted
    }

    #[test]
    fn sequences_are_indented_under_their_key() {
        assert_eq!(
            round_trips("organizations: [one, two]"),
            "organizations:\n  - one\n  - two\n"
        );
    }

    #[test]
    fn a_sequence_of_mappings_starts_on_the_dash_line() {
        let emitted = round_trips("repositories: [{org: acme, name: one}, {org: acme, name: two}]");
        assert_eq!(
            emitted,
            "repositories:\n  - org: acme\n    name: one\n  - org: acme\n    name: two\n"
        );
    }

    #[test]
    fn nesting_keeps_stacking_by_two() {
        let emitted = round_trips("a: {b: {c: [d]}}");
        assert_eq!(emitted, "a:\n  b:\n    c:\n      - d\n");
    }

    #[test]
    fn a_sequence_inside_a_sequence_item_is_indented_too() {
        let emitted = round_trips("repositories: [{org: acme, topics: [x, y]}]");
        assert_eq!(
            emitted,
            "repositories:\n  - org: acme\n    topics:\n      - x\n      - y\n"
        );
    }

    #[test]
    fn empty_collections_stay_inline() {
        assert_eq!(
            round_trips("{repositories: [], source: {}}"),
            "repositories: []\nsource: {}\n"
        );
    }

    /// Text that a parser would hand back as a bool, null or number gets
    /// double quotes — the style `yq` uses for exactly this case.
    #[test]
    fn scalars_that_would_change_type_are_double_quoted() {
        let emitted = round_trips(
            r#"{a: "true", b: "TRUE", c: "123", d: "1.5", e: "null", f: "0x1f", g: "0755", h: "1_000", i: "+7", j: "1e5", k: ""}"#,
        );
        for line in emitted.lines() {
            let (_, value) = line.split_once(": ").expect("one pair per line");
            assert!(value.starts_with('"'), "expected double quotes in `{line}`");
        }
    }

    /// Text that only collides with YAML punctuation gets single quotes, which
    /// is again what `yq` picks.
    #[test]
    fn punctuation_that_yaml_reads_as_syntax_is_single_quoted() {
        let emitted = round_trips(
            r##"{a: "- dash first", b: "key: value", c: "trailing #comment", d: "ends with:", e: " padded ", f: "#hash", g: "*anchor"}"##,
        );
        for line in emitted.lines() {
            let (_, value) = line.split_once(": ").expect("one pair per line");
            assert!(
                value.starts_with('\''),
                "expected single quotes in `{line}`"
            );
        }
        assert!(round_trips("a: \"it's\"").contains("a: it's\n"));
        assert!(round_trips("a: \"it's: here\"").contains("a: 'it''s: here'\n"));
    }

    /// YAML 1.1 read `yes` and `On` as booleans; the 1.2 core schema that `yq`
    /// and this crate's parser follow does not, so they stay unquoted.
    #[test]
    fn words_only_yaml_1_1_called_booleans_stay_plain() {
        assert_eq!(
            round_trips("{a: 'yes', b: 'On', c: 'inf', d: 'nan', e: '1:30'}"),
            "a: yes\nb: On\nc: inf\nd: nan\ne: 1:30\n"
        );
    }

    #[test]
    fn ordinary_text_stays_plain() {
        let emitted = round_trips(
            "{url: 'https://github.com/acme/one', when: '2024-09-17T07:53:35Z', name: liboqs-dotnet, desc: 'A toolset for CBOM (v2), fast', bmp: 'café → x'}",
        );
        assert!(emitted.contains("url: https://github.com/acme/one\n"));
        assert!(emitted.contains("when: 2024-09-17T07:53:35Z\n"));
        assert!(emitted.contains("name: liboqs-dotnet\n"));
        assert!(emitted.contains("desc: A toolset for CBOM (v2), fast\n"));
        assert!(emitted.contains("bmp: café → x\n"));
    }

    /// `yq` escapes anything above the basic multilingual plane, so emoji in a
    /// repository description come out as `\U…` rather than raw.
    #[test]
    fn characters_above_the_basic_plane_are_escaped() {
        assert_eq!(
            round_trips("desc: 🌔 Perun's framework"),
            "desc: \"\\U0001F314 Perun's framework\"\n"
        );
    }

    #[test]
    fn strings_that_span_lines_are_escaped_onto_one_line() {
        let emitted = round_trips("desc: \"one\\ntwo\"");
        assert_eq!(emitted, "desc: \"one\\ntwo\"\n");
    }

    #[test]
    fn numbers_and_booleans_are_not_quoted() {
        assert_eq!(
            round_trips("{stars: 120, archived: false, ratio: 1.5}"),
            "stars: 120\narchived: false\nratio: 1.5\n"
        );
    }

    #[test]
    fn a_top_level_sequence_starts_at_the_left_margin() {
        assert_eq!(emit("[one, two]"), "- one\n- two\n");
    }
}
