//! `voxtype-review` — edit-before-insert popup for Voxtype.
//!
//! Wired into Voxtype through its existing transformation hook, so no fork of
//! Voxtype is required:
//!
//! ```toml
//! [output.post_process]
//! command = "voxtype-review"
//! timeout_ms = 600000
//! ```
//!
//! Voxtype pipes the transcript in on stdin and types whatever comes back on
//! stdout. This binary opens a window in between.
//!
//! The order of operations at the end is load-bearing. T-007 measured that focus
//! does not return to the origin window unaided on xfwm4, and this host's Voxtype
//! runs in `paste` mode — it sends Ctrl+V to whatever is focused. So: restore
//! focus FIRST, then write stdout. Reversing those two lines types the operator's
//! dictation into the wrong window. T-023 added the third step: hold focus for a
//! grace period AFTER the write, because Voxtype cannot paste until our stdout
//! reaches EOF (process exit), and the WM's late focus handoff when our popup
//! window is destroyed used to steal the paste target in exactly that gap.

mod config;
mod core;
mod ui;
mod valet;
mod voxtype_config;

use std::sync::Arc;
mod webui;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--help") || has("-h") {
        print_help();
        return;
    }

    if has("--write-default-config") {
        let Some(path) = config::config_path() else {
            eprintln!("voxtype-review: cannot determine a config path (no HOME?)");
            std::process::exit(1);
        };
        match config::write_default_config(&path, has("--force")) {
            Ok(()) => println!("wrote {}", path.display()),
            Err(e) => {
                eprintln!("voxtype-review: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if has("--config-ui") {
        // `--port N`. Parsed here rather than in webui so the flag handling all
        // lives in one place.
        let port = args
            .iter()
            .position(|a| a == "--port")
            .and_then(|i| args.get(i + 1))
            .map(|s| {
                s.parse::<u16>().unwrap_or_else(|_| {
                    eprintln!("voxtype-review: --port wants a number 1-65535, got \"{s}\"");
                    std::process::exit(1);
                })
            })
            .unwrap_or(webui::DEFAULT_PORT);
        if let Err(e) = webui::serve(port) {
            eprintln!("voxtype-review: {e}");
            std::process::exit(1);
        }
        return;
    }

    if has("--list-actions") {
        let loaded = config::load();
        for w in &loaded.warnings {
            eprintln!("voxtype-review: {w}");
        }
        let source = match config::config_path() {
            Some(p) if p.exists() && !loaded.used_defaults => p.display().to_string(),
            Some(p) if p.exists() => format!("{} (unusable — showing built-in defaults)", p.display()),
            Some(p) => format!("{} (absent — showing built-in defaults)", p.display()),
            None => "built-in defaults".to_string(),
        };
        println!("source: {source}");
        if loaded.actions.is_empty() {
            println!("(no actions)");
        }
        for (i, a) in loaded.actions.iter().enumerate() {
            let key = a.key.map(|c| c.to_string()).unwrap_or_else(|| "-".into());
            println!(
                "{key}  {}   [{}ms]  {}",
                a.label,
                loaded.timeout_for(i).as_millis(),
                a.command
            );
        }
        return;
    }

    if has("--unpark") {
        match valet::unpark() {
            Some(path) => println!("restored parked clipboard image from {}", path.display()),
            None => println!("nothing parked"),
        }
        return;
    }

    let input = core::read_input();

    // Headless passthrough. Makes the post_process contract testable without an
    // X display — in CI, in the P-011 verification gate, and over SSH.
    if has("--no-gui") {
        core::emit(&input);
        return;
    }

    // Nothing to review. Say nothing and let Voxtype fall back.
    if input.trim().is_empty() {
        core::emit(&input);
        return;
    }

    let loaded = config::load();
    for w in &loaded.warnings {
        eprintln!("voxtype-review: {w}");
    }

    let origin = core::Origin::capture();

    // Image-only valet (T-023): if the clipboard holds a screenshot, park it
    // now, while the popup is still closed and the bytes are the operator's
    // pre-dictation state. Text clipboards are not parked — voxtype's own
    // restore handles them fine and must stay automatic.
    let parked = valet::park();

    // A panic anywhere in the GUI must still emit the transcript. This is the
    // last line of defence behind the per-path fallbacks in core.
    let text = input.clone();
    let acts = loaded.actions.clone();

    // T-024: actions now run INSIDE the popup on worker threads, so the
    // executor must own its data. Per-action timeouts are resolved up front
    // because the Loaded config is not shared with the closure.
    let exec_actions = loaded.actions.clone();
    let exec_timeouts: Vec<std::time::Duration> =
        (0..exec_actions.len()).map(|i| loaded.timeout_for(i)).collect();
    let executor: ui::Executor = Arc::new(move |index, payload, _instructions| {
        match exec_actions.get(index) {
            Some(action) => core::run_action(
                action,
                payload,
                exec_timeouts
                    .get(index)
                    .copied()
                    .unwrap_or(std::time::Duration::from_millis(config::DEFAULT_TIMEOUT_MS)),
            ),
            // Unreachable from the popup (it only offers valid indices), but a
            // no-op beats a panic in a worker thread.
            None => core::ActionOutcome {
                text: payload.to_string(),
                failure: None,
            },
        }
    });

    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ui::run(&text, &acts, executor)))
            .unwrap_or_else(|_| core::Outcome::Accept(input.clone()));

    // The timeout depends on which action was chosen, so it is resolved here
    // rather than passed in.
    let timeout = match &outcome {
        core::Outcome::Apply(i, _) => loaded.timeout_for(*i),
        _ => std::time::Duration::from_millis(config::DEFAULT_TIMEOUT_MS),
    };

    let result = core::resolve(outcome, &input, &loaded.actions, timeout);

    // Say why, when there is a why. The transcript still goes out either way —
    // that is the safety property and it does not change — but an action that
    // failed used to be indistinguishable from one that had nothing to change,
    // and the operator's only reading of that silence was "the model ignored me"
    // (T-020, from OBS-019). This is the same `voxtype-review:` surface every
    // other diagnostic in this file uses.
    if let Some(failure) = &result.failure {
        eprintln!("voxtype-review: {}", failure.message());
    }

    // Focus first, then output. See the module comment.
    origin.restore();

    // The watcher must own the clipboard before voxtype's text-set lands, so
    // it starts here — before the bytes that end this process. It defuses the
    // image restore and never touches a text-only dictation.
    if parked.is_some() {
        valet::spawn_watcher(&result.text);
    }
    core::emit(&result.text);
    // Voxtype cannot paste until our stdout hits EOF, which happens when this
    // process exits — so this grace period is free. It exists because the WM
    // can hand focus to its own next window after our popup's destroy event
    // finally lands, past the last check `restore()` made (T-023: a cancelled
    // dictation pasted into a Chromium window ~700ms after the popup closed).
    origin.hold(700);
}

fn print_help() {
    println!(
        "voxtype-review — edit-before-insert popup for Voxtype

USAGE:
    voxtype-review [OPTIONS]

    Reads a transcript on stdin, opens a window to review it, prints the
    result on stdout. Designed for Voxtype's [output.post_process] hook.

OPTIONS:
    --no-gui                 Pass stdin straight through. For testing without
                             a display.
    --list-actions           Print the action set that would be loaded, and
                             where it came from. Opens no window.
    --config-ui              Serve a config page on 127.0.0.1 for editing the
                             action set in a browser. Localhost only.
    --port N                 Port for --config-ui (default 8765).
    --write-default-config   Write a commented starter config.
    --force                  Allow --write-default-config to overwrite.
    -h, --help               This message.

KEYS:
    Enter          accept the transcript as it stands
    1-9            run that action over the text
    Up/Down        move the highlight; Space runs the highlighted action
    e / F2         click into the text box to edit by hand
    Ctrl+Enter     accept from anywhere
    Esc            cancel — emits the ORIGINAL transcript, never nothing

CONFIG:
    ~/.config/voxtype-review/config.toml   (override: $VOXTYPE_REVIEW_CONFIG)

    A missing or malformed config falls back to built-in defaults and warns on
    stderr; a single bad entry is skipped and the rest still load. A transcript
    is never lost because of a config error.

VOXTYPE:
    [output.post_process]
    command = \"voxtype-review\"
    timeout_ms = 600000"
    );
}
