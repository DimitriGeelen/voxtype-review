//! A local config page for the action set.
//!
//! M3 made the action set a config edit rather than a rebuild, which was the
//! right shape but not yet a usable one: the operator has to know the schema,
//! get the TOML right, and find out about mistakes from a stderr warning they
//! will never see — the popup is launched by Voxtype, and its stderr goes
//! nowhere anyone looks. This puts the same file behind a page, and shows those
//! warnings where the editing happens.
//!
//! Three constraints shape the implementation:
//!
//! 1. **Localhost only.** This page edits a file whose contents are executed as
//!    shell commands on every dictation. It binds `127.0.0.1` explicitly and
//!    there is a test asserting that, because "we only ever pass localhost" is
//!    the kind of thing that survives right up until someone adds a config
//!    option for it.
//! 2. **No new dependencies.** The project has three, on purpose. A page one
//!    person opens on their own machine does not justify a web framework;
//!    `TcpListener` and a hand-rolled request line parser cover it.
//! 3. **The file stays the source of truth.** The page is a view over the same
//!    TOML a text editor sees. It never holds state of its own.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use crate::config;
use crate::core;

pub const DEFAULT_PORT: u16 = 8765;

/// Bind address. A function rather than a literal at the call site so the test
/// below can assert on the actual thing that gets bound.
fn bind_addr(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

pub fn serve(port: u16) -> Result<(), String> {
    let addr = bind_addr(port);
    let listener = TcpListener::bind(addr).map_err(|e| {
        format!("cannot listen on {addr}: {e}\nAnother copy may already be running; try --port <N>.")
    })?;

    let path = config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(no config path)".into());

    println!("voxtype-review config page");
    println!("  http://127.0.0.1:{port}/");
    println!();
    println!("  editing: {path}");
    println!("  bound to 127.0.0.1 only — not reachable from the network");
    println!("  Ctrl+C to stop");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                // Single-threaded on purpose: one operator, one page. A thread
                // per connection would add a race on the config file for no
                // benefit anyone can observe.
                if let Err(e) = handle(s) {
                    core::trace(&format!("request failed: {e}"));
                }
            }
            Err(e) => core::trace(&format!("accept failed: {e}")),
        }
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);

    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut len = 0usize;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h).map_err(|e| e.to_string())?;
        if n == 0 || h.trim().is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }

    // A config page is not a file upload endpoint. Anything this large is a
    // mistake or a probe; either way it is not read into memory.
    if len > 1_000_000 {
        return Err("request body too large".into());
    }

    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    }
    Ok(Request {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &str) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}

fn handle(mut stream: TcpStream) -> Result<(), String> {
    let req = read_request(&mut stream)?;
    let route = req.path.split('?').next().unwrap_or("/");

    match (req.method.as_str(), route) {
        ("GET", "/") => respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE),
        ("GET", "/api/state") => {
            respond(&mut stream, "200 OK", "application/json", &state_json())
        }
        ("POST", "/api/save") => {
            let (status, body) = save(&req.body);
            respond(&mut stream, status, "application/json", &body)
        }
        ("GET", "/api/defaults") => {
            respond(&mut stream, "200 OK", "application/json", &defaults_json())
        }
        ("GET", "/api/voxtype") => {
            respond(&mut stream, "200 OK", "application/json", &voxtype_json())
        }
        ("POST", "/api/voxtype/save") => {
            let (status, body) = save_voxtype(&req.body);
            respond(&mut stream, status, "application/json", &body)
        }
        ("POST", "/api/test") => {
            let body = test_action(&req.body);
            respond(&mut stream, "200 OK", "application/json", &body)
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", "not found"),
    }
}

// ── JSON, by hand ────────────────────────────────────────────────────────────
// Only enough to emit what this page needs. Bringing in serde_json for four
// object shapes would cost more than it saves.

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Pull one string field out of a flat JSON object. The page is the only client
/// and it sends flat objects, so this does not need to be a parser.
fn field(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let i = body.find(&pat)? + pat.len();
    let rest = &body[i..];
    let c = rest.find(':')? + 1;
    let rest = &rest[c..];
    let start = rest.find('"')? + 1;
    let bytes = rest.as_bytes();
    let mut out = String::new();
    let mut j = start;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' if j + 1 < bytes.len() => {
                match bytes[j + 1] {
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    other => out.push(other as char),
                }
                j += 2;
            }
            b'"' => break,
            _ => {
                let ch = rest[j..].chars().next().unwrap_or('\u{fffd}');
                out.push(ch);
                j += ch.len_utf8();
            }
        }
    }
    Some(out)
}

fn which(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether Voxtype's own config points at a voxtype-review binary. Read-only —
/// this page never edits Voxtype's config; `install.sh --wire` owns that, and it
/// asks first.
fn voxtype_wiring() -> (bool, String) {
    let path = std::env::var("VOXTYPE_CONFIG").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".config/voxtype/config.toml")
    });
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (false, format!("no Voxtype config at {}", path.display()));
    };
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        if t.starts_with("command") && t.contains("voxtype-review") {
            return (true, format!("wired in {}", path.display()));
        }
    }
    (false, format!("not wired in {} — run ./install.sh --wire", path.display()))
}

fn state_json() -> String {
    let loaded = config::load();
    let path = config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let actions: Vec<String> = loaded
        .actions
        .iter()
        .enumerate()
        .map(|(i, a)| {
            format!(
                "{{\"key\":\"{}\",\"label\":\"{}\",\"command\":\"{}\",\"timeout_ms\":{}}}",
                esc(&a.key.map(|c| c.to_string()).unwrap_or_default()),
                esc(&a.label),
                esc(&a.command),
                loaded.timeout_for(i).as_millis()
            )
        })
        .collect();

    let warnings: Vec<String> = loaded.warnings.iter().map(|w| format!("\"{}\"", esc(w))).collect();
    let (wired, wire_msg) = voxtype_wiring();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "voxtype-review".into());

    format!(
        "{{\"path\":\"{}\",\"exists\":{},\"using_defaults\":{},\"actions\":[{}],\
         \"warnings\":[{}],\"binary\":\"{}\",\"wired\":{},\"wire_msg\":\"{}\",\
         \"xdotool\":{},\"ollama\":{}}}",
        esc(&path),
        config::config_path().map(|p| p.exists()).unwrap_or(false),
        loaded.used_defaults,
        actions.join(","),
        warnings.join(","),
        esc(&exe),
        wired,
        esc(&wire_msg),
        which("xdotool"),
        which("ollama"),
    )
}

/// Turn the page's action list back into TOML, validate it by parsing it the
/// same way the popup will, and only then write.
fn save(body: &str) -> (&'static str, String) {
    let toml_text = match field(body, "toml") {
        Some(t) => t,
        None => return ("400 Bad Request", "{\"ok\":false,\"error\":\"no toml field\"}".into()),
    };

    // Validate through the real loader, so what the page accepts and what the
    // popup accepts cannot diverge.
    let parsed = config::parse(&toml_text);
    let fatal: Vec<&String> = parsed
        .warnings
        .iter()
        .filter(|w| w.contains("not valid TOML") || w.contains("skipped"))
        .collect();
    if !fatal.is_empty() {
        let msgs: Vec<String> = fatal.iter().map(|w| format!("\"{}\"", esc(w))).collect();
        return (
            "400 Bad Request",
            format!(
                "{{\"ok\":false,\"error\":\"refused — the file on disk was not changed\",\"detail\":[{}]}}",
                msgs.join(",")
            ),
        );
    }

    let Some(path) = config::config_path() else {
        return ("500 Internal Server Error", "{\"ok\":false,\"error\":\"no config path\"}".into());
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return (
                "500 Internal Server Error",
                format!("{{\"ok\":false,\"error\":\"{}\"}}", esc(&e.to_string())),
            );
        }
    }
    // Back up whatever is there before replacing it. The operator may have hand
    // edits and comments this page cannot represent, and losing those to a Save
    // click would be unforgivable.
    if path.exists() {
        let bak = path.with_extension("toml.bak");
        let _ = std::fs::copy(&path, &bak);
    }
    match std::fs::write(&path, &toml_text) {
        Ok(()) => (
            "200 OK",
            format!(
                "{{\"ok\":true,\"count\":{},\"path\":\"{}\"}}",
                parsed.actions.len(),
                esc(&path.display().to_string())
            ),
        ),
        Err(e) => (
            "500 Internal Server Error",
            format!("{{\"ok\":false,\"error\":\"{}\"}}", esc(&e.to_string())),
        ),
    }
}

fn test_action(body: &str) -> String {
    let command = field(body, "command").unwrap_or_default();
    let text = field(body, "text").unwrap_or_default();
    let ms: u64 = field(body, "timeout_ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(config::DEFAULT_TIMEOUT_MS);

    if command.trim().is_empty() {
        return "{\"ok\":false,\"error\":\"no command\"}".into();
    }
    let action = core::Action {
        label: "test".into(),
        key: None,
        command,
    };
    let out = core::run_action(&action, &text, Duration::from_millis(ms));
    // Report the diagnosis rather than inferring one. This was
    // `unchanged = out == text`, a proxy wrong in both directions: an action that
    // legitimately returns its input read as broken, and a command that failed
    // while producing different-but-wrong output read as fine. The panel's own
    // copy conceded it — "either the command is a no-op, or it failed". T-020
    // gave run_action something true to say, so this passes it through.
    match out.failure {
        Some(f) => format!(
            "{{\"ok\":true,\"failed\":true,\"error\":\"{}\",\"output\":\"{}\"}}",
            esc(&f.message()),
            esc(&out.text)
        ),
        None => format!(
            "{{\"ok\":true,\"failed\":false,\"output\":\"{}\"}}",
            esc(&out.text)
        ),
    }
}

const PAGE: &str = include_str!("webui.html");

// The shipped action set, so the page can offer it to an operator whose config
// predates a fix to it. Returned, never written: replacing someone's actions is
// their decision, so this fills the editor and they still have to press Save.
fn defaults_json() -> String {
    let d = config::defaults();
    let actions: Vec<String> = d
        .actions
        .iter()
        .enumerate()
        .map(|(i, a)| {
            format!(
                "{{\"key\":\"{}\",\"label\":\"{}\",\"command\":\"{}\",\"timeout_ms\":{}}}",
                esc(&a.key.map(|c| c.to_string()).unwrap_or_default()),
                esc(&a.label),
                esc(&a.command),
                d.timeout_for(i).as_millis()
            )
        })
        .collect();
    format!("{{\"actions\":[{}]}}", actions.join(","))
}

// ── voxtype's own settings ───────────────────────────────────────────────────
//
// A different file from the action set, with a different owner: upstream's
// dashboard writes it too, and so does the operator by hand. Everything risky
// about that lives in `voxtype_config`; this is only the transport.

fn voxtype_json() -> String {
    let path = crate::voxtype_config::path();
    let shown = esc(&path.display().to_string());

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            return format!(
                "{{\"path\":\"{}\",\"ok\":false,\"error\":\"{}\",\"settings\":[]}}",
                shown,
                esc(&e.to_string())
            )
        }
    };

    match crate::voxtype_config::read(&text) {
        Ok(settings) => {
            let items: Vec<String> = settings
                .iter()
                .map(|s| {
                    format!(
                        "{{\"id\":\"{}\",\"value\":\"{}\",\"present\":{},\"help\":\"{}\"}}",
                        esc(s.id),
                        esc(&s.value),
                        s.present,
                        esc(s.help)
                    )
                })
                .collect();
            format!(
                "{{\"path\":\"{}\",\"ok\":true,\"settings\":[{}]}}",
                shown,
                items.join(",")
            )
        }
        Err(e) => format!(
            "{{\"path\":\"{}\",\"ok\":false,\"error\":\"{}\",\"settings\":[]}}",
            shown,
            esc(&e)
        ),
    }
}

fn save_voxtype(body: &str) -> (&'static str, String) {
    let (Some(id), Some(value)) = (field(body, "id"), field(body, "value")) else {
        return ("400 Bad Request", "{\"ok\":false,\"error\":\"need id and value\"}".into());
    };
    match crate::voxtype_config::save(&id, &value) {
        Ok(msg) => ("200 OK", format!("{{\"ok\":true,\"message\":\"{}\"}}", esc(&msg))),
        Err(e) => ("400 Bad Request", format!("{{\"ok\":false,\"error\":\"{}\"}}", esc(&e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listener_binds_loopback_and_nothing_else() {
        // The page writes a file whose contents are executed as shell commands.
        // If this ever becomes 0.0.0.0, anyone on the network can run code as
        // the operator on their next dictation.
        let addr = bind_addr(DEFAULT_PORT);
        assert!(addr.ip().is_loopback(), "must bind loopback, got {addr}");
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
    }

    #[test]
    fn binding_is_loopback_at_every_port() {
        for p in [1024u16, 8765, 65535] {
            assert!(bind_addr(p).ip().is_loopback());
        }
    }

    #[test]
    fn json_escapes_what_would_otherwise_break_the_page() {
        assert_eq!(esc(r#"a"b"#), r#"a\"b"#);
        assert_eq!(esc("a\\b"), "a\\\\b");
        assert_eq!(esc("a\nb"), "a\\nb");
        // A label with a control character must not emit a raw byte.
        assert!(esc("a\u{7}b").contains("\\u0007"));
    }

    #[test]
    fn field_reads_back_what_esc_wrote() {
        let body = format!("{{\"toml\":\"{}\"}}", esc("a\"b\nc\\d"));
        assert_eq!(field(&body, "toml").unwrap(), "a\"b\nc\\d");
    }

    #[test]
    fn field_returns_none_for_an_absent_key() {
        assert!(field("{\"a\":\"1\"}", "b").is_none());
    }

    #[test]
    fn a_saved_action_set_round_trips_through_the_real_loader() {
        // What the page saves must load back to exactly what was shown, or the
        // page is lying about the state of the system.
        let toml_text = "[[actions]]\nkey = \"1\"\nlabel = \"Tidy\"\ncommand = \"cat\"\ntimeout_ms = 5000\n";
        let parsed = config::parse(toml_text);
        assert_eq!(parsed.actions.len(), 1);
        assert_eq!(parsed.actions[0].label, "Tidy");
        assert_eq!(parsed.actions[0].key, Some('1'));
        assert_eq!(parsed.timeout_for(0), Duration::from_millis(5000));
    }

    #[test]
    fn save_refuses_a_duplicate_hotkey_and_says_so() {
        let toml_text = "[[actions]]\nkey=\"1\"\nlabel=\"a\"\ncommand=\"cat\"\n\
                         [[actions]]\nkey=\"1\"\nlabel=\"b\"\ncommand=\"cat\"\n";
        let body = format!("{{\"toml\":\"{}\"}}", esc(toml_text));
        let (status, out) = save(&body);
        assert_eq!(status, "400 Bad Request");
        assert!(out.contains("refused"), "{out}");
        assert!(out.contains("reuses hotkey"), "{out}");
    }

    #[test]
    fn save_refuses_an_entry_with_no_command() {
        let toml_text = "[[actions]]\nkey=\"1\"\nlabel=\"a\"\n";
        let body = format!("{{\"toml\":\"{}\"}}", esc(toml_text));
        let (status, out) = save(&body);
        assert_eq!(status, "400 Bad Request");
        assert!(out.contains("has no command"), "{out}");
    }

    #[test]
    fn save_refuses_malformed_toml() {
        let body = format!("{{\"toml\":\"{}\"}}", esc("not = = toml [[["));
        let (status, out) = save(&body);
        assert_eq!(status, "400 Bad Request");
        assert!(out.contains("not valid TOML"), "{out}");
    }

    #[test]
    fn save_without_a_toml_field_is_a_bad_request() {
        let (status, _) = save("{}");
        assert_eq!(status, "400 Bad Request");
    }

    #[test]
    fn reading_state_never_writes_to_the_config_file() {
        // Opening the page must be completely safe. Saving rewrites the file
        // from the action list — which is why comments do not survive a save,
        // and why the page and the README both say so — but merely LOOKING must
        // not touch anything.
        let dir = std::env::temp_dir().join(format!("vxr-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original = "# a comment the operator wrote\n[[actions]]\nkey = \"1\"\nlabel = \"Keep\"\ncommand = \"cat\"\n";
        std::fs::write(&path, original).unwrap();

        std::env::set_var("VOXTYPE_REVIEW_CONFIG", &path);
        let json = state_json();
        std::env::remove_var("VOXTYPE_REVIEW_CONFIG");

        assert!(json.contains("Keep"), "state should list the action: {json}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "reading state must leave the file byte-identical, comment included"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_page_is_embedded_and_not_a_stub() {
        assert!(PAGE.contains("voxtype-review"));
        assert!(PAGE.len() > 2000, "page looks truncated: {} bytes", PAGE.len());
    }
}
