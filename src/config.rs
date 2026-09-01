//! Action configuration.
//!
//! An action is a label, a hotkey digit, and a command the transcript is piped
//! through — the shape T-002 §6 argued for, and the same shape Voxtype's own
//! `post_process` uses. Adding one is a config edit, not a rebuild.
//!
//! # Everything here degrades rather than fails
//!
//! Config sits on the path between a dictation and the text that lands in the
//! operator's document. A parse error must never be the reason a dictation is
//! lost, so every failure mode falls back to something workable and says so on
//! stderr: no file → built-in defaults; malformed TOML → built-in defaults;
//! one bad entry → that entry is dropped and the rest load; no valid entries at
//! all → a popup with no actions, which still edits and still accepts.

use crate::core::Action;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Used when an action does not set its own `timeout_ms`. Generous because an
/// action may call a local model; Voxtype's own timeout is the real backstop.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    actions: Vec<RawAction>,
}

#[derive(Debug, Deserialize)]
struct RawAction {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// A validated action set plus the warnings produced while validating it.
/// Warnings are returned rather than printed so tests can assert on them.
#[derive(Debug, Default)]
pub struct Loaded {
    pub actions: Vec<Action>,
    pub timeouts: Vec<Duration>,
    pub warnings: Vec<String>,
    /// True when the built-in defaults were used because config was absent or
    /// unusable. Lets `--list-actions` tell the operator which they are seeing.
    pub used_defaults: bool,
}

impl Loaded {
    pub fn timeout_for(&self, index: usize) -> Duration {
        self.timeouts
            .get(index)
            .copied()
            .unwrap_or(Duration::from_millis(DEFAULT_TIMEOUT_MS))
    }
}

/// Where the config lives. `$VOXTYPE_REVIEW_CONFIG` wins so tests and one-off
/// runs never touch the operator's real file.
pub fn config_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("VOXTYPE_REVIEW_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("voxtype-review").join("config.toml"))
}

/// Load the action set, falling back to defaults at every failure.
pub fn load() -> Loaded {
    match config_path() {
        Some(path) if path.exists() => match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text),
            Err(e) => {
                let mut d = defaults();
                d.warnings
                    .push(format!("could not read {}: {e} — using defaults", path.display()));
                d
            }
        },
        _ => defaults(),
    }
}

/// Parse config text. Public so the failure paths are directly testable.
pub fn parse(text: &str) -> Loaded {
    let raw: RawConfig = match toml::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            let mut d = defaults();
            // Only the first line: toml's errors carry a multi-line span that
            // buries the message in a one-line stderr warning.
            let brief = e.to_string().lines().next().unwrap_or("parse error").to_string();
            d.warnings
                .push(format!("config is not valid TOML ({brief}) — using defaults"));
            return d;
        }
    };

    let global_timeout = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let mut out = Loaded {
        used_defaults: false,
        ..Default::default()
    };

    // serde ignores keys it does not know, so `[[action]]` — a plausible typo,
    // since TOML array-of-tables names read naturally in the singular — parses
    // as perfectly valid TOML and yields nothing. Without this the operator gets
    // an empty popup and no reason for it. Warn, and name the key, before the
    // "no actions" case below has to guess why.
    if let Ok(table) = text.parse::<toml::Table>() {
        for key in table.keys() {
            if key != "actions" && key != "timeout_ms" {
                let hint = if key == "action" {
                    " (did you mean [[actions]]?)"
                } else {
                    ""
                };
                out.warnings
                    .push(format!("config has unknown key \"{key}\"{hint} — ignored"));
            }
        }
    }

    let mut claimed: Vec<char> = Vec::new();

    for (i, entry) in raw.actions.into_iter().enumerate() {
        let position = i + 1;
        let label = entry
            .label
            .clone()
            .unwrap_or_else(|| format!("action {position}"));

        let Some(command) = entry.command.filter(|c| !c.trim().is_empty()) else {
            out.warnings
                .push(format!("action {position} (\"{label}\") has no command — skipped"));
            continue;
        };

        // A key is optional; an INVALID key is not. Silently dropping a bad key
        // would leave a hotkey in the operator's config that quietly does
        // nothing, which is worse than refusing the entry.
        let key = match entry.key.as_deref() {
            None | Some("") => None,
            Some(k) => {
                let mut chars = k.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_ascii_digit() && c != '0' => {
                        if claimed.contains(&c) {
                            out.warnings.push(format!(
                                "action {position} (\"{label}\") reuses hotkey {c} — skipped"
                            ));
                            continue;
                        }
                        claimed.push(c);
                        Some(c)
                    }
                    _ => {
                        out.warnings.push(format!(
                            "action {position} (\"{label}\") has hotkey \"{k}\", must be 1-9 — skipped"
                        ));
                        continue;
                    }
                }
            }
        };

        out.actions.push(Action {
            label,
            key,
            command,
        });
        out.timeouts.push(Duration::from_millis(
            entry.timeout_ms.unwrap_or(global_timeout),
        ));
    }

    // An empty action set stays legal — editing and accepting are the point, the
    // actions are a convenience. But say so, so that "nothing appeared" is never
    // a mystery the operator has to debug by reading source.
    if out.actions.is_empty() {
        out.warnings
            .push("config defines no usable actions — the popup will edit and accept only".into());
    }

    out
}

/// The built-in set. Deliberately small and entirely local: T-002 R4 notes that
/// Voxtype's identity is "no cloud, no telemetry", so a default action must not
/// ship the operator's dictation to a remote API. All of these run on Ollama,
/// which is already installed here.
pub fn defaults() -> Loaded {
    // Ollama's HTTP API, not `ollama run`.
    //
    // `ollama run` writes its progress spinner and word-wrap redraws to stdout
    // even when stdout is a pipe, so a transcript came back carrying `^[[1D`,
    // `^[[K` and `^[[?25l` — unusable as text no matter what the model said. It
    // also composed instruction and stdin unreliably: one run echoed the prompt
    // back, another replied that no sentence had been provided.
    //
    // `jq -Rs` reads the whole transcript and JSON-escapes it, so quotes,
    // newlines and backslashes in a dictation cannot break out of the request.
    // That is why the transcript is not interpolated into the string here.
    // One rule, applied to every action that is not itself a translation.
    //
    // "keeping the original language" — the wording every prompt used before —
    // is understood and then ignored: on a Dutch sentence, hermes3 kept Dutch
    // for "Tidy up" and translated it to English for "remove filler",
    // "concise" and "bullets" (T-018). Naming the rule as an instruction about
    // the *reply* rather than as an attribute of the rewrite holds, 2/2 on the
    // same input where the old wording was 0/2. Measured, not reasoned.
    //
    // It lives here rather than inside each prompt string so an action added
    // later inherits it instead of quietly not having it.
    //
    // The individual prompts no longer mention language at all, and that is not
    // tidying: stating the rule twice is actively worse than stating it once.
    // With both the centralised rule and "keep it in the language it was spoken
    // in" present, "Tidy up" answered an ENGLISH sentence in Spanish and Turkish
    // 4 times out of 4. Emphasis on the language axis makes an 8B model pick a
    // language rather than keep one.
    const KEEP_LANGUAGE: &str =
        " Reply in the SAME language as the input text. Never translate it.";

    let compose = |prompt: &str, rule: &str| {
        format!(
            "jq -Rs --arg m hermes3:8b --arg p '{prompt}{rule} Output only the resulting text, \
with no preamble, no explanation and no quotation marks.' \
'{{model:$m,prompt:($p+\"\\n\\n\"+.),stream:false}}' \
| curl -sf --max-time 110 http://localhost:11434/api/generate -d @- | jq -r '.response'"
        )
    };

    let mk = |label: &str, key: char, prompt: &str| Action {
        label: label.to_string(),
        key: Some(key),
        command: compose(prompt, KEEP_LANGUAGE),
    };

    // "Translate to English" is the one action whose whole job is to change the
    // language, so it must not inherit the rule that forbids that.
    let mk_translating = |label: &str, key: char, prompt: &str| Action {
        label: label.to_string(),
        key: Some(key),
        command: compose(prompt, ""),
    };

    let actions = vec![
        mk(
            "Tidy up",
            '1',
            "Fix grammar, spelling and punctuation in this dictation. Preserve the meaning.",
        ),
        mk(
            "Tidy up + remove filler",
            '2',
            "Fix grammar and punctuation in this dictation and remove filler words and false starts. Preserve the meaning.",
        ),
        mk_translating(
            // "If it is already English, return it unchanged" earns its place:
            // without it, hermes3 handed back "Okay, so here is the translation
            // of the provided text into English:" and then the text. Asking a
            // model to translate English into English invites it to explain
            // itself instead.
            "Translate to English",
            '3',
            "Translate this text into English. If it is already in English, return it unchanged.",
        ),
        mk(
            "Make it concise",
            '4',
            "Rewrite this text to be shorter and clearer while keeping every point.",
        ),
        mk(
            "Format as bullet points",
            '5',
            "Rewrite this text as a concise bulleted list.",
        ),
    ];

    let timeouts = vec![Duration::from_millis(DEFAULT_TIMEOUT_MS); actions.len()];
    Loaded {
        actions,
        timeouts,
        warnings: Vec::new(),
        used_defaults: true,
    }
}

/// TOML basic-string escaping, for embedding a command in the starter file.
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The commented starter file written by `--write-default-config`.
///
/// The action entries are **generated from `defaults()`**, not transcribed
/// beside them. They were transcribed until T-018, and they drifted exactly as
/// a transcription does: T-015 replaced the shipped `ollama run` commands with
/// HTTP-API ones in `defaults()` and this literal kept the broken originals —
/// so `install.sh`, which calls `--write-default-config`, wrote the defect onto
/// every fresh machine while `--list-actions` on a configured machine showed
/// the fix. A test asserts the two round-trip.
pub fn starter_file() -> String {
    let entries: Vec<String> = defaults()
        .actions
        .iter()
        .map(|a| {
            let key = a
                .key
                .map(|c| format!("key = \"{c}\"\n"))
                .unwrap_or_default();
            format!(
                "[[actions]]\nlabel = \"{}\"\n{}command = \"{}\"\n",
                toml_escape(&a.label),
                key,
                toml_escape(&a.command)
            )
        })
        .collect();
    let actions = entries.join("\n");

    r##"# voxtype-review — action configuration
#
# Each [[actions]] entry is one thing you can do to a transcript before it is
# inserted. The transcript arrives on the command's stdin; whatever the command
# prints on stdout is what gets typed.
#
#   label       shown in the popup
#   key         hotkey digit, "1".."9". Optional — omit to make it
#               arrow-selectable only. Must be unique.
#   command     any shell command. Pipes, quotes and scripts are all fine.
#   timeout_ms  optional per-action override of the global timeout below
#
# If an entry is malformed it is skipped with a warning on stderr and the rest
# still load. If this whole file is unparseable the built-in defaults are used.
# A transcript is never lost because of a config error.
#
# Check what actually loaded:   voxtype-review --list-actions

# Global fallback timeout for any action that does not set its own.
timeout_ms = 120000

{ACTIONS}
# Actions do not have to involve a language model. Anything that reads stdin
# and writes stdout works:
#
# [[actions]]
# label = "UPPERCASE"
# key = "6"
# command = "tr a-z A-Z"
#
# [[actions]]
# label = "Strip trailing whitespace"
# key = "7"
# command = "sed 's/[[:space:]]*$//'"
"##
    .replace("{ACTIONS}", &actions)
    .to_string()
}

/// Write the starter file. Refuses to clobber without `force`, because the
/// operator's action list is theirs and may be hand-tuned.
pub fn write_default_config(path: &Path, force: bool) -> Result<(), String> {
    if path.exists() && !force {
        return Err(format!(
            "{} already exists — pass --force to overwrite it",
            path.display()
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, starter_file())
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_config() {
        let got = parse(
            r#"
            [[actions]]
            label = "Upper"
            key = "1"
            command = "tr a-z A-Z"

            [[actions]]
            label = "Lower"
            key = "2"
            command = "tr A-Z a-z"
            "#,
        );
        assert_eq!(got.actions.len(), 2);
        assert_eq!(got.actions[0].label, "Upper");
        assert_eq!(got.actions[0].key, Some('1'));
        assert!(got.warnings.is_empty());
        assert!(!got.used_defaults);
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults_with_a_warning() {
        let got = parse("this is not toml [[[");
        assert!(got.used_defaults);
        assert!(!got.actions.is_empty(), "must still offer actions");
        assert_eq!(got.warnings.len(), 1);
        assert!(got.warnings[0].contains("not valid TOML"));
    }

    #[test]
    fn an_action_without_a_command_is_skipped_and_the_rest_survive() {
        let got = parse(
            r#"
            [[actions]]
            label = "Broken"
            key = "1"

            [[actions]]
            label = "Fine"
            key = "2"
            command = "cat"
            "#,
        );
        assert_eq!(got.actions.len(), 1);
        assert_eq!(got.actions[0].label, "Fine");
        assert!(got.warnings[0].contains("no command"));
        assert!(got.warnings[0].contains("Broken"), "warning must name it");
    }

    #[test]
    fn an_out_of_range_hotkey_is_rejected() {
        for bad in ["0", "a", "12", "!"] {
            let got = parse(&format!(
                r#"
                [[actions]]
                label = "Bad"
                key = "{bad}"
                command = "cat"
                "#
            ));
            assert_eq!(got.actions.len(), 0, "key {bad:?} should be rejected");
            assert!(got.warnings[0].contains("must be 1-9"));
        }
    }

    #[test]
    fn a_duplicate_hotkey_is_rejected_and_the_first_wins() {
        let got = parse(
            r#"
            [[actions]]
            label = "First"
            key = "1"
            command = "cat"

            [[actions]]
            label = "Second"
            key = "1"
            command = "cat"
            "#,
        );
        assert_eq!(got.actions.len(), 1);
        assert_eq!(got.actions[0].label, "First");
        assert!(got.warnings[0].contains("reuses hotkey"));
    }

    #[test]
    fn an_action_may_have_no_hotkey_at_all() {
        let got = parse(
            r#"
            [[actions]]
            label = "Arrow only"
            command = "cat"
            "#,
        );
        assert_eq!(got.actions.len(), 1);
        assert_eq!(got.actions[0].key, None);
        assert!(got.warnings.is_empty());
    }

    #[test]
    fn an_empty_config_is_legal_and_yields_no_actions() {
        let got = parse("");
        assert!(got.actions.is_empty());
        assert!(!got.used_defaults, "empty is a deliberate choice, not a failure");
        // Legal, but never silent — see the note at the end of `parse`.
        assert!(got.warnings.iter().any(|w| w.contains("no usable actions")));
    }

    #[test]
    fn a_singular_action_key_is_named_rather_than_silently_ignored() {
        // `[[action]]` is valid TOML that serde discards, so this used to produce
        // an empty popup and no explanation at all.
        let got = parse(
            r#"
            [[action]]
            key = "1"
            label = "Tidy"
            command = "cat"
            "#,
        );
        assert!(got.actions.is_empty());
        assert!(
            got.warnings.iter().any(|w| w.contains("unknown key \"action\"")
                && w.contains("[[actions]]")),
            "the warning must name the key and the correction: {:?}",
            got.warnings
        );
    }

    #[test]
    fn an_unknown_top_level_key_is_reported_but_does_not_lose_good_actions() {
        let got = parse(
            r#"
            wibble = 3

            [[actions]]
            key = "1"
            label = "Tidy"
            command = "cat"
            "#,
        );
        assert_eq!(got.actions.len(), 1, "one bad key must not cost a good action");
        assert!(got.warnings.iter().any(|w| w.contains("wibble")));
    }

    #[test]
    fn per_action_timeout_overrides_the_global_one() {
        let got = parse(
            r#"
            timeout_ms = 5000

            [[actions]]
            label = "Fast"
            key = "1"
            command = "cat"
            timeout_ms = 250

            [[actions]]
            label = "Inherits"
            key = "2"
            command = "cat"
            "#,
        );
        assert_eq!(got.timeout_for(0), Duration::from_millis(250));
        assert_eq!(got.timeout_for(1), Duration::from_millis(5000));
    }

    #[test]
    fn an_unknown_index_gets_the_default_timeout() {
        let got = parse("");
        assert_eq!(
            got.timeout_for(99),
            Duration::from_millis(DEFAULT_TIMEOUT_MS)
        );
    }

    /// The starter file is *generated* from `defaults()` rather than transcribed,
    /// and this is the test that keeps it that way. Shape assertions alone
    /// (five entries, keys 1..5) pass even when every command is wrong — which
    /// is exactly how the T-015 defect reached `install.sh --write-default-config`
    /// after the built-in defaults had already been fixed. Compare the commands.
    ///
    /// Timeouts are deliberately not compared: the starter file carries only the
    /// global `timeout_ms` and no per-action override, so a parse of it cannot
    /// reproduce the defaults' timeout map. Label, key and command are the parts
    /// that can drift silently.
    #[test]
    fn the_starter_file_round_trips_to_exactly_the_built_in_defaults() {
        let want = defaults();
        let got = parse(&starter_file());
        assert!(got.warnings.is_empty(), "starter file must parse clean: {:?}", got.warnings);
        assert_eq!(got.actions.len(), want.actions.len(), "action count drifted");
        for (i, (g, w)) in got.actions.iter().zip(want.actions.iter()).enumerate() {
            assert_eq!(g.label, w.label, "action {i}: label drifted");
            assert_eq!(g.key, w.key, "action {i}: hotkey drifted");
            assert_eq!(g.command, w.command, "action {i}: command drifted from the built-in default");
        }
    }

    /// T-015: `ollama run` writes progress spinners and ANSI escapes to stdout,
    /// so its output reached the text field as garbage. The fix replaced it with
    /// the HTTP API, but `install.sh` calls `--write-default-config`, so a
    /// starter file still carrying the old command would hand the defect to
    /// every fresh install regardless of what `defaults()` says.
    #[test]
    fn the_starter_file_never_ships_the_command_that_broke_t015() {
        let text = starter_file();
        assert!(
            !text.contains("ollama run"),
            "starter file ships `ollama run`, the exact command T-015 removed"
        );
        for a in parse(&text).actions {
            assert!(
                !a.command.contains("ollama run"),
                "action {:?} still calls `ollama run`",
                a.label
            );
        }
    }

    #[test]
    fn the_builtin_defaults_are_internally_consistent() {
        let d = defaults();
        assert_eq!(d.actions.len(), d.timeouts.len());
        let mut keys: Vec<char> = d.actions.iter().filter_map(|a| a.key).collect();
        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(before, keys.len(), "default hotkeys must be unique");
        assert!(d.actions.iter().all(|a| !a.command.trim().is_empty()));
    }

    #[test]
    fn write_default_config_refuses_to_clobber_without_force() {
        let dir = std::env::temp_dir().join(format!("vr-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(write_default_config(&path, false).is_ok());
        let err = write_default_config(&path, false).unwrap_err();
        assert!(err.contains("already exists"));
        assert!(write_default_config(&path, true).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
