# ccmux — Implementation Spec v1

Authoritative. Derived from and consistent with `PROBE-FINDINGS.md`; every tmux
mechanism below was additionally re-verified on this machine (tmux 3.4) during
spec authoring. Where this document and an engineer's intuition disagree, this
document wins. Where this document and `PROBE-FINDINGS.md` appear to disagree,
`PROBE-FINDINGS.md` wins — report it instead of improvising.

**Scope of v1:** one tmux session (`ccmux`), one window (`ccmux:cc`), a ratatui
sidebar pinned on the left, Claude Code TUIs in the panes to its right.

---

## 0. Ownership, parallelism, and the day-0 stub commit

The file split is fixed. Nobody edits a file they do not own.

| File | Owner (lane) | Depends on (in-crate) |
|---|---|---|
| `Cargo.toml`, `src/model.rs` | **Scaffold** | — |
| `src/tmux.rs` | **Tmux** | — |
| `src/agents.rs` | **Agents** | `model`, `tmux` (`sh_quote` only) |
| `src/ui.rs` | **UI** | `app`, `model`, `tmux` (read-only) |
| `src/app.rs`, `src/main.rs` | **Integrator** | `model`, `tmux`, `agents`, `ui` |

In-crate dependency DAG (no cycles; `ui` reads `app`, `app` never imports `ui`):

```
model ─────┐
           ├──> agents ──┐
tmux ──────┤             ├──> app ──> main
   │       └─────────────┘      │
   └────────────────────────────┴──> ui ──> main
```

`tmux` has no in-crate dependencies, so `agents -> tmux` introduces no cycle;
`agents` uses exactly one item from it, `sh_quote`.

**Day-0 stub commit (Scaffold does this first, before anyone else starts).**
Scaffold commits `Cargo.toml` plus all six `src/*.rs` files containing *exactly*
the public signatures in §3, with `unimplemented!()` bodies and
`#![allow(unused)]` at crate root. From that commit on, `cargo check` succeeds
for everyone, and the four lanes proceed without talking. Changing any signature
in §3 requires a spec amendment, not a local edit.

`Cargo.toml`:

```toml
[package]
name = "ccmux"
version = "0.1.0"
edition = "2024"

[dependencies]
ratatui = "0.29"
crossterm = "0.28"
clap = { version = "4", features = ["derive"] }
chrono = "0.4"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
```

No `dirs` dependency: home directory comes from `std::env::var("HOME")`.
No `unicode-width`: see §6.6 for the truncation rule.

Build check: `cd /home/dev/projects/ccmux && cargo build 2>&1 | tail -40`.

---

## 1. The two binaries in one

One binary, one optional subcommand.

```
ccmux                  # launcher: create-or-attach the `ccmux` tmux session
ccmux sidebar          # the ratatui explorer; runs INSIDE the left pane
```

### 1.1 CLI surface (`src/main.rs`, clap derive)

```rust
#[derive(clap::Parser)]
#[command(name = "ccmux", about = "tmux-backed frontend for Claude Code sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,

    /// tmux session name to create/attach
    #[arg(long, default_value = "ccmux", global = true, value_parser = validate_session_name)]
    pub session: String,

    /// Pinned sidebar width in columns
    #[arg(long, default_value_t = 34, global = true)]
    pub width: u16,

    /// Use the light palette (default: dark)
    #[arg(long, global = true)]
    pub light: bool,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Run the session-explorer sidebar (normally launched by `ccmux` itself)
    Sidebar {
        /// Poll interval for `claude agents --json`, milliseconds
        #[arg(long, default_value_t = 2500)]
        interval: u64,
    },
}

/// Reject anything tmux cannot address: `:` and `.` are tmux target separators.
pub fn validate_session_name(s: &str) -> Result<String, String>;
```

`validate_session_name` accepts `^[A-Za-z0-9_-]{1,64}$` and rejects everything
else with `"session name must match [A-Za-z0-9_-]{1,64}"`.

`--width` is clamped to `20..=120` before use. Values below 20 are accepted at
the CLI and clamped up, so a typo cannot produce an unusable sidebar.

### 1.2 Launcher algorithm (`main::run_launcher`)

Exact sequence. Every step is an argv vector to `tmux` — never a shell string.

```
 1. if tmux binary missing (`Command::new("tmux").arg("-V")` fails to spawn):
        eprintln "ccmux: tmux not found on PATH"; exit 1

 2. if tmux::inside_tmux() and tmux::current_session_name() == Some(cli.session):
        println "ccmux: already inside session '<name>'"; exit 0     # no-op, idempotent

 3. exists = tmux::has_session(&cli.session)          # `tmux has-session -t <name>` exit 0/1
                                                      # exit!=0 also covers "no server running"

 4. if !exists:
        a. pane = tmux new-session -d -s <name> -n cc -P -F '#{pane_id}' -- <SIDEBAR_CMD>
        b. tmux set-option -t <name> @ccmux_sidebar <pane>
        c. tmux set-option -t <name> @ccmux_width   <width>
        d. tmux set-option -t <name> @ccmux_map     {"v":1,"panes":{}}
        e. tmux set-option -t <name> status off
        f. tmux set-option -t <name> mouse on
        g. tmux resize-pane -t <pane> -x <width>

 5. if exists:
        a. sidebar = tmux::get_user_option(<name>, "@ccmux_sidebar")
        b. if sidebar is None, or not a valid %N, or not present in
           tmux::list_panes_in_session(<name>):
               # sidebar was quit with `q` or crashed — heal it
               leftmost = pane with the smallest `#{pane_left}` in <name>:cc
               pane = tmux split-window -h -b -t <leftmost> -P -F '#{pane_id}' -d -- <SIDEBAR_CMD>
               tmux set-option -t <name> @ccmux_sidebar <pane>
               tmux resize-pane -t <pane> -x <width>
        c. otherwise: tmux resize-pane -t <sidebar> -x <width>     # re-pin, harmless

 6. attach:
        if tmux::inside_tmux():  tmux switch-client -t <name>
        else:                    tmux attach-session -t <name>     # replaces our process view
```

`<SIDEBAR_CMD>` is a **shell-command string** (tmux runs it via `/bin/sh -c`),
built with `tmux::sh_quote` (§7):

```
<abs path to current exe> sidebar --session <name> --width <W> [--light]
```

Use `std::env::current_exe()` for the path so a sidebar launched from a
non-`PATH` build still works. Each component is `sh_quote`d and joined with
single spaces.

**Verified properties this relies on** (all re-tested during spec authoring):

- `new-session -P -F '#{pane_id}'` prints the new pane id directly; no
  `list-panes | head -1` dance is needed.
- `-n cc` names the window `cc`, addressable as `<name>:cc`.
- `split-window -h -b -t <pane>` inserts the new pane to the **left** of the
  target and it becomes `pane_index 1`.
- `resize-pane -t <pane> -x <W>` pins the width; on a lone pane it is a
  **no-op returning exit 0**, so it is always safe to call unconditionally.
- `has-session` returns exit 0 when the session exists, exit 1 otherwise.

### 1.3 Sidebar width pinning and re-pinning

The sidebar width is not maintained by a tmux layout; it is re-asserted.

- After **every** `split-window` issued by ccmux: `resize-pane -t <sidebar> -x <W>`.
- After **every** `kill-pane` issued by ccmux: same.
- On **every poll tick** (§4.2), unconditionally: same.

The unconditional tick re-pin is the important one: it makes the width
self-healing against the user manually splitting or resizing panes with tmux
keys, and it is a proven no-op when the sidebar is the only pane. Do not guard
it behind a "did the layout change" check.

Verified sequence (sidebar `%24`, width 34, window 274x76):

```
split-window -h -t %24  → %25 ; resize-pane -t %24 -x 34
   %24 0,0 34x76   %25 35,0 239x76
split-window -h -t %25  → %26 ; resize-pane -t %24 -x 34
   %24 0,0 34x76   %25 35,0 119x76   %26 155,0 119x76
split-window -v -t %26  → %27 ; resize-pane -t %24 -x 34
   %24 0,0 34x76   %25 35,0 119x76   %26 155,0 119x38   %27 155,39 119x37
kill-pane -t %25        ; resize-pane -t %24 -x 34
   %24 0,0 34x76   %26 35,0 239x38   %27 35,39 239x37
```

The sidebar stays leftmost, full height, exactly 34 columns throughout.

---

## 2. Blast-radius rules (non-negotiable)

These exist because tmux silently defaults an omitted or empty `-t` to the
*caller's current pane*. During spec authoring an empty `-t` variable caused
three stray panes to be created in an unrelated live session. That class of bug
must be impossible by construction.

**R1 — every mutating tmux call takes a mandatory, validated target.**
No function in `tmux.rs` that mutates state may accept an `Option<PaneId>` or a
`&str` target. Targets are `PaneId` (a newtype that can only be constructed by
`PaneId::parse`, which enforces `^%\d+$`) or a `&SessionName`-shaped `&str`
already validated by `validate_session_name`. An empty or malformed target is a
`TmuxError::BadTarget`, never a call.

**R2 — mutating calls are scoped to the ccmux session.**
Before `split-window`, `kill-pane`, `resize-pane`, `set-option`, `select-pane`,
or `respawn-pane`, the target pane must be confirmed to belong to `cli.session`.
`tmux::assert_in_session(&PaneId, &str)` performs this check and every mutating
helper calls it. A pane in `agents`, `dev`, or any other session is never
mutated.

**R3 — pane enumeration for reconciliation uses `-s`, never `-a`.**
`tmux list-panes -t <session> -s -F ...` lists only that session's panes.
`list-panes -a` lists the whole server, including the user's own work. `-a` is
permitted in exactly one place: the **read-only** discovery walk that locates an
interactive Claude session living outside ccmux (§5.4). It never feeds
reconciliation and never feeds a mutating call.

**R4 — ccmux never issues `claude stop`/`kill` without passing §8.2's
confirmation gate**, and never issues them at all for a session it did not
resolve from the current poll.

---

## 3. Module boundaries and exact signatures

Signatures below are authoritative. Bodies are the owner's business.

### 3.1 `src/model.rs` — owner: Scaffold

Pure data + parsing + grouping + formatting. No IO, no process spawning, no
tmux, no ratatui. Fully unit-testable.

**Exports:**

```rust
use serde::Deserialize;

// ── Enums ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Background,
    Interactive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Busy,
    Idle,
    /// Forward-compatibility: CLI versions churn; unknown strings are preserved.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Working,
    Done,
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Working = 0,
    Idle = 1,
    Completed = 2,
}

impl Group {
    pub fn title(self) -> &'static str;      // "Working" | "Idle" | "Completed"
    pub fn all() -> [Group; 3];              // [Working, Idle, Completed]
    pub fn next(self) -> Group;              // Working->Idle->Completed->Working
    pub fn prev(self) -> Group;
}

// ── Session ─────────────────────────────────────────────────────────────────

/// One row from `claude agents --json`. Field names mirror the CLI's JSON
/// exactly; `pid` is UNSTABLE across attach/detach and is used ONLY for the
/// /proc ancestry walk, never as an identity key. Identity is `session_id`.
#[derive(Debug, Clone)]
pub struct Session {
    pub pid: i32,
    /// 8-hex short id. `None` for `kind == Interactive`.
    pub id: Option<String>,
    /// UUID. Stable. THE primary key everywhere in ccmux.
    pub session_id: String,
    pub cwd: String,
    pub kind: Kind,
    /// epoch milliseconds
    pub started_at: i64,
    pub name: String,
    pub status: Status,
    /// `None` for `kind == Interactive`.
    pub state: Option<State>,
}

impl Session {
    /// Identity key. Always `&self.session_id`.
    pub fn key(&self) -> &str;

    /// Grouping rule, mirroring the stock fleet view:
    ///   Some(Working)                 -> Group::Working
    ///   Some(Done)                    -> Group::Completed
    ///   None | Some(Unknown(_))       -> Busy => Working, otherwise => Idle
    /// Interactive sessions have no `state`, so they fall through to the
    /// status rule and never land in Completed.
    pub fn group(&self) -> Group;

    /// True when `id.is_some()`. Gates `attach` / `stop` / `logs`.
    pub fn is_attachable(&self) -> bool;

    /// Lowercased haystack for `/` filtering: name + " " + cwd + " " + short id.
    pub fn filter_haystack(&self) -> String;
}

// ── Parsing ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ParseError {
    /// serde_json failed; carries a truncated excerpt of the offending input.
    Json { msg: String, excerpt: String },
    /// Top-level JSON was not an array.
    NotAnArray,
}

impl std::fmt::Display for ParseError {}
impl std::error::Error for ParseError {}

/// Parse the whole `claude agents --json` payload.
/// Rows missing `sessionId`, `name`, or `cwd` are SKIPPED, not fatal — a single
/// malformed row must never blank the sidebar. Unknown extra keys are ignored.
/// Returns rows in the order the CLI emitted them; ordering is imposed later.
pub fn parse_sessions(json: &str) -> Result<Vec<Session>, ParseError>;

// ── Rows: the display model shared by app.rs and ui.rs ──────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Header { group: Group, count: usize },
    /// Index into the `&[Session]` slice that was passed to `build_rows`.
    Session { idx: usize },
}

/// Build the flat render list: for each group in Working, Idle, Completed
/// order, emit a `Header` (only if the group has >=1 matching session) followed
/// by its `Session` rows.
///
/// Filtering: case-insensitive substring of `filter` against
/// `Session::filter_haystack()`. An empty `filter` matches everything.
/// `show_completed == false` omits the Completed group entirely.
///
/// Within a group, sessions sort by `started_at` DESCENDING (newest first),
/// tie-broken by `session_id` ASCENDING so the order is total and stable.
pub fn build_rows(sessions: &[Session], filter: &str, show_completed: bool) -> Vec<Row>;

// ── Formatting helpers (pure; used by ui.rs) ────────────────────────────────

/// Relative age. `now_ms` is epoch ms (chrono::Utc::now().timestamp_millis()).
///   < 60s      -> "12s"
///   < 60m      -> "2m"
///   < 24h      -> "17h"
///   otherwise  -> "3d"
/// Negative or future timestamps clamp to "0s". Never returns >5 chars.
pub fn format_age(started_at_ms: i64, now_ms: i64) -> String;

/// `home` is `std::env::var("HOME").ok()`, passed in so this stays pure.
/// 1. Replace a leading `home` with `~`.
/// 2. If the result fits `max`, return it.
/// 3. Otherwise drop leading path components, replacing them with `…/`, until
///    it fits, always keeping at least the final component.
/// 4. If the final component alone exceeds `max`, end-truncate it with `…`.
/// Returns a string of at most `max` chars. `max == 0` returns "".
pub fn shorten_cwd(cwd: &str, home: Option<&str>, max: usize) -> String;

/// End-truncate to `max` CHARS, appending '…' when truncation occurred.
/// `max == 0` -> ""; `max == 1` on an over-long input -> "…".
pub fn truncate_end(s: &str, max: usize) -> String;
```

**Consumes:** nothing in-crate.

### 3.2 `src/tmux.rs` — owner: Tmux

Every tmux interaction in the program, plus /proc ancestry resolution. No
ratatui, no `claude`, no knowledge of `model::Session`. The pane map stores
session ids as opaque strings so this module stays decoupled.

**Exports:**

```rust
use std::collections::BTreeMap;

pub const WINDOW_NAME: &str = "cc";
pub const OPT_MAP: &str = "@ccmux_map";
pub const OPT_SIDEBAR: &str = "@ccmux_sidebar";
pub const OPT_WIDTH: &str = "@ccmux_width";

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum TmuxError {
    /// `tmux` could not be spawned at all.
    NotFound(String),
    /// tmux ran and exited non-zero; carries trimmed stderr.
    Cmd { args: Vec<String>, code: i32, stderr: String },
    /// A target failed validation, or a pane is outside the ccmux session.
    BadTarget(String),
    /// tmux output did not match the requested `-F` format.
    Parse(String),
}

impl std::fmt::Display for TmuxError {}
impl std::error::Error for TmuxError {}

// ── PaneId: the only way to name a pane ─────────────────────────────────────

/// A validated tmux pane id in `%N` form. Stable across window/pane
/// renumbering (PROBE-FINDINGS §4). The ONLY accepted pane target type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(String);

impl PaneId {
    /// Accepts `^%\d+$` only. Everything else -> None.
    pub fn parse(s: &str) -> Option<PaneId>;
    pub fn as_str(&self) -> &str;
}

impl std::fmt::Display for PaneId {}

// ── Pane facts ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub id: PaneId,
    /// `#{pane_pid}` — the pane's shell. Match target for the ancestry walk.
    pub pid: i32,
    pub index: u32,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    pub active: bool,
    pub session_name: String,
    pub window_index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// vim `:vsplit` — side by side. tmux flag `-h`.
    Vertical,
    /// vim `:split` — stacked. tmux flag `-v`.
    Horizontal,
}

impl SplitDir {
    /// Vertical => "-h", Horizontal => "-v". Named explicitly because vim and
    /// tmux use opposite words for the same geometry.
    pub fn tmux_flag(self) -> &'static str;
}

// ── Raw runner ──────────────────────────────────────────────────────────────

/// The ONLY place `std::process::Command::new("tmux")` appears.
/// Always argv; never `sh -c`; never a formatted shell string.
pub fn tmux(args: &[&str]) -> Result<String, TmuxError>;

/// Convenience: true when `tmux(args)` returned Ok.
pub fn tmux_ok(args: &[&str]) -> bool;

// ── Environment probes ──────────────────────────────────────────────────────

/// `std::env::var("TMUX").is_ok()`
pub fn inside_tmux() -> bool;

/// `tmux display-message -p '#{session_name}'`; None when not inside tmux.
pub fn current_session_name() -> Option<String>;

/// `tmux has-session -t <session>` — exit 0 => true. A dead server also
/// yields exit != 0, which correctly reads as "does not exist".
pub fn has_session(session: &str) -> bool;

/// `tmux -V` spawns successfully.
pub fn server_available() -> bool;

// ── Session lifecycle (launcher only) ───────────────────────────────────────

/// `tmux new-session -d -s <session> -n cc -P -F '#{pane_id}' -- <sidebar_cmd>`
/// `sidebar_cmd` is a shell-command string already built with `sh_quote`.
/// Returns the sidebar pane id.
pub fn create_session(session: &str, sidebar_cmd: &str) -> Result<PaneId, TmuxError>;

/// Applies the ccmux session options: `status off`, `mouse on`,
/// `@ccmux_sidebar`, `@ccmux_width`, and an empty `@ccmux_map`.
pub fn configure_session(session: &str, sidebar: &PaneId, width: u16) -> Result<(), TmuxError>;

/// `switch-client -t` when inside tmux, else `attach-session -t`.
/// On the attach path this replaces the current terminal view and normally
/// does not return until the client detaches.
pub fn attach_or_switch(session: &str) -> Result<(), TmuxError>;

// ── Pane enumeration and mutation ───────────────────────────────────────────

/// `tmux list-panes -t <session> -s -F '<FMT>'` — session-scoped (R3).
/// FMT = "#{pane_id}\t#{pane_pid}\t#{pane_index}\t#{pane_left}\t#{pane_top}\t\
///        #{pane_width}\t#{pane_height}\t#{pane_active}\t#{session_name}\t#{window_index}"
pub fn list_panes_in_session(session: &str) -> Result<Vec<PaneInfo>, TmuxError>;

/// READ-ONLY server-wide enumeration. Permitted ONLY for interactive-session
/// discovery (§5.4). Never feeds reconciliation, never feeds a mutation.
pub fn list_panes_all() -> Result<Vec<PaneInfo>, TmuxError>;

/// R2 gate. Err(BadTarget) when `pane` is absent from `session`.
pub fn assert_in_session(pane: &PaneId, session: &str) -> Result<(), TmuxError>;

/// `tmux split-window <dir.tmux_flag()> -t <target> -P -F '#{pane_id}' -d -- <shell_cmd>`
/// `-d` keeps focus in the sidebar so the operator can keep driving the list.
/// Calls `assert_in_session(target, session)` first. Returns the new pane id.
pub fn split(
    session: &str,
    target: &PaneId,
    dir: SplitDir,
    shell_cmd: &str,
) -> Result<PaneId, TmuxError>;

/// Insert a pane to the LEFT of `target` (`split-window -h -b`). Used only to
/// heal a missing sidebar.
pub fn split_left_of(
    session: &str,
    target: &PaneId,
    shell_cmd: &str,
) -> Result<PaneId, TmuxError>;

/// `tmux kill-pane -t <pane>`. R2-gated. SAFE with respect to Claude sessions:
/// PROBE-FINDINGS §3 proves the agent survives.
pub fn kill_pane(session: &str, pane: &PaneId) -> Result<(), TmuxError>;

/// `tmux select-pane -t <pane>`. R2-gated.
pub fn select_pane(session: &str, pane: &PaneId) -> Result<(), TmuxError>;

/// `tmux resize-pane -t <pane> -x <cols>`. R2-gated.
/// Verified no-op (exit 0) when `pane` is the window's only pane.
pub fn resize_pane_width(session: &str, pane: &PaneId, cols: u16) -> Result<(), TmuxError>;

/// `resize_pane_width` with errors swallowed. Call this on every tick and after
/// every split/kill (§1.3).
pub fn pin_sidebar(session: &str, sidebar: &PaneId, cols: u16);

/// Leftmost pane by `#{pane_left}`, tie-broken by lowest `pane_index`.
pub fn leftmost_pane(panes: &[PaneInfo]) -> Option<PaneId>;

/// Rightmost pane by `#{pane_left}` EXCLUDING `sidebar`. None when the sidebar
/// is alone in the window.
pub fn rightmost_pane_excluding(panes: &[PaneInfo], sidebar: &PaneId) -> Option<PaneId>;

/// `tmux switch-client -t <session>` then `select-pane -t <pane>`. Used to jump
/// to an interactive session living in a foreign tmux session (§5.4). This is
/// the one mutation permitted outside `cli.session`, and it only moves the
/// client's focus — it creates, kills, and resizes nothing.
pub fn focus_foreign_pane(session_name: &str, pane: &PaneId) -> Result<(), TmuxError>;

// ── User options (map persistence) ──────────────────────────────────────────

/// `tmux show-options -t <session> -qv <key>`.
/// The `-q` is MANDATORY: without it an unset user option exits 1 with
/// "invalid option". With it: empty stdout, exit 0. Returns None for empty.
pub fn get_user_option(session: &str, key: &str) -> Option<String>;

/// `tmux set-option -t <session> <key> <value>` as argv, so `value` needs no
/// escaping whatsoever — JSON with quotes and backslashes round-trips verbatim
/// (verified).
pub fn set_user_option(session: &str, key: &str, value: &str) -> Result<(), TmuxError>;

// ── The pane map ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneEntry {
    /// `model::Session::session_id` (UUID). Opaque to this module.
    pub session_id: String,
    /// 8-hex short id when known; empty for interactive sessions.
    #[serde(default)]
    pub short_id: String,
    /// Name at open time, for display when the session has vanished from polls.
    #[serde(default)]
    pub name: String,
    /// epoch ms
    #[serde(default)]
    pub opened_at: i64,
}

/// Serialized into `@ccmux_map`. Lives and dies with the tmux session.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneMap {
    /// Schema version. Current = 1. A different value is treated as an empty map.
    pub v: u32,
    /// pane_id ("%25") -> entry. BTreeMap so serialization is deterministic and
    /// "did it change" comparisons are byte-stable.
    pub panes: BTreeMap<String, PaneEntry>,
}

impl PaneMap {
    pub fn new() -> Self;                                  // v = 1, empty
    pub fn get(&self, pane: &PaneId) -> Option<&PaneEntry>;
    pub fn insert(&mut self, pane: &PaneId, entry: PaneEntry);
    pub fn remove(&mut self, pane: &PaneId);
    /// First pane currently mapped to `session_id`, lowest pane id first.
    pub fn pane_for_session(&self, session_id: &str) -> Option<PaneId>;
    /// All panes mapped to `session_id`, ascending. Double-attach is legal
    /// (PROBE-FINDINGS §3), so this may return more than one.
    pub fn panes_for_session(&self, session_id: &str) -> Vec<PaneId>;

    /// Drop entries whose pane id is not in `live`. Entries whose *Claude
    /// session* has vanished are KEPT as long as the pane exists — the pane is
    /// still on screen showing its exit notice and must remain closable.
    /// Returns true when anything was removed.
    pub fn reconcile(&mut self, live: &[PaneInfo]) -> bool;
}

/// Read `@ccmux_map` and deserialize. Any failure (unset, empty, bad JSON,
/// `v != 1`) yields `PaneMap::new()` — never an error. A corrupt map must not
/// stop the sidebar from starting.
pub fn load_map(session: &str) -> PaneMap;

/// Serialize and write to `@ccmux_map`.
pub fn save_map(session: &str, map: &PaneMap) -> Result<(), TmuxError>;

// ── Shell quoting (§7) ──────────────────────────────────────────────────────

/// POSIX single-quote escaping for the ONE place tmux needs a shell string.
pub fn sh_quote(s: &str) -> String;

/// Join `parts` into a single `sh`-safe command line: `sh_quote` each, join
/// with a single space.
pub fn sh_join(parts: &[&str]) -> String;

// ── /proc ancestry (§5.4) ───────────────────────────────────────────────────

/// Parse `/proc/<pid>/stat`.
/// PARSE RULE: `comm` (field 2) is parenthesized and MAY CONTAIN SPACES AND
/// PARENTHESES. Find the LAST b')' in the line; the remainder splits on
/// whitespace as [state, ppid, ...]; ppid is index 1. Never `split_whitespace`
/// the whole line. Returns None on any IO or parse failure.
pub fn ppid_of(pid: i32) -> Option<i32>;

/// Walk `ppid_of` upward from `pid`, inclusive of `pid`, stopping at pid <= 1,
/// at a repeated pid, or after `max_depth` steps (use 32).
pub fn ancestry(pid: i32, max_depth: usize) -> Vec<i32>;

/// Walk up from `pid` and return the first pane whose `PaneInfo::pid` appears
/// in the ancestry chain. This is how an interactive Claude session is mapped
/// to the pane that hosts it (PROBE-FINDINGS §4). Background sessions are
/// daemon-owned and will always return None here — that is expected, not an error.
pub fn resolve_pane_for_pid(pid: i32, panes: &[PaneInfo]) -> Option<PaneInfo>;
```

**Consumes:** nothing in-crate (`serde` derives only).

### 3.3 `src/agents.rs` — owner: Agents

Everything that shells out to `claude`, plus the shell-command templates for
panes. Builds strings; never runs tmux.

**Exports:**

```rust
use crate::model::{ParseError, Session};

#[derive(Debug)]
pub enum AgentsError {
    /// `claude` could not be spawned.
    NotFound(String),
    /// Non-zero exit; carries trimmed stderr and the exit code.
    Cmd { code: i32, stderr: String },
    /// Output was not the expected JSON.
    Parse(ParseError),
    /// Caller asked for a verb that needs a short id on a session without one.
    NotAttachable,
}

impl std::fmt::Display for AgentsError {}
impl std::error::Error for AgentsError {}

/// Resolved once at startup: "claude" unless `CCMUX_CLAUDE_BIN` overrides it.
/// Everything in this module and every pane template uses this value.
pub fn claude_bin() -> String;

// ── Polling ─────────────────────────────────────────────────────────────────

/// `claude agents --json --all`.
/// ALWAYS passes `--all`: without it completed sessions are omitted and the
/// Completed group can never populate. Hiding completed rows is a UI concern
/// (the `a` key), not a fetch concern.
/// Measured cost 0.21s (PROBE-FINDINGS §1).
pub fn poll() -> Result<Vec<Session>, AgentsError>;

// ── Verbs ───────────────────────────────────────────────────────────────────

/// `claude stop <id>` — DESTRUCTIVE. Callers MUST have passed §8.2's
/// confirmation gate. `id` is the 8-hex short id; interactive sessions have
/// none, so `Session::is_attachable()` must be checked first.
pub fn stop(id: &str) -> Result<(), AgentsError>;

/// `claude logs <id>`. Output is a RAW ANSI/PTY DUMP including alt-screen setup
/// and cursor moves (PROBE-FINDINGS §2) — always pass it through `strip_ansi`
/// before display. Returns the last `lines` lines after stripping.
pub fn logs(id: &str, lines: usize) -> Result<String, AgentsError>;

/// `claude --bg <task>` executed with `current_dir(cwd)`, returning
/// immediately. Pure argv — `task` and `cwd` never touch a shell.
/// Empty `task` is rejected before the call by app.rs.
pub fn dispatch_background(cwd: &str, task: &str) -> Result<(), AgentsError>;

// ── Pane command templates (shell strings; see §7) ──────────────────────────

/// Shell command for a pane that attaches a background session.
/// The trailing `read` keeps the pane alive with a readable message when the
/// session has vanished between poll and open (§9.4).
///
/// Produces exactly:
///   <claude> attach <id>; rc=$?; printf '\n[ccmux] session exited (rc=%s). press enter to close pane.\n' "$rc"; read _
///
/// with `<claude>` and `<id>` passed through `sh_quote`.
pub fn attach_pane_cmd(id: &str) -> String;

/// Shell command for a pane running a NEW interactive session in `cwd`.
///
/// Produces exactly:
///   cd <cwd> || { printf '[ccmux] cannot cd to %s\n' <cwd>; read _; exit 1; }; <claude>; rc=$?; printf '\n[ccmux] claude exited (rc=%s). press enter to close pane.\n' "$rc"; read _
///
/// with `<cwd>` and `<claude>` passed through `sh_quote`.
pub fn interactive_pane_cmd(cwd: &str) -> String;

// ── ANSI ────────────────────────────────────────────────────────────────────

/// Strip CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL | ESC \`), and two-char
/// `ESC <byte>` sequences; drop remaining C0 controls except `\n` and `\t`;
/// normalize `\r\n` and lone `\r` to `\n`. Pure, allocation-only, no regex dep.
pub fn strip_ansi(raw: &str) -> String;
```

**Consumes:** `model::{Session, ParseError}` and `tmux::sh_quote` (the pane
command templates below are the shell boundary of RULE Q2 and must quote through
the same function the launcher uses). Nothing else from `tmux`.

### 3.4 `src/ui.rs` — owner: UI

**PURE RENDERING.** Takes `&App` and a `&mut Frame`, writes cells. It spawns no
process, opens no file, reads no clock beyond what `App` already carries, and
mutates nothing. Any `Command`, `std::fs`, or `&mut App` appearing in this file
is a spec violation.

**Exports:**

```rust
use ratatui::{style::Color, Frame};
use crate::app::App;

/// Gruvbox, matching ~/projects/slurm-tui/src/palette.rs.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub fg: Color,
    pub gray: Color,
    pub dim: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub blue: Color,
    pub purple: Color,
    pub aqua: Color,
    pub orange: Color,
    pub sel_bg: Color,
}

impl Palette {
    pub fn dark() -> Self;
    pub fn light() -> Self;
    pub fn for_app(app: &App) -> Self;   // app.dark ? dark() : light()
}

/// THE entry point. Everything else in this module is private.
pub fn draw(f: &mut Frame, app: &App);

/// Number of session rows the list viewport can show at `total_height`.
/// The single source of truth for viewport arithmetic. `main.rs` calls it once
/// per frame and writes the result into `App::viewport`, so `app.rs` never
/// imports `ui` and the dependency DAG stays acyclic.
/// Returns 0 when the area is degenerate.
pub fn list_viewport_rows(total_height: u16) -> u16;
```

**Consumes:** `app::{App, Mode, Confirm, Prompt, PromptKind, MsgLevel, LogsView}`,
`model::{Group, Row, Session, Kind, Status, format_age, shorten_cwd, truncate_end}`,
`tmux::PaneId` (for `Display` only).

### 3.5 `src/app.rs` — owner: Integrator

State, event loop body, keymap dispatch, actions. All IO orchestration.

**Exports:**

```rust
use std::time::{Duration, Instant};
use crossterm::event::KeyEvent;
use crate::model::{Row, Session};
use crate::tmux::{PaneId, PaneInfo, PaneMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgLevel { Info, Warn, Error }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// `S` — the only destructive confirmation in v1.
    StopSession { session_id: String, short_id: String, name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// `n` — two fields: 0 = cwd, 1 = task text.
    NewBackground,
    /// `c` — one field: cwd.
    NewInteractive,
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    /// `NewBackground`: ["<cwd>", "<task>"]. `NewInteractive`: ["<cwd>"].
    pub fields: Vec<String>,
    pub focus: usize,
    pub cursor: usize,
}

#[derive(Debug, Clone)]
pub struct LogsView {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
    Confirm(Confirm),
    Prompt(PromptKind),
    Help,
    Logs,
}

/// What a keypress resolved to. Returned by `App::on_key` so the event loop can
/// tell "redraw" from "quit" without inspecting state, and so the keymap is
/// testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Quit,
}

pub struct App {
    // config
    pub tmux_session: String,
    pub sidebar_width: u16,
    pub interval: Duration,
    pub dark: bool,
    pub home: Option<String>,

    // data
    pub sessions: Vec<Session>,
    pub rows: Vec<Row>,
    pub now_ms: i64,

    // selection: `selected` indexes `rows` and ALWAYS points at Row::Session
    // (or equals rows.len() when the list is empty). `selected_key` is the
    // session_id under the cursor; it is what survives a re-sort or regroup.
    pub selected: usize,
    pub selected_key: Option<String>,
    pub scroll: usize,
    /// Session rows the list can currently show. Written by `main.rs` every
    /// frame from `ui::list_viewport_rows(terminal_height)`. `app.rs` reads it
    /// for Ctrl-d/Ctrl-u and scroll clamping but never computes it.
    pub viewport: u16,

    // view state
    pub filter: String,
    pub show_completed: bool,
    pub mode: Mode,
    pub prompt: Option<Prompt>,
    pub logs: Option<LogsView>,

    // tmux
    pub map: PaneMap,
    pub map_dirty: bool,
    /// TRANSIENT resolution cache for interactive sessions, rebuilt from /proc
    /// every tick and NEVER persisted to `@ccmux_map`.
    /// session_id -> the ccmux pane hosting it (§5.4 first pass).
    /// Interactive sessions get no map entry (§5.5) because they have no
    /// session_id at open time; this cache is what makes their open marker,
    /// their `Enter` jump, and §8.5's refusal check work.
    pub interactive_panes: std::collections::BTreeMap<String, PaneId>,
    pub sidebar_pane: Option<PaneId>,
    pub panes: Vec<PaneInfo>,
    /// True when running outside tmux: list/filter/refresh/logs work, every
    /// pane verb refuses with a message (§9.5).
    pub degraded: bool,

    // messaging + health
    pub message: Option<(String, MsgLevel)>,
    pub msg_deadline: Option<Instant>,
    pub poll_error: Option<String>,
    pub fail_streak: u32,
    pub last_poll: Instant,

    pub should_quit: bool,
}

impl App {
    /// Does no IO beyond `env::var("HOME")` and `tmux::inside_tmux()`.
    pub fn new(tmux_session: String, sidebar_width: u16, interval: Duration, dark: bool) -> Self;

    /// One-time startup IO: load `@ccmux_map`, resolve `@ccmux_sidebar`,
    /// set `degraded`. Never fails; failures degrade.
    pub fn init(&mut self);

    /// Called when `last_poll.elapsed() >= effective_interval()`.
    /// Order is fixed:
    ///   1. now_ms = Utc::now().timestamp_millis()
    ///   2. panes = list_panes_in_session(tmux_session)   [skipped when degraded]
    ///   3. map.reconcile(&panes) -> map_dirty |= changed
    ///   4. agents::poll() -> sessions (on Err: keep last good, bump fail_streak)
    ///   5. rebuild `interactive_panes`: for every session with
    ///      `kind == Interactive`, tmux::resolve_pane_for_pid(pid, &panes)
    ///      [skipped when degraded; cleared and rebuilt, never merged]
    ///   6. rebuild rows, re-anchor selection by selected_key
    ///   7. flush the map if map_dirty
    ///   8. pin_sidebar (unconditional, §1.3)
    ///   9. last_poll = Instant::now()
    pub fn tick(&mut self);

    /// Poll interval in force: `interval`, or 10s once `fail_streak >= 3`.
    pub fn effective_interval(&self) -> Duration;

    /// True when a timed message just expired (caller should redraw).
    pub fn check_message_timeout(&mut self) -> bool;

    /// THE keymap. Full dispatch table in §8. Pure with respect to the
    /// terminal: it may shell out, but it never touches stdout.
    pub fn on_key(&mut self, key: KeyEvent) -> Action;

    // ── read-only accessors used by ui.rs ────────────────────────────────────
    pub fn selected_session(&self) -> Option<&Session>;
    /// First live pane showing `session_id`: the reconciled `map` first, then
    /// the transient `interactive_panes` cache. Both are ccmux-scoped, so a
    /// `Some` result is always safe to pass to an R2-gated mutation.
    pub fn pane_of(&self, session_id: &str) -> Option<PaneId>;
    /// `#{pane_index}` of `pane`, for the sidebar's pane badge.
    pub fn pane_index_of(&self, pane: &PaneId) -> Option<u32>;
    /// `pane_of(session_id).is_some()`. Drives the §6.4 open marker for both
    /// background (map) and interactive (/proc cache) sessions.
    pub fn is_open(&self, session_id: &str) -> bool;

    // ── selection ────────────────────────────────────────────────────────────
    pub fn select_next(&mut self);
    pub fn select_prev(&mut self);
    pub fn select_first(&mut self);
    pub fn select_last(&mut self);
    pub fn select_half_page(&mut self, down: bool);
    /// Move to the first session row of the next (or previous) non-empty group.
    pub fn cycle_group(&mut self, forward: bool);
    /// Re-point `selected` at `selected_key` after `rows` changed; falls back to
    /// the nearest valid session row, then to the first one.
    pub fn reanchor_selection(&mut self);
    pub fn clamp_scroll(&mut self);

    // ── verbs (each is one keymap entry's whole effect) ──────────────────────
    pub fn act_open(&mut self, dir: crate::tmux::SplitDir);
    pub fn act_enter(&mut self);
    pub fn act_close_pane(&mut self);
    pub fn act_request_stop(&mut self);
    pub fn act_confirm_stop(&mut self);
    pub fn act_open_logs(&mut self);
    pub fn act_force_refresh(&mut self);
    pub fn act_submit_prompt(&mut self);

    // ── messaging ────────────────────────────────────────────────────────────
    /// Shows `text` in the footer for 4s.
    pub fn flash(&mut self, text: impl Into<String>, level: MsgLevel);
}
```

**Consumes:** `model`, `tmux`, `agents`.

### 3.6 `src/main.rs` — owner: Integrator

```rust
mod agents;
mod app;
mod model;
mod tmux;
mod ui;

fn main() -> anyhow::Result<()>;

/// §1.2. Never enters raw mode.
fn run_launcher(cli: &Cli) -> anyhow::Result<()>;

/// Raw mode + alternate screen + the event loop; teardown runs on every exit
/// path including a panic-free error return.
fn run_sidebar(cli: &Cli, interval_ms: u64) -> anyhow::Result<()>;

fn event_loop(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut app::App,
) -> anyhow::Result<()>;
```

**Consumes:** everything.

---

## 4. The event loop

### 4.1 Structure (mirrors `~/projects/slurm-tui/src/main.rs`)

```
enable_raw_mode; EnterAlternateScreen; Terminal::new
app.init()
loop {
    if app.check_message_timeout() { needs_draw = true }

    if app.last_poll.elapsed() >= app.effective_interval() {
        app.tick();
        needs_draw = true;
    }

    app.viewport = ui::list_viewport_rows(terminal.size()?.height);
    app.clamp_scroll();

    if needs_draw { terminal.draw(|f| ui::draw(f, app))?; needs_draw = false }

    if event::poll(Duration::from_millis(120))? {
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match app.on_key(k) {
                Action::Quit   => break,
                Action::Redraw => needs_draw = true,
                Action::None   => {}
            },
            Event::Resize(..) => needs_draw = true,
            _ => {}
        }
    }
}
disable_raw_mode; LeaveAlternateScreen; terminal.show_cursor()
```

`KeyEventKind::Press` filtering is mandatory — without it, Windows-style
key-repeat/release events double every keystroke.

### 4.2 Polling policy — pinned, so nobody invents a thread

**Polling is synchronous, on the event-loop thread. No threads, no channels, no
async runtime.** `claude agents --json` costs 0.21s (PROBE-FINDINGS §1); the
default interval is 2500 ms; `event::poll` uses a 120 ms slice. Worst-case input
latency is therefore ~210 ms once every 2.5 s, which is acceptable and is
exactly how `slurm-tui` behaves. Do not "improve" this into a background thread
in v1 — it would require making `PaneMap` and `App` `Send`, and it is not the
bottleneck.

Backoff: after `fail_streak >= 3`, `effective_interval()` returns 10 s. The
first success resets `fail_streak` to 0 and the interval with it.

`r` (force refresh) sets `last_poll = Instant::now() - effective_interval()` so
the next loop iteration polls immediately; it does **not** call `tick()` inline.

---

## 5. The session → pane map

PROBE-FINDINGS §4 is decisive: a pane's cmdline does not identify the session it
displays (panes opened from the stock fleet view all read `claude agents`).
ccmux therefore owns the mapping.

### 5.1 Key and shape

Keyed on tmux `#{pane_id}` in `%N` form — stable across window and pane
renumbering, unlike `session:window.pane`. Never keyed on `pid` (unstable across
attach/detach) and never on `pane_index` (renumbers on kill).

Values are keyed back to `model::Session::session_id` (UUID, stable), not to the
8-hex `id` — the short id is stored only as a convenience for `stop`/`logs`.

```json
{"v":1,"panes":{"%25":{"session_id":"1c45d64f-9bba-4038-8de7-d5f112c92360","short_id":"1c45d64f","name":"bt/reg-update","opened_at":1787640000000}}}
```

### 5.2 Persistence: a tmux session user option

Stored in `@ccmux_map` on the tmux session:

```
write:  tmux set-option   -t <session> @ccmux_map <json>
read:   tmux show-options -t <session> -qv @ccmux_map
```

Chosen because the map's correct lifetime is *exactly* the tmux session's
lifetime — panes and their map die together, so a stale on-disk file can never
resurrect a mapping for a reused pane id. The sidebar can be killed with `q` and
relaunched by re-running `ccmux`, and it recovers the map intact.

Two verified properties this depends on:

- **`set-option` takes the JSON as an argv element**, so quotes and backslashes
  round-trip byte-for-byte with no escaping. Verified: writing
  `{"%1":{"session_id":"abc-123","name":"a b\"c"}}` read back identically.
- **`show-options` needs `-q`.** Without it, an unset user option exits 1 with
  `invalid option: @ccmux_map`. With `-qv`, unset yields empty stdout and exit 0.

Written back only when `map_dirty` is set, and `save_map` compares the freshly
serialized string against the last written value before issuing the tmux call.
`BTreeMap` makes that comparison deterministic. This keeps a steady-state tick
at exactly two tmux calls (`list-panes`, `resize-pane`).

### 5.3 Reconciliation, every tick

```
1. live = tmux::list_panes_in_session(app.tmux_session)     // -s scope (R3)
2. changed = app.map.reconcile(&live)
      // drop every entry whose pane id is absent from `live`
      // KEEP entries whose Claude session is gone but whose pane still exists:
      //   that pane is on screen showing "[ccmux] session exited (rc=..)"
      //   and the operator must still be able to close it with `x`.
3. app.map_dirty |= changed
4. app.panes = live
   // `interactive_panes` is NOT reconciled here: tick() step 5 clears and
   // rebuilds it wholesale from /proc, so it cannot go stale.
5. if app.sidebar_pane is absent from `live`:
      app.sidebar_pane = tmux::get_user_option(session, "@ccmux_sidebar")
                            .and_then(|s| PaneId::parse(&s))
                            .filter(|p| live.iter().any(|i| &i.id == p))
      // still None => the sidebar is somehow unmapped; pin_sidebar becomes a
      // no-op and split anchoring falls back to leftmost_pane. Never fatal.
```

A session appearing in the poll but absent from the map is simply "not open" —
no action. A pane present in `live` but absent from the map is a pane ccmux did
not create (a manual `tmux split-window` by the user, or the sidebar itself);
ccmux leaves it strictly alone: it is never adopted, never killed, and never
retargeted.

### 5.4 Resolving interactive sessions via /proc

Background sessions are daemon-owned: their pid is not a descendant of any pane,
so `resolve_pane_for_pid` returns `None` for them and that is correct, not an
error. They are reached only through `claude attach <id>`.

Interactive sessions **are** descendants of their pane's process. Resolution:

```
1. chain = tmux::ancestry(sess.pid, 32)
       read /proc/<pid>/stat, take ppid, repeat until pid <= 1 / repeat / depth
2. first pass:  match `chain` against PaneInfo::pid over app.panes  (ccmux only)
                -> found: the session is open inside ccmux. This pass runs for
                   EVERY interactive session on EVERY tick (`tick()` step 5) and
                   populates `App::interactive_panes`, which is what
                   `is_open()` / `pane_of()` consult for interactive rows.
                   `Enter` then becomes select-pane.
                Cost: one `/proc/<pid>/stat` read per ancestry hop, only for
                interactive sessions (typically 1-2 of them), depth-capped at 32.
                Cheap enough to run unconditionally; do not cache across ticks,
                because a pid's pane can change when the operator moves it.
3. second pass: only when the first fails AND the operator pressed a jump key.
                match `chain` against tmux::list_panes_all()   [READ-ONLY, R3]
                -> found in a foreign tmux session S:
                     Enter => tmux::focus_foreign_pane(S, pane)
                              flash "jumped to <S>:<w>.<p> (outside ccmux)"
                -> not found: flash "interactive session is not in a tmux pane"
```

`/proc/<pid>/stat` parse rule, restated because getting it wrong is a silent
bug: field 2 is `comm`, wrapped in parentheses, and it **may contain spaces and
parentheses** (a process named `foo bar) baz`). Split on the **last** `)` in the
line; the tail splits on whitespace as `[state, ppid, ...]`; `ppid` is index 1.
Verified: `/proc/<pid>/stat` for a zsh under an interactive Claude session
yielded `comm=(zsh) state=S ppid=2936154`, and 2936154 is exactly the
`kind:"interactive"` pid reported by `claude agents --json`.

### 5.5 Map mutations

| Event | Map change |
|---|---|
| `o` / `s` / `Enter`-opens a background session | insert `{new_pane -> entry}`; `map_dirty = true` |
| `c` creates an interactive session | **no entry** in `@ccmux_map` — there is no `session_id` yet. The next tick's step 5 resolves it by /proc into the transient `interactive_panes` cache. |
| `x` closes a pane | `map.remove(pane)`; `map_dirty = true` |
| pane disappears (user killed it, or `claude attach` exited and the pane closed) | dropped by `reconcile` |
| `S` stops a session | no map change; its pane stays until the operator closes it |

Double-attach is legal (PROBE-FINDINGS §3), so a session may legitimately map to
several panes. `pane_for_session` returns the lowest-numbered one for jumps;
`x` closes only that one.

---

## 6. Sidebar rendering spec

`ui.rs` only. Pure. Target width 30–40 columns; **must not panic at any width or
height, including 0**.

### 6.1 Vertical layout

```
row 0            header
rows 1..=H-6     list viewport   (headers + session rows, scrollable)
row H-5          separator  '─' × width, dim
rows H-4..=H-2   detail block for the selected session (3 lines)
row H-1          footer
```

Degradation by height, applied in this order:

| Available height | Layout |
|---|---|
| `H >= 12` | full layout above |
| `8 <= H < 12` | drop the separator and the detail block; list takes `1..=H-2` |
| `3 <= H < 8` | header, list, footer only; list is `1..=H-2` |
| `H == 2` | header + list; no footer |
| `H <= 1` | header only |
| `H == 0` or `W == 0` | return immediately, draw nothing |

`list_viewport_rows(total_height)` implements exactly this table and is the only
place the arithmetic lives. `main.rs` calls it once per frame and stores the
result in `App::viewport`; `app.rs` reads that field and never recomputes it.

**Every** height/width derivation uses `saturating_sub`. No `as i32` casts, no
`- 1` on a `u16` that could be 0, no slicing a `Vec` with an unclamped range.

### 6.2 Header (row 0)

```
 ccmux  7 sessions          ●        ← W >= 26
 ccmux  7                   ●        ← 14 <= W < 26
 ccmux                              ← W < 14
```

- `ccmux` in `p.aqua` + `BOLD`.
- Count in `p.gray`. When a filter is active: `4/7` (matching/total).
- Trailing single-cell poll indicator, right-aligned at `W-2`:
  - healthy → `●` in `p.dim`
  - `poll_error.is_some()` → `●` in `p.red`
  - `degraded` (outside tmux) → `○` in `p.yellow`

### 6.3 Group headers

Rendered only for non-empty groups. Full line, `p.dim`, with the title in the
group's accent colour:

```
── Working (3) ──────────────
── Idle (1) ─────────────────
── Completed (2) ────────────
```

Accent: Working `p.orange`, Idle `p.blue`, Completed `p.gray`.
At `W < 20` drop the rule characters and render `Working (3)` alone.
Header rows are never selectable; `j`/`k` skip over them.

### 6.4 Session rows — one line each

Fixed-width row, left to right:

```
col 0        open marker
col 1        status glyph
col 2        space
cols 3..     name (flexible, end-truncated)
             right side: [pane badge] [age]
```

**Open marker (col 0)** — this is the distinct mark for "currently open in a
pane" required by the brief:

| Condition | Glyph | Colour |
|---|---|---|
| open in >= 1 ccmux pane | `▌` | `p.aqua` |
| not open | ` ` | — |

**Status glyph (col 1)**, derived from `Group` + `Status`:

| Session | Glyph | Colour | Meaning |
|---|---|---|---|
| Working + Busy | `●` | `p.orange` | actively generating |
| Working + Idle | `◐` | `p.blue` | live, waiting on input |
| Idle group | `○` | `p.gray` | idle |
| Completed | `✓` | `p.green` | done |
| `Status::Unknown` / `State::Unknown` | `?` | `p.purple` | forward-compat |

Interactive sessions additionally render their **name** in `p.purple` instead of
`p.fg`, so the two kinds are separable at a glance without a second glyph column.

**Pane badge** — the tmux `#{pane_index}` of the pane showing this session,
right-aligned immediately left of the age, in `p.aqua`, formatted `%N` (e.g.
`2`). Shown only when the session is open and `W >= 34`.

**Age** — `model::format_age`, right-aligned in the final 4 columns, `p.dim`.

**Selected row** — background `p.sel_bg`, foreground forced to `p.fg`, and the
whole line padded to the full width so the highlight is a solid bar. The glyph
keeps its own colour. No cursor character is drawn; the bar is the cursor.

**Width degradation** (all thresholds on the list area's inner width `W`):

| `W` | Row content |
|---|---|
| `>= 34` | marker + glyph + name + pane badge + age |
| `28..=33` | marker + glyph + name + age (no pane badge) |
| `20..=27` | marker + glyph + name (no age, no badge) |
| `6..=19` | glyph + name, name truncated to `W-2` |
| `< 6` | glyph only; if `W == 0`, nothing |

Name budget is always computed as `W.saturating_sub(fixed_cols)` and passed to
`model::truncate_end`. When the budget is 0 the name is omitted, not panicked on.

### 6.5 Detail block (rows H-4..=H-2)

Three lines describing the **selected** session, all `p.gray` except values:

```
 name    prediction analysis ab_1_2_cd
 id      674b1d29  background  busy
 cwd     ~/p/shared/…/price-sanitize
```

- `cwd` uses `model::shorten_cwd(cwd, app.home.as_deref(), W - 9)`.
- For interactive sessions line 2 reads `id      —  interactive  busy`.
- When a session is open, append ` · pane 2` to line 2 in `p.aqua`.
- Empty selection (no sessions, or all filtered out) renders three blank lines.

### 6.6 Truncation and unicode

Truncation is **char-based** (`chars().take(n)`), not grapheme- or width-based;
`unicode-width` is deliberately not a dependency. Consequence: a name of CJK or
emoji characters can overflow its budget by up to `n` columns, and ratatui clips
it at the pane edge. This is a cosmetic overflow, never a panic, and never a
buffer write outside the `Rect`. Revisit only if real session names make it
visible.

### 6.7 Overlays

All overlays render **inside the sidebar Rect** — ccmux never draws over a
Claude pane.

- **Help (`?`)** — `Mode::Help`. Full-sidebar `Block` titled ` keys `, the §8
  table one binding per line, two columns (`key`, `action`) when `W >= 30`,
  otherwise `key action` on one line each. Scrolls with `j`/`k` when it
  overflows.
- **Confirm (`S`)** — `Mode::Confirm`. Centred block, `p.red` border, titled
  ` stop session `, body = the session name and short id, footer =
  `y: stop    n/Esc: cancel`. Detail in §8.2.
- **Prompt (`n`, `c`)** — `Mode::Prompt`. Block titled ` new background session `
  or ` new interactive session `. One line per field, the focused field prefixed
  `> ` and carrying a reverse-video cursor cell at `prompt.cursor`; unfocused
  fields dim. Footer = `Tab: field   Enter: run   Esc: cancel`.
- **Filter (`/`)** — no block; the footer becomes the input line: `/` + buffer +
  cursor. The list keeps updating live underneath.
- **Logs (`L`)** — `Mode::Logs`. Full-sidebar block titled with the session name,
  ANSI-stripped text, `j`/`k`/`Ctrl-d`/`Ctrl-u`/`g`/`G` scroll, `q`/`Esc` closes.

### 6.8 Footer (row H-1)

Priority order — the first applicable wins:

1. `Mode::Filter` → `/<buffer>▏`, `p.yellow`
2. `app.message` present → the text, coloured by `MsgLevel`
   (Info `p.green`, Warn `p.yellow`, Error `p.red`), auto-clearing after 4 s
3. `poll_error` present → `agents: <msg>` in `p.red` (truncated to width)
4. otherwise → the hint line in `p.dim`, truncated from the right as needed:
   `j/k move  ⏎ open  o/s split  x close  S stop  n new  ? help`

---

## 7. Shell quoting — the one hard rule

**RULE Q1 — argv everywhere.** Every external command is built with
`std::process::Command::new(prog).args([...])`. ccmux never spawns `sh -c`, never
uses `format!` to build a command line for its own execution, and never passes a
user-controlled string through a shell it controls. Session names, cwds, task
text, ids: all are argv elements. This makes them immune to spaces, quotes,
`$`, backticks, newlines, and `;`.

**RULE Q2 — exactly one shell boundary exists, and it is tmux's.**
tmux's `shell-command` argument (the trailing argument of `new-session` and
`split-window`) is executed by tmux via `/bin/sh -c`. That is the only string in
the program that a shell will parse. It is built solely by
`agents::attach_pane_cmd`, `agents::interactive_pane_cmd`, and the launcher's
sidebar command, and every interpolated value passes through `tmux::sh_quote`.

```rust
/// POSIX-safe single-quoting.
/// - Empty string -> "''"
/// - Matches ^[A-Za-z0-9_@%+=:,./-]+$ -> returned unchanged (readable panes)
/// - Otherwise -> '\'' + s.replace('\'', "'\\''") + '\''
pub fn sh_quote(s: &str) -> String {
    if s.is_empty() { return "''".into(); }
    if s.bytes().all(|b| b.is_ascii_alphanumeric()
        || b"_@%+=:,./-".contains(&b)) { return s.into(); }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' { out.push_str("'\\''"); } else { out.push(c); }
    }
    out.push('\'');
    out
}
```

Required unit tests (Tmux lane):

```text
input                        expected output
---------------------------  -----------------------------
/home/dev/projects/af        /home/dev/projects/af
<empty string>               ''
my project                   'my project'
it's here                    'it'\''s here'
a;rm -rf /                   'a;rm -rf /'
$(whoami)                    '$(whoami)'
back`tick`                   'back`tick`'
a<LF>b  (embedded newline)   'a<LF>b'      (newline kept inside the quotes)
```

**RULE Q3 — `--` before shell-commands.** Every `new-session` / `split-window`
call places `--` immediately before the shell-command argument, so a command
beginning with `-` can never be re-read as a tmux flag.

**RULE Q4 — no shell for `claude --bg`.** The task text is arbitrary prose and
must never be shell-parsed:
`Command::new(claude_bin()).arg("--bg").arg(task).current_dir(cwd)`.
`claude --bg <prompt>` takes the prompt as a positional argument (verified
against `claude --help`: `--bg, --background  Start the session as a background
agent and return immediately`).

---

## 8. The keymap

Vim-native. `KeyEventKind::Press` only. Unbound keys return `Action::None`.

### 8.1 Normal mode

| Key | Action | Destructive? |
|---|---|---|
| `j`, `Down` | next session row (skips group headers) | no |
| `k`, `Up` | previous session row (skips group headers) | no |
| `g` | first session row (`gg` also works — the second `g` is idempotent) | no |
| `G` | last session row | no |
| `Ctrl-d` | down half a viewport | no |
| `Ctrl-u` | up half a viewport | no |
| `Tab` | jump to the first row of the next non-empty group | no |
| `BackTab` (`Shift-Tab`) | previous non-empty group | no |
| `Enter` | **open or jump** — §8.3 | no |
| `o` | open in a **vertical** split (vim `:vsplit`, side by side, tmux `-h`) | no |
| `s` | open in a **horizontal** split (vim `:split`, stacked, tmux `-v`) | no |
| `x` | close the pane showing a **background** session — agent keeps running. **Refused for interactive sessions** (§8.5) | no |
| `S` | **stop the session** — requires confirmation | **YES** (§8.2) |
| `n` | dispatch a new background session with a typed task | no |
| `c` | new interactive session in a chosen cwd | no |
| `L` | show `claude logs` for this session (ANSI-stripped) | no |
| `/` | enter filter mode | no |
| `a` | toggle visibility of the Completed group | no |
| `r` | force refresh | no |
| `?` | help overlay | no |
| `q` | quit the sidebar (sessions and panes untouched) | no |
| `Esc` | clear the filter if one is active, otherwise quit (§8.8) | no |

`o` and `s` are named for vim's geometry, and `SplitDir::tmux_flag()` is where
the vim→tmux word inversion is resolved exactly once. `o` = vertical =
side-by-side = `-h`. `s` = horizontal = stacked = `-v`. Do not re-derive this
anywhere else.

### 8.2 Destructive actions and their confirmation UX

`S` is the **only** destructive binding in v1. There is no "kill pane group", no
"stop all", no bulk verb.

Pressing `S`:

1. If the selected session is interactive (`id.is_none()`):
   flash `cannot stop an interactive session` (`MsgLevel::Warn`) and stop. No
   modal. `claude stop` requires a short id and interactive sessions have none.
2. If the selected session is already in `Group::Completed`:
   flash `session already completed` (Warn) and stop.
3. Otherwise enter `Mode::Confirm(Confirm::StopSession { .. })` and render:

```
   ┌─ stop session ─────────────┐
   │                            │
   │  bt/reg-update             │
   │  1c45d64f                  │
   │                            │
   │  Stops the agent. The      │
   │  conversation is kept;     │
   │  resume with Enter later.  │
   │                            │
   │  y: stop     n/Esc: cancel │
   └────────────────────────────┘
```

Confirmation rules, all mandatory:

- **No default-affirmative.** There is no highlighted "Yes"; `Enter` alone does
  **not** confirm. Only the literal key `y` (lowercase, no modifiers) confirms.
- `n`, `Esc`, `q`, `Enter`, and any other key cancel and return to
  `Mode::Normal` with no side effect.
- The modal captures the session's `short_id` **at the moment `S` was pressed**
  and `act_confirm_stop` uses that captured id, not the live cursor. A poll
  landing between `S` and `y` can reorder the list; without capture, `y` would
  stop whatever slid under the cursor. This is the single most important detail
  in the keymap.
- Before calling `agents::stop`, re-check that `short_id` still appears in
  `app.sessions`. If it does not, cancel and flash
  `session <id> is gone — not stopped` (Warn).
- On success: flash `stopped <name>` (Info) and force a refresh. The pane, if
  any, is **left open** — closing it is the operator's separate `x`.
- On `AgentsError`: flash `stop failed: <stderr first line>` (Error).

Every other verb is non-destructive by construction and takes no confirmation:
`x` is proven safe (PROBE-FINDINGS §3: killing the pane leaves the agent
running), `q` only exits the sidebar process, `n`/`c` only create.

### 8.3 `Enter` semantics

```
sel = selected_session() or return

if sel.is_attachable():                       # background, has short id
    if let Some(pane) = app.pane_of(sel.session_id):
        tmux::select_pane(session, pane)      # jump to the existing pane
        flash "jumped to pane <index>"
    else:
        act_open(SplitDir::Vertical)          # identical to `o`
else:                                          # interactive, no short id
    §5.4 resolution:
      found in a ccmux pane   -> select_pane, flash "jumped to pane <index>"
      found in a foreign pane -> focus_foreign_pane, flash "jumped to <sess>:<w>.<p> (outside ccmux)"
      not found               -> flash "interactive session is not in a tmux pane" (Warn)
```

Jumping rather than re-splitting is a UX preference, not a correctness
requirement — double-attach is legal (PROBE-FINDINGS §3). If the operator wants
a second view of the same session, `o` and `s` always split unconditionally.

### 8.4 `o` / `s` — opening into a split

```
act_open(dir):
  1. degraded          -> flash "not inside tmux — open unavailable" (Warn); return
  2. sel = selected_session() or return
  3. !sel.is_attachable() -> delegate to the interactive branch of §8.3; return
  4. anchor = split_anchor()
  5. cmd  = agents::attach_pane_cmd(sel.id)
  6. pane = tmux::split(session, anchor, dir, cmd)?
  7. map.insert(pane, PaneEntry { session_id, short_id, name, opened_at: now_ms })
     map_dirty = true; save_map now (do not wait for the tick)
  8. tmux::pin_sidebar(session, sidebar, sidebar_width)
  9. flash "opened <name> in pane <index>" (Info)
     on Err(TmuxError) -> flash "split failed: <msg>" (Error)
```

**`split_anchor()`** — deterministic, so two engineers cannot disagree:

```
1. the active pane (`PaneInfo::active`) if it is not the sidebar
2. else `tmux::rightmost_pane_excluding(&panes, sidebar)`
3. else the sidebar itself   (first split: the window has only the sidebar,
                              so splitting the sidebar is the only option;
                              step 8's re-pin immediately restores its width)
```

Focus stays in the sidebar (`split -d`) so the operator can open several
sessions in a row without leaving the list.

### 8.5 `x` — close pane (safe for background sessions only)

```
1. degraded -> flash "not inside tmux — close unavailable" (Warn); return
2. sel = selected_session() or return
3. sel.kind == Interactive
       -> flash "refusing: closing this pane would end the interactive
                 session — exit Claude inside the pane instead" (Warn)
          return
4. pane = app.pane_of(sel.session_id)
   none -> flash "not open" (Warn); return
5. pane == sidebar_pane -> flash "refusing to close the sidebar" (Warn); return
6. tmux::kill_pane(session, pane)   [R2-gated]
7. map.remove(pane); map_dirty = true; save_map
8. tmux::pin_sidebar(...)
9. flash "closed pane <index> — agent still running" (Info)
```

**Step 3 is a correctness gate, not politeness.** PROBE-FINDINGS §3 verified
that killing a pane leaves the agent running — *for background sessions*, which
are daemon-owned and whose pid is not a descendant of any pane (§4 of the same
document). Interactive sessions are the opposite: they **are** descendants of
their pane's process, so `kill-pane` SIGHUPs the Claude process and ends the
session. Applying the background result to an interactive row would silently
destroy work while telling the operator "agent still running".

So `x` is refused for interactive sessions rather than routed through a
confirmation. This keeps `S` the only destructive binding in v1 (§8.2), and the
operator's route to ending an interactive session is the one Claude already
gives them: exit it inside its own pane.

Step 9's wording matters: it is the operator-facing statement of
PROBE-FINDINGS §3, shown every time, so nobody ever confuses `x` with `S`.

### 8.6 `n` — dispatch a new background session

`Mode::Prompt(NewBackground)`, two fields:

```
   ┌─ new background session ───┐
   │  cwd  ~/projects/shared    │
   │ > task  regenerate the ...  │
   │  Tab: field  ⏎ run  Esc: ✕ │
   └────────────────────────────┘
```

- Field 0 `cwd` prefills with the selected session's `cwd`, or `$PWD` when the
  list is empty. Field 1 `task` starts empty and is focused.
- `Tab` / `BackTab` move focus; `Left`/`Right`/`Home`/`End`/`Backspace` edit;
  printable chars insert at `cursor`.
- `Enter` submits from any field. `Esc` cancels, discarding both fields.
- Validation before dispatch: `task.trim()` non-empty (else flash
  `task cannot be empty`, Warn, stay in the prompt) and `cwd` is an existing
  directory (`std::path::Path::is_dir`; else flash `no such directory: <cwd>`,
  Warn, stay in the prompt).
- Dispatch: `agents::dispatch_background(cwd, task)` — pure argv (Q4).
- On success: leave the prompt, flash `dispatched background session` (Info),
  force a refresh. The new session appears in the next poll under **Working**.
  ccmux does **not** auto-open it — the operator decides with `Enter`.

### 8.7 `c` — new interactive session in a chosen cwd

`Mode::Prompt(NewInteractive)`, one field (`cwd`), prefilled from the selected
session's cwd or `$PWD`.

- `Enter`: validate `is_dir`, then
  `tmux::split(session, split_anchor(), SplitDir::Vertical, agents::interactive_pane_cmd(cwd))`,
  then `pin_sidebar`.
- **No `@ccmux_map` entry is written** — the session has no `session_id` until
  Claude starts. The next poll surfaces it as `kind: interactive`, and `tick()`
  step 5's /proc walk binds it to the pane in `App::interactive_panes`, which is
  what renders its open marker. Expect a 1-tick delay before the marker appears;
  that is correct behaviour, not a bug to paper over.
- Flash `started interactive session in <shortened cwd>` (Info).

### 8.8 Filter mode (`/`)

- `/` enters `Mode::Filter` with the existing `filter` preserved (so `/` then
  `Enter` re-opens the last filter).
- Printable chars, `Backspace`, `Ctrl-w` (delete word), `Ctrl-u` (clear) edit
  live; `rows` rebuild on every keystroke and the selection re-anchors.
- `Enter` commits and returns to `Mode::Normal`, keeping the filter active. The
  header shows `4/7`.
- `Esc` clears the filter entirely and returns to `Mode::Normal`.
- Matching is case-insensitive substring against
  `Session::filter_haystack()` = `name + " " + cwd + " " + short_id`, lowercased.
- In `Mode::Normal`, `Esc` with a non-empty filter clears the filter instead of
  quitting; `Esc` with an empty filter quits.

### 8.9 Mode key precedence

`on_key` dispatches on `self.mode` **first**; only `Mode::Normal` sees the §8.1
table.

- `Mode::Help` — any key returns to `Normal`, except `j`/`k`/`Ctrl-d`/`Ctrl-u`
  which scroll the overlay.
- `Mode::Logs` — `j`/`k`/`Ctrl-d`/`Ctrl-u`/`g`/`G` scroll; `q`/`Esc` closes.
- `Mode::Confirm` — only `y` confirms; everything else cancels (§8.2).
- `Mode::Prompt` — text editing; `Enter` submits; `Esc` cancels.
- `Mode::Filter` — text editing; `Enter` commits; `Esc` clears.

`Ctrl-c` quits from **any** mode, immediately, without confirming and without
touching a session — the sidebar owns no agent state, so there is nothing to
lose. It is deliberately not routed through `Mode::Confirm`.

---

## 9. Error and edge handling

Governing principle: **the sidebar never dies and never blanks.** Every failure
degrades to a message in the footer over the last known-good list. `unwrap()`,
`expect()`, and `panic!` are forbidden in `sidebar` code paths outside of
`Mutex` poisoning that cannot occur single-threaded.

### 9.1 `claude agents --json` exits non-zero

- Keep `app.sessions` and `app.rows` exactly as they are — the previous list
  stays on screen and stays navigable.
- `fail_streak += 1`; `poll_error = Some(first non-empty stderr line, or
  "exit <code>")`, truncated to 120 chars.
- Footer shows `agents: <msg>` in red; header indicator turns red.
- `fail_streak >= 3` → interval widens to 10 s (§4.2).
- Any success clears `poll_error` and resets `fail_streak`.
- Verbs that need fresh data (`Enter`, `o`, `s`, `S`) still act on the stale
  list. `S` re-validates against `app.sessions` (§8.2), so a stale entry fails
  closed with `session is gone — not stopped`.

### 9.2 Invalid JSON

`parse_sessions` returns `ParseError::Json { msg, excerpt }` where `excerpt` is
the first 80 chars of the output with control characters escaped. Handled
identically to §9.1; the footer shows `agents: bad json: <excerpt>`. A partially
valid array does **not** fail: rows missing `sessionId`, `name`, or `cwd` are
skipped individually and the rest render.

### 9.3 The daemon is down

Two shapes, both non-fatal:

- `claude agents --json` prints `[]` and exits 0 → a **valid empty result**, not
  an error. `sessions` becomes empty, `rows` empty, and the list area renders a
  centred `no sessions` in `p.dim`. `poll_error` stays `None`; the indicator
  stays healthy. Do not invent an error here — "no agents running" is a normal
  state.
- `claude agents --json` errors or hangs → §9.1.

`n` and `c` remain available with an empty list (both fall back to `$PWD`), so
the operator can bootstrap from zero sessions.

### 9.4 A session vanishes between poll and open

Unavoidable: the poll is up to 2.5 s stale.

- **`Enter`/`o`/`s`** — ccmux splits the pane anyway; `claude attach <id>` fails
  inside it. The pane command's trailer keeps the pane alive with a readable
  message:
  `[ccmux] session exited (rc=1). press enter to close pane.`
  The map entry stays (§5.3) so `x` still closes it. ccmux does not pre-validate
  by polling again — the race is unclosable and the readable pane is the
  correct handling.
- **`S`** — re-validated against `app.sessions` before the call, then `claude
  stop` may still fail; its stderr surfaces via `stop failed: <msg>`.
- **`x`** — targets a pane, not a session, so a vanished session is irrelevant.
  A vanished *pane* is caught by `assert_in_session` returning `BadTarget`, and
  `reconcile` drops the entry on the next tick.

### 9.5 tmux is not running / ccmux is running outside tmux

- **Launcher, `tmux` binary absent** → `eprintln!("ccmux: tmux not found on
  PATH")`, exit 1.
- **Launcher, tmux present but no server** → `has_session` exits non-zero, which
  reads correctly as "does not exist", and `new-session` starts the server. No
  special case needed.
- **`ccmux sidebar` outside tmux** (`$TMUX` unset) → **degraded mode**, not an
  error. `app.degraded = true`.
  - Works: polling, list, grouping, `/`, `a`, `r`, `L`, `?`, `j/k/g/G/Tab`, `n`
    (dispatch is pure `claude --bg`, no tmux involved), `S` (pure `claude stop`).
  - Refuses with `flash("not inside tmux — <verb> unavailable", Warn)`:
    `Enter`, `o`, `s`, `x`, `c`.
  - `PaneMap` is held in memory only; `load_map`/`save_map` are skipped.
  - Header indicator is a yellow `○`.
  This is what makes `ccmux sidebar` runnable standalone for development without
  a tmux server.

### 9.6 ccmux launched from inside its own session

`tmux::inside_tmux() && current_session_name() == Some(cli.session)` →
print `ccmux: already inside session 'ccmux'` and **exit 0**. Not an error, and
emphatically not a second `new-session`: re-running `ccmux` inside `ccmux` must
be a no-op, never a duplicate. Launched from a *different* tmux session →
`switch-client -t ccmux` (§1.2 step 6).

### 9.7 Sessions with `id` absent (interactive)

`Session::is_attachable()` is `false` and gates everything that needs a short id:

| Verb | Interactive behaviour |
|---|---|
| `Enter` | §5.4 /proc resolution → jump, or `interactive session is not in a tmux pane` |
| `o` / `s` | same as `Enter` — there is no `claude attach` for them, so a split would have nothing to run |
| `x` | **refused** — the pane owns the process, so killing it ends the session (§8.5 step 3) |
| `S` | refused: `cannot stop an interactive session` (§8.2 step 1) |
| `L` | refused: `no logs for an interactive session` |
| grouping | never `Completed` (no `state` field); Busy → Working, Idle → Idle |
| rendering | name in `p.purple`; detail block shows `id —  interactive` |

### 9.8 Other edges, each with a pinned answer

| Edge | Handling |
|---|---|
| The user manually splits a pane inside `ccmux` with tmux keys | The pane is unmapped; ccmux ignores it. The tick re-pin keeps the sidebar at its width. Never adopted, never killed. |
| The user resizes the sidebar with tmux keys | Reverted within one tick by the unconditional `pin_sidebar`. To change it for real, restart with `--width`. |
| `@ccmux_map` holds a pane id that no longer exists | Dropped by `reconcile` on the first tick. |
| `@ccmux_map` is corrupt, empty, or `v != 1` | `load_map` returns `PaneMap::new()`. Never an error, never a crash. |
| `@ccmux_sidebar` is missing or stale | §5.3 step 5 re-resolves; if it stays `None`, `pin_sidebar` no-ops and `split_anchor` falls back to `leftmost_pane`. |
| The sidebar pane is killed by the user | The ccmux window keeps its Claude panes. Re-running `ccmux` heals it via §1.2 step 5b. |
| The window shrinks below `sidebar_width` columns | tmux clamps `resize-pane`; the sidebar renders per §6.4's `< 6` rule and does not panic. |
| Two `ccmux sidebar` processes in one tmux session | Both poll and both write `@ccmux_map`; last write wins and `reconcile` converges them. Degraded but harmless. Not defended against in v1. |
| `claude` is not on `PATH` | Every verb returns `AgentsError::NotFound`; the footer shows `agents: claude not found on PATH`. The sidebar still runs. `CCMUX_CLAUDE_BIN` overrides the path. |
| Terminal is resized mid-modal | `Event::Resize` only sets `needs_draw`; all overlays recompute their `Rect` from `f.area()` each frame. |
| `format_age` on a future `startedAt` | Clamps to `"0s"`. |
| A session's `name` contains newlines or ANSI | Rendered through `truncate_end` into a ratatui `Span`; ratatui does not interpret control bytes, so this is a display artifact, never an injection. It never reaches a shell (Q1/Q4). |

---

## 10. Acceptance criteria and tests

### 10.1 Unit tests (no tmux, no `claude`; run in CI)

**model.rs (Scaffold)**
- `parse_sessions` on the exact 4-element payload in PROBE-FINDINGS §1 yields 4
  sessions; the interactive one has `id == None`, `state == None`.
- `parse_sessions` on a payload with one row missing `sessionId` yields
  `len - 1` sessions and no error.
- `parse_sessions("{}")` → `ParseError::NotAnArray`; `parse_sessions("oops")` →
  `ParseError::Json`.
- `group()`: `state=working, status=idle` → `Working` (state wins over status);
  `state=done` → `Completed`; interactive+busy → `Working`; interactive+idle →
  `Idle`.
- `build_rows` emits no header for an empty group; sorts newest-first; a filter
  matching nothing yields an empty `Vec`.
- `format_age`: `0 → "0s"`, `59_000 → "59s"`, `120_000 → "2m"`,
  `61_200_000 → "17h"`, `259_200_000 → "3d"`, negative → `"0s"`.
- `shorten_cwd("/home/dev/projects/shared/Foundation", Some("/home/dev"), 24)`
  starts with `~` and is ≤ 24 chars; `max = 0` → `""`.
- `truncate_end` at `max` 0 and 1 does not panic and respects char boundaries on
  multi-byte input.

**tmux.rs (Tmux)**
- `PaneId::parse`: `"%25"` → Some; `""`, `"25"`, `"%"`, `"%2a"`,
  `"agents:2.1"` → None.
- `sh_quote` — the full §7 table.
- `ppid_of` on a `/proc/<pid>/stat` fixture whose `comm` is `(a b) c)` returns
  the correct ppid (this is the last-`)` rule).
- `PaneMap::reconcile` drops absent panes, keeps present ones, returns the right
  `changed` flag; round-trips through serde byte-identically.
- `SplitDir::Vertical.tmux_flag() == "-h"`, `Horizontal → "-v"`.

**agents.rs (Agents)**
- `attach_pane_cmd("1c45d64f")` equals the §3.3 template exactly.
- `interactive_pane_cmd("/home/dev/my projects")` single-quotes the cwd.
- `strip_ansi` removes CSI/OSC/two-char escapes and keeps `\n`; a raw
  alt-screen preamble (`\x1b[?1049h\x1b[H\x1b[2J`) strips to empty.

**ui.rs (UI)**
- `ui::draw` into a `TestBackend` at **20x8, 30x24, 34x76, 40x76, 5x3, 1x1, and
  0x0** does not panic, in every `Mode`, with 0 sessions and with 40.
- `list_viewport_rows(0..=12)` matches the §6.1 table.

**app.rs (Integrator)**
- `on_key('S')` on an interactive row leaves `mode == Normal` and sets a Warn
  message.
- `on_key('x')` on an interactive row calls **no** tmux mutation and sets a Warn
  message — this is the §8.5 step 3 gate, and it is the test that stops someone
  "simplifying" the interactive branch back into the background path.
- `pane_of` resolves a background session through `map` and an interactive
  session through `interactive_panes`, and returns `None` for a session in
  neither.
- `on_key('S')` then `on_key('n')` leaves `mode == Normal` and calls nothing.
- The confirm modal captures the short id: mutate `app.sessions` between `S` and
  `y`, and the captured id is still the one used.
- `j`/`k` never land on a `Row::Header`; `G` on an empty list does not panic.

### 10.2 Integration checks (manual, against a throwaway session)

Run **only** against `--session ccmux-test-<something>`. Never against `ccmux`
while the operator is using it, and never against `agents` or `dev`.

1. `ccmux --session ccmux-test-a` from outside tmux → attaches; sidebar is
   leftmost, 34 cols, full height.
2. Re-run the same command from another terminal → attaches the **same** session;
   no second window, no second sidebar.
3. From inside it, re-run → prints `already inside session 'ccmux-test-a'`,
   exit 0.
4. `o` three times, then `s` → sidebar still exactly 34 cols (compare against the
   verified geometry table in §1.3).
5. `x` on an open session → pane closes; the session stays in the list; the next
   poll still shows it under Working with no open marker.
6. `q`, then re-run `ccmux --session ccmux-test-a` → sidebar is re-inserted on
   the left at 34 cols and `@ccmux_map` still resolves the open panes.
7. `tmux kill-session -t ccmux-test-a` to clean up.

### 10.3 Explicit non-goals for v1

Do not build these; do not leave hooks for them.

- Any use of `~/.claude/daemon/roster.json`, `ptySock`, `rendezvousSock`,
  `dispatch/`, `attach-journal/`, or `CLAUDE_CODE_MESSAGING_SOCKET`
  (PROBE-FINDINGS §5 — three CLI versions already coexist and internals move).
- `claude resume` / `claude list` — **not subcommands**; they fall through to
  generic help (PROBE-FINDINGS §2).
- Multiple ccmux windows, saved layouts, session reordering, drag-resize.
- A live log preview pane in the sidebar (`L` is an on-demand overlay only).
- Any background thread, async runtime, or IPC.
- Neovim-as-host or a hand-written terminal emulator. Both were evaluated and
  rejected: nvim terminal buffers do not survive detach/SSH-drop, and a custom
  VT parser means rebuilding zellij. Not open for relitigation.

---

## Appendix A — mechanisms empirically verified during spec authoring

Everything in this table was executed against tmux 3.4 on this machine, in a
throwaway session, while writing this spec. It is in addition to — and
consistent with — `PROBE-FINDINGS.md`. Treat these as facts, not proposals; do
not spend implementation time re-deriving them.

| Claim | Evidence |
|---|---|
| `show-options -v @key` on an **unset** user option exits **1** with `invalid option: @key`; `-qv` yields empty stdout and exit **0**. | `-q` is therefore mandatory in `get_user_option`. |
| `set-option -t <sess> @ccmux_map <json>` round-trips JSON byte-for-byte through argv, including `"` and `\`. | Wrote `{"%1":{"session_id":"abc-123","name":"a b\"c"}}`, read back identical. |
| `display-message -p '#{@key}'` also reads user options and returns empty for unset. | Alternative reader; `show-options -qv` is the one specified. |
| `has-session -t <name>` → exit 0 when present, exit 1 (`can't find session`) when absent. | Basis of §1.2 step 3. |
| `new-session -P -F '#{pane_id}'` prints the new pane id directly. | No `list-panes \| head -1` needed. |
| `-n cc` names the window; `<sess>:cc` addresses it. | §1.2. |
| `split-window -h -t <p>` then `resize-pane -t <sidebar> -x 34` keeps the sidebar leftmost, full height, exactly 34 cols — across three nested splits and a mid-window `kill-pane`. | Full geometry trace in §1.3. |
| `resize-pane -x` on a **lone** pane is a no-op returning exit **0**. | Makes the unconditional per-tick re-pin safe. |
| `split-window -h -b -t <leftmost>` inserts a pane to the **left** and it becomes `pane_index 1`. | Basis of sidebar healing, §1.2 step 5b. |
| `--` is accepted before the shell-command by both `new-session` and `split-window`, and guards a command starting with `-`. | Basis of RULE Q3. |
| `set-option -t <sess> status off` works per-session. | §1.2 step 4e. |
| `/proc/<pid>/stat` gives `comm` parenthesized in field 2; ppid is the 2nd whitespace token after the **last** `)`. | Confirmed on a live shell whose ppid was the `kind:"interactive"` pid reported by `claude agents --json`. |
| `claude agents --json` output matches PROBE-FINDINGS §1 exactly on the current build (2.1.245), including `id`/`state` absent for `kind:"interactive"`. | Re-read during authoring. |
| `claude --bg, --background` takes the prompt as a **positional** argument. | `claude --help`. Basis of RULE Q4. |

### A.1 A cautionary note that became RULE R1

While probing, a shell variable holding a pane target evaluated to the empty
string. `tmux split-window -t "" ...` did **not** error — tmux silently fell back
to the caller's current pane and created panes in an unrelated live session.
That is the entire reason `PaneId` is a validated newtype, mutating helpers take
mandatory targets, and `assert_in_session` gates every mutation (§2). An
`Option<PaneId>` reaching a tmux mutation is a bug even when it happens to work.
