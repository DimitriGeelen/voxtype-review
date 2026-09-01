//! The popup.
//!
//! # The decision model (T-024)
//!
//! The popup is a review workspace, not a one-shot gate. Nothing pastes while
//! it is open. Actions run *inside* the window through an executor callback and
//! their result lands in the transcript box for inspection; only Enter (or `0`,
//! or Ctrl+Enter) commits what the box holds, and Esc at any round depth falls
//! back to the original raw transcript. Every mutation of the text therefore
//! happens before insert, where undo may be impossible.
//!
//! # Why focus starts on the action list, not in a text box
//!
//! T-007 found that in an editable text widget the keystrokes this feature
//! needs belong to the text: `Tab` inserts a tab, and `1`-`9` type digits. One
//! of them has to yield, so editing is explicit and focus is a three-stop
//! cycle. This design makes the text boxes opt-in:
//!
//! | key             | focus = list                    | focus = a text box            |
//! |-----------------|---------------------------------|-------------------------------|
//! | `0`             | accept current text instantly   | —                             |
//! | `1`-`9`         | run action in-window            | types the digit               |
//! | `↑` / `↓`       | move the highlight              | moves the caret               |
//! | `Enter`         | accept current text             | newline                       |
//! | `Space`         | run the highlighted action      | types a space                 |
//! | `e` / `F2`      | enter the transcript box        | —                             |
//! | `Tab` / `Shift+Tab` | cycle list → transcript → instructions (both directions) |
//! | `Alt+←` / `Alt+→`   | step back / forward through agent rounds (everywhere)    |
//! | `Ctrl`+`Enter`  | accept, from anywhere           | accept                        |
//! | `Esc`           | cancel — the ORIGINAL is emitted | leave the box                |
//!
//! The common cases still cost one keystroke each: `Enter` for "the transcript
//! is fine", a single digit for "run this over it and show me".

use crate::core::{self, Action, ActionOutcome, Outcome};
use eframe::egui;
use std::sync::{mpsc, Arc, Mutex};

// Sized to the content. The first build asked for 420 high and rendered ~210 of
// content over a large empty gap; a review popup that is mostly void reads as
// broken. Text boxes grow with the window if the operator resizes it.
const WINDOW_W: f32 = 760.0;

/// Height of everything that is not an action row: the header, the seven-row
/// transcript box, the instructions box, the separator and the legend.
const CHROME_H: f32 = 252.0;
const ROW_H: f32 = 21.0;
/// The "Actions" heading plus the space around it.
const HEADING_H: f32 = 28.0;

/// M3 made the action count configurable, and a fixed 300px window silently
/// clipped anything past the seventh row — including the legend, so the operator
/// lost both the actions and the only on-screen record that they exist. Grow to
/// fit instead, and let the scroll area in `update` catch counts past the clamp
/// (hotkeys stop at 9, but an action may have no hotkey at all, so the list has
/// no hard upper bound).
fn window_height(action_count: usize) -> f32 {
    let heading = if action_count == 0 { 0.0 } else { HEADING_H };
    (CHROME_H + heading + action_count as f32 * ROW_H).clamp(380.0, 640.0)
}

/// Runs action `index` over `text` with the operator's extra `instructions`.
///
/// Called on a worker thread; the popup stays open and interactive while it
/// runs. The instructions are the operator's steering for agent rounds and are
/// wrapped in a delimited block before the payload, so every action — including
/// arbitrary custom commands — sees them without per-action template work.
pub type Executor = Arc<dyn Fn(usize, &str, &str) -> ActionOutcome + Send + Sync>;

pub fn run(initial: &str, actions: &[Action], executor: Executor) -> Outcome {
    let slot: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([WINDOW_W, window_height(actions.len())])
            .with_min_inner_size([420.0, 380.0])
            .with_always_on_top()
            .with_active(true)
            .with_title("Voxtype — review"),
        ..Default::default()
    };

    let app = ReviewApp {
        text: initial.to_string(),
        original: initial.to_string(),
        actions: actions.to_vec(),
        executor,
        selected: 0,
        focus: Focus::List,
        focus_pending: false,
        instructions: String::new(),
        last_action: 0,
        history: vec![initial.to_string()],
        history_idx: 0,
        running: None,
        recording: None,
        transcribing: None,
        error: None,
        slot: Arc::clone(&slot),
    };

    // A GUI that fails to open must not lose the dictation: fall through to
    // Accept(original) so the transcript still reaches Voxtype untouched.
    if eframe::run_native(
        "voxtype-review",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
    .is_err()
    {
        return Outcome::Accept(initial.to_string());
    }

    let decided = slot.lock().ok().and_then(|mut g| g.take());
    // Window closed by the WM with no decision == cancel.
    decided.unwrap_or(Outcome::Cancel)
}

/// Where the keyboard currently lives. `List` is the hotkey surface; the two
/// text stops are the deliberate editing paths. `Tab` cycles in the fixed
/// order List → Text → Instructions, which is also reading order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Text,
    Instructions,
}

/// Transcript + operator steering, as one payload for the action's stdin.
///
/// The instructions are wrapped in a delimited block rather than woven into a
/// per-action prompt template: custom actions are arbitrary shell commands, so
/// there is no template to weave into, and a model reading a clearly-labelled
/// instruction block behaves the same either way. Free function so the
/// composition rules are testable without opening a window.
fn compose_payload(text: &str, instructions: &str) -> String {
    let instr = instructions.trim();
    if instr.is_empty() {
        text.to_string()
    } else {
        // The block carries its own meta-rule: models kept echoing the
        // instruction back as content (round 4: it came back as a bullet).
        format!(
            "[operator instructions]\nApply these instructions to the text below. \
             Never repeat these instructions in your output. \
             Output only the final text, nothing else.\n{instr}\n[/operator instructions]\n\n{text}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::compose_payload;

    #[test]
    fn no_instructions_payload_is_the_bare_transcript() {
        assert_eq!(compose_payload("hello", ""), "hello");
        assert_eq!(compose_payload("hello", "   \n  "), "hello");
    }

    #[test]
    fn instructions_are_wrapped_and_preended() {
        let p = compose_payload("the text", "keep it formal");
        assert!(p.starts_with("[operator instructions]\n"));
        // The meta-rule must sit between the marker and the instruction so the
        // model cannot echo the instruction back as content (round 4).
        assert!(p.contains("Never repeat these instructions in your output."));
        assert!(p.contains("nothing else.\nkeep it formal\n"));
        assert!(p.ends_with("[/operator instructions]\n\nthe text"));
    }

    #[test]
    fn instructions_are_trimmed() {
        let p = compose_payload("t", "  formal  ");
        assert!(p.starts_with("[operator instructions]\nApply these instructions\nformal\n")
            || p.contains("nothing else.\nformal\n"));
    }
}

/// `voxtype transcribe` prints a wall of ANSI-coloured log lines on stdout
/// and puts the transcript in the quoted tail of its "Transcription
/// completed" line. The box wants only the words: capturing stdout raw was
/// the "trash in the box" — the operator read a build log where their
/// instruction should have been.
fn extract_transcript(stdout: &str) -> String {
    let mut clean = String::with_capacity(stdout.len());
    let mut esc = false;
    for c in stdout.chars() {
        if esc {
            if c == 'm' {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            clean.push(c);
        }
    }
    clean
        .lines()
        .rev()
        .find_map(|l| {
            if !l.contains("Transcription completed") {
                return None;
            }
            let start = l.find('"')?;
            let end = l.rfind('"')?;
            if end > start {
                Some(l[start + 1..end].to_string())
            } else {
                None
            }
        })
        .or_else(|| {
            clean
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod transcript_tests {
    use super::extract_transcript;

    #[test]
    fn transcript_is_pulled_from_the_log_wall() {
        let wall = "Loading audio file: \"x.wav\"\n\
                    \x1b[2m2026-09-01T09:58:22Z\x1b[0m \x1b[32m INFO\x1b[0m Flash attention enabled\n\
                    VAD: 2.01s speech (44.7% of audio)\n\
                    \x1b[2m2026-09-01T09:58:29Z\x1b[0m \x1b[32m INFO\x1b[0m Transcription completed in 5.23s: \"verwijder alle herhalingen\"\n";
        assert_eq!(extract_transcript(wall), "verwijder alle herhalingen");
    }

    #[test]
    fn plain_stdout_passes_through() {
        assert_eq!(extract_transcript("just words\n"), "just words");
        assert_eq!(extract_transcript(""), "");
    }
}

/// One in-flight action run. The worker owns the executor call; the UI polls
/// the channel so the window keeps repainting (and stays responsive) while the
/// model thinks.
struct Running {
    action: usize,
    rx: mpsc::Receiver<ActionOutcome>,
}

/// An in-flight voice capture. The child writes a raw s16le stream; raw is
/// used because a wav header needs a clean exit, and stopping a capture must
/// never depend on polite shutdown.
struct Recording {
    child: std::process::Child,
    raw: std::path::PathBuf,
    wav: std::path::PathBuf,
}

struct ReviewApp {
    text: String,
    original: String,
    actions: Vec<Action>,
    executor: Executor,
    selected: usize,
    focus: Focus,
    focus_pending: bool,
    instructions: String,
    /// The action `Enter` re-runs from the instructions box: the last one the
    /// operator fired, defaulting to the first action. The iteration loop is
    /// type → Enter → read → type → Enter with zero navigation between.
    last_action: usize,
    /// Every text state this session produced. `[0]` is always the original
    /// transcript; Esc and `Alt+Home`-style navigation can always reach it.
    history: Vec<String>,
    history_idx: usize,
    running: Option<Running>,
    /// Ctrl+I in the instructions box: parecord captures the mic while this is
    /// Some. The raw stream (no wav header, so SIGKILL-safe) is converted and
    /// transcribed only after the operator stops — see `stop_voice_recording`.
    recording: Option<Recording>,
    /// Voice capture that has stopped recording and is being transcribed by
    /// the `voxtype transcribe` CLI on a worker thread; polled like actions.
    transcribing: Option<mpsc::Receiver<String>>,
    error: Option<String>,
    slot: Arc<Mutex<Option<Outcome>>>,
}

impl ReviewApp {
    fn decide(&self, ctx: &egui::Context, outcome: Outcome) {
        // First decision wins. The close requested below comes back as
        // `close_requested()` next frame and is read as a cancel; writing that
        // over the real decision is T-014. The guard lives in core so it can be
        // tested without opening a window.
        core::record_decision(&self.slot, outcome);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// The prompt text for an action run: the transcript plus, when present,
    /// the operator's steering in a block the model is told to obey.
    fn payload(&self) -> String {
        compose_payload(&self.text, &self.instructions)
    }

    /// Start an action on a worker thread. The window keeps painting; the
    /// result is picked up in `poll_running`.
    fn start_action(&mut self, index: usize) {
        if self.running.is_some() || index >= self.actions.len() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let payload = self.payload();
        let executor = Arc::clone(&self.executor);
        std::thread::spawn(move || {
            let _ = tx.send(executor(index, &payload, ""));
        });
        self.error = None;
        self.last_action = index;
        self.running = Some(Running { action: index, rx });
    }

    /// Ctrl+I pressed in the instructions box. Starts a raw mic capture on the
    /// default source. The daemon is blocked while this popup exists, so the
    /// microphone is ours for the asking.
    fn start_voice_recording(&mut self) {
        if self.recording.is_some() || self.transcribing.is_some() {
            return;
        }
        let stem = std::env::temp_dir().join(format!(
            "voxtype-review-voice-{}",
            std::process::id()
        ));
        let raw = stem.with_extension("raw");
        let wav = stem.with_extension("wav");
        // parecord with --raw writes PCM to STDOUT, not to a file argument,
        // and its default fragsize is 64000 bytes — two seconds of buffer.
        // A short capture killed before the first flush delivers nothing, and
        // whisper "transcribes" the empty wav as hallucinated junk. A small
        // latency forces a continuous flush; stdout is the file.
        let sink = std::fs::File::create(&raw);
        let spawned = match sink {
            Ok(file) => std::process::Command::new("parecord")
                .args([
                    "--latency-msec=100",
                    "--format=s16le",
                    "--rate=16000",
                    "--channels=1",
                    "--raw",
                ])
                .stdout(std::process::Stdio::from(file))
                .stderr(std::process::Stdio::null())
                .spawn(),
            Err(e) => Err(e),
        };
        match spawned {
            Ok(child) => {
                self.error = None;
                self.recording = Some(Recording { child, raw, wav });
            }
            Err(e) => {
                self.error = Some(format!("voice capture failed to start: {e}"));
            }
        }
    }

    /// Ctrl+I again (or Esc): end the capture and hand the raw stream to a
    /// worker that converts it to wav and runs the `voxtype transcribe` CLI.
    /// The UI keeps polling; the text lands in the instructions box when the
    /// worker reports back.
    fn stop_voice_recording(&mut self) {
        let mut rec = match self.recording.take() {
            Some(r) => r,
            None => return,
        };
        let _ = rec.child.kill();
        let _ = rec.child.wait();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let converted = std::process::Command::new("sox")
                .args([
                    "-t",
                    "raw",
                    "-r",
                    "16000",
                    "-e",
                    "signed-integer",
                    "-b",
                    "16",
                    "-c",
                    "1",
                ])
                .arg(&rec.raw)
                .arg(&rec.wav)
                .output();
            let text = match converted {
                Ok(o) if o.status.success() => {
                    let out = std::process::Command::new("voxtype")
                        .arg("transcribe")
                        .arg(&rec.wav)
                        .output();
                    match out {
                        Ok(o) => extract_transcript(&String::from_utf8_lossy(&o.stdout)),
                        Err(e) => format!("__transcribe_error__{e}"),
                    }
                }
                _ => String::new(),
            };
            // Keep the exact audio the popup captured: when the transcription
            // is wrong, the wav tells whether the capture or the model lied.
            // copy+remove, not rename — /tmp is tmpfs and a cross-device
            // rename fails silently, which once threw the evidence away.
            let dbg = std::env::var("HOME")
                .map(|h| {
                    std::path::PathBuf::from(h)
                        .join(".local/state/voxtype-review/voice-debug")
                })
                .unwrap_or_else(|_| std::env::temp_dir());
            let _ = std::fs::create_dir_all(&dbg);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let raw_bytes = std::fs::metadata(&rec.raw)
                .map(|m| m.len())
                .unwrap_or(0);
            let wav_bytes = std::fs::metadata(&rec.wav)
                .map(|m| m.len())
                .unwrap_or(0);
            // Loudness of the capture: near-silence means the mic fed us
            // nothing regardless of byte counts.
            let rms_line = std::process::Command::new("sox")
                .arg(&rec.wav)
                .arg("-n")
                .arg("stat")
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .find(|l| l.contains("RMS") && l.contains("amplitude"))
                        .unwrap_or("no rms line")
                        .trim()
                        .to_string()
                })
                .unwrap_or_else(|_| "sox stat failed".to_string());
            let log = format!(
                "raw={raw_bytes}B wav={wav_bytes}B\n{rms_line}\ntext: {text}\n"
            );
            if wav_bytes > 0 {
                let _ = std::fs::copy(&rec.wav, dbg.join(format!("voice-{stamp}.wav")));
            } else {
                let _ = std::fs::copy(&rec.raw, dbg.join(format!("voice-{stamp}.raw")));
            }
            let _ = std::fs::write(dbg.join(format!("voice-{stamp}.log")), log);
            let _ = std::fs::remove_file(&rec.raw);
            let _ = std::fs::remove_file(&rec.wav);
            let _ = tx.send(text);
        });
        self.transcribing = Some(rx);
    }

    /// Non-blocking poll of a finished voice transcription; the recognized
    /// speech is appended to the instructions box (space-joined).
    fn poll_transcription(&mut self) {
        let rx = match &self.transcribing {
            Some(rx) => rx,
            None => return,
        };
        match rx.try_recv() {
            Ok(text) => {
                self.transcribing = None;
                if text.is_empty() {
                    self.error =
                        Some("voice transcription produced nothing — try again".to_string());
                } else if self.instructions.trim().is_empty() {
                    self.instructions = text;
                } else {
                    self.instructions.push(' ');
                    self.instructions.push_str(&text);
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.transcribing = None;
                self.error = Some("voice transcription worker died".to_string());
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn poll_running(&mut self) {
        let Some(running) = &self.running else { return };
        match running.rx.try_recv() {
            Ok(outcome) => {
                let index = running.action;
                self.running = None;
                if let Some(failure) = outcome.failure {
                    // A failed action must never look like "the model ignored
                    // me" (T-020): the banner says why, the text is unchanged.
                    self.error = Some(failure.message());
                    return;
                }
                self.text = outcome.text;
                self.history.truncate(self.history_idx + 1);
                self.history.push(self.text.clone());
                self.history_idx = self.history.len() - 1;
                let _ = index;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.running = None;
                self.error = Some("action worker died without a result".to_string());
            }
        }
    }

    /// Step through agent rounds. `delta` -1 is older, +1 is newer. The
    /// original is `history[0]`, so the raw dictation is always reachable.
    fn history_step(&mut self, delta: i32) {
        let target = self.history_idx as i64 + delta as i64;
        if target < 0 || target as usize >= self.history.len() {
            return;
        }
        self.history_idx = target as usize;
        self.text = self.history[self.history_idx].clone();
    }

    /// Keys that mean the same thing no matter where focus lives. Consumed
    /// here so no text box ever sees them.
    fn handle_global_keys(&mut self, ctx: &egui::Context) -> bool {
        let (accept, run_from_box, cancel) = ctx.input(|i| {
            (
                i.modifiers.ctrl && i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Enter),
                i.viewport().close_requested(),
            )
        });
        if accept {
            self.decide(ctx, Outcome::Accept(self.text.clone()));
            return true;
        }
        // Enter inside the instructions box runs the last-used action with the
        // instruction applied — the box IS the run button. Focus stays in the
        // box so the next refinement is one keystroke away. Plain Enter there
        // must not also insert a newline, so it is consumed here, before the
        // widget sees it.
        if run_from_box && self.focus == Focus::Instructions {
            self.start_action(self.last_action);
            return true;
        }
        // Ctrl+I in the instructions box toggles voice capture; Esc during a
        // capture stops it without leaving the box (let alone cancelling).
        let voice = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::I));
        if voice {
            if self.recording.is_some() {
                self.stop_voice_recording();
                return true;
            }
            if self.focus == Focus::Instructions {
                self.start_voice_recording();
                return true;
            }
        }
        if self.recording.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.stop_voice_recording();
            return true;
        }
        if cancel {
            self.decide(ctx, Outcome::Cancel);
            return true;
        }

        // Tab belongs to the focus cycle, never to a text box.
        let (tab, shift_tab) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::default(), egui::Key::Tab),
                i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab),
            )
        });
        let next = |f: Focus, fwd: bool| match (f, fwd) {
            (Focus::List, true) => Focus::Text,
            (Focus::Text, true) => Focus::Instructions,
            (Focus::Instructions, true) => Focus::List,
            (Focus::List, false) => Focus::Instructions,
            (Focus::Instructions, false) => Focus::Text,
            (Focus::Text, false) => Focus::List,
        };
        if tab {
            self.focus = next(self.focus, true);
            self.focus_pending = true;
        }
        if shift_tab {
            self.focus = next(self.focus, false);
            self.focus_pending = true;
        }

        // Round history, from any focus: Alt+arrows are free in every widget.
        let (back, fwd) = ctx.input(|i| {
            (
                i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft),
                i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight),
            )
        });
        if back {
            self.history_step(-1);
        }
        if fwd {
            self.history_step(1);
        }
        false
    }

    /// Keys that only apply while the action list has focus.
    fn handle_list_keys(&mut self, ctx: &egui::Context) -> bool {
        const DIGITS: [(egui::Key, usize); 9] = [
            (egui::Key::Num1, 0),
            (egui::Key::Num2, 1),
            (egui::Key::Num3, 2),
            (egui::Key::Num4, 3),
            (egui::Key::Num5, 4),
            (egui::Key::Num6, 5),
            (egui::Key::Num7, 6),
            (egui::Key::Num8, 7),
            (egui::Key::Num9, 8),
        ];

        for (key, index) in DIGITS {
            if ctx.input(|i| i.key_pressed(key)) && index < self.actions.len() {
                self.start_action(index);
                return true;
            }
        }
        // Option 0: paste the current text right now, no round trip.
        if ctx.input(|i| i.key_pressed(egui::Key::Num0)) {
            self.decide(ctx, Outcome::Accept(self.text.clone()));
            return true;
        }

        let (up, down, enter, space, edit, esc) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::E) || i.key_pressed(egui::Key::F2),
                i.key_pressed(egui::Key::Escape),
            )
        });

        if esc {
            // The one exit that emits the original: every other path commits
            // what the box holds.
            self.decide(ctx, Outcome::Cancel);
            return true;
        }
        if enter {
            self.decide(ctx, Outcome::Accept(self.text.clone()));
            return true;
        }
        if space && !self.actions.is_empty() {
            self.start_action(self.selected);
            return true;
        }
        if edit {
            self.focus = Focus::Text;
            self.focus_pending = true;
        }
        if !self.actions.is_empty() {
            if up && self.selected > 0 {
                self.selected -= 1;
            }
            if down && self.selected + 1 < self.actions.len() {
                self.selected += 1;
            }
        }
        false
    }

    fn legend_text(&self) -> String {
        if self.focus != Focus::List {
            "Enter run   ·   Ctrl+I speak   ·   Ctrl+Enter paste   ·   Esc leave the box   ·   Tab next box   ·   Alt+arrow rounds".to_string()
        } else if self.actions.is_empty() {
            "Enter accept   ·   0 accept now   ·   e edit   ·   Esc cancel (raw)".to_string()
        } else {
            "Enter accept   ·   0 accept now   ·   1-9 run action, result shows here   ·   ↑↓ select · Space run   ·   Tab boxes   ·   Alt+arrow rounds   ·   e edit   ·   Esc cancel (raw)".to_string()
        }
    }
}

impl eframe::App for ReviewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.handle_global_keys(ctx) {
            return;
        }
        self.poll_running();
        self.poll_transcription();
        if self.focus == Focus::List && self.handle_list_keys(ctx) {
            return;
        }

        // Keep repainting while a worker is out, so its result lands the
        // moment it arrives instead of on the next keystroke.
        if self.running.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // The legend goes in a bottom panel, not at the end of the central panel,
        // so that a long action list can never push it off the window. It is the
        // only place the key model is written down.
        egui::TopBottomPanel::bottom("legend").show(ctx, |ui| {
            ui.add_space(4.0);
            // ASCII only. egui's bundled font has no arrow glyphs, and they
            // render as tofu boxes — caught by looking at a screenshot.
            // Don't advertise keys that would do nothing. A config may legally
            // define no actions at all, and offering "1-9" there is a lie.
            ui.weak(self.legend_text());
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);
            let running_label = self
                .running
                .as_ref()
                .and_then(|r| self.actions.get(r.action))
                .map(|a| a.label.clone());
            ui.horizontal(|ui| {
                ui.strong("Transcript");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.history.len() > 1 {
                        ui.weak(format!(
                            "round {}/{}   Alt+arrow steps",
                            self.history_idx + 1,
                            self.history.len()
                        ));
                    }
                    if let Some(label) = &running_label {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 170, 60),
                            format!("running: {label} ..."),
                        );
                    } else if self.recording.is_some() {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 90, 70),
                            "● listening — Ctrl+I to stop",
                        );
                    } else if self.transcribing.is_some() {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 170, 60),
                            "transcribing…",
                        );
                    } else if self.focus != Focus::List {
                        ui.colored_label(egui::Color32::from_rgb(220, 170, 60), "editing");
                    } else if self.text != self.original {
                        ui.colored_label(egui::Color32::from_rgb(120, 180, 120), "edited by agent");
                    }
                });
            });
            if let Some(err) = &self.error {
                ui.colored_label(egui::Color32::from_rgb(230, 90, 90), format!("action failed: {err}"));
            }
            ui.add_space(2.0);

            let editor = ui.add(
                egui::TextEdit::multiline(&mut self.text)
                    .desired_width(f32::INFINITY)
                    .desired_rows(7)
                    .interactive(self.focus == Focus::Text)
                    .font(egui::TextStyle::Monospace),
            );

            // Clicking a box is the mouse equivalent of Tab-ing to it.
            if editor.clicked() && self.focus != Focus::Text {
                self.focus = Focus::Text;
                self.focus_pending = true;
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.strong(
                    "Instructions for the agent — Enter runs the last action with this; it stays for every run",
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕ clear").clicked() {
                        self.instructions.clear();
                    }
                });
            });
            ui.add_space(2.0);
            let instr_box = ui.add(
                egui::TextEdit::multiline(&mut self.instructions)
                    .hint_text("e.g. keep the technical terms, make it formal ...")
                    .desired_width(f32::INFINITY)
                    .desired_rows(2)
                    .interactive(self.focus == Focus::Instructions),
            );
            if instr_box.clicked() && self.focus != Focus::Instructions {
                self.focus = Focus::Instructions;
                self.focus_pending = true;
            }

            // One deferred focus grant per frame, whichever box asked for it.
            if self.focus_pending {
                match self.focus {
                    Focus::Text => editor.request_focus(),
                    Focus::Instructions => instr_box.request_focus(),
                    Focus::List => {
                        editor.surrender_focus();
                        instr_box.surrender_focus();
                    }
                }
                self.focus_pending = false;
            }
            // Esc inside a box leaves the box rather than cancelling; losing an
            // edit to a reflex Esc would be worse than one extra keystroke.
            if self.focus != Focus::List && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.focus = Focus::List;
                self.focus_pending = true;
            }

            ui.add_space(10.0);

            if self.actions.is_empty() {
                ui.weak("No actions configured.");
            } else {
                ui.strong("Actions");
                ui.add_space(4.0);
                // The 0 row is a first-class citizen of the list, not a legend
                // footnote: the operator looks at the list, not the legend, and
                // "there is no 0" was a fair reading of the old UI.
                let zero_row = egui::Label::new(
                    egui::RichText::new("  0   Accept — paste this now").strong(),
                )
                .sense(egui::Sense::click());
                if ui.add(zero_row).clicked() {
                    self.decide(ui.ctx(), Outcome::Accept(self.text.clone()));
                    return;
                }
                ui.add_space(2.0);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, action) in self.actions.iter().enumerate() {
                        let highlighted = self.focus == Focus::List && i == self.selected;
                        let digit = action
                            .key
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| " ".to_string());
                        let row = format!("  {}   {}", digit, action.label);
                        let label = if highlighted {
                            egui::RichText::new(row).strong().background_color(
                                ui.visuals().selection.bg_fill,
                            )
                        } else {
                            egui::RichText::new(row)
                        };
                        if ui
                            .add(egui::Label::new(label).sense(egui::Sense::click()))
                            .clicked()
                        {
                            self.selected = i;
                            self.start_action(i);
                            return;
                        }
                    }
                });
            }
        });
    }
}

/// A voice capture must never outlive the window: a dead popup with a live
/// parecord would keep the microphone and never deliver its text anywhere.
impl Drop for ReviewApp {
    fn drop(&mut self) {
        if let Some(mut rec) = self.recording.take() {
            let _ = rec.child.kill();
            let _ = rec.child.wait();
            let _ = std::fs::remove_file(&rec.raw);
        }
    }
}
