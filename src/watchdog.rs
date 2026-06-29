//! A tiny sibling process that guarantees the system taskbar is restored even when the
//! dock dies *abnormally* — a crash, a panic-abort, or being killed from Task Manager —
//! cases where our in-process `taskbar::restore()` on exit never runs.
//!
//! Design (near-zero cost, one binary):
//! * The dock launches a second copy of ITSELF with `--watchdog <pid> <orig_autohide>`
//!   (no separate executable — respects the "one exe" rule; it's just another mode).
//! * The watchdog opens the dock process and blocks in `WaitForSingleObject(INFINITE)`.
//!   A thread waiting on a kernel object is never scheduled until it is signalled, so the
//!   watchdog uses **0% CPU** while the dock runs; we also trim its working set so its
//!   idle memory footprint is negligible.
//! * When the dock process ends *for any reason*, the wait returns and the watchdog puts
//!   the taskbar back to the user's original auto-hide preference, then exits.
//! * On a CLEAN exit the dock first `kill()`s the watchdog (while it is still blocked),
//!   so the watchdog never double-restores; the dock then restores the bar itself.
//! * A power loss — or both processes being killed at once — is caught separately by the
//!   on-disk guard flag, which `taskbar::recover_if_stranded()` consults at next start.

use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

use windows::Win32::Foundation::{CloseHandle, BOOL};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, SetProcessWorkingSetSize, WaitForSingleObject, INFINITE,
    PROCESS_SYNCHRONIZE,
};

use crate::{desktop_icons, taskbar};

const WATCHDOG_FLAG: &str = "--watchdog";
/// Don't pop a console window for the helper process.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Parsed `--watchdog <pid> <orig_autohide>` arguments.
pub struct WatchdogArgs {
    parent_pid: u32,
    original_autohide: bool,
    restore_taskbar: bool,
    restore_desktop_icons: bool,
}

/// If this process was launched as the watchdog, return its parsed arguments. Checked at
/// the very top of `main`, before any GUI / single-instance setup.
pub fn parse_args() -> Option<WatchdogArgs> {
    parse_from(std::env::args().skip(1))
}

/// Argument parsing, factored out of the environment for unit testing. The iterator must
/// already have the program name skipped, so its first item is the flag.
fn parse_from(mut args: impl Iterator<Item = String>) -> Option<WatchdogArgs> {
    if args.next().as_deref() != Some(WATCHDOG_FLAG) {
        return None;
    }
    let parent_pid = args.next()?.parse().ok()?;
    let original_autohide = args.next().as_deref() == Some("1");
    let restore_taskbar = args.next().as_deref() == Some("1");
    let restore_desktop_icons = args.next().as_deref() == Some("1");
    Some(WatchdogArgs {
        parent_pid,
        original_autohide,
        restore_taskbar,
        restore_desktop_icons,
    })
}

/// Launch a sibling watchdog (a second copy of our own exe in `--watchdog` mode).
/// Returns the child so the dock can `kill()` it on a clean exit. `None` if it could not
/// be started — the on-disk guard flag still covers us at next launch.
pub fn spawn(
    original_autohide: bool,
    restore_taskbar: bool,
    restore_desktop_icons: bool,
) -> Option<Child> {
    let exe = std::env::current_exe().ok()?;
    Command::new(exe)
        .arg(WATCHDOG_FLAG)
        .arg(std::process::id().to_string())
        .arg(if original_autohide { "1" } else { "0" })
        .arg(if restore_taskbar { "1" } else { "0" })
        .arg(if restore_desktop_icons { "1" } else { "0" })
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()
}

/// Watchdog entry point: sleep until the dock disappears, then restore the taskbar.
/// Costs nothing while waiting and exits immediately afterwards.
pub fn run(args: WatchdogArgs) {
    unsafe {
        // We only need to wake once — release our pages back to the OS while we sleep so
        // the helper's idle footprint is tiny (they fault back in for the restore).
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);

        // Holding the process HANDLE (not just the pid) makes the wait immune to pid
        // recycling: it refers to this exact process instance until we close it.
        if let Ok(parent) = OpenProcess(PROCESS_SYNCHRONIZE, BOOL(0), args.parent_pid) {
            if !parent.is_invalid() {
                // 0% CPU until the dock ends — clean kill, crash, panic, or power-off.
                WaitForSingleObject(parent, INFINITE);
                let _ = CloseHandle(parent);
            }
        }
        // The dock is gone (or never existed): undo only the system state this watchdog
        // was armed to protect. This keeps a desktop-icon-only helper from rewriting a
        // taskbar preference the user changed while FeatherDock was running.
        if args.restore_taskbar {
            taskbar::restore_to(args.original_autohide);
            taskbar::clear_guard();
        }
        if args.restore_desktop_icons {
            desktop_icons::set_hidden(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> std::vec::IntoIter<String> {
        items
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_watchdog_invocation() {
        let parsed = parse_from(args(&["--watchdog", "4321", "1", "1", "0"])).unwrap();
        assert_eq!(parsed.parent_pid, 4321);
        assert!(parsed.original_autohide);
        assert!(parsed.restore_taskbar);
        assert!(!parsed.restore_desktop_icons);
    }

    #[test]
    fn original_autohide_is_false_unless_explicitly_one() {
        assert!(
            !parse_from(args(&["--watchdog", "10", "0", "0", "0"]))
                .unwrap()
                .original_autohide
        );
        // Missing / malformed flags default to no restore rather than failing.
        assert!(
            !parse_from(args(&["--watchdog", "10"]))
                .unwrap()
                .original_autohide
        );
        assert!(
            !parse_from(args(&["--watchdog", "10"]))
                .unwrap()
                .restore_taskbar
        );
        assert!(
            !parse_from(args(&["--watchdog", "10"]))
                .unwrap()
                .restore_desktop_icons
        );
    }

    #[test]
    fn parses_independent_restore_scopes() {
        let parsed = parse_from(args(&["--watchdog", "42", "0", "0", "1"])).unwrap();
        assert!(!parsed.original_autohide);
        assert!(!parsed.restore_taskbar);
        assert!(parsed.restore_desktop_icons);
    }

    #[test]
    fn ignores_a_normal_launch() {
        assert!(parse_from(args(&[])).is_none());
        assert!(parse_from(args(&["C:/some/path.lnk"])).is_none());
        assert!(parse_from(args(&["--other", "1"])).is_none());
    }

    #[test]
    fn rejects_a_non_numeric_pid() {
        assert!(parse_from(args(&["--watchdog", "notapid", "1"])).is_none());
    }
}
