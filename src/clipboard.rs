//! The system clipboard without linking a clipboard library: use whichever
//! helper the desktop provides. Copying falls back to the OSC 52 terminal
//! escape, which also works over SSH.

use std::io::Write;
use std::process::{Command, Stdio};

type Run = (&'static str, &'static [&'static str]);

/// A clipboard helper: the command that copies and the one that pastes.
struct Helper {
    copy: Run,
    paste: Run,
}

/// The helpers this desktop may have, best first.
fn helpers() -> Vec<Helper> {
    let mut helpers = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        helpers.push(Helper {
            copy: ("wl-copy", &[]),
            paste: ("wl-paste", &["--no-newline", "--type", "text"]),
        });
    }
    if std::env::var_os("DISPLAY").is_some() {
        helpers.push(Helper {
            copy: ("xclip", &["-selection", "clipboard"]),
            paste: ("xclip", &["-selection", "clipboard", "-o"]),
        });
        helpers.push(Helper {
            copy: ("xsel", &["--clipboard", "--input"]),
            paste: ("xsel", &["--clipboard", "--output"]),
        });
    }
    if cfg!(target_os = "macos") {
        helpers.push(Helper {
            copy: ("pbcopy", &[]),
            paste: ("pbpaste", &[]),
        });
    }
    helpers
}

/// Copies `text` and says how, for the status line.
pub fn copy(text: &str) -> Result<&'static str, String> {
    for helper in helpers() {
        let (program, args) = helper.copy;
        if pipe_to(program, args, text) {
            return Ok(program);
        }
    }
    osc52(text).map(|()| "terminal").map_err(|e| e.to_string())
}

/// The text on the clipboard, or `None` when there is no helper to read it
/// with, or nothing on it that is text. There is no terminal fallback: few
/// terminals let a program read the clipboard.
pub fn paste() -> Option<String> {
    helpers().into_iter().find_map(|helper| {
        let (program, args) = helper.paste;
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        String::from_utf8(output.stdout)
            .ok()
            .filter(|text| !text.is_empty())
    })
}

fn pipe_to(program: &str, args: &[&str], text: &str) -> bool {
    // The helper may stay alive to serve the clipboard, so it must not hold
    // on to the terminal.
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    written && child.wait().is_ok_and(|status| status.success())
}

fn osc52(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("blåbær".as_bytes()), "YmzDpWLDpnI=");
    }
}
