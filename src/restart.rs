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
//! A FOURTH population has no pane at all: a live background worker ccmux has
//! not got open anywhere. `claude --bg --resume <sessionId>` resumes one into
//! the daemon with no client, so the stop finally has a partner that is not an
//! attach, and such a session need no longer keep its old `claude` for ever.
//! It is scoped by LIVENESS — `Session::has_worker` — because a session with
//! no worker is dormant rather than stale, and "restarting" it would start a
//! process nobody asked for. See `headless_targets`.
//!
//! This module owns the four questions that answer badly if guessed: WHICH
//! panes may be respawned (`plan`, pure), WHICH paneless agents may be stopped
//! and resumed (`headless_targets`, pure), WHICH file to exec (`exe_path`),
//! and whether that file actually RUNS (`probe`).

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::model::{Group, Kind, Session, State};
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
    /// EVERY SESSION A LIVE CCMUX PANE NAMES, respawned or not — the line
    /// between `R`'s two agent populations.
    ///
    /// `agent_ids()` is the panes about to re-attach; this is the wider set
    /// that includes the ones deliberately left alone (parked after `Ctrl+Z`,
    /// or carrying no `short_id`). `headless_targets` subtracts it from the
    /// fleet, and the subtraction is what makes the two populations DISJOINT:
    /// one session, one mechanism, never a second `claude stop` for one
    /// restart. A pane ccmux is leaving alone is still a pane, and its session
    /// stays the pane pass's business — being stopped and resumed behind the
    /// operator's back is not what "skipped" means.
    ///
    /// KEYED BY BOTH IDS, short and UUID, because the two callers take
    /// different ones (`claude stop` the short id, `claude --bg --resume` the
    /// UUID) and because a map entry written by a build with no `short_id`
    /// field still carries a `session_id` — without the second key that pane's
    /// session would read as paneless and be stopped out from under whatever
    /// attach is in it.
    pub held: BTreeSet<String>,
}

impl Plan {
    /// What `note` reports as "skipped": everything ccmux owns, found alive,
    /// and deliberately left running.
    pub fn skipped(&self) -> usize {
        self.unattachable + self.detached
    }

    /// THE PANED AGENT SCOPE: the short ids whose AGENT this plan's own panes
    /// will resume, in the order those panes will be respawned.
    ///
    /// It is derived from `targets`, and that is the whole rule: a session is
    /// in THIS population **exactly when this same plan is about to respawn a
    /// pane with `claude attach <id>`**. The reason is composition rather than
    /// tidiness. Restarting an agent is `claude stop` followed by something
    /// that resumes it; here the resume is that pane's attach, so the set that
    /// may be stopped is the set that will be resumed, by construction.
    ///
    /// It is no longer the whole of `R`'s agent scope. A session with a LIVE
    /// WORKER and no ccmux pane at all is restarted by `headless_targets` +
    /// `agents::resume`, which needs no pane; the two populations are made
    /// disjoint by `held`. What is still true, and is the invariant this
    /// function exists for, is that nothing is stopped without something
    /// lined up to resume it.
    ///
    /// That answers the two populations `plan` counts but does not target, and
    /// neither of them moves to the headless pass — both are HELD by a live
    /// pane, and a pane ccmux is deliberately leaving alone is still a pane:
    ///
    ///   * a pane parked after `Ctrl+Z` (`@ccmux_detached`) is NOT re-attached
    ///     — the operator's shell is in it and respawning would destroy their
    ///     work — so nothing resumes its session and it is out of scope. The
    ///     session keeps running the old binary, which is the same trade `R`
    ///     already makes for a busy one, and it is the trade the operator
    ///     chose when they parked the pane.
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

/// THE STATE RULE: what `R` does about this session's agent, and why.
///
/// Three answers, because there are three different things to say. `Restart`
/// is a running worker on a stale binary. `Busy` is a running worker `R`
/// deliberately leaves on its stale binary. `NotRunning` is neither — there is
/// no worker, so there is nothing to bring onto the new binary and nothing to
/// report as an exception either.
///
/// **RESTART: a row under Idle, or a `done` row.** Both mean a worker that is
/// up and has nothing in flight. Stated in `Group` rather than in `State`,
/// deliberately, on three counts.
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
/// `Session::group` answers Blocked from either axis AT EVERY STATE THAT IS
/// STILL RUNNING. Reading `state` alone would stop a session that is sitting on
/// a permission prompt, mid-task, with a half-applied edit on disk — which is
/// precisely the interruption `R` exists to avoid. The same belt covers a CLI
/// that invents a new busy-ish word: an unmodelled state with `status: "busy"`
/// groups as Working and is skipped.
///
/// **NOT RUNNING: `state: "stopped"`, and it is checked BEFORE the group.**
/// This is the one place the rule has to see past `Group`, because
/// `Session::group` maps `Done` AND `Stopped` to the same `Completed` heading
/// — correctly, they are both terminal — while `R` must treat them opposite
/// ways. A `done` session's worker is up and idle; a `stopped` session's worker
/// is GONE, `claude stop` having taken it. So the group cannot answer this one
/// on its own, and reading it alone is what made a single `Ctrl+X` reversible
/// by a keypress named "restart": the operator stopped a session on purpose,
/// pressed `R` for an unrelated upgrade, and got it back running.
///
/// It is a refusal on both counts, which is why it needs no other argument. A
/// stopped session is not on the old binary — it is on no binary — so there is
/// nothing here for `R` to refresh; and stopping it again would be a no-op
/// while resuming it would be an un-stop the operator never asked for. Exactly
/// the same reasoning refuses a `pid: null` row in the headless population
/// (`headless_targets`): both are the one rule "`R` restarts RUNNING workers",
/// read off the two different fields that can say a worker is absent.
///
/// The ordering is load-bearing in one direction only: `Stopped` is tested
/// first so it cannot be swallowed by `Completed`, and everything below it is
/// still `group()`'s answer, belt and braces intact. In particular a
/// `stopped` row carrying `status: "waiting"` is still refused — by this arm
/// now rather than by the terminal-exclusion rule, and to the same end.
///
/// **BUSY: Working or Blocked.** A worker that IS running, deliberately left on
/// its old version until it finishes. The intended trade, and what keeps `R`
/// safe to press at any moment.
///
/// WHERE THE BELT STOPS, exactly, and it is not "nowhere": a `done` row
/// outranks the waiting status, so `done` + `status: "waiting"` groups as
/// Completed and IS restarted. That is the terminal-exclusion rule of
/// `Session::group` (PROBE-FINDINGS §1) and it is deliberate — a finished
/// session is waiting on nobody, and a stale status word on a `done` row is a
/// live shape rather than a hypothetical one: 5 of the 14 `done` rows in the
/// fleet on 2026-08-31 carried a status of their own. Refusing those would
/// report real finished sessions as `busy`, leave them on the old binary, and
/// give the operator a count they cannot account for from the screen. What the
/// belt does NOT cover, therefore, is a CLI that reports a HUMAN-BLOCKED
/// session as `done`; nothing here can, because such a build would file that
/// row under Completed on screen too and every other verb — `x`, `Ctrl+X`,
/// `Tab` — would follow it there. The cost is bounded either way: `claude stop`
/// on a session that has already finished exits 0, changes nothing and leaves
/// `state` as it was (PROBE-FINDINGS §2, verified), and it is resumed straight
/// after. `only_a_running_idle_or_done_row_may_be_stopped` pins every cell.
///
/// The residual case is an unmodelled state with an idle-looking status: it
/// groups as Idle and is restarted. That is deliberate — it is where every
/// other verb in ccmux files it, `Ctrl+X` included, and `note_drift` is already
/// shouting about the word. Inventing a stricter private rule here would make
/// `R` disagree with the heading the operator is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Stop it, and resume it — a running worker with nothing in flight.
    Restart,
    /// Running and busy (Working or Blocked). Left on its old binary and
    /// COUNTED: the footer says `N busy`.
    Busy,
    /// No worker: `state: "stopped"`. Not restarted, and not counted either —
    /// there is no exception to report about a process that is not there. See
    /// `Agents::busy` for why folding this into that word would be a lie.
    NotRunning,
}

/// See `Verdict`.
pub fn agent_verdict(s: &Session) -> Verdict {
    // BEFORE the group: `Stopped` and `Done` share the Completed heading and
    // must not share this answer.
    if s.state == Some(State::Stopped) {
        return Verdict::NotRunning;
    }
    match s.group() {
        Group::Idle | Group::Completed => Verdict::Restart,
        Group::Working | Group::Blocked => Verdict::Busy,
    }
}

/// THE HEADLESS POPULATION: live agents `R` can restart with no pane at all.
///
/// `Plan::agent_ids` can only ever name sessions ccmux has open, because the
/// only resume it had was the pane's own `claude attach`. That left a session
/// ccmux never opened — or opened and closed — on the `claude` it was
/// dispatched with forever, which is the gap this closes:
/// `claude --bg --resume <sessionId>` resumes a session into the daemon with no
/// client, so the stop has a partner without a pane (PROBE-FINDINGS §2).
///
/// THREE CONDITIONS, and the first is the one that matters:
///
/// 1. **A LIVE WORKER** (`Session::has_worker`, i.e. `pid` present). This is
///    the guard the whole feature stands on. On the operator's fleet, 9 of 16
///    sessions carry `pid: null` — they are DORMANT, not stale: not running an
///    old binary, not running at all. "Restarting" one would SPAWN a worker for
///    a session that had none, so a single `R` would resurrect nine finished
///    conversations into live processes nobody asked for — the exact opposite
///    of the verb's job. Presence of `pid`, never its value: see
///    `model::Session::pid` for why that distinction is not a dodge.
/// 2. **NO CCMUX PANE** (`Plan::held`). A session a live ccmux pane names
///    belongs to the pane population and to that one only — including the
///    panes `R` deliberately leaves alone. The two sets are disjoint by
///    construction, so no session can collect two `claude stop`s for one
///    restart, and a pane the footer calls `skipped` really was left alone.
/// 3. **THE COMMANDS' OWN SHAPE** — a background kind, because `--bg --resume`
///    names nothing else and an interactive session is somebody's terminal;
///    and a non-empty short id, because `claude stop` takes that and building
///    `claude stop ''` at all is the bug the fail-closed guard in
///    `agents::stop` exists to catch after the fact.
///
/// WHAT IS DELIBERATELY NOT HERE IS THE STATE RULE. `agent_verdict` is applied
/// by the caller, at the re-poll immediately before each stop and from the
/// same function the paned population uses — a snapshot's state may report,
/// never authorise. Baking it in here would decide the population from a
/// listing already one round trip stale by the time the first stop lands, and
/// it would also make a working paneless agent INVISIBLE: it would drop out of
/// the population instead of being counted `busy`, and the footer would go
/// quiet about live agents `R` deliberately left on the old binary.
///
/// Liveness is filtered here AND re-checked there, and that asymmetry is the
/// point: a dormant row must never enter the population at all, because every
/// exception this pass reports is a claim that some agent is still running
/// something old, and about a dormant session that claim is simply false.
///
/// Deduped by `session_id`, and ORDERED BY IT, so two runs of `R` against one
/// fleet issue the same commands in the same order — the fleet's own row order
/// is the CLI's and is not promised to be stable.
///
/// Pure, so every one of those refusals is testable without a `claude`.
pub fn headless_targets(sessions: &[Session], held: &BTreeSet<String>) -> Vec<Headless> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<Headless> = sessions
        .iter()
        .filter(|s| s.has_worker())
        .filter(|s| s.kind == Kind::Background)
        .filter(|s| !held.contains(s.session_id.as_str()))
        .filter_map(|s| {
            let short = s.id.as_deref().filter(|i| !i.is_empty())?;
            if held.contains(short) || !seen.insert(s.session_id.as_str()) {
                return None;
            }
            Some(Headless {
                short_id: short.to_string(),
                session_id: s.session_id.clone(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    out
}

/// One session in the headless population, carrying BOTH ids because the two
/// halves of the restart take different ones: `claude stop` the 8-hex short id,
/// `claude --bg --resume` the UUID. Keeping them together is what stops a call
/// site pairing the wrong one with the wrong verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headless {
    pub short_id: String,
    pub session_id: String,
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
            if !live.contains(&pane) {
                continue;
            }
            // HELD, whichever arm the pane lands in below — and BEFORE the
            // `seen` check, so a pane this loop is about to skip for any
            // reason at all still shields its session from the headless pass.
            // A dead pane holds nothing, which is why this sits under the
            // liveness check and above everything else.
            if !entry.short_id.is_empty() {
                out.held.insert(entry.short_id.clone());
            }
            if !entry.session_id.is_empty() {
                out.held.insert(entry.session_id.clone());
            }
            if seen.contains(&pane) {
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
/// Four buckets, and the line between them is what the operator can act on:
/// `restarted` needs nothing, `busy` needs only patience, `failed` may need a
/// human, and `stranded` DEFINITELY does — it is the one state `R` can leave
/// behind that does not fix itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Agents {
    /// Stopped AND RESUMED — so the worker is running again, as a new process
    /// on the `claude` that is installed NOW (PROBE-FINDINGS §2).
    ///
    /// Two resumes reach this one count, and the word means the same thing for
    /// both: a paned session's pane came back up on `claude attach <id>`, or a
    /// headless session's `claude --bg --resume <sessionId>` returned Ok.
    ///
    /// Never counted at the stop. A stop is only half of a restart; the resume
    /// is the other half, and it is the half that can fail independently. For
    /// a paned session that verdict cannot even be reached until PHASE 2 has
    /// run; for a headless one it arrives one call later. See `stranded`.
    pub restarted: usize,
    /// Deliberately left on the old binary because the row was under Working or
    /// Blocked. THE INTENDED TRADE, not a failure: a busy agent keeps its
    /// version until it finishes, and `R` stays safe to press at any moment.
    ///
    /// A RUNNING worker, always. A session that is `stopped`, and a paneless
    /// one with no `pid`, are counted here — and anywhere — NOT AT ALL: they
    /// are not on the old binary, they are on no binary, so there is no
    /// exception to report about them. Folding either into this word would
    /// tell the operator an agent is working when it is halted. See
    /// `Verdict::NotRunning`.
    pub busy: usize,
    /// Everything that stopped ccmux doing the job: a `claude stop` that
    /// errored, a state that could not be read, a paned session no longer in
    /// the fleet, or an agent the pass ran out of budget for. All of them mean
    /// the same thing to the operator — that agent is still on the old binary
    /// and ccmux could not change it — so they are one number, and it is
    /// reported rather than swallowed.
    ///
    /// EVERY ONE OF THESE LEFT THE AGENT RUNNING. Nothing was stopped, so
    /// nothing needs resuming; the trade is identical to `busy`, one cause
    /// along. That is also why a HEADLESS target that has left the fleet, or
    /// whose worker exited, between the enumeration and its stop is not counted
    /// here: there is no agent left running for the operator to be told about.
    pub failed: usize,
    /// STOPPED, AND NOT RESUMED: the `claude stop` succeeded and then nothing
    /// brought the session back.
    ///
    /// Two ways in, one meaning. A paned session's pane never came back up —
    /// its respawn failed, or the pane was gone by the time PHASE 2 reached
    /// it. A headless session's `claude --bg --resume <sessionId>` errored,
    /// which is the same halt one call earlier and with no pane that might
    /// still have covered it.
    ///
    /// Its own word because it is its own outcome, and the only one `R` can
    /// produce that an operator must act on: the agent is halted, ccmux has
    /// nothing left that will resume it, and the conversation sits there until
    /// somebody presses `Enter` on the row or runs `claude attach <id>`. Rolled
    /// into `restarted` it would be a plain lie — the footer would report an
    /// upgrade for a worker that is not running — and rolled into `failed` it
    /// would read as "still on the old binary", which is the one thing it is
    /// not.
    ///
    /// From a pane it needs a respawn to fail to happen at all, which is why
    /// it is rare rather than impossible: `tmux kill-pane` on a Claude pane in
    /// the seconds between PHASE 1 and PHASE 2 reproduces it exactly.
    pub stranded: usize,
}

impl Agents {
    /// Did the agent pass have anything at all to say?
    fn spoke(&self) -> bool {
        self.restarted + self.busy + self.failed + self.stranded > 0
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
/// restarted` an agent still on the old binary, `left stopped` an agent that
/// was stopped and that nothing came back to resume, `busy` an agent
/// deliberately left alone, `skipped` a pane deliberately left alone. They
/// could have shared the word "failed" and must not: "a pane is gone", "an
/// agent kept its version" and "an agent is halted with nothing to resume it"
/// are three different problems with three different answers, and only the
/// last one needs the operator to do something.
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
    if r.agents.stranded > 0 {
        ex.push(format!("{} left stopped", r.agents.stranded));
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
            shell: false,
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

    /// The latch's second value. `s` re-writes it as `shell`, which the parser
    /// reads as `detached` too (`tmux::parse_pane_line`), so the operator's
    /// shell is skipped here on the same flag as the parked prompt: there is
    /// no attach in it to restart, and respawning it would destroy whatever
    /// they are running. §8.2's delete is the only reader that tells the two
    /// apart, and it is not this one.
    #[test]
    fn a_pane_marked_as_the_operators_shell_is_skipped_like_a_parked_one() {
        let mut shell = pane("%2", "@1");
        shell.detached = true;
        shell.shell = true;
        let panes = vec![pane("%1", "@1"), shell];
        let tabs = vec![tab("@1", Some("%1"), &[("%2", "aaaaaaaa")])];
        let p = plan(&tabs, &panes, None);
        assert_eq!(ids(&p), vec!["%1"], "the shell is not a target");
        assert_eq!((p.claude, p.detached), (0, 1));
        assert!(p.agent_ids().is_empty(), "and nothing would stop its agent");
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

    /// A row with NO WORKER. Dormant is the safe default and the one that
    /// leaves the headless population empty, so a test that wants a live agent
    /// has to say so — see `live`.
    fn sess(short: &str, state: Option<State>, status: Status) -> Session {
        Session {
            id: Some(short.to_string()),
            pid: None,
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

    /// THE STATE RULE, across the whole state x status matrix — every state
    /// this build models, and the unmodelled one, against every status. Three
    /// answers, so the matrix names the answer rather than a bool.
    ///
    /// Working and Blocked are refused from either axis at every state that is
    /// STILL RUNNING. The row that matters most is the `waiting`-without-
    /// `blocked` group at the end: a CLI that stops emitting `state:
    /// "blocked"` and leaves `status: "waiting"` behind must still be refused,
    /// because that shape has already shipped once.
    ///
    /// `stopped` is refused at EVERY status, including the two where the group
    /// alone would have said yes. It is the one verdict the heading cannot
    /// supply — `Done` and `Stopped` share the Completed heading and want
    /// opposite answers — and it is the whole of the change that stopped `R`
    /// un-stopping a session the operator stopped on purpose.
    ///
    /// The row the rule deliberately does NOT reach is here too, spelled out
    /// rather than left to be discovered: `done` outranks the waiting status,
    /// so `done` + `waiting` is Completed and IS restarted. That is
    /// `Session::group`'s terminal-exclusion rule (PROBE-FINDINGS §1), it is
    /// what keeps the 5-of-14 live `done` rows that carry a status of their own
    /// out of the `busy` count, and pinning it here is what stops the prose
    /// above quietly overstating the belt again.
    #[test]
    fn only_a_running_idle_or_done_row_may_be_stopped() {
        let cases: &[(Option<State>, Status, Verdict)] = &[
            (Some(State::Done), Status::Idle, Verdict::Restart),
            (Some(State::Done), Status::Busy, Verdict::Restart),
            // TERMINAL OUTRANKS WAITING. Deliberate, and the limit of the belt.
            (Some(State::Done), Status::Waiting, Verdict::Restart),
            (None, Status::Idle, Verdict::Restart),
            (Some(State::Unknown("napping".into())), Status::Idle, Verdict::Restart),
            // STOPPED STAYS STOPPED, whatever status rides along with it — and
            // the last two are the cells `Group` alone gets wrong.
            (Some(State::Stopped), Status::Idle, Verdict::NotRunning),
            (Some(State::Stopped), Status::Waiting, Verdict::NotRunning),
            (Some(State::Stopped), Status::Busy, Verdict::NotRunning),
            (Some(State::Working), Status::Busy, Verdict::Busy),
            (Some(State::Working), Status::Idle, Verdict::Busy),
            (Some(State::Blocked), Status::Idle, Verdict::Busy),
            (Some(State::Blocked), Status::Waiting, Verdict::Busy),
            (None, Status::Busy, Verdict::Busy),
            (Some(State::Unknown("compacting".into())), Status::Busy, Verdict::Busy),
            // The belt, without the braces: a waiting status alone.
            (Some(State::Working), Status::Waiting, Verdict::Busy),
            (Some(State::Unknown("halted".into())), Status::Waiting, Verdict::Busy),
            (None, Status::Waiting, Verdict::Busy),
        ];
        for (state, status, want) in cases {
            let s = sess("aaaaaaaa", state.clone(), status.clone());
            assert_eq!(
                agent_verdict(&s),
                *want,
                "state {state:?} / status {status:?} grouped as {:?}",
                s.group()
            );
            // `stopped` aside, the rule never disagrees with the heading the
            // operator is reading. `stopped` is the documented exception: the
            // row still sits under **Completed** on screen, and the footer says
            // nothing about it, so there is no count to explain from it.
            if *state != Some(State::Stopped) {
                assert_eq!(
                    agent_verdict(&s) == Verdict::Restart,
                    matches!(s.group(), Group::Idle | Group::Completed),
                    "the rule drifted from the group it claims to read"
                );
            } else {
                assert_eq!(
                    s.group(),
                    Group::Completed,
                    "a stopped row still belongs under Completed on screen"
                );
            }
        }
    }

    // ── the headless population (`R`'s live paneless agents) ───────────────

    /// The same row WITH a worker. The value is never read, only its presence,
    /// so the number is deliberately not a plausible pid.
    fn live(s: Session) -> Session {
        Session { pid: Some(4242), ..s }
    }

    fn held(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    fn shorts(t: &[Headless]) -> Vec<&str> {
        t.iter().map(|h| h.short_id.as_str()).collect()
    }

    /// THE DORMANT-RESURRECTION GUARD, and the single most important test in
    /// this file.
    ///
    /// Of the 16 sessions in the operator's fleet, 9 carry `pid: null`. They
    /// are FINISHED — not running an old `claude`, not running anything — and
    /// `claude stop` + `claude --bg --resume` on one does not restart it, it
    /// STARTS it. A rule that scoped on state alone would have turned one `R`
    /// into nine background workers the operator never asked for, and it would
    /// have looked exactly like the feature working.
    ///
    /// Every state a live row WOULD be restarted at is checked, so the guard
    /// cannot be half-applied.
    #[test]
    fn a_dormant_session_is_never_a_headless_target() {
        for state in [
            Some(State::Done),
            None,
            Some(State::Unknown("napping".into())),
        ] {
            let s = sess("aaaaaaaa", state.clone(), Status::Idle);
            assert_eq!(
                agent_verdict(&s),
                Verdict::Restart,
                "the state is not what is under test here ({state:?})"
            );
            assert!(!s.has_worker(), "the fixture's default row is dormant");
            assert!(
                headless_targets(&[s], &held(&[])).is_empty(),
                "a session with no worker was about to be started ({state:?})"
            );
        }
    }

    /// The population the feature exists for: a live worker, no ccmux pane, a
    /// restartable state. Both ids come back, because the stop and the resume
    /// take different ones.
    #[test]
    fn a_live_paneless_session_is_a_headless_target() {
        let t = headless_targets(&[live(sess("aaaaaaaa", Some(State::Done), Status::Idle))], &held(&[]));
        assert_eq!(
            t,
            vec![Headless {
                short_id: "aaaaaaaa".into(),
                session_id: "aaaaaaaa-uuid".into(),
            }],
            "the short id is for `claude stop`, the uuid for `--bg --resume`"
        );
    }

    /// THE DISJOINTNESS RULE. A session a live ccmux pane names belongs to the
    /// pane population and to that one only — otherwise one restart would
    /// spend two `claude stop`s and the footer would count it twice.
    ///
    /// Both keys are checked, because a map entry written by a build with no
    /// `short_id` field carries only the uuid: without the second key that
    /// pane's session would read as paneless and be stopped out from under
    /// whatever attach is sitting in it.
    #[test]
    fn a_session_a_ccmux_pane_holds_is_never_a_headless_target() {
        let fleet = [live(sess("aaaaaaaa", Some(State::Done), Status::Idle))];
        assert!(
            headless_targets(&fleet, &held(&["aaaaaaaa"])).is_empty(),
            "held by short id"
        );
        assert!(
            headless_targets(&fleet, &held(&["aaaaaaaa-uuid"])).is_empty(),
            "held by uuid — the key a `short_id`-less map entry has"
        );
        assert_eq!(
            shorts(&headless_targets(&fleet, &held(&["bbbbbbbb", "sid-b"]))),
            vec!["aaaaaaaa"],
            "some OTHER pane's session must not shield this one"
        );
    }

    /// WHERE THE STATE RULE IS APPLIED, and where it deliberately is not.
    ///
    /// A stopped, working or blocked session with a live worker IS in the
    /// population — the population is liveness, ownership and command shape —
    /// and `agent_verdict` is what refuses it, at the re-poll immediately
    /// before the stop. Two reasons the split has to fall here. A state read at
    /// enumeration is already stale by the time the first stop lands, and only
    /// the reading that authorises an act may decide it. And a working
    /// paneless agent that dropped out of the population would be invisible
    /// rather than `busy`: the footer would go quiet about a live agent `R`
    /// left on the old binary.
    ///
    /// Blocked is checked from EITHER axis here too — the shape that has
    /// shipped a bug twice, now in a population with no pane to soften it.
    #[test]
    fn a_stopped_or_busy_paneless_session_is_a_candidate_that_the_verdict_refuses() {
        for (s, want) in [
            (
                live(sess("aaaaaaaa", Some(State::Stopped), Status::Idle)),
                Verdict::NotRunning,
            ),
            (
                live(sess("aaaaaaaa", Some(State::Working), Status::Busy)),
                Verdict::Busy,
            ),
            (
                live(sess("aaaaaaaa", Some(State::Blocked), Status::Idle)),
                Verdict::Busy,
            ),
            (
                live(sess("aaaaaaaa", Some(State::Working), Status::Waiting)),
                Verdict::Busy,
            ),
            (
                live(sess("aaaaaaaa", Some(State::Unknown("halted".into())), Status::Waiting)),
                Verdict::Busy,
            ),
        ] {
            let label = format!("{:?}/{:?}", s.state, s.status);
            assert_eq!(
                shorts(&headless_targets(std::slice::from_ref(&s), &held(&[]))),
                vec!["aaaaaaaa"],
                "the population is liveness and shape, not state ({label})"
            );
            assert_eq!(agent_verdict(&s), want, "and the verdict is what refuses it ({label})");
        }
    }

    /// Two shape checks, and neither is pedantry. An interactive session has
    /// no short id, so there is nothing to pass `claude stop`; and
    /// `--bg --resume` names a background session, which an interactive one is
    /// not — it is somebody's terminal.
    #[test]
    fn an_interactive_session_is_never_a_headless_target() {
        let mut s = live(sess("aaaaaaaa", None, Status::Idle));
        s.id = None;
        s.kind = Kind::Interactive;
        assert!(headless_targets(&[s.clone()], &held(&[])).is_empty());

        // And a background row that somehow lost its short id, which would
        // otherwise build `claude stop ''`.
        let mut blank = live(sess("aaaaaaaa", Some(State::Done), Status::Idle));
        blank.id = Some(String::new());
        assert!(headless_targets(&[blank], &held(&[])).is_empty());
    }

    /// Deterministic order and one entry per session, so two runs of `R`
    /// against one fleet issue the same commands in the same order — the row
    /// order the CLI hands back is not promised to be stable.
    #[test]
    fn the_headless_targets_are_ordered_and_deduped_by_session() {
        let a = live(sess("aaaaaaaa", Some(State::Done), Status::Idle));
        let b = live(sess("bbbbbbbb", None, Status::Idle));
        let fleet = [b.clone(), a.clone(), a.clone()];
        assert_eq!(shorts(&headless_targets(&fleet, &held(&[]))), vec!["aaaaaaaa", "bbbbbbbb"]);
    }

    /// `Plan::held` is what makes the two populations disjoint, so it has to
    /// cover the arms `plan` does NOT target as well as the one it does: a
    /// pane parked after `Ctrl+Z` and a pane whose entry has no short id are
    /// both still panes, and their sessions stay the pane pass's business.
    #[test]
    fn the_plan_holds_every_session_a_live_pane_names() {
        let mut parked = pane("%3", "@1");
        parked.detached = true;
        let panes = vec![pane("%1", "@1"), pane("%2", "@1"), parked, pane("%4", "@1")];
        let mut tabs = vec![tab(
            "@1",
            Some("%1"),
            &[("%2", "aaaaaaaa"), ("%3", "bbbbbbbb")],
        )];
        // `%4`: an entry from a build that had no `short_id` field. Only the
        // uuid can shield it.
        tabs[0].map.insert(
            &PaneId::parse("%4").expect("pane id"),
            PaneEntry {
                session_id: "sid-cccccccc".into(),
                short_id: String::new(),
                name: "n".into(),
                opened_at: 0,
            },
        );
        // `%9` is in the map but is NOT live, and a dead pane holds nothing.
        tabs[0].map.insert(
            &PaneId::parse("%9").expect("pane id"),
            PaneEntry {
                session_id: "sid-dddddddd".into(),
                short_id: "dddddddd".into(),
                name: "n".into(),
                opened_at: 0,
            },
        );

        let p = plan(&tabs, &panes, None);

        assert_eq!((p.claude, p.detached, p.unattachable), (1, 1, 1));
        assert!(p.held.contains("aaaaaaaa"), "the respawned pane's session");
        assert!(p.held.contains("bbbbbbbb"), "the PARKED pane's session");
        assert!(p.held.contains("sid-cccccccc"), "the short-id-less pane's session");
        assert!(
            !p.held.contains("dddddddd") && !p.held.contains("sid-dddddddd"),
            "a dead pane holds nothing: {:?}",
            p.held
        );

        // Which is the property that matters: only the dead pane's session is
        // left for the headless pass.
        let fleet: Vec<Session> = ["aaaaaaaa", "bbbbbbbb", "cccccccc", "dddddddd"]
            .iter()
            .map(|s| live(sess(s, Some(State::Done), Status::Idle)))
            .collect();
        // `map` writes `sid-<short>` as the uuid, and `sess` writes
        // `<short>-uuid`; line them up so the uuid key is really being tested.
        let fleet: Vec<Session> = fleet
            .into_iter()
            .map(|s| Session {
                session_id: format!("sid-{}", s.id.clone().unwrap_or_default()),
                ..s
            })
            .collect();
        assert_eq!(shorts(&headless_targets(&fleet, &p.held)), vec!["dddddddd"]);
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
        // THE ONE THAT ASKS FOR SOMETHING: an agent stopped whose pane never
        // came back. It is neither a restart nor a stale binary, and saying so
        // is the difference between "press Enter on that row" and "nothing to
        // do here".
        assert_eq!(
            note(&Report {
                sidebars: 1,
                panes: 2,
                failed: 1,
                agents: Agents { restarted: 2, stranded: 1, ..Agents::default() },
                ..Report::default()
            }),
            "restarted 1 sidebar, 2 panes, 2 agents (1 failed, 1 left stopped)"
        );
        // Five exceptions, five words, worst first — and the three "failed"
        // populations stay distinguishable.
        assert_eq!(
            note(&Report {
                sidebars: 2,
                panes: 3,
                failed: 1,
                skipped: 4,
                agents: Agents { restarted: 1, busy: 2, failed: 3, stranded: 5 },
            }),
            concat!(
                "restarted 2 sidebars, 3 panes, 1 agent ",
                "(1 failed, 3 not restarted, 5 left stopped, 2 busy, 4 skipped)"
            )
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
