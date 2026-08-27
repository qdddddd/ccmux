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
        g. tmux set-option -w -t <pane> @ccmux_tab_sidebar <pane>   # mark tab 1
        h. tmux resize-pane -t <pane> -x <width>

 5. if exists — heal ONCE PER TAB (§11.5). Healing is the launcher's job
    exclusively: a running sidebar never lays out another tab, because that
    would be a second writer of another window's options.
        a. tmux set-option -t <name> @ccmux_width <width>          # first, always
        b. for each window W in tmux::list_tabs(<name>):
             live = W's @ccmux_tab_sidebar names a pane still in W
             i.   live                       -> tmux resize-pane -t <sidebar> -x <width>
             ii.  W is a ccmux tab (a marker is set — the SOLE test) but the
                  sidebar is gone — quit with `q` or crashed:
                      leftmost = pane with the smallest `#{pane_left}` in W
                      pane = tmux split-window -h -b -t <leftmost> -P -F '#{pane_id}' -d -- <SIDEBAR_CMD>
                      tmux set-option -w -t <pane> @ccmux_tab_sidebar <pane>
                      tmux resize-pane -t <pane> -x <width>
             iii. no marker -> LEAVE THE WINDOW COMPLETELY ALONE. It is the
                  operator's own (a bare `prefix-c` inside the ccmux session),
                  or a tab `t` is building right now.
        c. if NO window carried a marker, this session was written
           by a build that had no tabs: fall back to the pre-tabs rule exactly —
           the pane named by the session-scoped `@ccmux_sidebar` if it parses
           and is live (adopt and re-pin it; never split a second one in beside
           it), else the `cc`-named window, else the lowest window — then give
           that window a proper `@ccmux_tab_sidebar` so this path cannot fire
           twice. The marker must be read with `show-options -w -qv` or through
           `list_tabs`'s `_tab_`-prefixed names, NEVER as a format lookup of the
           legacy key: `#{@name}` falls back window -> session -> global, so on
           a legacy session that would report EVERY window as marked and inject
           a sidebar into each of the operator's own windows.

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

### 1.4 Spreading the content panes evenly

`split-window` HALVES its target, so successive `o` presses give progressively
narrower panes: on a 120x40 window with a 34-column sidebar, four of them leave
`34 42 21 20 …`. After a split or a kill — and at **no other time** — ccmux
re-lays the window so the panes it opened share the axis that was split.

- `o` (`SplitDir::Vertical`, tmux `-h`) puts panes side by side, so it divides
  **width**. `s` (`SplitDir::Horizontal`, tmux `-v`) stacks them, so it divides
  **height**. The sidebar spans the window's full height and is never part of a
  height division, only of a width division.
- The sidebar keeps its pinned width. It is never given a share.
- The remainder is spread **one cell per pane**, never piled onto a single
  pane, so no two content panes ever differ by more than one cell: 120 − 34 − 3
  separators = 83 content columns over three panes is `28 + 28 + 27`. This is
  what tmux's own `even-horizontal` does. Remainder-to-one-pane looks the same
  at three panes and absurd at eight, where it would leave seven panes of 9
  columns beside one of 15.
- **Only a clean row or a clean column is evened.** A mix of `o` and `s` builds
  a tree, where "even along the axis" has no single meaning; `select-layout
  tiled` and evening the top-level groups both move panes the operator placed
  on purpose, so a tree is a **no-op**.
- The complete list of no-ops, each leaving the window byte-identical: zero or
  one content pane; a ragged tree; a split whose direction did not produce the
  matching shape; a window with too few columns or rows to give every pane a
  cell; a sidebar that is not the full-height left edge; a window whose pane
  list disagrees with the order the panes are seen in (below); an `x` that
  killed a pane in **another tab**, which leaves that tab's geometry alone,
  because only the process whose own window lost a pane evens anything; and
  degraded mode.
- **Never on the poll tick.** The mouse is on; evening on a timer would undo a
  border the operator dragged, every 2.5 seconds. The two verbs that made the
  layout uneven are the only ones that even it.

The geometry is written as ONE tmux layout string through `select-layout`,
not as a sequence of `resize-pane` calls. A sequence works — right to left,
twice, because one pass does not settle — but it only *converges* to a fixed
point. A layout string *is* one: it states every cell's size and position
absolutely, so re-applying it changes nothing, and the sidebar cell is written
at exactly the width §1.3 is about to re-assert, which makes the per-tick pin a
no-op instead of a fight.

**tmux binds cells to panes positionally, and the pane ids in a layout string
are cosmetic.** `layout_parse` hands the parsed leaves to the window's pane list
in order and discards the id it read from each one — verified on 3.4, where a
string carrying the ids `3,2,1` was accepted with no pane moving and read back
renumbered `0,1,2,3`. So the cell order ccmux emits (the sidebar, then the
content along the axis) must BE the pane-list order that `#{pane_index}`
reports, or panes would swap places and the sidebar itself would land in a
content cell. Every window ccmux can build already satisfies that; it is
checked anyway, and a window that fails the check is one more no-op.

Verified on tmux 3.4 (120x40 window, sidebar pinned to 34):

```
o o o o     34 85 / 34 42 42 / 34 28 28 27 / 34 21 21 20 20   (widths)
x           34 28 28 27
5 ticks     34 28 28 27   (unchanged)

o s s s s   40 40 / 40 20 19 / 40 13 13 12 / 40 10 9 9 9 / 40 8 7 7 7 7  (heights)
x           40 10 9 9 9
5 ticks     40 10 9 9 9   (unchanged)

o o s       sidebar 34 | 42 | 42 over (20, 19)  — a tree, left exactly as built
```

The first number of each heights row is the sidebar, which spans the window's
full height throughout and is never given a share of it.

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

**R3 — pane enumeration uses `-s`, never `-a`.**
`tmux list-panes -t <session> -s -F ...` lists only that session's panes.
`list-panes -a` lists the whole server, including the user's own work. `-a` is
permitted **nowhere**: `list_panes_in_session` is the only pane enumeration in
the program, so ccmux never so much as sees a pane outside its own session. The
one read-only exception this rule used to carve out — the discovery walk that
located an interactive Claude session living outside ccmux — went with §5.4.

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
/// exactly. Identity is `session_id` and nothing else: the CLI's `pid` is
/// UNSTABLE across attach/detach, and with the /proc ancestry walk gone
/// nothing reads it, so it is no longer carried.
#[derive(Debug, Clone)]
pub struct Session {
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

Every tmux interaction in the program. No ratatui, no `claude`, no knowledge of `model::Session`. The pane map stores
session ids as opaque strings so this module stays decoupled.

**Exports:**

```rust
use std::collections::BTreeMap;

pub const WINDOW_NAME: &str = "cc";
pub const OPT_MAP: &str = "@ccmux_map";
pub const OPT_SIDEBAR: &str = "@ccmux_sidebar";
pub const OPT_WIDTH: &str = "@ccmux_width";
pub const OPT_HIDDEN: &str = "@ccmux_hidden";

// Per-window (per-tab) options — see §11. New NAMES, not reused ones: a
// `#{@name}` format lookup falls back window -> session -> global, so a legacy
// SESSION value under a reused name would be reported as every window's value.
pub const OPT_TAB_SIDEBAR: &str = "@ccmux_tab_sidebar";
pub const OPT_TAB_MAP: &str = "@ccmux_tab_map";
pub const OPT_TAB_HIDDEN: &str = "@ccmux_tab_hidden";
pub const EMPTY_MAP_JSON: &str = r#"{"v":1,"panes":{}}"#;

/// A validated tmux window id in `@N` form. An IDENTITY, never a target: no
/// function in the crate renders one into a `-t` argument (§2, §11.4).
pub struct WindowId(String);
impl WindowId {
    pub fn parse(s: &str) -> Option<WindowId>;   // ^@\d+$
    pub fn as_str(&self) -> &str;
    pub fn num(&self) -> u64;                    // NUMERIC: "@9" < "@10"
}

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
    pub index: u32,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    pub active: bool,
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

/// `tmux list-panes -t <session> -s -F '<FMT>'` — session-scoped (R3), and the
/// ONLY pane enumeration in the program. ccmux never lists the server.
/// FMT = "#{pane_id}\t#{pane_index}\t#{pane_left}\t#{pane_top}\t\
///        #{pane_width}\t#{pane_height}\t#{pane_active}\t#{window_index}\t\
///        #{window_id}"
/// `#{window_id}` is not a duplicate of `#{window_index}`: the index shifts
/// under `renumber-windows`, the id never does, and everything that must
/// survive a tick is keyed by the id.

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

/// tmux's own layout checksum (`layout_checksum` in layout-custom.c).
pub fn layout_checksum(layout: &str) -> u16;

/// The geometry the non-sidebar panes of one window form (§1.4). Only `Row`
/// and `Column` divide along a single axis; a tree is `Ragged` and is a no-op.
pub enum ContentShape { Row, Column, Ragged }

/// Classify one WINDOW-SCOPED pane slice against its sidebar.
pub fn content_shape(panes: &[PaneInfo], sidebar: &PaneId) -> ContentShape;

/// `total` cells over `n` panes, the remainder spread ONE CELL PER PANE so no
/// two differ by more than a cell. None below one each.
pub fn even_shares(total: u16, n: usize) -> Option<Vec<u16>>;

/// The `<checksum>,<layout>` string that spreads one window's content panes
/// evenly (§1.4). `sidebar_cols` MUST be the width `pin_sidebar` will assert.
/// `want` is the shape the pressed key produced, or None on the kill path.
/// None for every no-op in §1.4's list, including a pane list whose order
/// disagrees with the order the panes are seen in — tmux binds layout cells to
/// panes positionally, so emitting them in any other order would permute the
/// window.
pub fn even_layout(
    panes: &[PaneInfo],
    sidebar: &PaneId,
    sidebar_cols: u16,
    want: Option<ContentShape>,
) -> Option<String>;

/// `tmux select-layout -t <pane> <checksum,layout>`. R2-gated, pane-addressed
/// (never a `WindowId`), and refuses anything that is not a checksummed layout
/// string — a layout NAME like `tiled` cannot get through this door.
pub fn apply_layout(session: &str, target: &PaneId, layout: &str) -> Result<(), TmuxError>;

/// Leftmost pane by `#{pane_left}`, tie-broken by lowest `pane_index`.
pub fn leftmost_pane(panes: &[PaneInfo]) -> Option<PaneId>;

/// Rightmost pane by `#{pane_left}` EXCLUDING `sidebar`. None when the sidebar
/// is alone in the window.
pub fn rightmost_pane_excluding(panes: &[PaneInfo], sidebar: &PaneId) -> Option<PaneId>;

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
    /// 8-hex short id when known; empty when the session had none.
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

/// Read the LEGACY session-scoped `@ccmux_map`. Any failure (unset, empty, bad
/// JSON, `v != 1`) yields `PaneMap::new()` — never an error. Only the one-time
/// migration reads it now; the live map is `@ccmux_tab_map`, per window.
pub fn load_map(session: &str) -> PaneMap;

// ── Tabs: per-window state (§11) ────────────────────────────────────────────

/// Everything one tab persists, plus its identity.
pub struct TabInfo {
    pub window: WindowId,
    pub index: u32,
    pub sidebar: Option<PaneId>,
    pub map: PaneMap,
    pub hidden: HiddenLog,
}

/// `tmux list-windows -t '=<session>:' -F …` — the whole cross-tab picture and
/// the session-scoped `@ccmux_width`, in ONE invocation. It REPLACES the
/// per-tick `show-options @ccmux_width`, so the cross-tab picture is free.
/// A window whose fields are absent, malformed, or version-mismatched yields
/// the empty value, never an error.
pub fn list_tabs(session: &str) -> Result<(Vec<TabInfo>, Option<u16>), TmuxError>;

/// `tmux new-window -d -t '=<session>:' -n cc -P -F … -- <first_cmd>`.
/// The target is built by `session_target` ALONE — there is no parameter
/// through which a window target can be passed (§11.4).
pub fn new_tab(session: &str, first_cmd: &str) -> Result<(WindowId, u32, PaneId), TmuxError>;

/// Record `pane` as its own window's sidebar. R2-gated on `pane`.
pub fn set_tab_sidebar(session: &str, pane: &PaneId) -> Result<(), TmuxError>;

/// Write this window's `@ccmux_tab_map` / `@ccmux_tab_hidden`, addressed
/// through the writing process's own sidebar pane. R2-gated, and skipped when
/// the identical value was last written for that (option, session, window).
pub fn save_tab_map(session: &str, pane: &PaneId, window: &WindowId, map: &PaneMap)
    -> Result<(), TmuxError>;
pub fn save_tab_hidden(session: &str, pane: &PaneId, window: &WindowId, log: &HiddenLog)
    -> Result<(), TmuxError>;

/// The ONE write into a window this process does not own: `t` seeds the new
/// tab's map through the Claude pane BEFORE that tab's sidebar exists.
/// Deliberately uncached (§11.2).
pub fn write_tab_map_uncached(session: &str, pane: &PaneId, map: &PaneMap)
    -> Result<(), TmuxError>;

// ── The dismissal log (§11.3) ───────────────────────────────────────────────

/// One dismissal (`add: true`) or restoration (`add: false`), stamped so every
/// reader orders it identically.
pub struct HiddenOp { pub id: String, pub add: bool, pub seq: u64, pub org: u64 }

/// One window's fragment of the shared dismissal log. Schema version 2.
pub struct HiddenLog { pub v: u32, pub ops: Vec<HiddenOp> }
impl HiddenLog {
    pub fn new() -> Self;
    pub fn push(&mut self, op: HiddenOp) -> bool;   // idempotent, capped
    pub fn max_seq(&self) -> u64;
    pub fn forget<'a, I: IntoIterator<Item = &'a str>>(&mut self, ids: I) -> bool;
}

/// Fold every fragment into the shared dismissed set: a last-writer-wins
/// register per id, keyed by the total order `(seq, org)`. Ordered by winning
/// stamp ascending, which is `HiddenSet`'s existing "oldest first" contract.
pub fn fold_hidden<'a, I: IntoIterator<Item = &'a HiddenLog>>(frags: I) -> HiddenSet;

/// Garbage-collect one's OWN fragment against what the others already carry.
pub fn prune_hidden_log(mine: &mut HiddenLog, others: &[&HiddenLog]) -> bool;

// ── Shell quoting (§7) ──────────────────────────────────────────────────────

/// POSIX single-quote escaping for the ONE place tmux needs a shell string.
pub fn sh_quote(s: &str) -> String;

/// Join `parts` into a single `sh`-safe command line: `sh_quote` each, join
/// with a single space.
pub fn sh_join(parts: &[&str]) -> String;

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
/// confirmation gate. `id` is the 8-hex short id; a session without one cannot
/// be stopped, so `Session::is_attachable()` must be checked first.
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
    /// The confirm mechanism. DORMANT since stop moved to `Ctrl+X` (§8.2):
    /// no key path reaches it. Kept for the next verb that needs a modal.
    StopSession { session_id: String, short_id: String, name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// `n` — two fields: 0 = cwd, 1 = task text. The only prompt: `c` and its
    /// `NewInteractive` variant are gone (§8.7).
    NewBackground,
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    /// `NewBackground`: ["<cwd>", "<task>"].
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
    ///   4. agents::poll() -> sessions, MINUS every `kind == Interactive` row
    ///      (on Err: keep last good, bump fail_streak). The exclusion is a
    ///      display policy and lives in `apply_poll`, not in the parser.
    ///   5. rebuild rows, re-anchor selection by selected_key
    ///   6. flush the map if map_dirty
    ///   7. pin_sidebar (unconditional, §1.3)
    ///   8. last_poll = Instant::now()
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
    /// First live pane showing `session_id`, from the reconciled `map` and
    /// nothing else. The map is ccmux-scoped, so a `Some` result is always
    /// safe to pass to an R2-gated mutation.
    pub fn pane_of(&self, session_id: &str) -> Option<PaneId>;
    /// `#{pane_index}` of `pane`, for the sidebar's pane badge.
    pub fn pane_index_of(&self, pane: &PaneId) -> Option<u32>;
    /// `pane_of(session_id).is_some()`. Drives the §6.4 open marker.
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

**AMENDED BY §11.** The live map is `@ccmux_tab_map`, a WINDOW option, one per
tab, each written by exactly one process. The session-scoped `@ccmux_map`
described below is still written once at session creation and re-written to the
empty value by the one-time migration, because `main::run_launcher`'s ownership
guard reads its presence to prove the session is ccmux's — but it is no longer
updated after that. Everything else in this section (the shape, the `-qv` rule,
the ~16 KB ceiling, reconciliation) applies unchanged to the per-window copies.

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

**Removed.** ccmux lists only background sessions (§3.5, `apply_poll`), so no
interactive row can be selected and there is nothing to resolve. Gone with it:
the /proc ancestry walk (`ppid_of`, `ancestry`, `resolve_pane_for_pid`), the
server-wide `list-panes -a` enumeration (`list_panes_all`), and
`focus_foreign_pane`. Background sessions are daemon-owned — their pid is not a
descendant of any pane — so the walk had no second use, and its removal is what
lets R2 hold without exception and R3 forbid `-a` outright.

The section number is kept so §5.5 does not move.

### 5.5 Map mutations

| Event | Map change |
|---|---|
| `o` / `s` / `Enter`-opens a background session | insert `{new_pane -> entry}`; `map_dirty = true` |
| `x` closes a pane | `map.remove(pane)`; `map_dirty = true` |
| pane disappears (user killed it, or `claude attach` exited and the pane closed) | dropped by `reconcile` |
| `Ctrl+X` stops a session | no map change; its pane stays until the operator closes it |
| `Ctrl+X` deletes a session (second press) | no map change; the pane stays and its trailer reports the exit |

Double-attach is legal (PROBE-FINDINGS §3), so a session may legitimately map to
several panes — including one per tab. `pane_for_session` returns the one in the
asking sidebar's OWN tab when there is one, and otherwise the lowest-numbered;
`x` closes only that one. Own-tab-first is not a preference: the pane beside a
sidebar is the one its badge calls "here", the one `Enter` must not travel away
from, and the one `x` must kill. Resolving all three to whichever window
happened to draw the lower pane id made a visible row's verbs act on another
tab's pane while the badge said the opposite.

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
- A session with no short id reads `id      —  background  busy` (§9.7).
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
- **Confirm** — `Mode::Confirm`. Centred block, `p.red` border, titled
  ` stop session `, body = the session name and short id, footer =
  `y: stop    n/Esc: cancel`. **No key opens it** since stop moved to `Ctrl+X`
  (§8.2); it is retained, unreached, for the next verb that needs a modal.
- **Prompt (`n`)** — `Mode::Prompt`. Block titled ` new background session `.
  One line per field, the focused field prefixed
  `> ` and carrying a reverse-video cursor cell at `prompt.cursor`; unfocused
  fields dim. Footer = `Tab: field   Enter: run   Esc: cancel`.
- **Filter (`/`)** — no block; the footer becomes the input line: `/` + buffer +
  cursor. The list keeps updating live underneath.
- **Logs (`L`)** — `Mode::Logs`. Full-sidebar block titled with the session name,
  ANSI-stripped text, `j`/`k`/`Ctrl-d`/`Ctrl-u`/`g`/`G` scroll, `q`/`Esc` closes.

### 6.8 Footer (row H-1)

Priority order — the first applicable wins:

1. `Mode::Filter` → `/<buffer>▏`, `p.yellow`
2. `Ctrl+X`'s delete window is open → the §8.2 warning in `p.red`. Above the
   message on purpose: it is the line that says what the NEXT keypress does, and
   an unrelated flash must not be able to take it off screen while the verb is
   loaded.
3. `app.message` present → the text, coloured by `MsgLevel`
   (Info `p.green`, Warn `p.yellow`, Error `p.red`), auto-clearing after 4 s
4. `poll_error` present → `agents: <msg>` in `p.red` (truncated to width)
5. otherwise → the hint line in `p.dim`, truncated from the right as needed:
   `⏎ open  o/s split  x close  t tab  d/u hide  C-x stop  n new  ? help`

2–4 share one source (`ui::footer_message`), so the wrap of §6.8's amendment
applies to all three: a line wider than the sidebar takes over the detail block
and wraps rather than clipping.

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
`agents::attach_pane_cmd` and the launcher's sidebar command, and every
interpolated value passes through `tmux::sh_quote`.

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
| `t` | open in a **new tab** — a window with its own sidebar — and go there (§8.10) | no |
| `x` | close the pane showing a session — the agent keeps running (§8.5) | no |
| `Ctrl-x` | **stop the session**, immediately; again within 2 s **deletes** it and its worktree (§8.2) | **YES** (§8.2) |
| `S` | unbound as a verb — flashes `stop is Ctrl+X now` and does nothing else | no |
| `n` | dispatch a new background session with a typed task | no |
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

### 8.2 Destructive actions and their UX

`Ctrl+X` is the **only** destructive binding in v1. There is no "kill pane
group", no "stop all", no bulk verb. It carries two verbs, one recoverable and
one not:

> `Ctrl+X` — Stop the session; press again within two seconds to delete it.

which is Claude Code's own agent-view shortcut, adopted verbatim. `stop` keeps
the conversation (`Enter` resumes it); `claude rm` deletes the session **and its
git worktree** and cannot be undone (PROBE-FINDINGS §2).

`S` used to be this binding. It is now unbound as a verb: it flashes
`stop is Ctrl+X now` (Info) and does nothing else. It is answered rather than
ignored because it was the stop key for the whole life of the previous build.

**Placement, mandatory:** the `Ctrl+X` arm MUST sit above `key_normal`'s
`_ if ctrl => Action::None` catch-all. Below it the arm compiles, reads
correctly, and never fires. The unit suite drives the binding through `on_key`,
not through the inner function, for exactly this reason.

#### First press

1. No selection → flash `no session selected` (Warn). No window.
2. No short id (`id.is_none()`) → flash `no short id — cannot stop this session`
   (Warn). No window: a session `claude` cannot be told to stop is one it cannot
   be told to delete either (§9.7).
3. Already in `Group::Completed` → nothing to stop. Flash `<label> is already
   stopped` (Info) and open the window anyway: `claude rm` is documented to work
   on already-exited sessions, and the session the PREVIOUS press stopped is
   Completed by the time the window lapses. Refusing here would make the
   just-stopped session undeletable.
4. Otherwise `claude stop <short_id>`.
   - Ok → force a refresh, open the window, flash `stopped <label>` (Info). The
     pane, if any, is **left open** — closing it is the operator's separate `x`.
   - Err → flash `stop failed: <stderr first line>` (Error) and open **no**
     window. The escalation is only ever an escalation of a stop that happened.

Opening the window CAPTURES `session_id`, `short_id` and `name`, and stamps
`at` **after** `claude stop` returns — the operator's two seconds must not be
spent inside the shell-out.

#### Second press, inside `CX_WINDOW` (2 s)

- The cursor must still be on the captured `session_id`. This is the only check
  that reads the live selection, and it reads it to REFUSE, never to retarget:
  what would be deleted is always the capture. If the cursor has moved, the
  window closes, nothing is stopped, nothing is deleted, and the flash names
  which of the two things happened: `moved off <label> — nothing deleted` (Warn)
  when the captured row is still on screen, and `<label> left the list —
  nothing deleted` (Warn) when it is not. Re-arming on the new row was
  rejected: with the Completed group hidden (`a`), a stopped row leaves the list
  and the cursor falls to a neighbour, so re-arming would stop an innocent agent
  on two deliberate presses.

  Two things make the row leave the list under a still cursor, and the operator
  can act on both once the flash distinguishes them: `a` hiding the Completed
  group, and a `/` filter that matches the WORKTREE path — `claude stop` reverts
  a session's reported `cwd` from `.claude/worktrees/<name>` to the parent
  directory, so the forced refresh behind the first press drops the row out of a
  filter written against the worktree. The escalation is then unreachable in one
  gesture, by design and failing safe. The capability is not lost: clear the
  filter (or press `a`) and press `Ctrl+X` twice on the stopped row — a first
  press on a Completed row arms without stopping anything (§8.2, below).
- The press SCHEDULES the delete; it does not run it (see the settle below).
- Before `claude rm`, the CAPTURED short id is re-validated against
  `app.sessions`. Absent → flash `session <id> is gone — not deleted` (Warn).
- Ok → flash `deleted <label> + worktree` (Warn) and force a refresh.
- Err → flash `delete failed: <first line>` (Error). The line comes from stderr,
  or from **stdout when stderr is blank**: `claude rm` refuses to delete a
  worktree holding unpushed commits or uncommitted changes, and it prints that
  refusal on stdout while exiting 1 (PROBE-FINDINGS §2). Reading stderr alone
  renders `delete failed: exit 1` and drops the sentence that says the work is
  safe. This is the ONE case where a failure is good news, so it must be
  legible.

A press after the window has lapsed is a FIRST press again: it stops, it never
deletes.

#### Mis-fire defence

Moving off `S` removed the paste and prose exposure — no run of text produces a
Ctrl chord, and Normal mode discards pastes (§8.9). Two ways the chord can still
arrive without a human meaning it twice remain, and both are closed:

- **A buffered burst.** Every press stamps `cx_last_press` — on ENTRY, and
  AGAIN when the handler returns — and a press within `CX_MIN_GAP` (750 ms) of
  that stamp acts on nothing. A burst of any length therefore performs exactly
  one stop.

  The re-stamp on the way out is load-bearing, not belt-and-braces. A first
  press shells out to `claude stop` (~0.66 s) and then to `claude agents --json`
  (~0.19 s), and the UI is frozen for all of it. Stamping only on entry measured
  the gap between the times two presses were DEQUEUED, not between the
  keystrokes: two `Ctrl+X` events ~100 ms apart were read ~1.1 s apart, cleared
  the bar, and deleted a session and its worktree before the warning had ever
  been drawn. `run_delete` re-stamps for the same reason — `claude rm` plus its
  refresh blocks just as long, from the event-loop tick where no keypress
  stamped anything, and a press buffered through THAT freeze used to read as a
  fresh first press and stop whichever row the cursor fell to. `main.rs` also
  drains the tty after any keypress that blocked longer than `SLOW_KEY`
  (300 ms), which drops the rest of the replay too — the keys that are not
  `Ctrl+X`.
- **A held key.** With `KeyEventKind::Press`-only events there is no release to
  observe, so "press, wait 660 ms, press, release" and "hold for 665 ms" are
  the SAME event stream. Nothing downstream of the gap can separate them, so
  the gap itself has to exclude the auto-repeat delay: `CX_MIN_GAP` is 750 ms,
  clear of GNOME's 500 ms, KDE's 600 ms and X11 `xset`'s 660 ms. At any stock
  setting a hold's first repeat does not qualify, so no delete is ever
  scheduled and the release timing cannot matter. Holding `Ctrl+X` down stops
  one session and deletes nothing.

  Behind that, a qualifying second press still does not delete on the spot: it
  is SETTLED for `CX_SETTLE` (180 ms) and executed from the event-loop tick, and
  a further `Ctrl+X` inside that beat cancels it. That covers a repeat delay
  configured ABOVE `CX_MIN_GAP`, where the first repeat does qualify: the next
  repeat is 25–40 ms behind it and lands inside the settle.

  Stated residual: a repeat delay configured above 750 ms AND a release inside
  the first repeat interval (or a repeat rate below ~5.6 Hz, so the next repeat
  misses the settle). Both conditions must hold, and no stock configuration
  meets the first. The layers behind it still hold — the delete targets only the
  captured id, only while the cursor is still on it, only after re-validation
  against the current poll, and `claude rm` itself refuses a worktree with
  unpushed work (§8.2, above).

A press suppressed by `CX_MIN_GAP` is not silent: it flashes
`too fast — press Ctrl+X again` and asks for a redraw. Silence there was
indistinguishable from a wedged sidebar, because the press acted on nothing and
left whatever stale line was already on screen. While the window is open the
warning outranks the flash, so this is read exactly when there is no warning to
displace. The window is NOT consumed — a press inside the bar costs the operator
one keystroke, never the escalation.

#### The window on screen

While it is open the footer says
`Ctrl+X again: delete <label> and its worktree — cannot be undone`, in `p.red`.
It **outranks the flashed message and the poll error**, so nothing can push the
warning off screen while the verb is loaded, and it inherits §6.8's overflow
wrap, so at 34 columns it wraps rather than clipping. It is 70 columns — the
longest line ccmux produces, and the only one whose TAIL carries the
consequence — so the wrap has to work at every height, not only where there is
a detail block to wrap into. §6.8 AMENDMENT: below height 12, where `slots`
allocates no detail block, the wrapped message is carved out of the bottom of
the LIST instead (`overflow_rect`), capped so the header and at least one
session row always survive; the carve is transient and changes nothing about
`list_viewport_rows`, so the row-index-to-screen-line mapping underneath it is
untouched. Only a sidebar under 5 rows tall still loses the tail, which is
below the height at which the list itself is usable. It names the session
because the cursor is free to move while it is open. `u` undoes a dismissal
(§8.1), never a delete; the footer says so because there is no other way to
learn it in time.

#### The window closes on

two seconds passing (expired from the event-loop tick, so it closes with no key
pressed), `Esc`, `q`/`Ctrl-c`, a second press (whether it deleted or refused),
and any departure from `Mode::Normal` — `/`, `n`, `?`, `L`. The last is enforced
in one place, at the end of `on_key`, so no handler can forget it.

#### The confirm modal

`Mode::Confirm`, `Confirm::StopSession`, `CONFIRM_ARM_DELAY`, `key_confirm`,
`act_confirm_stop` and `draw_confirm` still exist and still behave exactly as
they always did — no default-affirmative, only a literal `y` confirms, a `y`
inside `CONFIRM_ARM_DELAY` is type-ahead and cancels, the modal captures the
short id at open. **No key path reaches any of it.** `act_request_stop` is its
only entry point and nothing calls it. It is retained deliberately for the next
verb that needs a modal; whether it is deleted is a separate decision.

Every other verb is non-destructive by construction and takes no confirmation:
`x` is proven safe (PROBE-FINDINGS §3: killing the pane leaves the agent
running), `q` only exits the sidebar process, `n` only creates, `d` only hides
and `u` puts it back.

### 8.3 `Enter` semantics

```
sel = selected_session() or return

if not sel.is_attachable():                   # no short id (§9.7)
    flash "no short id — cannot open this session" (Warn); return

if let Some(pane) = app.pane_of(sel.session_id):
    tmux::select_pane(session, pane)          # jump to the existing pane
    flash "jumped to pane <index>"
else:
    act_open(SplitDir::Vertical)              # identical to `o`
```

Jumping rather than re-splitting is a UX preference, not a correctness
requirement — double-attach is legal (PROBE-FINDINGS §3). If the operator wants
a second view of the same session, `o` and `s` always split unconditionally.

### 8.4 `o` / `s` — opening into a split

```
act_open(dir):
  1. degraded          -> flash "not inside tmux — open unavailable" (Warn); return
  2. sel = selected_session() or return
  3. !sel.is_attachable() -> flash "no short id — cannot open this session" (Warn); return
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
3. pane = app.pane_of(sel.session_id)
   none -> flash "not open" (Warn); return
4. pane is ANY tab's sidebar -> flash "refusing to close the sidebar" (Warn); return
5. tmux::kill_pane(session, pane)   [R2-gated; the session gate spans every window]
6. if the pane was in MY tab's map: map.remove(pane); map_dirty = true; flush
   otherwise write NOTHING — that window's option has one writer and it is not
   this process. Its owner reconciles the entry away on its next tick, and
   until then every process already hides it, because `open` is rebuilt as
   `union(maps) ∩ live panes` (§11.2).
7. tmux::pin_sidebar(...)
8. flash "closed pane <index>[ in tab <N>] — agent still running" (Info)
```

**`x` is unconditionally safe here, and only because of what is listed.**
PROBE-FINDINGS §3 verified that killing a pane leaves the agent running — *for
background sessions*, which are daemon-owned and whose pid is not a descendant
of any pane (§4 of the same document). Interactive sessions are the opposite:
they **are** descendants of their pane's process, so `kill-pane` would SIGHUP
Claude and end the session. Earlier drafts carried a refusal step for that case.
It is now **gone, not relaxed**: §3.5's `apply_poll` excludes interactive rows,
so no such row can ever be selected, and the refusal had nothing left to fire
on. This keeps `Ctrl+X` the only destructive binding in v1 (§8.2).

Step 8's wording matters: it is the operator-facing statement of
PROBE-FINDINGS §3, shown every time, so nobody ever confuses `x` with `Ctrl+X`.

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

**Removed.** The `c` binding, `Mode::Prompt(NewInteractive)`, and
`agents::interactive_pane_cmd` are gone. ccmux does not start interactive
sessions because it does not list them (§3.5, `apply_poll`): `c` would have
opened a pane and then had no row to show for it. Start interactive Claude in a
tmux pane the ordinary way.

The section number is kept so §8.8 and §8.9 do not move.

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

### 8.10 `t` — open in a new tab

```
1. degraded -> flash "not inside tmux — tabs unavailable" (Warn); return
2. sel = selected_session() or return
3. !sel.is_attachable() -> flash "no short id — cannot open this session" (Warn); return
4. no sidebar command -> flash "sidebar command unknown — cannot open a tab" (Error); return
5. (win, index, claude) = tmux::new_tab(session, agents::attach_pane_cmd(short_id))
6. tmux::write_tab_map_uncached(session, claude, {claude -> PaneEntry})   [R2-gated]
7. sidebar = tmux::split_left_of(session, claude, <SIDEBAR_CMD>)          [R2-gated]
   tmux::set_tab_sidebar(session, sidebar)                                [R2-gated]
   tmux::pin_sidebar(session, sidebar, requested_width())
8. tmux::select_pane(session, claude)                                    [R2-gated]
9. refresh_panes; pin_sidebar; flash "opened <name> in tab <index>" (Info)
```

Guards 1-3 mirror `o`/`s` exactly, and all four refusals happen before any tmux
command is issued.

**Steps 5-7 are ordered by necessity, not by style.** The Claude pane is created
FIRST, as the new window's only pane, and that window's `@ccmux_tab_map` is
written through it before the sidebar exists. Create the sidebar first and its
process is already running when the map is written, so its own next flush —
built from the map it loaded BEFORE that write — clobbers the entry, orphaning a
Claude pane from every map permanently and leaving it unclosable by `x`. This
ordering makes the write provably precede the existence of any process in that
window, which is what keeps `@ccmux_tab_map` a single-writer option (§11.1).

**`t` writes `@ccmux_tab_sidebar` itself, in step 7, the moment the split
returns.** It used to leave that to the new process, which registers itself from
`$TMUX_PANE` on its first tick 30-40 ms later — and in that gap the window was
identifiable as a ccmux tab (its map was already seeded) while carrying no
marker, so a launcher run in the same instant split a SECOND sidebar into it.
Two ccmux processes then wrote one window's `@ccmux_tab_map` and
`@ccmux_tab_hidden`, the exact state §11.1 exists to make unrepresentable: they
render permanently different session lists, `adopt_own_state` runs once so
neither re-reads, and the in-process `LAST_SAVED` cache suppresses the re-write
that might have converged them. It never self-heals.

The gap is now closed from BOTH ends, and the second end closes it by
construction: `@ccmux_tab_sidebar` is the launcher's sole test for "this window
is a ccmux tab" (§1.2 step 5b), so at every instant of `t` the new window is
either unidentifiable — bare, then map-seeded-but-unmarked — and skipped, or
marked and merely re-pinned. There is no third state. Writing the marker from
`t` is safe under §11.1's own rule for this key: nothing about the window is
read to compute the value, which is simply the id of the pane the call just
created for the purpose, so `t` and that process compute the same answer.

The price is deliberate: a window whose `t` failed at step 7's split keeps its
seeded map and will never be healed by a later launch. The seed stays anyway —
it is what keeps that live, attached Claude pane visible to `Enter` and closable
by `x` from every other tab.

Step 8 moves the client, and it is the ONLY thing that does. It rides on the
existing R2-gated `select_pane`, which has always issued `select-window -t
<pane>` before `select-pane`, so no new capability is added. Focus lands on the
Claude pane, matching `Enter`'s jump — with `t` the operator is leaving for the
session they asked for, where `o`/`s` keep them driving the list.

There is deliberately **no close-tab verb and no `kill-window` anywhere in the
crate**. A tab ends when its last pane does, which tmux already handles, and
every pane death goes through the R2-gated `kill_pane`. There is likewise no
tab-cycling key: `Enter` on a row in another tab already switches, and tmux's
own bindings remain.

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
- Verbs that need fresh data (`Enter`, `o`, `s`, `Ctrl-x`) still act on the
  stale list. `Ctrl-x`'s delete re-validates the CAPTURED short id against
  `app.sessions` (§8.2), so a stale entry fails closed with
  `session <id> is gone — not deleted`.

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

`n` remains available with an empty list (it falls back to `$PWD`), so the
operator can bootstrap from zero sessions.

### 9.4 A session vanishes between poll and open

Unavoidable: the poll is up to 2.5 s stale.

- **`Enter`/`o`/`s`** — ccmux splits the pane anyway; `claude attach <id>` fails
  inside it. The pane command's trailer keeps the pane alive with a readable
  message:
  `[ccmux] session exited (rc=1). press enter to close pane.`
  The map entry stays (§5.3) so `x` still closes it. ccmux does not pre-validate
  by polling again — the race is unclosable and the readable pane is the
  correct handling.
- **`Ctrl-x`** — the first press may find the session already gone; `claude
  stop` fails and its stderr surfaces via `stop failed: <msg>`, and a failed
  stop does **not** open the delete window. The second press re-validates the
  captured id against `app.sessions` before `claude rm` and fails closed with
  `session <id> is gone — not deleted`.
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
    (dispatch is pure `claude --bg`, no tmux involved), `Ctrl-x` (pure
    `claude stop` / `claude rm`).
  - Refuses with `flash("not inside tmux — <verb> unavailable", Warn)`:
    `Enter`, `o`, `s`, `x`.
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

### 9.7 Sessions with `id` absent

`Session::is_attachable()` is `false` and gates every verb that needs the 8-hex
short id. Interactive sessions never have one — but they are never listed
(§3.5, `apply_poll`), so they are not what this gate is for any more. It is
still live: `parse_sessions` honours an explicit `kind: "background"` even when
the CLI omits `id`, so a **listed** row can reach it.

| Verb | Behaviour with no short id |
|---|---|
| `Enter` / `o` / `s` | refused: `no short id — cannot open this session`. There is no `claude attach` without one, so a split would have nothing to run |
| `x` | unaffected — it targets the *pane*, and `pane_of` is keyed on `session_id`, not on the short id |
| `Ctrl-x` | refused: `no short id — cannot stop this session` (§8.2 step 1). No window opens, so the delete is unreachable too |
| `L` | refused: `no short id — no logs for this session` |
| grouping | unchanged: `group()` reads `state`/`status`, never `id` |
| rendering | detail block line 2 reads `id      —  background  <status>` (§6.5) |

### 9.8 Other edges, each with a pinned answer

| Edge | Handling |
|---|---|
| The user manually splits a pane inside `ccmux` with tmux keys | The pane is unmapped; ccmux ignores it. The tick re-pin keeps the sidebar at its width. Never adopted, never killed. |
| The user resizes the sidebar with tmux keys | Reverted within one tick by the unconditional `pin_sidebar`. To change it for real, restart with `--width`. |
| `@ccmux_map` holds a pane id that no longer exists | Dropped by `reconcile` on the first tick. |
| `@ccmux_map` is corrupt, empty, or `v != 1` | `load_map` returns `PaneMap::new()`. Never an error, never a crash. |
| `@ccmux_tab_sidebar` is missing or stale | The sidebar re-registers itself from `$TMUX_PANE`; if that stays `None`, `pin_sidebar` no-ops and `split_anchor` falls back to `leftmost_pane`. |
| `list-windows` fails on the tick that first resolves identity | `refresh_panes` swallows it, so `tabs` stays empty while `own_window` resolves from the successful `list-panes`. `adopt_own_state` does NOT latch on an enumeration that does not carry my window, and no flush is issued from an un-adopted base — the dirty flags are kept, not cleared, and the write happens on the tick adoption lands. Adoption then MERGES the stored state with anything pressed in between, so neither side is lost. |
| A tab's sidebar was quit with `q` while its Claude panes live on | That tab is sidebar-less until the next `ccmux` launch heals it (§1.2 step 5b). Its `@ccmux_tab_map` goes unreconciled meanwhile — invisible, because every reader intersects it with the live pane list. Re-running `ccmux` from INSIDE the session is still the §9.6 no-op, so healing means detaching and relaunching, or driving from another tab's sidebar in the meantime. |
| A tab's last pane dies | tmux destroys the window and its window options with it. Its map described panes that died with it, so there is nothing to preserve; its dismissal fragment is adopted by the lowest live tab (§11.3). |
| Every sidebar is gone when a tab closes | Nobody adopts the orphaned fragment, so the rows that tab dismissed reappear. Nothing else breaks and `d` restores them. This is the one residual hole in §11.3, and it is a deliberate ceiling on how much machinery a view filter is worth. |
| A session is open in two tabs at once | The badge, `Enter` and `x` all name the pane in the ASKING sidebar's own tab, falling back to the lowest-numbered pane when it has none there (§5.5). So each tab's badge is blank while its own pane is on screen, and `x` never kills a pane in a window the operator is not looking at. The other pane is reachable by switching to its tab. Double-attach is legal (PROBE-FINDINGS §3). |
| The binary is rebuilt and `t` pressed | The new tab runs the NEW build beside OLD sidebars in the existing tabs. A fragment carrying a schema version a build does not recognise reads as empty rather than being rewritten, so the two cannot flap an option between shapes. |
| The sidebar pane is killed by the user | The ccmux window keeps its Claude panes. Re-running `ccmux` heals it via §1.2 step 5b. |
| The window shrinks below `sidebar_width` columns | tmux clamps `resize-pane`; the sidebar renders per §6.4's `< 6` rule and does not panic. |
| Two `ccmux sidebar` processes in one tmux session | EXPECTED now, one per tab, and safe by construction: each writes only its OWN window's options (§11.1). Two in the SAME window is still pathological — the second adopts the first's marker rather than stealing it, so it pins and anchors against the real sidebar. |
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
- `PaneMap::reconcile` drops absent panes, keeps present ones, returns the right
  `changed` flag; round-trips through serde byte-identically.
- `SplitDir::Vertical.tmux_flag() == "-h"`, `Horizontal → "-v"`.

**agents.rs (Agents)**
- `attach_pane_cmd("1c45d64f")` equals the §3.3 template exactly.
- `strip_ansi` removes CSI/OSC/two-char escapes and keeps `\n`; a raw
  alt-screen preamble (`\x1b[?1049h\x1b[H\x1b[2J`) strips to empty.

**ui.rs (UI)**
- `ui::draw` into a `TestBackend` at **20x8, 30x24, 34x76, 40x76, 5x3, 1x1, and
  0x0** does not panic, in every `Mode`, with 0 sessions and with 40.
- `list_viewport_rows(0..=12)` matches the §6.1 table.

**app.rs (Integrator)**
- `apply_poll` on a payload containing a `kind: "interactive"` row leaves that
  row out of `app.sessions` — the §3.5 listing policy, and the test that stops
  someone "simplifying" the filter away.
- `Ctrl-x` on a row with no short id leaves `mode == Normal`, sets a Warn
  message, opens no delete window, and spawns nothing.
- `on_key('c')` is inert: no mode change, no prompt, no message (§8.7).
- `pane_of` resolves a session through `map` and returns `None` for one that is
  not in it.
- `Ctrl-x` is reached through `on_key` — proving it sits ABOVE `key_normal`'s
  `_ if ctrl => Action::None` catch-all, below which any Ctrl arm is dead.
- `Ctrl-x` captures the short id: mutate `app.sessions` between the two presses
  and the captured id is still the one deleted.
- A burst of `Ctrl-x` in one instant stops once and deletes nothing; a held key
  (a qualifying press followed by a repeat stream) deletes nothing.
- The delete window does not survive `/`, `n`, `?`, `q`, `Esc`, a moved cursor,
  or two seconds passing.
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

## 11. Tabs — per-window state and its concurrency model

A **tab** is a tmux window of ccmux's own session, and **every tab carries its
own pinned sidebar pane running its own ccmux process**. That is a deliberate
trade: N processes each polling `claude agents --json`, bought so the session
list is on screen wherever the operator is. No threads, no shared daemon, no
change to the poll interval.

N sidebars means N concurrent writers, and every option §5.2 described was
session-scoped, written with `set-option -t <session>`. The in-process
write-dedupe cache is a `static` and gives ZERO protection across processes.
Measured on tmux 3.4: two concurrent read-modify-writes of one session option
left `{"v":1,"ids":["A"]}` — the other writer's dismissal silently gone.

### 11.1 The invariant

**No option is ever read-modify-written, and no option has two writers that
could disagree.** The key space is partitioned so a collision is not
representable, rather than made unlikely.

| option | scope | writer | readers |
|---|---|---|---|
| `@ccmux_tab_map` | **window** | that window's own sidebar, addressed through its own pane. One documented exception: `t`'s pre-write, provably ordered before that window has a process (§8.10). | every sidebar, as a union |
| `@ccmux_tab_hidden` | **window** | that window's own sidebar | every sidebar, as a fold |
| `@ccmux_tab_sidebar` | **window** | the launcher's heal, `t` (§8.10 step 7), AND that window's own sidebar — the only multi-writer key, and safely so: all three write the same function of ground truth, the pane id of the process that IS that window's sidebar. Nobody reads the value to compute the value, so there is no read-modify-write to lose and every writer converges. | every sidebar, the launcher |
| `@ccmux_width` | session | the launcher only | every sidebar, every tick |
| `@ccmux_map` | session | `configure_session` once, and the one-time migration | `main::run_launcher`'s ownership guard |
| `@ccmux_hidden` | session | LEGACY — read once by the migration, then cleared to `""` | migration only |

Two sidebars acting in the same tick therefore issue `set-option` against
DIFFERENT options. The tmux server executes commands serially and each
`set-option` is atomic, so both land.

The per-tab options carry NEW names because `#{@name}` format expansion falls
back window -> session -> global (Appendix A): a leftover session value under a
reused name would be reported as every window's value. The same inheritance is
what lets `@ccmux_width` ride along free in the `list_tabs` window read, which
is why a steady tick costs exactly what it cost before tabs — the window read
REPLACES the per-tick `show-options @ccmux_width`.

### 11.2 `@ccmux_tab_map` — conflict-free by disjoint key spaces

A pane belongs to exactly one window, so the union of all windows' maps is a
disjoint union. `App::open` is rebuilt every refresh as
`union(every tab's map) ∩ live panes`, and that intersection is what makes a
cross-tab `x` correct in the same frame it happens: the killer writes nothing of
the other window's option, and every process — killer, owner, and any third tab
— already hides the entry, because the map is a cache over ground truth only
tmux can supply, and everything derivable from tmux is re-derived every tick.

A window that dies takes a map describing panes that died with it: nothing to
preserve. A window whose sidebar was quit keeps an unreconciled map: invisible,
by the same intersection, and cleaned by the sidebar the launcher heals in.

The write-dedupe cache key gains the WINDOW (`"{key} {session} {window}"`), and
that is load-bearing: one process can write two windows' copies of the same
option — the `t` path — and without the window one window's value would suppress
the other's write inside its own cache. `t`'s write is additionally uncached, so
a one-shot foreign-window write can never seed an entry the owner would trust.
The cache hit-check runs BEFORE the R2 gate, deliberately: a hit means no write,
so there is nothing to gate and no tmux spawn — which is what keeps the unit
suite's seeding seam hermetic now that writes are pane-targeted.

### 11.3 `@ccmux_tab_hidden` — a shared set, still one writer per option

A dismissal is about the SESSION LIST, which is the same list in every tab, so
its effect must be session-wide. Session-wide and single-writer are reconciled
by making each window's option an op-log FRAGMENT and the shared set a pure fold
of every fragment.

**Fold.** Group ops by id; the winner for an id is the op with the greatest
`(seq, org)`. The hidden set is the ids whose winner has `add == true`, ordered
by winning stamp ascending — exactly `HiddenSet`'s existing "oldest first"
contract, so `model::build_rows` and every dismissal test are unaffected and
`HiddenSet::undo` still pops the newest. It is a last-writer-wins register per
id keyed by a total order: a standard convergent structure, not an ad-hoc merge.
`org` is `WindowId::num()`, the parsed integer — not the string, because
`"@9" > "@10"` lexicographically is the same trap `PaneId::num` documents.

**Clock.** `seq = max(now_ms, seq_seen + 1)`, a Lamport clock over a wall clock
on one host, monotonic per process even across a backward NTP step.

**`d`** appends `{id, add: true, …}` to my fragment and applies it locally.
**`u`** resolves the newest dismissal from the fold — by IDENTITY, never by
position — and appends a tombstone `{id, add: false, …}`. Keyed removal commutes
with another process's append, so no intent is lost and no reordering can
misapply an undo. A tombstone's stamp is strictly greater than the dismissal it
targets — guaranteed, not hoped: that dismissal came out of the fold `seq_seen`
was just computed from.

Both stay in memory: `d` and `u` still issue **no tmux command at all**, and the
write is deferred to the next tick or to `shutdown`, exactly as before. This is
a constraint, not an accident — a pane-targeted write drags `assert_in_session`'s
`list-panes` into the keypress path, and the unit suite must never reach a tmux
server, because the default socket is the operator's live one.

The fold runs over every other tab's STORED fragment plus my own IN-MEMORY log,
never my stored copy: I am the authority on my fragment, and folding a read
taken before my own last write would flicker a dismissal back onto the screen.

**Same-tick cases.** Different rows in different tabs: two fragments, both
survive. The same row in both: identical effect either way. A dismisses X while
B undoes X: the later stamp wins, and a true same-millisecond tie breaks on the
origin window, identically for every reader. Convergence follows from the fold
being a pure function of the multiset of ops, and every process reading every
fragment each tick.

**The one visible anomaly**, stated plainly: press `u` in tab A, switch to tab B
and press `u` again within one poll interval, and B — which has not yet seen A's
tombstone — undoes the same dismissal. That is idempotent on convergence (two
tombstones for one id, the later wins, the row is restored exactly once).
Nothing is corrupted and no dismissal is lost; press `u` again for the next one.

**Adoption.** A window option dies with its window, so a closed tab would take
every dismissal it made with it. Each sidebar keeps the previous tick's
fragments; a window id that disappears is orphaned, and the adopter — the live
tab with the lowest `WindowId::num()`, where LIVE means its marker names a pane
that still exists — merges the orphan's ops into its own VERBATIM. Ops carry
their own stamp and origin, so adoption changes only where an op is stored and
nothing about the fold, and a race between two would-be adopters resolves to a
byte-identical duplicate that `push` drops. If EVERY sidebar is gone when a
window dies, nobody adopts and those rows reappear — the residual hole in §9.8.

**Pruning**, each process only ever on its own fragment: compact to my
highest-ranked op per id; drop mine when another fragment holds a strictly
higher-ranked op on that id; drop my tombstone once no other fragment holds any
op on that id at all, since there is then nothing left for it to suppress. A
settled disagreement garbage-collects to empty in two ticks. The two-strike
absence rule retires the OPS as well as the folded view — a surviving op would
re-hide the id at the next fold — and a per-fragment cap bounds a pathological
tab, failing in `HIDDEN_MAX`'s safe direction: the row reappears.

**Migration.** A session written by a build with no tabs is imported once: the
legacy `@ccmux_map` entries for panes in this window become this tab's map and
`@ccmux_map` is reset to `EMPTY_MAP_JSON` — not unset, because the ownership
guard reads its presence — and the legacy `@ccmux_hidden` ids become dismissal
ops with tiny stamps, after which that option is cleared to `""` to mark it
consumed.

### 11.4 Blast radius (RULE R1-R4 under tabs)

Tabs add three would-be capabilities, each closed off by construction:

1. **`new-window`** — the target is built by `session_target` ALONE. There is no
   parameter through which a window target can be passed, and `new-window`
   refuses a pane target outright, so the call is structurally incapable of
   naming a window or a session that is not ccmux's own.
2. **Window-option writes** — addressed by an `assert_in_session`-gated `PaneId`,
   never by a window id. `set-option -w -t %N` resolves to %N's window and a
   stale pane fails loudly; the only thing it could otherwise reach is a pane in
   another session, which the R2 gate has always blocked. **Window-id targets
   are banned outright**: a bare `-t '@2'` reaches a foreign session's window and
   a session-qualified `-t '=ccmux:@99'` silently retargets the current one, so
   `WindowId` is an in-process identity and is never rendered into a `-t`
   argument. A grep for `-t` next to a `WindowId` must return nothing — that is
   the invariant a reviewer can check mechanically.
3. **`select-window`** — no new helper. `t` and `Enter` both go through the
   existing R2-gated `select_pane`, which issues `select-window -t <pane>` on a
   pane already proven in-session, which proves the window it names is too.

`list_panes_in_session` keeps `-s` and already spans every window of ccmux's
session, never the server. `list_tabs` is a read and is session-targeted with
the same exact-match form. `kill-window` never enters the crate.

### 11.5 What the sidebar shows

Two places, neither of which adds a row, and both needed because ccmux runs its
session with `status off` — tmux's own window list is not on screen, so the
sidebar is the ONLY place tab identity can appear.

- **Gutter column 2** is the tab a session's pane lives in, inked only when that
  is not the tab you are looking at: `▌ ` = open here, `▌5` = open over in tab
  5, `  ` = not open. The badge used to be `#{pane_index}`, which is per-window
  and so named nothing once "pane 2" existed in every tab; the actionable
  question became "where is this", and the answer is the tab. Blank for the
  current tab is the other half: a digit that could mean "here" would have to be
  read against a tab number the operator must remember, whereas with this rule a
  digit ALWAYS means "somewhere else", and a single-tab session renders exactly
  the ink it rendered before. The exact pane index survives where it is
  actionable — the `opened <name> in pane N` flash.
- **A header chip `tab N`**, rendered only from two ccmux tabs up, so all tab UI
  is invisible until there is a second tab to name. `N` is tmux's own
  `#{window_index}`, so `prefix-3` goes exactly where a `tab 3` chip — or a `3`
  badge — points. There is deliberately **no denominator**: a window INDEX and a
  window COUNT agree only while the indices happen to be a contiguous `1..M`,
  and tmux's `renumber-windows` defaults to OFF (`configure_session` never turns
  it on), so closing a middle tab left a permanent gap and the chip rendered
  `tab 4/2` — tab four of two. A count cannot be reconciled with an index; the
  index is the actionable half, so the count is what went. The two-tab gate
  counts windows carrying `@ccmux_tab_sidebar`, the same sole criterion §1.2
  step 5b uses — not windows that merely have panes, or the operator's own bare
  `prefix-c` window summons a chip and inflates it while heal correctly refuses
  to treat it as a tab.

**The gutter does not widen.** `GUTTER` stays 4 and the name column keeps its
fixed left edge on column 5: at W=34 the name budget is
`34 - 4 - (1 + 3) - 1 = 25`, unchanged. A two-column badge would push this row's
glyph and name right while its neighbours stayed put, and the eye reads a broken
left edge as broken far faster than a short name. A window index of ten or more
clamps to `+`, the same rule and the same reason as before. Because
`renumber-windows` is OFF, indices are not a contiguous `1..M`: `+` needs ten
windows to have EXISTED at once, not ten to be alive now.

A session open in two tabs shows the tab `Enter` would take you to, which is also
the one `x` acts on — this sidebar's own tab whenever the session has a pane
there (§5.5), so the badge is blank exactly when the pane is on screen in front
of you. The digit therefore never lies about where the verbs go.

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
| `set-option -w -t <PANE>` writes the WINDOW option of that pane's window, and a stale pane target fails LOUDLY. | On tmux 3.4: only that pane's window received the value; `-t '%999'` gave `no such window: %999`, exit 1, nothing written. |
| A window-id target has a silent-mistarget mode a pane target does not. | Bare `-t '@2'` wrote into a FOREIGN session's window; `-t '=ccmux:@99'` returned exit 0 and read/wrote ccmux's CURRENT window. Only a bad *index* errors. Hence: window ids are an identity in this crate, never a target. |
| `show-options -w -qv` does NOT inherit, but `#{@name}` format expansion DOES fall back window -> session -> global. | With a value set at both levels, `list-windows -F '#{@k}'` reported the window value for the marked window and the SESSION value for the unmarked ones; `show-options -w -qv` reported empty for the unmarked ones. This is why the per-tab options carry new names, and why `@ccmux_width` rides along free in the window read. |
| tmux never re-expands format sequences inside an option value. | A value containing `#{window_index}`, `##`, `#H`, `"` and `\` came back byte-identical through `#{@opt}`. JSON is safe to carry in a format string. |
| Window options die with their window; window ids are never reused; `renumber-windows on` shifts window INDICES when a tab closes. | Killed a window's last pane: the window and its `@ccmux_tab_hidden` were gone, and the window above it moved from index 3 to 2 while keeping id `@4`. |
| Two concurrent read-modify-writes of ONE session option silently lose a write; the same two writers against TWO window options both land. | Ran both pairs on a throwaway socket: the session option ended `{"ids":["B"]}` with A's value gone; the two window options held `["A"]` and `["B"]`. This experiment is the entire argument for §11. |
| `$TMUX_PANE` is set in a pane's process, so a sidebar can identify itself with no tmux call. | A pane launched by `new-window -- …` reported `PANE=%2`. |
| `select-window -t <PANE>` moves the session's current window. | Exit 0, and `#{window_active}` moved to that pane's window — which is what makes cross-tab `Enter` work through the existing `select_pane`. |
| `new-window -t '=<sess>:'` is exact-match and appends. | With `probe` and `probedecoy` alive, it created only in `probe`; the decoy kept its single window. |
| `display-message -p '#{@key}'` also reads user options and returns empty for unset. | Alternative reader; `show-options -qv` is the one specified. |
| `has-session -t <name>` → exit 0 when present, exit 1 (`can't find session`) when absent. | Basis of §1.2 step 3. |
| `new-session -P -F '#{pane_id}'` prints the new pane id directly. | No `list-panes \| head -1` needed. |
| `-n cc` names the window; `<sess>:cc` addresses it. | §1.2. |
| `split-window -h -t <p>` then `resize-pane -t <sidebar> -x 34` keeps the sidebar leftmost, full height, exactly 34 cols — across three nested splits and a mid-window `kill-pane`. | Full geometry trace in §1.3. |
| `resize-pane -x` on a **lone** pane is a no-op returning exit **0**. | Makes the unconditional per-tick re-pin safe. |
| `split-window -h -b -t <leftmost>` inserts a pane to the **left** and it becomes `pane_index 1`. | Basis of sidebar healing, §1.2 step 5b. |
| `--` is accepted before the shell-command by both `new-session` and `split-window`, and guards a command starting with `-`. | Basis of RULE Q3. |
| `set-option -t <sess> status off` works per-session. | §1.2 step 4e. |
| `/proc/<pid>/stat` gives `comm` parenthesized in field 2; ppid is the 2nd whitespace token after the **last** `)`. | Confirmed on a live shell whose ppid was the `kind:"interactive"` pid reported by `claude agents --json`. **Nothing consumes this any more** — §5.4 and the whole /proc walk were removed. |
| `claude agents --json` output matches PROBE-FINDINGS §1 exactly on the current build (2.1.245), including `id`/`state` absent for `kind:"interactive"`. | Re-read during authoring. |
| `claude --bg, --background` takes the prompt as a **positional** argument. | `claude --help`. Basis of RULE Q4. |

### A.1 A cautionary note that became RULE R1

While probing, a shell variable holding a pane target evaluated to the empty
string. `tmux split-window -t "" ...` did **not** error — tmux silently fell back
to the caller's current pane and created panes in an unrelated live session.
That is the entire reason `PaneId` is a validated newtype, mutating helpers take
mandatory targets, and `assert_in_session` gates every mutation (§2). An
`Option<PaneId>` reaching a tmux mutation is a bug even when it happens to work.

---

## Appendix B — v1 amendments (adversarial review)

Each entry amends the section named, was found by empirical reproduction against
a throwaway `-L ccmux` server, and is implemented. Where the spec pinned the old
behaviour, the pin is superseded by this appendix.

| Amends | Was | Is |
|---|---|---|
| §8.2 | `y` confirms whenever the modal is open. | `y` is ignored for 250 ms after the modal opens (`CONFIRM_ARM_DELAY`), and the event loop drains queued input the instant the modal is drawn. Type-ahead — a paste, or `Sync` typed without `/` — cannot stop an agent. Now dormant: no key opens the modal. |
| §8.2 | `S` stops the selected session behind a `y`/`n` modal, and there is no way to delete one. | `Ctrl+X` stops it immediately with no modal, and a second `Ctrl+X` within two seconds deletes it and its worktree via `claude rm` (PROBE-FINDINGS §2). `S` is unbound as a verb and only says where stop went. The modal is gone from the key paths, not from the crate. |
| §8.2 | `CX_MIN_GAP` is 250 ms and every press stamps it on entry, so "a buffered burst of any length performs exactly one stop" and "holding `Ctrl+X` down deletes nothing". | Both were falsified live. The entry-only stamp measured DEQUEUE time, so the ~0.85 s the first press spent blocked inside `claude stop` plus the forced refresh became the gap: two `Ctrl+X` events ~100 ms apart deleted a session and its worktree. Presses now re-stamp when the handler returns, `run_delete` re-stamps after its own shell-out, and `main.rs` drains the tty after any keypress that blocked over `SLOW_KEY` (300 ms). And 250 ms was under every stock auto-repeat delay, so a hold released inside its first repeat interval deleted; `CX_MIN_GAP` is now 750 ms, clear of GNOME 500 / KDE 600 / X11 660. |
| §8.2 | A press suppressed by `CX_MIN_GAP` returns `Action::None` and draws nothing. | It flashes `too fast — press Ctrl+X again` and asks for a redraw. Silence was indistinguishable from a wedged sidebar: the press acted on nothing and left the previous line on screen. The window is not consumed. |
| §8.2 | A refused second press always flashes `moved off <label> — nothing deleted`. | It says `<label> left the list — nothing deleted` when the captured row is no longer in the list at all — `a` hiding Completed, or a `/` filter written against the worktree path that `claude stop` reverts. The refusal is unchanged and still fails safe; only the cause it names is. |
| §6.8, §8.2 | The overflow wrap needs the detail block, which exists only at height >= 12. | Below 12 the wrapped message is carved out of the bottom of the list (`overflow_rect`), capped so the header and one session row survive. `Ctrl+X`'s armed warning is 70 columns and the only line whose tail carries the consequence; a 34x10 sidebar was clipping it to `Ctrl+X again: delete rv burst tas…`. |
| §8.9 | Paste arrives as key events and runs the keymap. | Bracketed paste is enabled for the sidebar's lifetime. `Normal` discards a paste; `Filter` and `Prompt` take it as literal text with control characters stripped. |
| §1.2 step 5 | Any tmux session with the configured name is healed. | Healing requires `@ccmux_map` to be set, which only `configure_session` writes. A foreign session with a colliding name is refused, never split or resized. |
| §8.4 | The split anchor is chosen from the session-wide pane list. | Anchor selection is scoped to the sidebar's `#{window_index}`. `pane_left`, `pane_index` and `pane_active` are per-window, so the unscoped choice could open Claude in a window the operator is not looking at. `list_panes_in_session` stays session-scoped for `PaneMap::reconcile`. |
| §1.2 step 5b | The sidebar is re-inserted left of the session's leftmost pane. | Left of the leftmost pane **of the window that will host it** — the `cc` window by name, else the lowest window index. |
| §9.6, §1.2 step 6 | `inside_tmux()` decides "already inside" and `switch-client` vs `attach-session`. | `inside_target_server()` decides both: `$TMUX`'s socket path compared against the target server's own `#{socket_path}`. Under `--socket` the two disagree, which made the launcher either fail on `switch-client` or report success without creating anything. |
| §1.2 step 5 | `@ccmux_width` is written once at creation and never read. | It is the source of truth. `heal_sidebar` writes it; the running sidebar re-reads it each tick, so a relaunch with a new `--width` takes effect instead of being reverted. |
| §1.3, §9.8 | The per-tick re-pin is unconditional; "tmux clamps `resize-pane`". | tmux does not clamp — it takes the columns from the other panes, and a 30-column window left a Claude pane at 1 column. The pin is bounded by the window (`MIN_CONTENT_COLS = 20`) and skipped while the sidebar is alone in its window. |
| §4.2 | `agents::poll()` is synchronous and unbounded. | Still synchronous and thread-free, now bounded by `POLL_TIMEOUT = 5s`; the child is killed and reaped on expiry and the timeout surfaces as §9.1's red indicator plus `agents: timed out after 5s`. A `tick()` slower than 1 s also drains buffered input, so keys typed at a frozen UI are not replayed against it. |
| §6.6 | Row budgets count chars; the overflow is "cosmetic". | Budgets count display columns (`model::display_width`). A CJK session name was deleting the age column, the pane badge and the selection bar, not merely overflowing. |
| §8.9 | `help_scroll` caps at 64; the logs scroll caps at `len - 1`. | Both clamp to `len - overlay_viewport`, written by `main.rs` each frame, so `k` after `G` always moves the view. |
| §6.8 | The footer truncates to one line. | A message wider than the sidebar takes over the detail block and wraps. §8.5's "agent still running" wording and §9.7's `no short id — …` refusals do not fit 34 columns. |
| §8.8, §8.1 | `Esc` in Normal quits when no filter is set. | `Esc` clears the filter and is otherwise inert; `q` and `Ctrl-c` remain the quit keys. |
| §6.2 | The header shows `matching/total` only while `/` is active. | It shows the ratio whenever fewer than all sessions are listed, so `a` (hide Completed) cannot leave the header contradicting the list. |
| §6.5 | The detail block renders `Status::Unknown("")` as `?`. | A `state: "done"` session renders `done`, matching `status_glyph`. The live payload omits `status` on almost every completed row. |
| §5.5 | `PaneEntry::name` is stored verbatim. | Truncated to 80 columns, and a failed `@ccmux_map` write is flashed once and clears `map_dirty` instead of being reissued every tick. tmux rejects a `set-option` value over ~16 KB. |
