# ccmux

A tmux-backed, neovim-style frontend for Claude Code background sessions and
Codex sessions.

`claude agents` gives you a fleet view. ccmux gives you a workspace: a session
list pinned to the left of a tmux window, with the real agent TUIs open in
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

## Codex sessions

Codex is on by default when `~/.config/agents/codex-serve.token` exists;
disable it with `--codex-url ''`. If `$XDG_CONFIG_HOME` is set and non-empty,
ccmux uses `$XDG_CONFIG_HOME/agents/codex-serve.token` instead. Override the
defaults when pointing ccmux at another existing Codex app-server:

```sh
ccmux --codex-url ws://127.0.0.1:8965 \
  --codex-token-file "$HOME/.config/agents/codex-serve.token"
```

The environment equivalents are `CCMUX_CODEX_URL` and
`CCMUX_CODEX_TOKEN_FILE`; flags take precedence. `CCMUX_CODEX_BIN`
selects the Codex executable. Settings follow new tabs and `R` restarts.
The token is read inside each attach pane, never placed in tmux options or
command arguments. An empty URL disables Codex; an absent URL uses the
loopback default above.

Only loopback `ws://` is supported: `localhost`, `127.0.0.0/8` or
`[::1]`. For a remote server, open your own SSH tunnel, for example
`ssh -N -L 8965:127.0.0.1:8965 host`, then use the local URL above.

Loopback does not prove which process owns the port. If the server is down,
another process that binds the configured port receives the
`Authorization: Bearer` header on the next automatic poll and roughly every
10 seconds once failure backoff engages. This matters on shared multi-user
hosts; run there with `--codex-url ''` or `CCMUX_CODEX_URL=` unless the
endpoint is trusted. A process under your own uid can already read the token
file.

The list includes loaded threads and unloaded threads updated in the last
seven days, limited to persistent, top-level threads. They share a final
**Codex** group, with blocked threads first and unloaded threads last.
Names have no provider prefix. `◇` means unloaded; it does not establish
the last turn's outcome. `a` hides unloaded Codex rows along with the
Claude Completed group.

`Enter/o/s/t` open the official Codex TUI; `x` closes a pane while work
stays on the server. Filtering, navigation, `a`, `d/u` and `r` work
as usual; `r` also reloads credentials and DNS. On an idle (`○`) or unloaded
(`◇`) Codex row, press `Ctrl-x` twice within 2 s to archive it. The first
press sends no RPC. ccmux refuses running, blocked, error/unknown, or mapped
pane rows, and refuses when it cannot read the tmux pane maps. The second
press re-reads the pane maps and the server status. It archives only if the
server still reports the state the row shows: an unloaded row that the
server now reports idle has been opened by some other client, and is refused.
`L` still refuses. `n` always creates a Claude session, using the sidebar's
local cwd when Codex is selected. `R` skips Codex panes.

After `/quit` in the Codex TUI, the pane parks: Enter resumes, `s` opens a
shell, `q` closes it. The pane map records the **launch target**.
`/resume`, `/new` and `/fork` can change the TUI's thread without
changing that record: sidebar Enter/`x` still refer to the original launch,
and a parked retry returns to it. The Codex TUI owns every other thread
mutation.

Archive is a soft delete: turns stay intact and the official client can undo
it. Either run
`codex unarchive <id> --remote ws://127.0.0.1:8965 --remote-auth-token-env CODEX_REMOTE_TOKEN`
with that environment variable set, or resume the archived ID and choose
**Unarchive and resume**. The thread reappears in the sidebar on a later poll.
Codex Desktop's **Delete all archived** action permanently deletes archived
threads. ccmux has no unarchive key.

The fresh read and archive are separate RPCs. Another client can start a turn
between them, and that turn would be aborted. ccmux cannot detect an idle TUI
attached outside its pane maps; archiving leaves that TUI silent and its next
message fails `thread not found`. A standalone non-remote Codex process is
also invisible between turns. Mid-turn, its writer lock refused archive in the
measured probes; between turns remains a risk.

## Keys

The stop, delete and logs keys below apply to Claude rows; Codex differences
are listed above.

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
| `Ctrl-x` | Claude: stop, then delete on a second press. Codex: archive eligible `○`/`◇` rows on a second press |
| `n` | Dispatch a new background session |
| `L` | Show the session's logs |
| `d` / `u` | Hide the row from the list / undo the last hide |
| `/` | Filter by name, cwd or short id |
| `a` | Show or hide Completed and unloaded Codex rows |
| `r` | Refresh, and re-assert the layout |
| `R` | Restart ccmux and idle agents after an upgrade |
| `?` | Help |
| `q` | Quit the sidebar. Sessions and panes are untouched |
| `Esc` | Cancel a pending delete/archive, or clear the filter |
| `Ctrl-c` | Quit from any mode |

`o` and `s` follow vim's naming: `o` puts panes side by side, `s` stacks them.

Inside the overlays, `j`/`k`, `Ctrl-d`/`Ctrl-u` and `g`/`G` scroll. In the
`n` prompt, `Tab` moves between the task and cwd fields and `Enter` submits.
Pasted text is never run as keys.

## The list

Claude rows list only background sessions: those started with `claude --bg` or `n`.
Interactive Claude sessions cannot be attached into a split, so they are left out.

Groups run **Blocked**, **Working**, **Idle**, **Completed**, **Codex**.
The first four contain Claude rows; every Codex state stays in Codex.
Empty groups have no header.

| Glyph | Meaning |
|---|---|
| `▲` yellow | Blocked on a permission prompt or question |
| `●` orange | Working |
| `◐` blue | Working, waiting on input |
| `○` gray | Idle |
| `✓` green | Completed |
| `■` gray | Stopped. Opening it resumes it |
| `◇` gray | Codex thread unloaded from this server |
| `?` purple | An unrecognized state, or Codex `systemError`; the footer reports the value or runtime error |
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

`Ctrl-z` in a Claude pane detaches from the session. The agent keeps running.
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
| Claude panes ccmux opened | `respawn-pane` with `claude attach` |
| Idle or done agents in those panes | `claude stop`, then the pane re-attaches |
| Idle or done running agents with no pane | `claude respawn` |
| Working or blocked agents | Skipped, and counted as `busy` |
| Stopped or finished sessions | Skipped, never started |
| Detached panes | Skipped, and counted as `skipped` |
| Codex panes | Skipped; the app-server is untouched |
| Agents attached anywhere else on the machine | Skipped |

A restarted agent keeps its id, name and conversation. Its age resets, because
the start time is the worker's. Pane scrollback is lost.

`R` checks that the new binary runs before it restarts anything. If it does not,
nothing is touched and the footer shows the error. It also requires Codex flag
support, even with Codex disabled, so `R` cannot roll back to a pre-Codex build.

Before installing a pre-Codex build, quit every running sidebar with `q` and
close Codex panes, or kill the whole ccmux tmux session. A mixed-version `t`
can leave a new tab without a sidebar because the old binary rejects the Codex
flags. An old sidebar that adopts a Codex-bearing window can also discard its
v2 pane map, including Claude pane records, and strip Codex dismissal tags.

## Safety

- `x` closes a pane and never stops the agent.
- `d` only hides a row.
- On a Claude row, `Ctrl-x` is the only verb that can **delete** a session.
  The first press stops it; the conversation is kept and `Enter` resumes it.
  A second press within 2 s runs `claude rm`, which deletes the session and
  its git worktree. `claude rm` refuses a worktree with uncommitted or
  unpushed work.
- On an eligible Codex row, the first `Ctrl-x` only arms a two-second window.
  The second performs a fresh status read and one soft archive. It never calls
  `thread/delete` or `turn/interrupt`, and never retries an uncertain result.
- The footer names the session while the delete window is open.
  Moving between Claude rows does **not** close it; a second press on another
  row deletes nothing. Selecting a Codex row closes it. A Codex archive window
  closes on any row move, provider change or repeated press, and further
  presses until its 2 s would have ended only say
  `archive window closed — nothing archived`. A press within 750 ms of the
  previous one is ignored, so a held key deletes or archives nothing.
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
`d` and `u` work, and so does `Ctrl-x` on Claude rows. `Enter`, `o`, `s`, `t`,
`x`, `R` and `Ctrl-x` on Codex rows refuse with a message.

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

`SPEC.md` is the implementation contract. `PROBE-FINDINGS.md` records the tmux,
Claude and Codex behaviour it is based on.
