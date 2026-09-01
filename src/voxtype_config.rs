//! Read and write the handful of *voxtype* settings this workflow depends on.
//!
//! This module touches `~/.config/voxtype/config.toml` — a file the operator
//! hand-edits, that ships ~200 lines of explanatory comments, and that
//! upstream's own dashboard on `:8087` may write too. So every rule here exists
//! to make our writes survivable:
//!
//! * **Targeted line replacement, never serialise-and-rewrite.** Parsing to a
//!   `toml::Value` and writing it back would produce a valid file with every
//!   comment gone. The comments are the documentation; losing them is not a
//!   cosmetic regression.
//! * **A backup before every write**, and a refusal if the backup cannot be
//!   written. We are not the only writer of this file.
//! * **Re-parse after editing.** A targeted line edit can still produce invalid
//!   TOML if the value was formatted wrong, and it is better to find that here
//!   than for the daemon to find it at next start.
//!
//! `$VOXTYPE_CONFIG` overrides the path so no test ever touches the real file,
//! matching `$VOXTYPE_REVIEW_CONFIG` in `config.rs`.

use std::path::{Path, PathBuf};

/// What shape a setting's value takes in the file.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// A TOML string.
    Str,
    /// A TOML string *or* an array of strings. `whisper.language` is the reason
    /// this exists: `"en"` and `["en", "nl"]` are both valid and mean different
    /// things, and the array form is what stops whisper wandering into the
    /// other ~97 languages it knows.
    StrOrList,
}

pub struct Field {
    pub id: &'static str,
    pub table: &'static str,
    pub key: &'static str,
    pub kind: Kind,
    pub help: &'static str,
}

/// The settings we surface. Deliberately short: these are the ones that decide
/// what the popup receives. Everything else stays on upstream's dashboard,
/// linked rather than duplicated.
pub const FIELDS: &[Field] = &[
    Field {
        id: "language",
        table: "whisper",
        key: "language",
        kind: Kind::StrOrList,
        help: "One language (en) or several (en, nl). Pinning to one stops \
               whisper drifting; listing several lets it choose between them.",
    },
    Field {
        id: "model",
        table: "whisper",
        key: "model",
        kind: Kind::Str,
        help: "Whisper model name, e.g. large-v3-turbo.",
    },
    Field {
        id: "initial_prompt",
        table: "whisper",
        key: "initial_prompt",
        kind: Kind::Str,
        help: "Primes whisper with vocabulary and register. Written in one \
               language, it biases transcription toward that language.",
    },
    Field {
        id: "output_mode",
        table: "output",
        key: "mode",
        kind: Kind::Str,
        help: "How text reaches the field. \"paste\" goes via the clipboard; \
               \"type\" injects keycodes and scrambles on non-US layouts.",
    },
    Field {
        id: "hotkey_key",
        table: "hotkey",
        key: "key",
        kind: Kind::Str,
        help: "Push-to-talk key, e.g. RIGHTCTRL.",
    },
];

pub fn field(id: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.id == id)
}

/// Where voxtype's config lives.
pub fn path() -> PathBuf {
    if let Ok(explicit) = std::env::var("VOXTYPE_CONFIG") {
        return PathBuf::from(explicit);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/voxtype/config.toml")
}

/// One setting as the page should show it.
pub struct Setting {
    pub id: &'static str,
    pub value: String,
    pub present: bool,
    pub help: &'static str,
}

/// Read the settings we surface. Missing keys come back `present: false` with
/// an empty value rather than an error — a config that has never set
/// `initial_prompt` is normal, not broken.
pub fn read(text: &str) -> Result<Vec<Setting>, String> {
    let doc: toml::Value = toml::from_str(text).map_err(|e| {
        // toml's errors carry a multi-line span; the first line is the useful part.
        e.to_string().lines().next().unwrap_or("invalid TOML").to_string()
    })?;

    let mut out = Vec::with_capacity(FIELDS.len());
    for f in FIELDS {
        let found = doc.get(f.table).and_then(|t| t.get(f.key));
        let (value, present) = match found {
            Some(toml::Value::String(s)) => (s.clone(), true),
            Some(toml::Value::Array(a)) => {
                let parts: Vec<String> = a
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                (parts.join(", "), true)
            }
            Some(other) => (other.to_string(), true),
            None => (String::new(), false),
        };
        out.push(Setting { id: f.id, value, present, help: f.help });
    }
    Ok(out)
}

/// Render a user-supplied value as a TOML literal for this field.
///
/// Returns an error rather than guessing: a value that cannot be represented
/// should be refused at the edge, not written and discovered later.
pub fn literal(f: &Field, value: &str) -> Result<String, String> {
    let v = value.trim();
    if v.contains('\n') || v.contains('\r') {
        return Err("value must be a single line".into());
    }
    match f.kind {
        Kind::Str => {
            if v.is_empty() {
                return Err(format!("{} cannot be empty", f.key));
            }
            Ok(quote(v))
        }
        Kind::StrOrList => {
            let parts: Vec<&str> = v.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
            if parts.is_empty() {
                return Err(format!("{} needs at least one language", f.key));
            }
            for p in &parts {
                // Language tags, not free text: "en", "nl", "zh-TW", "auto".
                if !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                    return Err(format!("\"{p}\" is not a language tag"));
                }
            }
            if parts.len() == 1 {
                Ok(quote(parts[0]))
            } else {
                let items: Vec<String> = parts.iter().map(|p| quote(p)).collect();
                Ok(format!("[{}]", items.join(", ")))
            }
        }
    }
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Replace one key's line in one table, leaving every other byte alone.
///
/// The walk tracks the current table header so `key` in `[whisper]` is not
/// confused with the same `key` in `[output]` — the config has several of those.
/// A commented-out key is treated as absent, and the new line is inserted just
/// after the table header, where it reads next to the comment that documents it.
pub fn set_key(text: &str, table: &str, key: &str, literal: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let mut current = String::new();
    let mut header_at: Option<usize> = None;

    for i in 0..lines.len() {
        let t = lines[i].trim();
        if t.starts_with('[') && t.ends_with(']') {
            current = t.trim_matches(|c| c == '[' || c == ']').to_string();
            if current == table {
                header_at = Some(i);
            }
            continue;
        }
        if current != table || t.starts_with('#') {
            continue;
        }
        // `key = ...`, tolerating whitespace before the `=`.
        let Some(rest) = t.strip_prefix(key) else { continue };
        let rest = rest.trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
        lines[i] = format!("{indent}{key} = {literal}");
        return finish(lines, text);
    }

    match header_at {
        Some(i) => lines.insert(i + 1, format!("{key} = {literal}")),
        None => {
            lines.push(String::new());
            lines.push(format!("[{table}]"));
            lines.push(format!("{key} = {literal}"));
        }
    }
    finish(lines, text)
}

fn finish(lines: Vec<String>, original: &str) -> String {
    let mut out = lines.join("\n");
    if original.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Back the file up beside itself. Refusing here is the point: if we cannot
/// make a backup we do not write, because we are not the only writer.
pub fn backup(path: &Path) -> Result<PathBuf, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Second resolution is coarse enough that two saves a moment apart collide,
    // and the second copy would overwrite the backup holding the *original*
    // state -- the one worth having. Never clobber an existing backup.
    let mut dest = path.with_extension(format!("toml.bak-{stamp}"));
    let mut n = 1;
    while dest.exists() {
        dest = path.with_extension(format!("toml.bak-{stamp}-{n}"));
        n += 1;
        if n > 100 {
            return Err("too many backups in the same second".into());
        }
    }
    std::fs::copy(path, &dest).map_err(|e| format!("could not back up {}: {e}", path.display()))?;
    Ok(dest)
}

/// Apply one setting change to the file on disk.
///
/// Order matters: read, edit, re-parse, back up, write. The re-parse happens
/// before the backup so a malformed edit costs nothing at all.
pub fn save(id: &str, value: &str) -> Result<String, String> {
    let f = field(id).ok_or_else(|| format!("unknown setting \"{id}\""))?;
    let lit = literal(f, value)?;
    let p = path();
    let text = std::fs::read_to_string(&p)
        .map_err(|e| format!("could not read {}: {e}", p.display()))?;

    let updated = set_key(&text, f.table, f.key, &lit);
    toml::from_str::<toml::Value>(&updated)
        .map_err(|e| format!("edit would break the config: {}", e.to_string().lines().next().unwrap_or("")))?;

    if updated == text {
        return Ok("no change".into());
    }

    let b = backup(&p)?;
    std::fs::write(&p, &updated).map_err(|e| format!("could not write {}: {e}", p.display()))?;
    Ok(format!("saved — backup at {}", b.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# voxtype configuration
# Hand-written, with comments that matter.

[hotkey]
key = "RIGHTCTRL"       # push to talk
modifiers = []

[whisper]
# .en models are English-only but faster.
model = "large-v3-turbo"

# Single language: "en". Several: ["en", "nl"].
language = "en"
initial_prompt = "A technical conversation."

[output]
# "paste" goes via the clipboard.
mode = "paste"
"#;

    #[test]
    fn reads_every_surfaced_setting() {
        let s = read(SAMPLE).unwrap();
        let get = |id: &str| s.iter().find(|x| x.id == id).unwrap();
        assert_eq!(get("language").value, "en");
        assert_eq!(get("model").value, "large-v3-turbo");
        assert_eq!(get("output_mode").value, "paste");
        assert_eq!(get("hotkey_key").value, "RIGHTCTRL");
        assert!(get("initial_prompt").present);
    }

    #[test]
    fn a_missing_key_is_absent_not_an_error() {
        let s = read("[whisper]\nmodel = \"tiny\"\n").unwrap();
        let lang = s.iter().find(|x| x.id == "language").unwrap();
        assert!(!lang.present);
        assert_eq!(lang.value, "");
    }

    /// The whole reason this module does line surgery instead of round-tripping
    /// through `toml::Value`.
    #[test]
    fn editing_one_key_leaves_every_comment_and_every_other_line_alone() {
        let out = set_key(SAMPLE, "whisper", "language", r#"["en", "nl"]"#);
        assert!(out.contains(r#"language = ["en", "nl"]"#));

        let before: Vec<&str> = SAMPLE.lines().collect();
        let after: Vec<&str> = out.lines().collect();
        assert_eq!(before.len(), after.len(), "line count changed");
        for (b, a) in before.iter().zip(after.iter()) {
            if b.trim_start().starts_with("language") {
                continue;
            }
            assert_eq!(b, a, "an unrelated line changed");
        }
    }

    /// The decoy table comes **first** on purpose. With it second, a walk that
    /// ignores table headers still happens to hit the right line first and the
    /// test passes for the wrong reason — which is exactly what this one did
    /// until a mutation run caught it.
    #[test]
    fn the_same_key_in_another_table_is_not_touched() {
        let src = "[other]\nkey = \"leave me\"\n\n[hotkey]\nkey = \"RIGHTCTRL\"\n";
        let out = set_key(src, "hotkey", "key", "\"F9\"");
        assert!(out.contains("key = \"leave me\""), "wrote into the wrong table");
        assert!(out.contains("key = \"F9\""), "did not write the right table");
        assert!(!out.contains("\"RIGHTCTRL\""), "old value survived");
    }

    #[test]
    fn a_commented_out_key_counts_as_absent_and_the_comment_survives() {
        let src = "[whisper]\n# language = \"de\"\nmodel = \"tiny\"\n";
        let out = set_key(src, "whisper", "language", "\"nl\"");
        assert!(out.contains("# language = \"de\""), "the comment was consumed");
        assert!(out.contains("language = \"nl\""));
    }

    #[test]
    fn a_missing_table_is_appended_rather_than_dropped() {
        let out = set_key("[whisper]\nmodel = \"tiny\"\n", "output", "mode", "\"paste\"");
        assert!(out.contains("[output]"));
        assert!(out.contains("mode = \"paste\""));
        toml::from_str::<toml::Value>(&out).expect("must still parse");
    }

    #[test]
    fn language_round_trips_in_both_of_its_forms() {
        let f = field("language").unwrap();

        let one = set_key(SAMPLE, "whisper", "language", &literal(f, "nl").unwrap());
        assert_eq!(read(&one).unwrap().iter().find(|s| s.id == "language").unwrap().value, "nl");

        let many = set_key(SAMPLE, "whisper", "language", &literal(f, "en, nl, de").unwrap());
        assert!(many.contains(r#"language = ["en", "nl", "de"]"#));
        assert_eq!(
            read(&many).unwrap().iter().find(|s| s.id == "language").unwrap().value,
            "en, nl, de"
        );
    }

    #[test]
    fn the_file_still_parses_after_every_field_is_written() {
        let mut text = SAMPLE.to_string();
        for f in FIELDS {
            let lit = literal(f, if f.kind == Kind::StrOrList { "en, nl" } else { "value" }).unwrap();
            text = set_key(&text, f.table, f.key, &lit);
        }
        toml::from_str::<toml::Value>(&text).expect("whole file must parse");
    }

    #[test]
    fn a_value_that_cannot_be_represented_is_refused_not_guessed() {
        let lang = field("language").unwrap();
        assert!(literal(lang, "").is_err(), "empty language accepted");
        assert!(literal(lang, "en; rm -rf /").is_err(), "non-tag accepted");

        let model = field("model").unwrap();
        assert!(literal(model, "  ").is_err(), "empty model accepted");
        assert!(literal(model, "a\nb").is_err(), "multi-line accepted");
    }

    #[test]
    fn quotes_and_backslashes_in_a_prompt_survive_the_round_trip() {
        let f = field("initial_prompt").unwrap();
        let raw = r#"He said "hi" \ then left"#;
        let out = set_key(SAMPLE, "whisper", "initial_prompt", &literal(f, raw).unwrap());
        let got = read(&out).unwrap();
        assert_eq!(got.iter().find(|s| s.id == "initial_prompt").unwrap().value, raw);
    }

    #[test]
    fn saving_refuses_an_unknown_setting() {
        assert!(save("not_a_setting", "x").is_err());
    }

    /// Two saves in the same second must not leave one backup. The second copy
    /// would land on the name holding the pre-first-write state, which is the
    /// one an operator actually wants back.
    #[test]
    fn a_second_backup_in_the_same_second_does_not_clobber_the_first() {
        let dir = std::env::temp_dir().join(format!("vxbak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config.toml");
        std::fs::write(&f, "original\n").unwrap();

        let first = backup(&f).unwrap();
        std::fs::write(&f, "changed\n").unwrap();
        let second = backup(&f).unwrap();

        assert_ne!(first, second, "the same backup name was reused");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "original\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "changed\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backing_up_a_file_that_is_not_there_fails_so_the_write_never_happens() {
        assert!(backup(Path::new("/nonexistent/dir/config.toml")).is_err());
    }
}
