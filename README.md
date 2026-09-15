# ccmux

A tmux-backed, neovim-style frontend for Claude Code background sessions.

`claude agents` gives you a fleet view. ccmux gives you a workspace: a session
list pinned to the left of a tmux window, with the real Claude Code TUIs open in
splits to its right. tmux keeps everything alive across detaches and SSH drops.

```
┌──────────────────┬───────────────────────┐
│ ccmux  5       ● │                       │
│── Blocked (1) ───│                       │
│  ▲ review auth pr│                       │
│                  │                       │
│── Working (2) ───│   live Claude TUI     │
│▌ ● api/refactor  │                       │
│  ◐ flaky test    │   ✶ Refactoring… 3m   │
│                  │   >                   │
│── Idle (1) ──────│                       │
│  ○ docs/readme   │                       │
│                  │                       │
│── Completed ─────│                       │
│  ✓ bump deps     │                       │
├──────────────────┤                       │
│ name  api/refac… │                       │
│ id    3f9a07c2   │                       │
│ cwd   ~/src/api  │                       │
│ j/k move ⏎ open  │                       │
└──────────────────┴───────────────────────┘
```

## Install

```sh
cargo install --path .
```

Requires `tmux` and the `claude` CLI on `PATH`, and Rust with edition 2024.

## Launch

```sh
ccmux                     # create or attach the `ccmux` tmux session
ccmux --session work      # a differently named session
ccmux --width 40 --dark   # wider sidebar, dark palette
```

Running `ccmux` again from another terminal attaches the same session. If a
sidebar was quit with `q`, running `ccmux` puts it back.

| Flag | Default | Meaning |
|---|---|---|
| `--session <NAME>` | `ccmux` | tmux session name, `[A-Za-z0-9_-]{1,64}` |
| `--width <COLS>` | `34` | Sidebar width, clamped to `20..=120` |
| `--light` / `--dark` | light | Palette. Not auto-detected |
| `-L`, `--socket <NAME>` | tmux default | Use a separate tmux server |
| `--interval <MS>` | `2500` | Poll interval, for `ccmux sidebar` only |

The palette can also be set with `CCMUX_THEME=dark`. A flag wins over the
variable. `CCMUX_CLAUDE_BIN` overrides the `claude` binary, and
`CCMUX_TMUX_SOCKET` is the variable form of `--socket`.

## Keys

| Key | Action |
|---|---|
| `j` / `k`, arrows | Next / previous session |
| `g` / `G` | First / last session |
| `Ctrl-d` / `Ctrl-u` | Half a page down / up |
| `Tab` / `Shift-Tab` | Next / previous group |
| `Enter` | Jump to the session's pane, or open it in a vertical split |
| `o` / `s` | Open in a side-by-side / stacked split |
| `t` | Open in a new tab |
| `x` | Close the session's pane. The agent keeps running |
| `Ctrl-x` | Stop the session. Press again within 2 s to **delete** it |
| `n` | Dispatch a new background session |
| `L` | Show the session's logs |
| `d` / `u` | Hide the row from the list / undo the last hide |
| `/` | Filter by name, cwd or short id |
| `a` | Show or hide the Completed group |
| `r` | Refresh, and re-assert the layout |
| `R` | Restart ccmux and idle agents after an upgrade |
| `?` | Help |
| `q` | Quit the sidebar. Sessions and panes are untouched |
| `Esc` | Cancel a pending delete, or clear the filter |
| `Ctrl-c` | Quit from any mode |

`o` and `s` follow vim's naming: `o` puts panes side by side, `s` stacks them.

Inside the overlays, `j`/`k`, `Ctrl-d`/`Ctrl-u` and `g`/`G` scroll. In the
`n` prompt, `Tab` moves between the task and cwd fields and `Enter` submits.
Pasted text is never run as keys.

## The list

Only background sessions are listed: those started with `claude --bg` or `n`.
Interactive sessions cannot be attached into a split, so they are left out.

Groups run **Blocked**, **Working**, **Idle**, **Completed**.

| Glyph | Meaning |
|---|---|
| `▲` yellow | Blocked on a permission prompt or question |
| `●` orange | Working |
| `◐` blue | Working, waiting on input |
| `○` gray | Idle |
| `✓` green | Completed |
| `■` gray | Stopped. Opening it resumes it |
| `?` purple | A state this build does not recognize. The footer names it |
| `▌` aqua | Open in a pane in this tab |
| `▌` grey | Open in a pane in another tab |
| `2` | The tab it is open in. `+` means tab 10 or higher |

`d` hides a row from the list and nothing else: the agent keeps running and its
pane stays open. Hidden rows are kept for the life of the tmux session.

## Layout

The sidebar keeps its width. ccmux checks it on every poll and resizes it only
when it is wrong, so a split or a manual resize heals on the next tick.

`o`, `s` and `x` spread the panes ccmux opened evenly along the split axis. A
window mixing `o` and `s` panes is a tree, and is left as you built it. Panes
are never re-evened on a timer, so a border you drag stays put.

A zoomed pane (`prefix z`) stays zoomed. The automatic resizing above pauses
while the window is zoomed.

`r` puts the layout back by hand: it re-evens the panes and re-pins the sidebar,
even through a zoom. The footer says when it changed something.

## Tabs

`t` opens a session in a new tmux window with its own sidebar. Once a second tab
exists, the header shows which tab you are in and the list shows where each
session is open. `Enter` and `x` work across tabs.

A tab closes when its last pane does. Running `ccmux` again restores any tab's
missing sidebar. Windows you create yourself are never touched.

## Detaching

`Ctrl-z` in an attached pane detaches from the session. The agent keeps running.
The pane then waits instead of closing:

```
[ccmux] attach exited (rc=0). resume: claude attach 3f9a07c2
[ccmux] enter=resume  s=shell  q=close pane:
```

- **enter** re-attaches in the same pane.
- **s** gives you a login shell in the pane. It is behind a key because shell
  startup files that touch tmux would run against the ccmux server.
- **q** closes the pane.

While a pane is detached, the sidebar no longer counts it as open, and `R`
leaves it alone. `x` still closes it.

## Dispatching with `n`

`n` asks for a task and a working directory, prefilled from the selected row.
The directory field expands `~` and `Tab`-completes. If the directory does not
exist, a second `Enter` creates it. Only one level is created, and never over an
existing file.

The cursor moves to the new session once it is listed. Moving the cursor
yourself first cancels that.

## Restarting after an upgrade

After `cargo install` or a `claude` upgrade, the running processes still use
the old binaries. `R` restarts them in place. Windows, panes and layout stay
as they are.

| Target | What `R` does |
|---|---|
| This sidebar | `exec` |
| Other tabs' sidebars | `respawn-pane` |
| Panes ccmux opened | `respawn-pane` with `claude attach` |
| Idle or done agents in those panes | `claude stop`, then the pane re-attaches |
| Idle or done running agents with no pane | `claude respawn` |
| Working or blocked agents | Skipped, and counted as `busy` |
| Stopped or finished sessions | Skipped, never started |
| Detached panes | Skipped, and counted as `skipped` |
| Agents attached anywhere else on the machine | Skipped |

A restarted agent keeps its id, name and conversation. Its age resets, because
the start time is the worker's. Pane scrollback is lost.

`R` checks that the new binary runs before it restarts anything. If it does not,
nothing is touched and the footer shows the error.

## Safety

- `x` closes a pane and never stops the agent.
- `d` only hides a row.
- `Ctrl-x` is the only verb that can **delete** a session. The first press stops
  it; the conversation is kept and `Enter` resumes it. A second press within
  2 s runs `claude rm`, which deletes the session and its git worktree.
  `claude rm` refuses a worktree with uncommitted or unpushed work.
- The footer names the session while the delete window is open.
  Moving the cursor does **not** close it, and a second press on another row
  deletes nothing. A press within 750 ms of the previous one is ignored, so a
  held key deletes nothing.
- A delete closes the panes parked on that session, but not a pane you turned
  into a shell with `s`.
- `R` also runs `claude stop` and `claude respawn`, only on idle or done agents,
  and resumes each one straight away. It never deletes anything and never starts
  a session that was not running.
- Every tmux command is scoped to the ccmux session. Panes in your other
  sessions are never touched.

## Polling

The sidebar runs `claude agents --json --all` every 2.5 s, but only while its
window is on screen. A list that stops changing is polled less often, down to
every 30 s. A keypress or `r` restores the fast rate. The header dot is hollow
while polling is paused.

## Degraded mode

`ccmux sidebar` also runs outside tmux. The list, filtering, `n`, `L`,
`Ctrl-x`, `d` and `u` work. `Enter`, `o`, `s`, `t`, `x` and `R` refuse
with a message.

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets
```

Tests that drive a real tmux server or `claude` are `#[ignore]`d. Run them by
name, one at a time, because the tmux socket is process-global:

```sh
cargo test -- --ignored --nocapture live_even_layout
```

Try the launcher against a throwaway tmux server:

```sh
ccmux --socket ccmux-test --session scratch
tmux -L ccmux-test kill-server
```

`SPEC.md` is the implementation contract. `PROBE-FINDINGS.md` records the tmux
and `claude` behaviour it is based on.
