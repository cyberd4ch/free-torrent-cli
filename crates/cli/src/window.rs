//! The Windows window.
//!
//! `ftc gui` hands the local page to the default browser everywhere else, and
//! on macOS the app bundle wraps the same page in a real NSWindow. On Windows
//! that left a tab with an address bar and a Back button, which is a web site,
//! not an application. Edge and Chrome both ship Chromium's app mode
//! (`--app=<url>`), so the page opens in a window with no browser chrome, at a
//! fixed size, in a profile directory of its own — it never lands in someone's
//! normal browsing history, and closing the window is how you quit.
//!
//! Only the Windows build has anything to do here; elsewhere this compiles to
//! stubs so `run_server` stays one shape.

#[cfg(target_os = "windows")]
mod win {
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};

    // kernel32, already linked by the standard library's CRT dependency.
    extern "system" {
        fn FreeConsole() -> i32;
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }

    /// Close the console window a shortcut-launched `ftc gui` would otherwise
    /// leave behind its app window.
    ///
    /// Only when this process is the console's *only* user. Typed into a real
    /// terminal, the shell shares that console, and detaching must not make the
    /// user's terminal vanish — `GetConsoleProcessList` is how we tell the two
    /// cases apart, and the two-entry buffer is enough for that: 1 means "ours".
    pub fn hide_console() {
        let mut pids = [0u32; 2];
        let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 2) };
        if count == 1 {
            unsafe {
                FreeConsole();
            }
        }
    }

    /// Edge first — it is present on every Windows 10/11 install — then Chrome,
    /// in the two places each is normally installed.
    fn browser_candidates() -> Vec<PathBuf> {
        let mut out = Vec::new();
        let roots = [
            std::env::var_os("ProgramFiles"),
            std::env::var_os("ProgramFiles(x86)"),
            std::env::var_os("LOCALAPPDATA"),
        ];
        for root in roots.into_iter().flatten() {
            let root = PathBuf::from(root);
            out.push(root.join("Microsoft/Edge/Application/msedge.exe"));
            out.push(root.join("Google/Chrome/Application/chrome.exe"));
        }
        out
    }

    /// Spawn the app window, or `None` if no Chromium browser was found so the
    /// caller can fall back to the default one.
    pub fn open_app_window(url: &str) -> Option<Child> {
        let local = std::env::var_os("LOCALAPPDATA")?;
        let profile = PathBuf::from(local).join("Footorrent").join("ui");
        let _ = std::fs::create_dir_all(&profile);

        for browser in browser_candidates() {
            if !browser.is_file() {
                continue;
            }
            let spawned = Command::new(&browser)
                .arg(format!("--app={url}"))
                .arg(format!("--user-data-dir={}", profile.display()))
                .arg("--window-size=1180,820")
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(child) = spawned {
                return Some(child);
            }
        }
        None
    }
}

#[cfg(target_os = "windows")]
pub use win::{hide_console, open_app_window};

#[cfg(not(target_os = "windows"))]
pub fn hide_console() {}

#[cfg(not(target_os = "windows"))]
pub fn open_app_window(_url: &str) -> Option<std::process::Child> {
    None
}
