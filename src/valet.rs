//! The clipboard valet — park what was there, get it back on demand.
//!
//! T-023. The operator dictates with a screenshot on the clipboard and wants
//! BOTH: the dictation lands as text everywhere (including opencode, whose
//! TUI reads the clipboard late and prefers image/png), AND the screenshot is
//! not lost. Those contend for the one X clipboard if "restore" means racing
//! the readers — so it must not mean that. Instead:
//!
//!   * voxtype's own restore stays OFF; after a paste its text owner just
//!     keeps serving the transcript. Every reader, however late, gets text.
//!   * before the paste, the hook parks any IMAGE content the clipboard
//!     held into the state dir. Nothing serves it anymore — the failure mode
//!     where opencode attaches the screenshot is removed, not out-raced.
//!   * `voxtype-review --unpark` puts the parked image back on the clipboard
//!     in one explicit keystroke. The operator is told, via notification,
//!     that parking happened.
//!
//! Text is never parked: after a dictation the clipboard holds the dictated
//! text, which IS text — parking it would be a no-op with extra steps.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const PARK_DIR: &str = "park";
pub const PARK_FILE: &str = "parkedImage.png";
pub const PARK_META: &str = "parked.txt";

/// One append-only line per park decision — the valet was invisible in the
/// field until this existed (T-023: a dictation ran with park skipped and
/// left no trace of why).
fn valet_log(msg: &str) {
    if let Some(dir) = state_dir() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true).open(dir.join("valet-watch.log"))
        {
            let _ = writeln!(f, "[{}] {msg}", chrono_like_now());
        }
    }
}

fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    let dir = base.join("voxtype-review").join(PARK_DIR);
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Run xclip and return its stdout bytes. Empty on any failure — the valet
/// must never break a dictation over a parking problem.
///
/// Input mode (`-i`) is special: xclip takes ownership and serves the
/// selection FOREVER, so waiting for it would hang. We write stdin, close
/// it (that is how xclip knows the data is complete), and let it run.
fn xclip(args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
    let mut cmd = Command::new("xclip");
    cmd.args(args);
    if let Some(data) = stdin {
        cmd.stdin(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        if let Some(mut si) = child.stdin.take() {
            use std::io::Write;
            let _ = si.write_all(data);
        } // dropping si closes stdin; xclip detaches to serve
        Vec::new()
    } else {
        cmd.stdout(Stdio::piped())
            .output()
            .map(|o| if o.status.success() { o.stdout } else { Vec::new() })
            .unwrap_or_default()
    }
}

/// True when the clipboard currently serves an image target. Text-only
/// clipboards are not worth parking (see module comment).
pub fn clipboard_holds_image() -> bool {
    let targets = xclip(&["-o", "-selection", "clipboard", "-t", "TARGETS"], None);
    String::from_utf8_lossy(&targets).contains("image/")
}

/// Park the clipboard's image content. Returns the park path on success so
/// the caller can name it in the notification; None means "nothing to park"
/// or "parking failed" — both are non-events for the dictation itself.
pub fn park() -> Option<PathBuf> {
    let holds_image = clipboard_holds_image();
    let targets = xclip(&["-o", "-selection", "clipboard", "-t", "TARGETS"], None);
    valet_log(&format!(
        "park check: holds_image={holds_image} targets=[{}]",
        String::from_utf8_lossy(&targets).replace('\n', ",")
    ));
    if !holds_image {
        return None;
    }
    let dir = state_dir()?;
    let png = xclip(
        &["-o", "-selection", "clipboard", "-t", "image/png"],
        None,
    );
    if png.is_empty() {
        return None;
    }
    let file = dir.join(PARK_FILE);
    fs::write(&file, &png).ok()?;
    let meta = format!(
        "parked={} bytes={} via=T-023-valet\n",
        chrono_like_now(),
        png.len()
    );
    let _ = fs::write(dir.join(PARK_META), meta);
    Some(file)
}

/// Put the parked image back on the clipboard. Returns the restored path, or
/// None when nothing is parked (the honest answer to a double-unpark).
pub fn unpark() -> Option<PathBuf> {
    let dir = state_dir()?;
    let file = dir.join(PARK_FILE);
    let png = fs::read(&file).ok()?;
    if png.is_empty() {
        return None;
    }
    let out = xclip(
        &["-selection", "clipboard", "-t", "image/png", "-i"],
        Some(&png),
    );
    let _ = out;
    let _ = fs::remove_file(&file);
    let _ = fs::remove_file(dir.join(PARK_META));
    Some(file)
}

/// "2026-08-31T09:00:00Z" without pulling in a time crate for one log line.
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        (secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60)
}

/// Howard Hinnant's days-to-civil algorithm, trimmed.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Start the defusing watcher for this dictation. Detached and silent by
/// design: it must outlive this process (voxtype's set and restore happen
/// after our EOF) and must never be able to break the paste itself. The
/// watcher's script is looked up next to the binary first (installed
/// layouts), then in the repo (dev runs).
pub fn spawn_watcher(transcript: &str) {
    let Some(dir) = state_dir() else { return };
    if fs::write(dir.join("transcript.txt"), transcript).is_err() {
        return;
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("valet-watch.py")))
        .filter(|p| p.exists())
        .or_else(|| {
            // Dev fallback: the script lives in the repo next to spikes/lib.
            let mut cwd = std::env::current_dir().ok()?;
            for _ in 0..3 {
                cwd.push("spikes/lib/valet-watch.py");
                if cwd.exists() {
                    return Some(cwd);
                }
                cwd.pop();
                cwd.pop();
            }
            None
        });
    let Some(script) = exe else { return };
    use std::process::Stdio;
    let log = dir.join("valet-watch.log");
    let out = std::fs::File::create(&log)
        .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());
    let err = std::fs::OpenOptions::new().append(true).open(&log)
        .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());
    let ok = Command::new("python3")
        .arg(&script)
        .arg(&dir)
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map(|_| true)
        .unwrap_or(false);
    if ok {
        let _ = Command::new("notify-send")
            .args(["-t", "4000", "voxtype-review",
                   "screenshot parked — voxtype-review --unpark restores it"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_right_at_the_era_edges() {
        // 1970-01-01 and 2026-08-31 in days since the epoch.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_696), (2026, 8, 31));
    }

    #[test]
    fn unpark_with_an_empty_park_answers_none() {
        // Point the state dir at a throwaway HOME so the test cannot eat a
        // real park, and so it passes on machines without X.
        let tmp = tempdir_for_tests();
        std::env::set_var("XDG_STATE_HOME", &tmp);
        assert!(unpark().is_none());
        let _ = std::fs::remove_dir_all(tmp);
    }

    fn tempdir_for_tests() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "voxtype-review-valet-test-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&p);
        p
    }
}
