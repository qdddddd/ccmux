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
//! THE ORDER IS THE SAFETY PROPERTY. Population 1 goes FIRST, and populations
//! 2 and 3 are respawned by the image it `exec`s into — never by the image that
//! is about to be replaced. A `respawn-pane -k` cannot be undone: the pane is
//! killed, the new command runs, and if that command exits at once the pane
//! closes and takes the window layout with it. Doing that to another tab's
//! sidebar on the strength of a binary this process has not yet proven can run
//! is exactly how a half-finished upgrade costs the operator every sidebar in
//! the session. So the candidate is `exec`d first, and the fact that the new
//! image is RUNNING is what licenses it to touch anyone else's pane. If the
//! `exec` fails, nothing anywhere has been killed and this sidebar says so.
//!
//! This module owns the three questions that answer badly if guessed: WHICH
//! panes may be respawned (`plan`, pure), WHICH file to exec (`exe_path`), and
//! whether that file actually RUNS (`probe`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::model::{Group, Session};
use crate::tmux::{self, PaneId, PaneInfo, TabInfo};

/// The env var that tells a fresh sidebar it is the second half of an `R`, and
/// so must finish the job by respawning everything this image could not.
///
/// Its VALUE is the pid of the process that set it, and that is what makes the
/// handoff self-validating: `exec(2)` keeps the pid, so the image that comes up
/// matches, while any other process that merely INHERITED the variable does
/// not. Without that check a stray `CCMUX_RESTART` in the tmux server's
/// environment would reach every sidebar tmux ever starts, and each would
/// respawn all the others on startup — a respawn loop with no exit.
pub const HANDOFF_ENV: &str = "CCMUX_RESTART";

/// The value `main::exec_pending` writes into `HANDOFF_ENV`: this process's own
/// pid, which `exec` preserves across the image swap.
pub fn handoff_token() -> String {
    std::process::id().to_string()
}

/// Is this image the one an `R` in this very process `exec`d into? Only then
/// may it respawn other panes — see the module header on ordering.
pub fn is_handoff() -> bool {
    handoff_matches(std::env::var(HANDOFF_ENV).ok().as_deref())
}

/// The rule `is_handoff` applies, split out so it can be tested without
/// writing to the process environment — `setenv` while other test threads sit
/// inside `getenv` is exactly the race Rust 2024 made `set_var` unsafe for.
fn handoff_matches(value: Option<&str>) -> bool {
    value.is_some_and(|v| v == handoff_token())
}

/// How long `probe` waits for the candidate binary to answer `--version`.
/// Generous beside a cold start (milliseconds) and short enough that a binary
/// that hangs costs a keypress rather than the session.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How often `probe` looks at the child. Small enough to be invisible.
const PROBE_STEP: Duration = Duration::from_millis(5);

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
    /// Mapped, live panes whose attach has already exited and handed the pane
    /// to a shell (`#{@ccmux_detached}`). Counted, never respawned.
    pub detached: usize,
}

impl Plan {
    /// What `note` reports as "skipped": everything ccmux owns, found alive,
    /// and deliberately left running.
    pub fn skipped(&self) -> usize {
        self.unattachable + self.detached
    }

    /// THE AGENT SCOPE: the short ids whose AGENT `R` may restart, in the order
    /// the panes will be respawned.
    ///
    /// It is derived from `targets`, and that is the whole rule: an agent is in
    /// scope **exactly when this same plan is about to respawn a pane with
    /// `claude attach <id>`**. Nothing else is, and the reason is composition
    /// rather than tidiness. Restarting an agent is `claude stop` followed by
    /// something that resumes it, and the only thing ccmux has that resumes one
    /// is that pane's attach. Stop a session no pane is about to attach and
    /// ccmux has not restarted it — it has STOPPED it, permanently, on a
    /// keypress whose name is "restart". So the set that may be stopped is the
    /// set that will be resumed, by construction.
    ///
    /// That answers the two populations `plan` counts but does not target:
    ///
    ///   * a pane parked after `Ctrl+Z` (`@ccmux_detached`) is NOT re-attached
    ///     — the operator's shell is in it and respawning would destroy their
    ///     work — so nothing would resume its session, and it is out of scope.
    ///     The session keeps running the old binary, which is the same trade
    ///     `R` already makes for a busy one.
    ///   * a mapped pane with no `short_id` has neither an attach to rebuild
    ///     nor an id to stop.
    ///
    /// DEDUPED BY SESSION, not by pane: double-attach is legal (PROBE-FINDINGS
    /// §3) and two panes may hold the same id, but `claude stop` on a session
    /// already stopped a moment ago is a second destructive call for one
    /// restart, and it would be counted twice in the footer. Both panes still
    /// re-attach; the resume just happens twice, which is exactly what
    /// double-attach means.
    pub fn agent_ids(&self) -> Vec<&str> {
        let mut seen: HashSet<&str> = HashSet::new();
        self.targets
            .iter()
            .filter_map(|t| match &t.role {
                Role::Claude { short_id } => Some(short_id.as_str()),
                Role::Sidebar => None,
            })
            .filter(|id| seen.insert(id))
            .collect()
    }
}

/// THE STATE RULE: may `R` stop this session's agent?
///
/// Yes for a row under **Idle** or **Completed**; no for one under **Working**
/// or **Blocked**. That is the operator's rule stated in the operator's own
/// vocabulary, and stating it in `Group` rather than in `State` is deliberate
/// on three counts.
///
/// FIRST, it is what the screen says. The sidebar files every session under one
/// of four headings, and this reads the same verdict from the same function
/// (`Session::group`) that drew the heading. An operator looking at two rows
/// under Working and pressing `R` gets `2 busy` in the footer, and can point at
/// the two rows that account for it. A private rule here — a state whitelist,
/// say — would produce counts that cannot be explained from the screen.
///
/// SECOND, there is no `State::Idle` to whitelist. The CLI's `state` vocabulary
/// is `working` / `done` / `stopped` / `blocked`; "idle" is a row that has no
/// state this build models and no busy status, and it is `group()` that makes
/// that a group. A rule written against `State` alone could not express the
/// operator's "idle" at all.
///
/// THIRD, `group()` carries the belt AND the braces for the one verdict that
/// must never be got wrong. A blocked session is not always `state: "blocked"`:
/// on 2026-08-31 one of two live blocked sessions reported `status: "idle"` and
/// the other `status: "waiting"` with no agreement between them, and
/// `Session::group` answers Blocked from EITHER axis. Reading `state` alone
/// would stop a session that is sitting on a permission prompt, mid-task, with
/// a half-applied edit on disk — which is precisely the interruption `R` exists
/// to avoid. The same belt covers a CLI that invents a new busy-ish word: an
/// unmodelled state with `status: "busy"` groups as Working and is skipped.
///
/// The residual case is an unmodelled state with an idle-looking status: it
/// groups as Idle and is restarted. That is deliberate — it is where every
/// other verb in ccmux files it, `Ctrl+X` included, and `note_drift` is already
/// shouting about the word. Inventing a stricter private rule here would make
/// `R` disagree with the heading the operator is reading.
pub fn agent_restartable(s: &Session) -> bool {
    match s.group() {
        Group::Idle | Group::Completed => true,
        Group::Working | Group::Blocked => false,
    }
}

/// THE OWNERSHIP RULE, and the only place it is decided.
///
/// A pane is restartable if and only if ccmux can prove it created it:
///
///   * it is some window's `@ccmux_tab_sidebar`, and that marker names a live
///     pane OF THAT WINDOW, or
///   * it appears in some window's `@ccmux_tab_map` and is still live AND is
///     still running the attach that put it there.
///
/// That last clause is not a refinement, it is the whole difference between `R`
/// and a data-loss bug. A Claude pane now outlives its attach: `Ctrl+Z` exits
/// `claude attach` and the pane command parks the pane on its resume prompt, or
/// hands it on to the operator's shell from there (`agents::attach_pane_cmd`),
/// so a mapped, live pane is quite normally a shell the operator has been
/// working in for an hour. Respawning that with
/// `claude attach <id>` would destroy whatever was running in it, with no undo
/// — the same harm the ownership rule exists to prevent, arriving through a
/// record that used to be proof and no longer is. `#{@ccmux_detached}` is the
/// pane's own statement that it made that transition, and it is checked here
/// rather than at the call site so the rule stays one function.
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
            // ONE COUNT PER PANE, in every arm. `seen` is what makes that
            // true, and the skipped arms need it as much as the respawned one:
            // a pane named by two tabs' maps would otherwise be counted twice
            // in the footer's "(N skipped)" while being one pane.
            if panes.iter().any(|p| p.id == pane && p.detached) {
                // Still ccmux's pane, still closable with `x` — but there is no
                // attach in it to restart, and something else may be there.
                seen.insert(pane);
                out.detached += 1;
                continue;
            }
            if entry.short_id.is_empty() {
                // Written by a build that had no `short_id` field. There is no
                // command to rebuild, and guessing one is how a pane gets
                // respawned into something it never ran.
                seen.insert(pane);
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

/// WHAT THE AGENT PASS DID, counted by the image that did it.
///
/// Three buckets, and the line between them is what the operator can act on:
/// `restarted` needs nothing, `busy` needs only patience, and `failed` is the
/// one that may need a human.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Agents {
    /// Stopped, so the pane's `claude attach` brings the worker back as a new
    /// process on the `claude` that is installed NOW (PROBE-FINDINGS §2).
    pub restarted: usize,
    /// Deliberately left on the old binary because the row was under Working or
    /// Blocked. THE INTENDED TRADE, not a failure: a busy agent keeps its
    /// version until it finishes, and `R` stays safe to press at any moment.
    pub busy: usize,
    /// Everything that stopped ccmux doing the job: a `claude stop` that
    /// errored, a state that could not be read, a session no longer in the
    /// fleet, or an agent the pass ran out of budget for. All of them mean the
    /// same thing to the operator — that agent is still on the old binary and
    /// ccmux could not change it — so they are one number, and it is reported
    /// rather than swallowed.
    pub failed: usize,
}

impl Agents {
    /// Did the agent pass have anything at all to say?
    fn spoke(&self) -> bool {
        self.restarted + self.busy + self.failed > 0
    }
}

/// Everything one `R` did, as the restarted sidebar counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Sidebars restarted, INCLUDING this process — it restarted first, by the
    /// better mechanism, which is why it is here to do the rest.
    pub sidebars: usize,
    /// Claude panes respawned.
    pub panes: usize,
    /// Pane respawns that failed.
    pub failed: usize,
    /// Panes ccmux owns, found alive, and deliberately left running
    /// (`Plan::skipped`).
    pub skipped: usize,
    /// The agent pass.
    pub agents: Agents,
}

/// The footer line the restarted sidebar shows. `sidebars` counts THIS process
/// too — it is restarting, by the better mechanism — because the operator asked
/// for a restart of the session and wants to read what happened to it, not a
/// census of which mechanism did what.
///
/// SUCCESSES IN THE HEAD, EXCEPTIONS IN THE PARENTHESIS, and every exception
/// gets its own word: `failed` is a pane that did not come back, `not
/// restarted` an agent still on the old binary, `busy` an agent deliberately
/// left alone, `skipped` a pane deliberately left alone. Two of those could
/// have shared the word "failed" and must not: "a pane is gone" and "an agent
/// kept its version" are different problems with different answers.
///
/// The agent clause is omitted entirely when the pass had nothing to say —
/// no session ccmux opened was in scope — so a session with no Claude panes
/// reads exactly as it did before agents were part of `R`.
///
/// Sized for the 34-column default: `restarted 3 sidebars, 4 panes` is 29
/// columns and the clauses only appear when they apply. A line that outgrows
/// the sidebar is not truncated — §6.8's overflow carve wraps it into the
/// detail block — so the counts survive at any width.
pub fn note(r: &Report) -> String {
    let mut s = format!(
        "restarted {} {}, {} {}",
        r.sidebars,
        plural(r.sidebars, "sidebar"),
        r.panes,
        plural(r.panes, "pane")
    );
    if r.agents.spoke() {
        s.push_str(&format!(
            ", {} {}",
            r.agents.restarted,
            plural(r.agents.restarted, "agent")
        ));
    }
    let mut ex: Vec<String> = Vec::new();
    if r.failed > 0 {
        ex.push(format!("{} failed", r.failed));
    }
    if r.agents.failed > 0 {
        ex.push(format!("{} not restarted", r.agents.failed));
    }
    if r.agents.busy > 0 {
        ex.push(format!("{} busy", r.agents.busy));
    }
    if r.skipped > 0 {
        ex.push(format!("{} skipped", r.skipped));
    }
    if !ex.is_empty() {
        s.push_str(&format!(" ({})", ex.join(", ")));
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
/// Resolution only says WHICH file. `probe` says whether that file runs, and
/// the `exec`-before-respawn ordering says what it costs if it does not: with
/// no pane killed until the new image is up, a wrong answer here is a flash,
/// not a lost sidebar.
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
/// A FILTER, not a proof. It is how `exe_path` decides whether a candidate is
/// worth considering at all — and how `which` picks between `$PATH` entries,
/// where a directory or a mode-644 file of the right name would otherwise win.
/// The mode check earns its keep there: a `chmod -x` on the installed binary
/// still passes `is_file()`.
///
/// It says NOTHING about whether `execve` will succeed. A `#!` line naming an
/// interpreter that is not installed, an ELF for the wrong architecture, a
/// truncated download and a build linked against a `.so` that has since gone
/// all pass this and all fail when they are run. `probe` is what settles that
/// question, and the `exec` ordering is what makes the residue survivable.
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

/// WHETHER THE CANDIDATE ACTUALLY RUNS — the question `runnable` cannot answer
/// and the one `R` bets the session on.
///
/// An execute bit is not an `execve`. The whole point of `R` is that the file
/// at that path was replaced by something the operator installed seconds ago,
/// and "it is a file, and it is executable" is true of a truncated download, a
/// wrong-architecture ELF, a script whose interpreter is not installed and a
/// build linked against a `.so` that is no longer there. Every one of them
/// passes `runnable`; every one of them leaves this pane dead if it is `exec`d.
///
/// So the candidate is RUN, in a child, before this process hands itself over:
/// `<exe> --version`, which touches no tmux server, no `claude` daemon and no
/// terminal — stdin is `/dev/null`, stdout a pipe and stderr discarded, so a
/// chatty or broken binary cannot scribble over the alternate screen. It must
///
///   * spawn at all, which is where `execve`'s own failures land (ENOENT for a
///     missing `#!` interpreter, ENOEXEC for the wrong arch, EACCES for a lost
///     execute bit),
///   * exit within `timeout` rather than hang,
///   * exit 0, which is where a missing shared library (127) and a panic during
///     startup (101) land, and
///   * name itself `ccmux`, so a `$PATH` lookup that found somebody else's
///     binary of that name is refused instead of `exec`d.
///
/// A ccmux so old that it has no `--version` fails the third test and the
/// restart is refused. That is the conservative direction, and under the
/// `exec`-first ordering a refusal costs exactly one flash: no pane has been
/// killed, because none is killed until the new image is up.
pub fn probe(exe: &Path) -> Result<(), String> {
    probe_within(exe, PROBE_TIMEOUT)
}

/// `probe` with the deadline as a parameter, so the hang case is a test that
/// finishes rather than a test that waits `PROBE_TIMEOUT`.
fn probe_within(exe: &Path, timeout: Duration) -> Result<(), String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("the new ccmux will not start: {e}"))?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot check the new ccmux: {e}"));
            }
        }
        if Instant::now() >= deadline {
            // Reaped, not left behind: an orphan holding the pipe would
            // outlive the sidebar that spawned it.
            let _ = child.kill();
            let _ = child.wait();
            return Err("the new ccmux does not answer --version".into());
        }
        std::thread::sleep(PROBE_STEP);
    };
    if !status.success() {
        return Err(format!("the new ccmux does not run ({status})"));
    }

    let mut out = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut out);
    }
    match out.split_whitespace().next() {
        Some(env!("CARGO_PKG_NAME")) => Ok(()),
        _ => Err("that binary is not ccmux".into()),
    }
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

/// A restart that `App` has authorised and `main` has still to perform.
///
/// NOTHING HAS BEEN KILLED YET, and nothing will be until the image named here
/// is running: the respawns are the new image's job. There is no note to carry
/// either — the new image counts what it restarts and says so itself, which is
/// also why the count cannot be a promise made before the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The file to exec: resolved from argv[0] by `exe_path` and proven to run
    /// by `probe`, so the exec is not a leap of faith.
    pub exe: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{State, Status};
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
            detached: false,
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
        assert_eq!(
            note(&Report { sidebars: 1, skipped: p.unattachable, ..Report::default() }),
            "restarted 1 sidebar, 0 panes (1 skipped)"
        );
    }

    /// THE PANE THAT IS NO LONGER RUNNING WHAT THE MAP SAYS. `Ctrl+Z` exits
    /// `claude attach` and the pane command parks the pane, and `s` from there
    /// leaves the operator's shell in it, so a mapped, live pane is quite
    /// normally a shell the operator has been working in for an hour. Respawning it with `claude attach <id>`
    /// destroys whatever was in it, with no undo — the exact harm the ownership
    /// rule exists to prevent, reached through a record that used to be proof.
    /// Skipped, counted, reported; never respawned.
    #[test]
    fn a_pane_whose_attach_exited_into_a_shell_is_never_respawned() {
        let mut shell = pane("%2", "@1");
        shell.detached = true;
        let panes = vec![pane("%1", "@1"), shell, pane("%3", "@1")];
        let tabs = vec![tab("@1", Some("%1"), &[("%2", "aaaaaaaa"), ("%3", "bbbbbbbb")])];
        let p = plan(&tabs, &panes, None);
        assert_eq!(ids(&p), vec!["%1", "%3"], "the shell is not a target");
        assert_eq!((p.claude, p.detached, p.unattachable), (1, 1, 0));
        assert_eq!(p.skipped(), 1);
        assert_eq!(
            note(&Report { sidebars: 1, panes: p.claude, skipped: p.skipped(), ..Report::default() }),
            "restarted 1 sidebar, 1 pane (1 skipped)"
        );
    }

    /// REGRESSION. One pane, one count, in the SKIPPED arms too. A pane named
    /// by two tabs' maps reached `out.detached += 1` twice because the arm
    /// `continue`d without recording the pane as seen, so the footer told the
    /// operator it had skipped two panes when it had skipped one. The
    /// `unattachable` arm had the same hole.
    #[test]
    fn a_skipped_pane_named_by_two_maps_is_counted_once() {
        let mut shell = pane("%2", "@1");
        shell.detached = true;
        let panes = vec![pane("%1", "@1"), shell];
        let tabs = vec![
            tab("@1", Some("%1"), &[("%2", "aaaaaaaa")]),
            tab("@2", None, &[("%2", "aaaaaaaa")]),
        ];
        let p = plan(&tabs, &panes, None);
        assert_eq!((p.detached, p.skipped()), (1, 1), "one pane, one skip");
        assert_eq!(
            note(&Report { sidebars: 1, skipped: p.skipped(), ..Report::default() }),
            "restarted 1 sidebar, 0 panes (1 skipped)"
        );

        // Same for the entry with no `short_id` to rebuild from.
        let panes = vec![pane("%1", "@1"), pane("%2", "@1")];
        let tabs = vec![tab("@1", Some("%1"), &[("%2", "")]), tab("@2", None, &[("%2", "")])];
        let p = plan(&tabs, &panes, None);
        assert_eq!((p.unattachable, p.skipped()), (1, 1));
    }

    /// A sidebar is restarted on the marker, not on the map, so the latch — a
    /// PANE option a Claude pane writes about itself — can never reach one. If
    /// it somehow did, `R` would stop being able to restart the sidebar that
    /// runs it.
    #[test]
    fn the_latch_is_read_only_for_mapped_claude_panes() {
        let mut sb = pane("%1", "@1");
        sb.detached = true;
        let panes = vec![sb, pane("%2", "@1")];
        let tabs = vec![tab("@1", Some("%1"), &[("%2", "aaaaaaaa")])];
        let p = plan(&tabs, &panes, None);
        assert_eq!(ids(&p), vec!["%1", "%2"]);
        assert_eq!((p.sidebars, p.claude, p.detached), (1, 1, 0));
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

    // ── the agent scope and the state rule ──────────────────────────────────

    fn sess(short: &str, state: Option<State>, status: Status) -> Session {
        Session {
            id: Some(short.to_string()),
            session_id: format!("{short}-uuid"),
            cwd: "/home/dev/projects".into(),
            kind: crate::model::Kind::Background,
            started_at: 0,
            name: short.to_string(),
            status,
            state,
        }
    }

    /// THE SCOPE. An agent is in scope exactly when this plan is about to
    /// respawn a pane with `claude attach <id>` — so the ids are the Claude
    /// targets, in target order, and nothing else. A sidebar is not a session,
    /// a pane parked after `Ctrl+Z` has no attach to resume its session, and a
    /// mapped pane with no short id has no id to stop.
    #[test]
    fn the_agent_scope_is_exactly_the_panes_that_will_re_attach() {
        let panes = vec![
            pane("%1", "@1"),
            pane("%2", "@1"),
            PaneInfo { detached: true, ..pane("%3", "@1") },
            pane("%4", "@1"),
        ];
        let tabs = vec![tab(
            "@1",
            Some("%1"),
            &[("%2", "aaaaaaaa"), ("%3", "cccccccc"), ("%4", "")],
        )];
        let p = plan(&tabs, &panes, None);

        assert_eq!(p.agent_ids(), vec!["aaaaaaaa"]);
        assert_eq!(p.detached, 1, "the parked pane is still counted as skipped");
        assert_eq!(p.unattachable, 1);
    }

    /// One session in two panes is one stop. Both panes still re-attach —
    /// double-attach is legal (PROBE-FINDINGS §3) — but a second `claude stop`
    /// for the same restart is a second destructive call and a double count.
    #[test]
    fn a_session_named_by_two_panes_yields_one_agent() {
        let panes = vec![pane("%1", "@1"), pane("%2", "@1"), pane("%3", "@1")];
        let tabs = vec![tab(
            "@1",
            Some("%1"),
            &[("%2", "aaaaaaaa"), ("%3", "aaaaaaaa")],
        )];
        let p = plan(&tabs, &panes, None);

        assert_eq!(p.claude, 2, "both panes are respawned");
        assert_eq!(p.agent_ids(), vec!["aaaaaaaa"], "one session, one stop");
    }

    /// THE STATE RULE, across the whole state x status matrix. Working and
    /// Blocked are refused from EITHER axis; everything else is restartable.
    /// The row that matters most is the last pair: a CLI that stops emitting
    /// `state: "blocked"` and leaves `status: "waiting"` behind must still be
    /// refused, because that shape has already shipped once.
    #[test]
    fn only_a_row_under_idle_or_completed_may_be_stopped() {
        let cases: &[(Option<State>, Status, bool)] = &[
            (Some(State::Done), Status::Idle, true),
            (Some(State::Done), Status::Busy, true),
            (Some(State::Stopped), Status::Idle, true),
            (None, Status::Idle, true),
            (Some(State::Unknown("napping".into())), Status::Idle, true),
            (Some(State::Working), Status::Busy, false),
            (Some(State::Working), Status::Idle, false),
            (Some(State::Blocked), Status::Idle, false),
            (Some(State::Blocked), Status::Waiting, false),
            (None, Status::Busy, false),
            (Some(State::Unknown("compacting".into())), Status::Busy, false),
            // The belt, without the braces: a waiting status alone.
            (Some(State::Working), Status::Waiting, false),
            (Some(State::Unknown("halted".into())), Status::Waiting, false),
            (None, Status::Waiting, false),
        ];
        for (state, status, want) in cases {
            let s = sess("aaaaaaaa", state.clone(), status.clone());
            assert_eq!(
                agent_restartable(&s),
                *want,
                "state {state:?} / status {status:?} grouped as {:?}",
                s.group()
            );
            // And it never disagrees with the heading the operator is reading.
            assert_eq!(
                agent_restartable(&s),
                matches!(s.group(), Group::Idle | Group::Completed),
                "the rule drifted from the group it claims to read"
            );
        }
    }

    #[test]
    fn the_note_names_what_happened_and_fits_the_default_width() {
        let r = |sidebars, panes, failed, skipped| Report {
            sidebars,
            panes,
            failed,
            skipped,
            agents: Agents::default(),
        };
        assert_eq!(note(&r(3, 4, 0, 0)), "restarted 3 sidebars, 4 panes");
        assert_eq!(note(&r(1, 1, 0, 0)), "restarted 1 sidebar, 1 pane");
        assert_eq!(note(&r(2, 3, 1, 0)), "restarted 2 sidebars, 3 panes (1 failed)");
        assert!(
            crate::model::display_width(&note(&r(3, 4, 0, 0))) <= 34,
            "the ordinary note must fit the 34-column default"
        );
    }

    /// THE AGENT CLAUSE. It appears only when the agent pass had something to
    /// say, so a session with no Claude panes reads exactly as it did before
    /// `R` learned about agents — and every exception gets its own word, so a
    /// pane that did not come back is never confused with an agent left on the
    /// old binary.
    #[test]
    fn the_note_reports_the_agents_without_rewriting_the_old_line() {
        // Nothing in scope: byte-for-byte the pre-agents line.
        assert_eq!(
            note(&Report { sidebars: 3, panes: 4, ..Report::default() }),
            "restarted 3 sidebars, 4 panes"
        );
        // All of them restarted.
        assert_eq!(
            note(&Report {
                sidebars: 1,
                panes: 2,
                agents: Agents { restarted: 2, ..Agents::default() },
                ..Report::default()
            }),
            "restarted 1 sidebar, 2 panes, 2 agents"
        );
        // The intended trade, named as such.
        assert_eq!(
            note(&Report {
                sidebars: 1,
                panes: 5,
                agents: Agents { restarted: 2, busy: 3, ..Agents::default() },
                ..Report::default()
            }),
            "restarted 1 sidebar, 5 panes, 2 agents (3 busy)"
        );
        // Zero restarted still reports the pass, rather than going quiet on the
        // one outcome the operator most needs to know about.
        assert_eq!(
            note(&Report {
                sidebars: 1,
                panes: 1,
                agents: Agents { busy: 1, ..Agents::default() },
                ..Report::default()
            }),
            "restarted 1 sidebar, 1 pane, 0 agents (1 busy)"
        );
        // Four exceptions, four words, worst first — and the two "failed"
        // populations stay distinguishable.
        assert_eq!(
            note(&Report {
                sidebars: 2,
                panes: 3,
                failed: 1,
                skipped: 4,
                agents: Agents { restarted: 1, busy: 2, failed: 3 },
            }),
            "restarted 2 sidebars, 3 panes, 1 agent (1 failed, 3 not restarted, 2 busy, 4 skipped)"
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

    /// `is_file()` is not enough for `exe_path` to pick a candidate: a
    /// mode-644 file of the right name on `$PATH` would win the lookup and
    /// resolve `R` to something that cannot be `exec`d at all.
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

    // ── `probe`: does the candidate actually RUN? ───────────────────────────

    /// A throwaway executable with `body` in it. Named per-test so two of
    /// these cannot collide, and per-pid so two `cargo test` runs cannot.
    fn script(name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ccmux-probe-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let f = dir.join("ccmux");
        std::fs::write(&f, body).expect("write");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        f
    }

    /// `probe`, retried past Linux's ETXTBSY race.
    ///
    /// `cargo test` runs these in parallel with tests that spawn processes, and
    /// a child that has forked but not yet reached its own `exec` still holds
    /// an inherited write descriptor on the file this test created a moment
    /// ago. `execve` answers ETXTBSY until it closes. It is a property of the
    /// test rig, not of the probe, so it is absorbed here rather than in
    /// production code that would then be papering over a real failure.
    fn settled(f: &Path, timeout: Duration) -> Result<(), String> {
        for _ in 0..100 {
            match probe_within(f, timeout) {
                Err(e) if e.contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => return other,
            }
        }
        probe_within(f, timeout)
    }

    fn scrub(f: &Path) {
        if let Some(d) = f.parent() {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// THE REGRESSION TEST for the pre-check that only checked a mode bit.
    ///
    /// This is the exact shape that took out another tab's sidebar live: a
    /// regular file, execute bit set, `#!` naming an interpreter that does not
    /// exist. `runnable` says yes — it has always said yes — and `execve` says
    /// ENOENT. `probe` is the check that tells them apart.
    #[test]
    fn a_file_that_cannot_be_execd_is_refused_however_executable_it_looks() {
        let f = script("badinterp", "#!/nonexistent/interp\n");
        assert!(runnable(&f), "the mode check passes it — that was the whole bug");
        let err = settled(&f, PROBE_TIMEOUT).expect_err("but it cannot be run");
        assert!(err.contains("will not start"), "{err:?}");
        scrub(&f);
    }

    /// The superset: `execve` SUCCEEDS and the image dies immediately. A
    /// missing `.so` (127), a wrong `--session`, a panic in startup (101) —
    /// all of them reach `exec` intact and then close the pane. Exit status is
    /// how the probe sees them coming.
    #[test]
    fn a_binary_that_starts_and_exits_nonzero_is_refused() {
        let f = script("exits", "#!/bin/sh\nexit 127\n");
        let err = settled(&f, PROBE_TIMEOUT).expect_err("it runs, and that is not enough");
        assert!(err.contains("does not run"), "{err:?}");
        scrub(&f);
    }

    /// A bare argv[0] is resolved off `$PATH`, and `$PATH` can hand back
    /// somebody else's `ccmux`. Exiting 0 is not identity.
    #[test]
    fn a_binary_that_is_not_ccmux_is_refused() {
        let f = script("stranger", "#!/bin/sh\necho 'notccmux 9.9.9'\n");
        let err = settled(&f, PROBE_TIMEOUT).expect_err("wrong binary");
        assert_eq!(err, "that binary is not ccmux");
        scrub(&f);
    }

    /// A candidate that never answers must not take the sidebar down with it.
    /// The deadline is a parameter so this test costs 150 ms, not `PROBE_TIMEOUT`.
    #[test]
    fn a_binary_that_hangs_is_killed_and_refused() {
        let f = script("hangs", "#!/bin/sh\nsleep 30\n");
        let started = Instant::now();
        let err = settled(&f, Duration::from_millis(150)).expect_err("it hung");
        assert!(err.contains("does not answer"), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "the probe is bounded");
        scrub(&f);
    }

    /// And the happy path: something that runs, exits 0 and says it is ccmux.
    #[test]
    fn a_binary_that_runs_and_names_itself_is_accepted() {
        let f = script("good", "#!/bin/sh\necho 'ccmux 9.9.9'\n");
        assert_eq!(settled(&f, PROBE_TIMEOUT), Ok(()));
        scrub(&f);
    }

    /// `exec` keeps the pid, so a token that IS the pid is a handoff only the
    /// process that wrote it can claim. Anything else — a variable inherited
    /// from a tmux environment, most of all — must not start a restart pass,
    /// or every sidebar would respawn every other one on startup, forever.
    #[test]
    fn only_the_process_that_set_the_token_is_a_handoff() {
        assert!(!handoff_matches(None), "no variable, no handoff");
        assert!(!handoff_matches(Some("")), "an empty value is not a pid");
        assert!(
            !handoff_matches(Some("1")),
            "a value that is not my pid belongs to somebody else"
        );
        assert!(
            handoff_matches(Some(&handoff_token())),
            "the pid `exec` preserved is the proof"
        );
        // And the wiring: the live reader agrees with the rule it applies.
        assert_eq!(
            is_handoff(),
            handoff_matches(std::env::var(HANDOFF_ENV).ok().as_deref())
        );
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
