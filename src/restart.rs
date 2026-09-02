//! `R` — restart the ccmux binaries in place (SPEC §8.11).
//!
//! Three populations of process run under one ccmux session, and only one of
//! them can be restarted the cheap way:
//!
//!   1. THIS sidebar. `exec()` replaces the process image without tmux ever
//!      being told, so the pane, its geometry and the window layout survive by
//!      construction — there is nothing to preserve, because nothing changes.
//!   2. The OTHER tabs' sidebars. Separate processes; this one cannot `exec`
//!      them. `respawn-pane -k` restarts the command inside their pane, which
//!      keeps the pane id, the geometry and the layout.
//!   3. The Claude panes. Each runs `claude attach <id>` under a shell wrapper,
//!      so respawning one is how a new `claude` binary is picked up. The AGENT
//!      is daemon-owned and outlives its pane (PROBE-FINDINGS §3), so an attach
//!      client is disposable.
//!
//! This module owns the two questions that answer badly if guessed: WHICH panes
//! may be respawned (`plan`, pure) and WHICH file to exec (`exe_path`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::tmux::{self, PaneId, PaneInfo, TabInfo};

/// The env var `R` uses to hand its summary to the process image it execs
/// into. The flash it names is written by a process that no longer exists by
/// the time anything could draw it, so the message has to survive the exec —
/// and the environment is the one thing that does.
pub const NOTE_ENV: &str = "CCMUX_RESTART_NOTE";

/// What a pane must be restarted as. The role is what decides the command, and
/// it can only be derived from ccmux's own ownership records — never from what
/// the pane happens to be running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// A window's `@ccmux_tab_sidebar`: restart it with the sidebar command.
    Sidebar,
    /// A pane in some window's `@ccmux_tab_map`: restart it with
    /// `claude attach <short_id>` under the usual wrapper.
    Claude { short_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub pane: PaneId,
    pub role: Role,
}

/// Everything `R` will touch, and everything it deliberately will not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// In window order, sidebar before that window's Claude panes.
    pub targets: Vec<Target>,
    /// Sidebars in `targets` — this process's own is NOT among them, because
    /// it restarts by `exec` and never by respawn.
    pub sidebars: usize,
    /// Claude panes in `targets`.
    pub claude: usize,
    /// Mapped, live panes whose entry carries no short id: there is no
    /// `claude attach` to rebuild, so they are left running (§9.7's rule, one
    /// verb later). Counted, never respawned.
    pub unattachable: usize,
}

/// THE OWNERSHIP RULE, and the only place it is decided.
///
/// A pane is restartable if and only if ccmux can prove it created it:
///
///   * it is some window's `@ccmux_tab_sidebar`, and that marker names a live
///     pane OF THAT WINDOW, or
///   * it appears in some window's `@ccmux_tab_map` and is still live.
///
/// Anything else is the operator's. The ccmux session holds unmanaged panes
/// right now — a shell, an editor, another agent — and respawning one would
/// destroy whatever was running in it with no undo. So the test is positive
/// evidence of ownership, never the absence of evidence to the contrary: a
/// pane missing from every map and every marker is not a candidate, whatever
/// window it is in and whatever it is running.
///
/// `me` — this process's own pane — is excluded because it is restarted by
/// `exec`, which is strictly better: no respawn, no kill, no new process, and
/// the terminal state is handed over rather than reset.
///
/// Pure, so the rule is testable without a tmux server, and so the test that an
/// unmanaged pane is never touched is a test of the rule itself rather than of
/// one call site's discipline.
pub fn plan(tabs: &[TabInfo], panes: &[PaneInfo], me: Option<&PaneId>) -> Plan {
    let live: HashSet<&PaneId> = panes.iter().map(|p| &p.id).collect();
    let mut out = Plan::default();
    let mut seen: HashSet<PaneId> = HashSet::new();
    if let Some(me) = me {
        // Never a target of a respawn, whatever the records say.
        seen.insert(me.clone());
    }

    for tab in tabs {
        // The marker is a window option that outlives the process it names, so
        // it can name a dead pane, or (after a hand-launched second sidebar) a
        // pane of another window. Both are refused: a marker is evidence only
        // about the window that carries it.
        if let Some(sb) = tab.sidebar.as_ref()
            && live.contains(sb)
            && panes.iter().any(|p| &p.id == sb && p.window_id == tab.window)
            && seen.insert(sb.clone())
        {
            out.targets.push(Target { pane: sb.clone(), role: Role::Sidebar });
            out.sidebars += 1;
        }

        // `BTreeMap`, so the order is deterministic and two runs of `R` issue
        // the same commands in the same order.
        for (raw, entry) in &tab.map.panes {
            let Some(pane) = PaneId::parse(raw) else {
                continue;
            };
            if !live.contains(&pane) || seen.contains(&pane) {
                continue;
            }
            if entry.short_id.is_empty() {
                // Written by a build that had no `short_id` field. There is no
                // command to rebuild, and guessing one is how a pane gets
                // respawned into something it never ran.
                out.unattachable += 1;
                continue;
            }
            seen.insert(pane.clone());
            out.targets.push(Target {
                pane,
                role: Role::Claude { short_id: entry.short_id.clone() },
            });
            out.claude += 1;
        }
    }
    out
}

/// The footer line the restarted sidebar shows. `sidebars` counts THIS process
/// too — it is restarting, by the better mechanism — because the operator asked
/// for a restart of the session and wants to read what happened to it, not a
/// census of which mechanism did what.
///
/// Sized for the 34-column default: "restarted 3 sidebars, 4 panes" is 29
/// columns, and the failure suffix only appears when there is a failure.
pub fn note(sidebars: usize, panes: usize, failed: usize, unattachable: usize) -> String {
    let mut s = format!(
        "restarted {sidebars} {}, {panes} {}",
        plural(sidebars, "sidebar"),
        plural(panes, "pane")
    );
    if failed > 0 {
        s.push_str(&format!(" ({failed} failed)"));
    } else if unattachable > 0 {
        s.push_str(&format!(" ({unattachable} skipped)"));
    }
    s
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 { word.to_string() } else { format!("{word}s") }
}

/// WHICH FILE TO EXEC — the one thing in this feature that fails silently if it
/// is got wrong.
///
/// `R` exists for the moment after `cargo install` replaced the binary, and
/// `cargo install` replaces it by RENAMING a new file over the old path. The
/// old inode then has no name, and every "where am I" answer the kernel offers
/// is about that dead inode. Measured, on this machine, in a process whose
/// binary was replaced after it started:
///
/// ```text
///   current_exe()        -> Ok("…/bin/probe (deleted)")   exec: ENOENT
///   readlink /proc/self/exe -> "…/bin/probe (deleted)"    exec: ENOENT
///   exec("/proc/self/exe")  -> ran the OLD image again    silently a no-op
///   argv[0]              -> "…/bin/probe"                 ran the NEW image
/// ```
///
/// So `current_exe()` — the obvious choice, and the one `sidebar_command` uses
/// at LAUNCH time where it is correct — would make `R` either fail outright or,
/// through `/proc/self/exe`, restart the very binary the operator just
/// replaced while reporting success. argv[0] is a PATH, resolved fresh by the
/// kernel at exec time, which is exactly the semantics this needs.
///
/// The sidebar's argv[0] is an absolute path in every path that starts one:
/// `sidebar_command` builds the command from `current_exe()` at launch, when it
/// is still the live inode. A hand-typed `ccmux sidebar` gives a bare name
/// instead, which is why the PATH branch exists.
///
/// Everything is resolved to a path that EXISTS before any pane is killed, so a
/// half-finished upgrade cannot cost the operator their sidebars.
pub fn exe_path() -> Option<PathBuf> {
    let argv0 = std::env::args_os().next().map(PathBuf::from).unwrap_or_default();
    if !argv0.as_os_str().is_empty() {
        if argv0.components().count() > 1 {
            // A path, relative or absolute. The cwd is the pane's, unchanged
            // since launch, so a relative one still resolves.
            if runnable(&argv0) {
                return Some(argv0);
            }
        } else if let Some(found) = which(&argv0) {
            return Some(found);
        }
    }
    // Last resort: `current_exe()` with the kernel's `" (deleted)"` marker
    // stripped, accepted only when a runnable file is actually there. This is
    // the ordinary case for a build that was NOT replaced, and the marker strip
    // is what makes it the right answer for one that was.
    let cur = strip_deleted(&std::env::current_exe().ok()?);
    runnable(&cur).then_some(cur)
}

/// A regular file with an execute bit set.
///
/// The mode check is not pedantry, it is the pre-check doing its job. `R`
/// respawns every other pane BEFORE it execs, so a path that passes here and
/// fails at `execve` costs the operator the sidebars that were already
/// restarted with it. Verified live: a `chmod -x` on the installed binary
/// passes `is_file()`, and the restart then took out the other tab's sidebar
/// on its way to an `EACCES` this process could only report. The mode check
/// refuses that restart before anything is killed.
///
/// It is a pre-check, not a guarantee — the file can still be replaced between
/// this call and the `exec`, which is why the failure path exists at all.
fn runnable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `/path/to/ccmux (deleted)` -> `/path/to/ccmux`. The suffix is the kernel's,
/// appended to the `/proc/self/exe` link target once the inode loses its last
/// name; Rust's `current_exe` passes it through verbatim.
fn strip_deleted(p: &Path) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => p.to_path_buf(),
    }
}

/// First executable named `name` on `$PATH`. Used only for a bare argv[0].
fn which(name: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|c| runnable(c))
}

/// The shell command that starts a sidebar running `exe`, for the panes this
/// process cannot `exec`.
///
/// It is built from THIS process's own argv — `exe` plus every argument after
/// argv[0] — rather than from `App::sidebar_cmd`, and that is the point:
/// `sidebar_cmd` was built at launch from a `current_exe()` that may now name a
/// deleted inode, so respawning another tab's sidebar with it would leave that
/// pane running nothing at all. Same binary, same flags, same session, same
/// socket, same width — resolved fresh.
pub fn sidebar_command(exe: &Path) -> Option<String> {
    let exe = exe.to_str()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut parts: Vec<&str> = Vec::with_capacity(args.len() + 1);
    parts.push(exe);
    parts.extend(args.iter().map(String::as_str));
    Some(tmux::sh_join(&parts))
}

/// A restart that `App` has authorised and `main` has still to perform: every
/// other pane has been respawned, and what is left is this process's own image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The file to exec — resolved and proven to exist BEFORE anything was
    /// killed, so the exec is not a leap of faith.
    pub exe: PathBuf,
    /// What to say once the new image is drawing. Handed over in `NOTE_ENV`.
    pub note: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::{HiddenLog, PaneEntry, PaneMap, WindowId};

    fn pane(id: &str, window: &str) -> PaneInfo {
        PaneInfo {
            id: PaneId::parse(id).expect("pane id"),
            index: 0,
            left: 0,
            top: 0,
            width: 80,
            height: 24,
            active: false,
            window_index: 1,
            window_id: WindowId::parse(window).expect("window id"),
            window_active: true,
            session_clients: 1,
            window_viewers: Some(1),
        }
    }

    fn map(entries: &[(&str, &str)]) -> PaneMap {
        let mut m = PaneMap::new();
        for (pane, short) in entries {
            m.insert(
                &PaneId::parse(pane).expect("pane id"),
                PaneEntry {
                    session_id: format!("sid-{short}"),
                    short_id: (*short).to_string(),
                    name: "n".into(),
                    opened_at: 0,
                },
            );
        }
        m
    }

    fn tab(window: &str, sidebar: Option<&str>, entries: &[(&str, &str)]) -> TabInfo {
        TabInfo {
            window: WindowId::parse(window).expect("window id"),
            index: 1,
            sidebar: sidebar.map(|s| PaneId::parse(s).expect("pane id")),
            map: map(entries),
            hidden: HiddenLog::new(),
        }
    }

    fn ids(plan: &Plan) -> Vec<String> {
        plan.targets.iter().map(|t| t.pane.to_string()).collect()
    }

    /// THE SAFETY TEST. `%3` is in no map and is no window's marker — it is the
    /// operator's own pane, in ccmux's own window, and `R` must not so much as
    /// name it.
    #[test]
    fn an_unmanaged_pane_is_never_a_restart_target() {
        let panes = vec![pane("%1", "@1"), pane("%2", "@1"), pane("%3", "@1")];
        let tabs = vec![tab("@1", Some("%1"), &[("%2", "aaaaaaaa")])];

        let p = plan(&tabs, &panes, None);
        assert_eq!(ids(&p), vec!["%1", "%2"]);
        assert_eq!((p.sidebars, p.claude, p.unattachable), (1, 1, 0));
        assert!(
            !p.targets.iter().any(|t| t.pane.as_str() == "%3"),
            "an unmanaged pane in ccmux's own window is not ccmux's to respawn"
        );

        // And it stays untouched when it is the ACTIVE pane, and when it is the
        // only thing in a window of its own: neither is evidence of ownership.
        let mut panes = panes;
        panes[2].active = true;
        panes[2].window_id = WindowId::parse("@9").expect("window id");
        let p = plan(&tabs, &panes, None);
        assert_eq!(ids(&p), vec!["%1", "%2"]);
    }

    /// The whole session, not just this tab: every marked sidebar and every
    /// mapped pane of every window, in window order, sidebar first.
    #[test]
    fn every_tabs_sidebar_and_mapped_panes_are_targets() {
        let panes = vec![
            pane("%1", "@1"),
            pane("%2", "@1"),
            pane("%4", "@2"),
            pane("%5", "@2"),
            pane("%6", "@2"),
        ];
        let tabs = vec![
            tab("@1", Some("%1"), &[("%2", "aaaaaaaa")]),
            tab("@2", Some("%4"), &[("%5", "bbbbbbbb"), ("%6", "cccccccc")]),
        ];
        let p = plan(&tabs, &panes, None);
        assert_eq!(ids(&p), vec!["%1", "%2", "%4", "%5", "%6"]);
        assert_eq!((p.sidebars, p.claude), (2, 3));
        assert_eq!(
            p.targets[1].role,
            Role::Claude { short_id: "aaaaaaaa".into() },
            "a mapped pane is restarted as its own attach client"
        );
    }

    /// This process restarts by `exec`, so its own pane must never appear in
    /// the respawn list — a respawn would kill the process mid-`R`.
    #[test]
    fn my_own_pane_is_excluded_from_the_respawn_list() {
        let panes = vec![pane("%1", "@1"), pane("%2", "@1")];
        let tabs = vec![tab("@1", Some("%1"), &[("%2", "aaaaaaaa")])];
        let me = PaneId::parse("%1").expect("pane id");
        let p = plan(&tabs, &panes, Some(&me));
        assert_eq!(ids(&p), vec!["%2"]);
        assert_eq!((p.sidebars, p.claude), (0, 1));
    }

    /// Dead records name nothing live, and a marker is evidence only about the
    /// window that carries it.
    #[test]
    fn dead_and_foreign_records_are_dropped() {
        let panes = vec![pane("%1", "@1"), pane("%7", "@2")];
        let tabs = vec![
            // `%9` died; `%8` was never live.
            tab("@1", Some("%9"), &[("%8", "aaaaaaaa")]),
            // The marker names a live pane, but of ANOTHER window.
            tab("@2", Some("%1"), &[]),
        ];
        let p = plan(&tabs, &panes, None);
        assert!(p.targets.is_empty(), "{:?}", ids(&p));
        assert_eq!((p.sidebars, p.claude, p.unattachable), (0, 0, 0));
    }

    /// A map entry with no short id has no command to rebuild. It is counted
    /// and reported, never respawned into a guess.
    #[test]
    fn a_mapped_pane_without_a_short_id_is_counted_not_respawned() {
        let panes = vec![pane("%1", "@1"), pane("%2", "@1")];
        let tabs = vec![tab("@1", Some("%1"), &[("%2", "")])];
        let p = plan(&tabs, &panes, None);
        assert_eq!(ids(&p), vec!["%1"]);
        assert_eq!(p.unattachable, 1);
        assert_eq!(note(1, 0, 0, p.unattachable), "restarted 1 sidebar, 0 panes (1 skipped)");
    }

    /// One pane, one respawn, however many maps name it.
    #[test]
    fn a_pane_named_by_two_maps_is_respawned_once() {
        let panes = vec![pane("%1", "@1"), pane("%2", "@1")];
        let tabs = vec![
            tab("@1", Some("%1"), &[("%2", "aaaaaaaa")]),
            tab("@2", None, &[("%2", "aaaaaaaa")]),
        ];
        let p = plan(&tabs, &panes, None);
        assert_eq!(ids(&p), vec!["%1", "%2"]);
        assert_eq!(p.claude, 1);
    }

    #[test]
    fn the_note_names_what_happened_and_fits_the_default_width() {
        assert_eq!(note(3, 4, 0, 0), "restarted 3 sidebars, 4 panes");
        assert_eq!(note(1, 1, 0, 0), "restarted 1 sidebar, 1 pane");
        assert_eq!(note(2, 3, 1, 0), "restarted 2 sidebars, 3 panes (1 failed)");
        assert!(
            crate::model::display_width(&note(3, 4, 0, 0)) <= 34,
            "the summary must fit the 34-column default"
        );
    }

    /// The kernel's marker for a replaced binary, which is what `current_exe()`
    /// hands back after a `cargo install`.
    #[test]
    fn the_deleted_marker_is_stripped() {
        assert_eq!(
            strip_deleted(Path::new("/home/x/.cargo/bin/ccmux (deleted)")),
            PathBuf::from("/home/x/.cargo/bin/ccmux")
        );
        assert_eq!(
            strip_deleted(Path::new("/home/x/.cargo/bin/ccmux")),
            PathBuf::from("/home/x/.cargo/bin/ccmux")
        );
    }

    /// The exec path must resolve to a file that EXISTS — the whole point is
    /// that it is re-resolved rather than inherited from a stale inode.
    #[test]
    fn the_exec_path_exists_before_anything_is_killed() {
        let exe = exe_path().expect("the test binary is on disk");
        assert!(runnable(&exe), "{exe:?}");
    }

    /// `is_file()` is not enough, and the difference is not academic: `R`
    /// respawns every other pane before it execs, so a path that passes the
    /// pre-check and fails at `execve` costs the sidebars already restarted
    /// with it.
    #[test]
    fn a_readable_but_not_executable_file_is_not_a_binary() {
        let dir = std::env::temp_dir().join(format!("ccmux-runnable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let f = dir.join("ccmux");
        std::fs::write(&f, b"#!/bin/sh\n").expect("write");

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(f.is_file(), "the weaker check passes");
        assert!(!runnable(&f), "the mode check refuses it");

        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(runnable(&f));

        assert!(!runnable(&dir), "a directory is never a binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The respawn command is this process's own argv with the resolved binary
    /// in front, so every flag rides along and nothing is re-derived.
    #[test]
    fn the_sidebar_command_is_the_resolved_binary_plus_my_own_argv() {
        let cmd = sidebar_command(Path::new("/opt/ccmux")).expect("build");
        assert!(cmd.starts_with("/opt/ccmux"), "{cmd:?}");
        let args: Vec<String> = std::env::args().skip(1).collect();
        for a in &args {
            assert!(cmd.contains(&tmux::sh_quote(a)), "{a:?} missing from {cmd:?}");
        }
    }
}
