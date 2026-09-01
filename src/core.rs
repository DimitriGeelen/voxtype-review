//! Toolkit-independent core: the parts of `voxtype-review` that must be correct
//! whether or not a GUI ever opens.
//!
//! Everything here is driven by measurements taken in T-007 on this host:
//!   * focus does NOT return to the origin window unaided on xfwm4, so we capture
//!     it and restore it ourselves;
//!   * Voxtype's output mode is `paste`, so the clipboard is its delivery channel
//!     and we never touch it;
//!   * losing a dictation is the one unacceptable outcome, so every failure path
//!     emits the original text.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

/// The window that was focused when we started — i.e. the one the operator
/// dictated into, and the one Voxtype will paste into once we exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub window_id: Option<String>,
}

/// Trace to stderr when `VOXTYPE_REVIEW_DEBUG` is set. Voxtype ignores our
/// stderr, so this is safe to leave in — and focus bugs are invisible without it.
pub fn trace(msg: &str) {
    if std::env::var_os("VOXTYPE_REVIEW_DEBUG").is_some() {
        eprintln!("[voxtype-review] {msg}");
    }
}

/// Ask X which window is focused right now. Empty when X won't say.
fn active_window() -> String {
    Command::new("xdotool")
        .arg("getactivewindow")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The window holding the X SERVER'S core input focus. This is the window
/// keystrokes are delivered to — which voxtype's ydotool Ctrl+V follows —
/// and it is NOT always the window the WM reports as active. T-023: after
/// the popup closes, xfwm4's active-window belief and the core focus can
/// point at different windows; restore() used to verify only the WM's
/// belief while the paste went to the core-focus window (a Chromium wid in
/// the field failure, a ghost wid in the isolated repro). When they
/// disagree, `getwindowfocus` is the one that matters.
fn input_focus() -> String {
    Command::new("xdotool")
        .arg("getwindowfocus")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Point both the WM's notion AND the core input focus at `id`. Activation
/// alone leaves the core focus wherever the WM's handoff left it; keystrokes
/// then go elsewhere while every "active window" check still passes.
fn focus_window(id: &str) {
    let _ = Command::new("xdotool")
        .args(["windowactivate", "--sync", id])
        .status();
    let _ = Command::new("xdotool")
        .args(["windowfocus", "--sync", id])
        .status();
}

/// True when no voxtype-review window exists anymore. `xdotool search` fails
/// when nothing matches, which is exactly the answer we want.
fn our_window_gone() -> bool {
    Command::new("xdotool")
        .args(["search", "--classname", "voxtype-review"])
        .output()
        .map(|o| !o.status.success() || String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(true)
}

impl Origin {
    /// Ask X which window is focused. A `None` id is not an error: the binary is
    /// still usable, it just cannot restore focus, and `--no-gui` never needs to.
    pub fn capture() -> Self {
        let now = active_window();
        let window_id = if now.is_empty() { None } else { Some(now) };
        trace(&format!("captured origin window: {window_id:?}"));
        Origin { window_id }
    }

    /// Hand focus back before Voxtype pastes. Best-effort by design: if this
    /// fails we still emit the text, because emitting into the wrong window is
    /// bad but emitting nothing loses the dictation entirely.
    ///
    /// Retries: `windowactivate --sync` can race the window manager while it is
    /// still tearing down the popup, and on xfwm4 a single early attempt can
    /// "succeed" and then be overridden as the WM settles. We activate, verify,
    /// and retry a few times rather than trusting the exit code alone.
    ///
    /// Ordering (T-023): the popup window's destroy event is processed by the
    /// WM asynchronously — possibly after our activation. When that happens,
    /// xfwm4 hands focus to its own next choice (measured: the MRU window, a
    /// Chromium instance), and Voxtype's later Ctrl+V pastes the dictation
    /// there. So we first wait until our window is really gone, THEN activate;
    /// `hold()` covers whatever stragglers remain.
    pub fn restore(&self) -> bool {
        let Some(id) = self.window_id.as_deref() else {
            trace("no origin window to restore");
            return false;
        };

        // Phase 0: our own window must be destroyed first. Activating before
        // the WM has seen the destroy means our activation is immediately
        // overridden by its focus handoff.
        for attempt in 1..=10 {
            if our_window_gone() {
                trace(&format!("popup window gone after {attempt} probe(s)"));
                break;
            }
            if attempt == 10 {
                trace("popup window still mapped after 1s — activating anyway");
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        for attempt in 1..=5 {
            focus_window(id);
            std::thread::sleep(Duration::from_millis(60));

            let focused = input_focus();
            if focused == id && active_window() == id {
                trace(&format!("focus restored to {id} on attempt {attempt}"));
                return true;
            }
            trace(&format!(
                "attempt {attempt}: core focus is {focused}, active is {}, wanted {id}",
                active_window()
            ));
            std::thread::sleep(Duration::from_millis(80));
        }

        trace(&format!("FAILED to restore focus to {id}"));
        false
    }

    /// Keep focus on the origin window for a grace period after `emit`.
    ///
    /// Voxtype cannot paste before our stdout reaches EOF, and EOF arrives
    /// when this process exits — so time spent here delays the paste at zero
    /// cost, and any focus steal that lands inside the window (the same late
    /// WM handoff `restore` guards against, arriving after its final check)
    /// gets undone before the Ctrl+V goes out. T-023: without this, a m6
    /// roundtrip cancelled dictation pasted into a Chromium window that took
    /// focus ~700ms after the popup closed.
    ///
    /// Like `restore`, this never fights the transcript: it runs after the
    /// output is written, so even a total failure here only risks focus, not
    /// text. And it gives up quietly if there is no origin to hold.
    pub fn hold(&self, ms: u64) {
        let Some(id) = self.window_id.as_deref() else {
            return;
        };
        let mut steals = 0;
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            let now = input_focus();
            if !now.is_empty() && now != id {
                steals += 1;
                trace(&format!("hold: core focus stolen by {now} — taking it back"));
                focus_window(id);
            }
        }
        if steals == 0 {
            trace("hold: focus stayed on the origin for the whole grace window");
        } else {
            trace(&format!("hold: undid {steals} steal(s) during the grace window"));
        }
    }
}

/// Read the transcript Voxtype pipes in. Invalid UTF-8 is replaced rather than
/// rejected — a mangled transcript is still better than no transcript.
pub fn read_input() -> String {
    let mut buf = Vec::new();
    if std::io::stdin().read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Write the result Voxtype will type. No trailing newline is added: Voxtype
/// trims by default (`[output.post_process] trim = true`), but a hook that does
/// not trim should not receive whitespace we invented.
pub fn emit(text: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// One predefined transformation the operator can apply to the transcript.
///
/// Declarative on purpose (T-002 §6): an action is a label, a digit, and a
/// command the text is piped through. Adding one is a config edit, not a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// Shown in the popup.
    pub label: String,
    /// Hotkey digit, 1-9. `None` means selectable by arrows only.
    pub key: Option<char>,
    /// Shell command; the text arrives on stdin and the result is read from stdout.
    pub command: String,
}

/// Why an action produced no usable result.
///
/// The variants exist because each one sends the operator somewhere different: a
/// missing binary is a shell problem, a non-zero exit is the command complaining
/// about its own arguments, a timeout is usually a model still loading or absent,
/// empty output is a command that ran perfectly and had nothing to say, and a
/// missing action means nothing ran because the list changed underneath them.
///
/// Before T-020 the process ones were a single observable — the transcript,
/// returned unchanged, with no message on any surface — and the only available
/// reading of that was "the model ignored me", which is the one explanation that
/// is never true. T-022 closed the last hole, in `resolve` rather than here.
///
/// Adding a variant is deliberately not free: `message()` matches exhaustively,
/// so a new one cannot be introduced without deciding what the operator is told.
/// That is the property to preserve — a catch-all arm here would quietly restore
/// the silence this type exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionFailure {
    /// The OS would not run the command for us, or would not let us watch it.
    /// Both land here because the operator's next move is the same for either.
    Spawn(String),
    /// It ran and exited non-zero. `stderr` is its last non-empty line, which is
    /// where `jq: command not found` actually lives.
    Exit {
        code: Option<i32>,
        stderr: Option<String>,
    },
    /// It outlived its timeout and was killed.
    Timeout { ms: u128 },
    /// It succeeded and produced nothing usable.
    Empty { stderr: Option<String> },
    /// The popup offered an action the loaded config no longer has, so nothing
    /// ran at all. The odd one out on purpose: there is no exit code and no
    /// stderr because there was no child process. `position` is 1-based, counting
    /// the way the operator counts a list they are looking at.
    NoSuchAction { position: usize, available: usize },
}

impl ActionFailure {
    /// One line addressed to the operator, quoting the command's own words where
    /// it said any. The quoted line is the part that solves the problem — the
    /// rest only says which of our paths noticed.
    pub fn message(&self) -> String {
        match self {
            ActionFailure::Spawn(e) => format!("could not start the action: {e}"),
            ActionFailure::Exit { code, stderr } => {
                let c = match code {
                    Some(c) => format!("exited {c}"),
                    None => "was killed by a signal".to_string(),
                };
                match stderr {
                    Some(s) => format!("the action {c}: {s}"),
                    None => format!("the action {c} and wrote nothing to stderr"),
                }
            }
            ActionFailure::Timeout { ms } => {
                format!("the action was still running after {ms}ms and was stopped")
            }
            ActionFailure::Empty { stderr } => match stderr {
                Some(s) => format!("the action produced no output: {s}"),
                None => "the action produced no output".to_string(),
            },
            ActionFailure::NoSuchAction {
                position,
                available,
            } => {
                let list = match available {
                    0 => "the action list is now empty".to_string(),
                    1 => "the action list now has 1 action".to_string(),
                    n => format!("the action list now has {n} actions"),
                };
                format!(
                    "you asked for action {position}, but {list} — \
                     it changed while the popup was open"
                )
            }
        }
    }
}

/// What an action produced: the text to use, and why it is the original if it is.
///
/// One type rather than an optional second entry point. `run_action` used to
/// return a bare `String`, so no caller could distinguish a successful no-op from
/// a crash — and `webui::test_action` was reduced to guessing by comparing the
/// output to the input, a proxy that is wrong in both directions. A signature
/// that can hand back the text without the diagnosis is exactly how this defect
/// returns, so there is deliberately no such signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutcome {
    /// Always safe to paste: the action's output on success, the input otherwise.
    pub text: String,
    /// `None` on success.
    pub failure: Option<ActionFailure>,
}

impl ActionOutcome {
    fn ok(text: String) -> Self {
        ActionOutcome {
            text,
            failure: None,
        }
    }

    fn failed(text: &str, failure: ActionFailure) -> Self {
        ActionOutcome {
            text: text.to_string(),
            failure: Some(failure),
        }
    }
}

/// The last non-empty line a child wrote to stderr, cleaned and bounded.
///
/// Last rather than first: a shell pipeline reports the failing stage last, and
/// tools that print a banner before their error would otherwise hand back the
/// banner. Run through `sanitize` because anything that draws a progress bar
/// draws it on stderr, and bounded because an operator-supplied command can write
/// as much as it likes and this ends up in a popup.
fn last_stderr_line(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let line = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()?;
    let cleaned = sanitize(line);
    let kept = if cleaned.is_empty() { line } else { &cleaned };
    if kept.is_empty() {
        return None;
    }
    Some(match kept.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &kept[..cut]),
        None => kept.to_string(),
    })
}

/// Run an action over `text`.
///
/// The returned `text` is the original unchanged on ANY failure — spawn error,
/// non-zero exit, timeout, or empty output. This mirrors Voxtype's own
/// `fallback_on_empty` rather than relying on it, because by the time Voxtype
/// sees our output it can no longer tell a deliberate empty result from a crashed
/// action. That property is unchanged by T-020; what changed is that the outcome
/// now also says *why*, so the fallback is no longer indistinguishable from a
/// command that simply had nothing to change.
pub fn run_action(action: &Action, text: &str, timeout: Duration) -> ActionOutcome {
    let spawned = Command::new("sh")
        .arg("-c")
        .arg(&action.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return ActionOutcome::failed(text, ActionFailure::Spawn(e.to_string())),
    };

    // Both pipes are drained on their own threads, and both drains start before
    // anything else can block. A pipe holds about 64KB: a command that produces
    // more than that on a stream nobody is reading blocks on write, never exits,
    // and is killed at the deadline — so an action that works is reported as a
    // hang, and since T-020 reported as one in so many words, sending the
    // operator to raise a timeout that cannot help.
    //
    // stderr was drained by T-020 because capturing it at all introduced the
    // hazard. stdout had the same hazard from the beginning and kept it, because
    // `wait_with_output` reads only after the child has exited and the child
    // cannot exit while it is blocked writing (T-021, measured: 256KB of stdout
    // produced `Timeout { ms: 5000 }` after 5.02s).
    //
    // The drains also start before stdin is written, so a large transcript into a
    // command that writes as it reads cannot deadlock the other way.
    let out_drain = drain(child.stdout.take());
    let err_drain = drain(child.stderr.take());

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
        // Dropping stdin closes it, so the child sees EOF and can exit.
    }

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return ActionOutcome::failed(
                        text,
                        ActionFailure::Timeout {
                            ms: timeout.as_millis(),
                        },
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return ActionOutcome::failed(text, ActionFailure::Spawn(e.to_string())),
        }
    };

    let produced = out_drain
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let errline = err_drain
        .and_then(|h| h.join().ok())
        .and_then(|b| last_stderr_line(&b));

    if !status.success() {
        return ActionOutcome::failed(
            text,
            ActionFailure::Exit {
                code: status.code(),
                stderr: errline,
            },
        );
    }

    let cleaned = sanitize(&String::from_utf8_lossy(&produced));
    if cleaned.is_empty() {
        ActionOutcome::failed(text, ActionFailure::Empty { stderr: errline })
    } else {
        ActionOutcome::ok(cleaned)
    }
}

/// Read a child's pipe to the end on its own thread.
///
/// The thread is the point. Reading a pipe only after the child exits is a
/// deadlock whenever the child produces more than the pipe will hold, because
/// the child cannot exit while it is blocked writing into it. Used for both
/// streams so neither can reintroduce it.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    pipe.map(|mut r| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            buf
        })
    })
}

/// Strip terminal control sequences and surrounding whitespace from an action's
/// output.
///
/// The output of an action is pasted into whatever the operator was typing in,
/// so a control code is never something they meant. `ollama run` is the reason
/// this exists — it writes its spinner, cursor-hide and word-wrap redraws to
/// stdout even when stdout is a pipe, and a tidied sentence came back carrying
/// `^[[?25l`, `^[[1D` and `^[[K` (T-015). The shipped actions no longer use it,
/// but `ollama run` is the obvious thing for an operator to type into their own
/// action, so the guard belongs here rather than only in our config.
///
/// Whitespace goes too: a trailing newline is nearly universal in command
/// output and would otherwise break the line in the field being dictated into.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            // ESC — drop the whole escape sequence, not just the ESC byte.
            '\u{1b}' => match it.peek() {
                // CSI: ESC [ params ... final byte in @..~
                Some('[') => {
                    it.next();
                    while let Some(n) = it.next() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... terminated by BEL or ST (ESC \)
                Some(']') => {
                    it.next();
                    while let Some(n) = it.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if it.peek() == Some(&'\\') {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                // Two-character escapes (charset selection and friends).
                Some(_) => {
                    it.next();
                }
                None => {}
            },
            // Carriage returns are how a terminal redraws a line in place.
            '\r' => {}
            // Other C0 controls carry no meaning in a text field. Newline and
            // tab do, so they stay.
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// What the operator decided in the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Accept the (possibly edited) text.
    Accept(String),
    /// Apply action `usize` to the current text. Since T-024 the popup runs
    /// actions in-window through an executor callback and no longer returns
    /// this variant; it is kept because `resolve` still honours it (headless
    /// callers, the webui test path) and removing it would silently change
    /// that contract.
    #[allow(dead_code)]
    Apply(usize, String),
    /// Cancelled — emit the ORIGINAL text, never nothing. Esc must not be a way
    /// to lose a dictation.
    Cancel,
}

/// Record the operator's decision, and keep the FIRST one.
///
/// egui delivers the close that a decision itself requested back as
/// `close_requested()` on the following frame. The popup reads that as a
/// cancel, so an unconditional write let the cancel land on top of the real
/// decision — and because Cancel resolves to the original transcript, every
/// accept and every action silently degraded to "emit what was dictated".
///
/// That is T-014. It hid for four milestones because Accept and Cancel produce
/// identical output whenever the text is unedited, so only an edited transcript
/// or an action could tell them apart.
///
/// A window-manager close with no prior decision still records `Cancel`: the
/// slot is empty, so the first write wins as it should.
pub fn record_decision(slot: &Mutex<Option<Outcome>>, outcome: Outcome) {
    if let Ok(mut g) = slot.lock() {
        if g.is_none() {
            *g = Some(outcome);
        }
    }
}

/// Resolve an outcome against the original input. The one place that decides
/// what actually reaches Voxtype.
///
/// Carries `run_action`'s diagnosis outward rather than dropping it here. The
/// non-action paths cannot fail, so they report success explicitly — a caller
/// reading `.failure` gets the same answer whichever branch ran.
pub fn resolve(
    outcome: Outcome,
    original: &str,
    actions: &[Action],
    timeout: Duration,
) -> ActionOutcome {
    match outcome {
        Outcome::Accept(edited) => {
            if edited.trim().is_empty() {
                // The operator cleared the box. Almost certainly a mistake, and
                // Voxtype would fall back anyway — be explicit about it here.
                ActionOutcome::ok(original.to_string())
            } else {
                ActionOutcome::ok(edited)
            }
        }
        Outcome::Apply(index, edited) => {
            let base = if edited.trim().is_empty() {
                original
            } else {
                &edited
            };
            match actions.get(index) {
                Some(action) => run_action(action, base, timeout),
                // An index the popup offered but the config no longer has. Keep
                // the text — losing a dictation is the one unacceptable outcome —
                // but say so. This branch used to return `ok`, which meant the
                // operator pressed a key, watched the popup close, got their
                // transcript back and was told nothing (T-022, from OBS-021). It
                // was the last place in this file that reported success for
                // having done nothing.
                None => ActionOutcome::failed(
                    base,
                    ActionFailure::NoSuchAction {
                        position: index + 1,
                        available: actions.len(),
                    },
                ),
            }
        }
        Outcome::Cancel => ActionOutcome::ok(original.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upper() -> Vec<Action> {
        vec![Action {
            label: "Uppercase".into(),
            key: Some('1'),
            command: "tr a-z A-Z".into(),
        }]
    }

    fn t() -> Duration {
        Duration::from_secs(5)
    }

    #[test]
    fn cancel_emits_the_original_not_nothing() {
        assert_eq!(
            resolve(Outcome::Cancel, "hello world", &[], t()).text,
            "hello world"
        );
    }

    #[test]
    fn accept_emits_the_edit() {
        assert_eq!(
            resolve(Outcome::Accept("edited".into()), "original", &[], t()).text,
            "edited"
        );
    }

    #[test]
    fn clearing_the_box_falls_back_rather_than_losing_the_dictation() {
        assert_eq!(
            resolve(Outcome::Accept("   ".into()), "original", &[], t()).text,
            "original"
        );
        assert_eq!(
            resolve(Outcome::Accept("".into()), "original", &[], t()).text,
            "original"
        );
    }

    #[test]
    fn apply_runs_the_action_over_the_edited_text() {
        let got = resolve(Outcome::Apply(0, "edited".into()), "original", &upper(), t());
        assert_eq!(got.text.trim(), "EDITED");
    }

    #[test]
    fn apply_with_an_unknown_index_keeps_the_text() {
        let got = resolve(Outcome::Apply(9, "edited".into()), "original", &upper(), t());
        assert_eq!(got.text, "edited");
    }

    #[test]
    fn apply_over_an_emptied_box_falls_back_to_the_original() {
        let got = resolve(Outcome::Apply(0, "".into()), "original", &upper(), t());
        assert_eq!(got.text.trim(), "ORIGINAL");
    }

    /// The non-action paths cannot fail, and they have to *say* they did not.
    /// A caller reading `.failure` must get the same answer whichever branch of
    /// `resolve` ran, or it will learn to only trust the Apply branch.
    #[test]
    fn the_paths_that_cannot_fail_report_success_rather_than_nothing() {
        assert!(resolve(Outcome::Cancel, "x", &[], t()).failure.is_none());
        assert!(
            resolve(Outcome::Accept("e".into()), "x", &[], t())
                .failure
                .is_none()
        );
        assert!(
            resolve(Outcome::Apply(0, "e".into()), "x", &upper(), t())
                .failure
                .is_none()
        );
    }

    /// The exact bytes that made T-015 unusable: `ollama run` writing its
    /// redraws to a pipe. Reproduced from the captured output, not invented.
    #[test]
    fn terminal_control_sequences_never_reach_the_text_field() {
        let a = Action {
            label: "t".into(),
            key: None,
            command: "printf 'I have bad grammar\\033[1D\\033[K and stuff.\\033[?25h\\n'".into(),
        };
        let got = run_action(&a, "i has bad grammar and stuff", Duration::from_secs(5)).text;
        assert!(!got.contains('\u{1b}'), "escape byte survived: {got:?}");
        assert!(!got.contains('['), "a CSI body leaked through: {got:?}");
        assert_eq!(got, "I have bad grammar and stuff.");
    }

    #[test]
    fn a_trailing_newline_does_not_break_the_line_being_dictated_into() {
        let a = Action { label: "t".into(), key: None, command: "printf 'tidied\\n\\n'".into() };
        assert_eq!(run_action(&a, "messy", Duration::from_secs(5)).text, "tidied");
    }

    /// Stripping must not turn a real answer into an empty one — and if it
    /// does, the original transcript still has to survive.
    #[test]
    fn output_that_is_nothing_but_control_codes_returns_the_original() {
        let a = Action {
            label: "t".into(),
            key: None,
            command: "printf '\\033[?25l\\033[2K\\033[1G'".into(),
        };
        assert_eq!(run_action(&a, "keep me", Duration::from_secs(5)).text, "keep me");
    }

    #[test]
    fn ordinary_punctuation_and_internal_newlines_are_left_alone() {
        let a = Action {
            label: "t".into(),
            key: None,
            command: "printf -- '- one [a]\\n- two (b)\\n\\tindented\\n'".into(),
        };
        let got = run_action(&a, "x", Duration::from_secs(5));
        assert_eq!(got.text, "- one [a]\n- two (b)\n\tindented");
    }

    #[test]
    fn action_transforms_via_stdin_stdout() {
        let a = Action {
            label: "upper".into(),
            key: Some('1'),
            command: "tr a-z A-Z".into(),
        };
        let got = run_action(&a, "hello", Duration::from_secs(5));
        assert_eq!(got.text.trim(), "HELLO");
    }

    // ── T-020 / OBS-019 ──────────────────────────────────────────────────────
    //
    // The four tests below already existed and asserted only that the transcript
    // survives. Every one of them passed for the whole time the defect was live,
    // because "returns the original" is true of all four failure modes AND of a
    // command that succeeded with nothing to change. They were not wrong; they
    // were silent about the only thing that distinguishes those cases. Each now
    // keeps its original assertion — the safety property is still the property
    // that matters most — and adds the one it was missing.

    fn run(command: &str, text: &str, ms: u64) -> ActionOutcome {
        run_action(
            &Action {
                label: "t".into(),
                key: Some('1'),
                command: command.into(),
            },
            text,
            Duration::from_millis(ms),
        )
    }

    #[test]
    fn failing_action_returns_the_original_and_says_it_failed() {
        let got = run("exit 3", "keep me", 5000);
        assert_eq!(got.text, "keep me");
        match got.failure {
            Some(ActionFailure::Exit { code, .. }) => assert_eq!(code, Some(3)),
            other => panic!("expected a non-zero exit, got {other:?}"),
        }
    }

    #[test]
    fn action_producing_nothing_returns_the_original_and_says_it_was_empty() {
        let got = run("true", "keep me", 5000);
        assert_eq!(got.text, "keep me");
        assert!(
            matches!(got.failure, Some(ActionFailure::Empty { .. })),
            "a command that succeeds with no output is not a crash, and must not \
             be reported as one: {:?}",
            got.failure
        );
    }

    /// The OBS-018 scenario end to end, and the reason T-019 existed: since T-015
    /// every shipped action pipes through `jq` and `curl`. On a machine without
    /// them the operator saw their transcript come back and concluded the model
    /// had ignored them. The sentence that solves it — `not found` — was written
    /// by the shell to a stderr we were sending to /dev/null.
    #[test]
    fn a_missing_binary_names_itself_instead_of_vanishing() {
        let got = run("this-binary-does-not-exist-anywhere", "keep me", 5000);
        assert_eq!(got.text, "keep me");
        let f = got.failure.expect("a missing binary must not look like success");
        assert!(
            f.message().contains("this-binary-does-not-exist-anywhere"),
            "the diagnosis does not name the binary, so it does not help: {}",
            f.message()
        );
        assert!(
            f.message().contains("not found"),
            "the shell's own words are the useful part and were dropped: {}",
            f.message()
        );
    }

    #[test]
    fn hanging_action_times_out_returns_the_original_and_says_it_timed_out() {
        let started = std::time::Instant::now();
        let got = run("sleep 30", "keep me", 300);
        assert_eq!(got.text, "keep me");
        assert!(
            matches!(got.failure, Some(ActionFailure::Timeout { ms: 300 })),
            "expected a timeout carrying its own budget, got {:?}",
            got.failure
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout did not fire; took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_successful_action_carries_no_failure() {
        assert!(run("tr a-z A-Z", "hello", 5000).failure.is_none());
    }

    /// The AC this task exists for. Distinguishable *from each other*, not merely
    /// from success — the operator's next move differs for each, and a scheme
    /// that collapsed them into one "it failed" would be the old defect wearing a
    /// message. Compares the rendered messages, because that is what is actually
    /// shown; two variants that render identically are not distinguishable to the
    /// person reading them, whatever the enum says.
    #[test]
    fn every_failure_mode_is_distinguishable_from_every_other() {
        let cases = [
            ("exit 3", "non-zero exit"),
            ("true", "empty output"),
            ("this-binary-does-not-exist-anywhere", "missing binary"),
            ("sleep 30", "timeout"),
        ];
        let mut seen: Vec<(String, String)> = Vec::new();
        for (cmd, name) in cases {
            let f = run(cmd, "keep me", 300)
                .failure
                .unwrap_or_else(|| panic!("{name} produced no diagnosis at all"));
            let msg = f.message();
            for (prev_msg, prev_name) in &seen {
                assert_ne!(
                    &msg, prev_msg,
                    "{name} and {prev_name} are indistinguishable to the operator"
                );
            }
            seen.push((msg, name.to_string()));
        }

        // T-022: the fifth mode never runs a child process, so it is reached
        // through `resolve` rather than `run`. It belongs in this test and not a
        // parallel one — the property is that no two modes read alike, and a
        // second test comparing a different subset would not check that.
        let f = resolve(Outcome::Apply(9, "e".into()), "original", &upper(), t())
            .failure
            .expect("a missing action produced no diagnosis at all");
        let msg = f.message();
        for (prev_msg, prev_name) in &seen {
            assert_ne!(
                &msg, prev_msg,
                "missing action and {prev_name} are indistinguishable to the operator"
            );
        }
        seen.push((msg, "missing action".to_string()));

        assert_eq!(seen.len(), 5);
    }

    /// OBS-021 / T-022. The operator pressed a key, the popup closed, the
    /// transcript came back — and before this, nothing said the action they asked
    /// for no longer existed. Config drift, not a command failure: nothing ran.
    #[test]
    fn applying_an_action_the_config_lost_is_reported_not_swallowed() {
        let got = resolve(Outcome::Apply(9, "edited".into()), "original", &upper(), t());
        assert_eq!(got.text, "edited", "the text must survive regardless");
        match got.failure {
            Some(ActionFailure::NoSuchAction {
                position,
                available,
            }) => {
                assert_eq!(position, 10, "the position should read as the operator counts");
                assert_eq!(available, 1, "and say how many there actually are");
            }
            other => panic!("a lost action still reports success: {other:?}"),
        }
    }

    /// Both properties at once: the fallback to the original transcript is the
    /// older and more important guarantee, and reporting the failure must not
    /// have disturbed it.
    #[test]
    fn a_lost_action_over_an_emptied_box_still_falls_back_to_the_original() {
        let got = resolve(Outcome::Apply(9, "".into()), "original", &upper(), t());
        assert_eq!(got.text, "original");
        assert!(
            matches!(got.failure, Some(ActionFailure::NoSuchAction { .. })),
            "got {:?}",
            got.failure
        );
    }

    /// A shell pipeline reports its failing stage last, and plenty of tools print
    /// a banner before their error. Keeping the first line would hand back the
    /// banner — true, useless, and confidently wrong-looking.
    #[test]
    fn the_stderr_line_kept_is_the_last_one_not_the_first() {
        let got = run(
            "echo 'loading model...' >&2; echo 'connection refused' >&2; exit 1",
            "keep me",
            5000,
        );
        let msg = got.failure.expect("expected a failure").message();
        assert!(msg.contains("connection refused"), "got: {msg}");
        assert!(!msg.contains("loading model"), "kept the banner: {msg}");
    }

    /// Regression guard for the cost of this feature. A pipe holds about 64KB;
    /// before T-020 stderr went to /dev/null and could not fill. Capturing it
    /// without draining it concurrently would block a chatty command on write
    /// until it was killed as a timeout — reporting a spurious failure for an
    /// action that works. That would be a worse defect than the one being fixed,
    /// so it is asserted rather than assumed.
    #[test]
    fn a_command_that_floods_stderr_still_succeeds() {
        let got = run("yes x | head -c 200000 >&2; echo done", "keep me", 10_000);
        assert_eq!(
            got.text, "done",
            "200KB on stderr blocked the action: {:?}",
            got.failure
        );
        assert!(got.failure.is_none(), "got {:?}", got.failure);
    }

    /// OBS-020 / T-021, the same defect on the other stream. Reading stdout only
    /// after the child exits means a command producing more than a pipe buffer
    /// blocks on write, never exits, and is killed at the deadline — so an action
    /// that works is reported as a hang, and since T-020 reported as a hang *in
    /// so many words*, which sends the operator to raise a timeout that cannot
    /// help.
    ///
    /// 256KB is four buffers, picked so the test cannot pass by accident wherever
    /// the pipe is larger than the usual 64KB. The comparison is byte-exact
    /// rather than by length, because a fix that truncated at a buffer boundary
    /// would satisfy any assertion about "enough" output.
    #[test]
    fn a_long_output_comes_back_whole_instead_of_looking_like_a_hang() {
        let want: String = std::iter::repeat("x\n").take(131_072).collect();
        let want = want.trim_end();

        let got = run("yes x | head -c 262144", "keep me", 5_000);

        assert!(
            got.failure.is_none(),
            "an action that produced 256KB was reported as failing: {:?}",
            got.failure
        );
        assert_eq!(
            got.text.len(),
            want.len(),
            "output was truncated: got {} bytes, wanted {}",
            got.text.len(),
            want.len()
        );
        assert_eq!(got.text, want, "output came back altered, not merely short");
    }

    /// `Spawn` is the one variant no in-process test can reach: it fires when the
    /// OS will not run `sh` at all, and making that reachable would mean a test
    /// seam for the shell — the exact shape peer 1023 reported as a defect, where
    /// the branch the tests take is not the branch production takes. So its
    /// construction path is unexercised, and only its rendering is covered here.
    /// Recorded as a known gap rather than papered over with a seam.
    #[test]
    fn every_variant_renders_a_message_that_names_its_cause() {
        assert!(ActionFailure::Spawn("no such file".into())
            .message()
            .contains("could not start"));
        assert!(ActionFailure::Exit {
            code: Some(127),
            stderr: None
        }
        .message()
        .contains("127"));
        assert!(ActionFailure::Timeout { ms: 1200 }
            .message()
            .contains("1200ms"));
        assert!(ActionFailure::Empty { stderr: None }
            .message()
            .contains("no output"));
        assert!(ActionFailure::NoSuchAction {
            position: 3,
            available: 2
        }
        .message()
        .contains("action 3"));
        // The empty case reads differently on purpose — "has 0 actions" is worse
        // English than "is now empty", and this is a sentence a person reads once
        // in a bad moment.
        assert!(ActionFailure::NoSuchAction {
            position: 1,
            available: 0
        }
        .message()
        .contains("is now empty"));
    }

    #[test]
    fn origin_without_a_window_cannot_restore_but_does_not_panic() {
        let o = Origin { window_id: None };
        assert!(!o.restore());
    }

    #[test]
    fn hold_without_a_window_returns_instead_of_sleeping() {
        // T-023. `hold(ms)` exists to cover the emit→paste gap; with no origin
        // there is nothing to hold and it must not add the full grace period
        // to every headless `--no-gui` invocation.
        let o = Origin { window_id: None };
        let t = std::time::Instant::now();
        o.hold(5_000);
        assert!(
            t.elapsed() < std::time::Duration::from_secs(1),
            "hold with no origin slept {}ms",
            t.elapsed().as_millis()
        );
    }

    #[test]
    fn hold_runs_at_least_as_long_as_asked_when_it_has_a_window() {
        // T-023. The grace window is the whole point: if it can elide its
        // duration the late-steal race comes back. A bogus window id still
        // exercises the loop (getactivewindow runs, steals are detected, the
        // re-activate fails best-effort) without needing a display we own.
        // Skipped where xdotool is absent so the suite stays hermetic.
        if std::env::var_os("DISPLAY").is_none()
            || std::process::Command::new("xdotool")
                .arg("-version")
                .output()
                .is_err()
        {
            return;
        }
        let o = Origin {
            window_id: Some("0x1".into()),
        };
        let t = std::time::Instant::now();
        o.hold(250);
        assert!(
            t.elapsed() >= std::time::Duration::from_millis(250),
            "hold(250) returned after only {}ms",
            t.elapsed().as_millis()
        );
    }

    #[test]
    fn the_close_a_decision_requests_does_not_overwrite_that_decision() {
        // T-014. Fails against the pre-fix code, which wrote unconditionally.
        let slot = Mutex::new(None);
        record_decision(&slot, Outcome::Accept("edited".into()));
        record_decision(&slot, Outcome::Cancel); // arrives one frame later
        assert_eq!(
            slot.lock().unwrap().clone(),
            Some(Outcome::Accept("edited".into()))
        );
    }

    #[test]
    fn an_action_survives_the_close_it_requested() {
        let slot = Mutex::new(None);
        record_decision(&slot, Outcome::Apply(0, "text".into()));
        record_decision(&slot, Outcome::Cancel);
        assert_eq!(
            slot.lock().unwrap().clone(),
            Some(Outcome::Apply(0, "text".into()))
        );
    }

    #[test]
    fn a_window_manager_close_with_no_decision_is_still_a_cancel() {
        let slot = Mutex::new(None);
        record_decision(&slot, Outcome::Cancel);
        assert_eq!(slot.lock().unwrap().clone(), Some(Outcome::Cancel));
    }
}
