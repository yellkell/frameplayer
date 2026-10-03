//! Hand text to the user: clipboard, open a file in a text editor, open a
//! URL in the browser. All best effort; callers print a fallback.
//!
//! Windows uses the Win32 clipboard (`clipboard-win`), `notepad.exe` and the
//! URL protocol handler (no `cmd /c start`, whose parser mangles `&` in
//! URLs). macOS uses `pbcopy`/`open`; Linux `wl-copy`/`xclip`/`xsel` and
//! `xdg-open`.

use anyhow::{bail, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// Put `text` on the clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    #[cfg(windows)]
    {
        clipboard_win::set_clipboard_string(text).map_err(|e| anyhow::anyhow!("clipboard: {e}"))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
            &[("pbcopy", &[])]
        } else {
            &[
                ("wl-copy", &[]),
                ("xclip", &["-selection", "clipboard"]),
                ("xsel", &["--clipboard", "--input"]),
            ]
        };
        for (bin, args) in tools {
            if crate::ssh::find_binary(bin).is_none() {
                continue;
            }
            if pipe_to(bin, args, text).is_ok() {
                return Ok(());
            }
        }
        bail!("no clipboard tool available")
    }
}

#[cfg(not(windows))]
fn pipe_to(bin: &str, args: &[&str], text: &str) -> Result<()> {
    use std::io::Write;
    let mut c = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    c.stdin.take().expect("piped").write_all(text.as_bytes())?;
    if !c.wait()?.success() {
        bail!("{bin} failed");
    }
    Ok(())
}

fn spawn_detached(mut c: Command) -> Result<()> {
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = c.spawn()?;
    // Reap in the background so no zombie is left on Unix.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Open a text file for the user to look at (non-blocking).
pub fn open_text_file(path: &Path) -> Result<()> {
    let mut c;
    if cfg!(windows) {
        c = Command::new("notepad.exe");
        c.arg(path);
    } else if cfg!(target_os = "macos") {
        c = Command::new("open");
        c.arg("-t").arg(path);
    } else {
        if crate::ssh::find_binary("xdg-open").is_none() {
            bail!("xdg-open not found");
        }
        c = Command::new("xdg-open");
        c.arg(path);
    }
    spawn_detached(c)
}

/// Open `url` in the default browser (non-blocking).
pub fn open_url(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("refusing to open non-https URL");
    }
    let mut c;
    if cfg!(windows) {
        // Hands the URL straight to the registered protocol handler.
        c = Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler").arg(url);
    } else if cfg!(target_os = "macos") {
        c = Command::new("open");
        c.arg(url);
    } else {
        if crate::ssh::find_binary("xdg-open").is_none() {
            bail!("xdg-open not found");
        }
        c = Command::new("xdg-open");
        c.arg(url);
    }
    spawn_detached(c)
}
