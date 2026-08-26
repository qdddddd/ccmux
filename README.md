# ccmux

A tmux-backed, neovim-style frontend for Claude Code sessions.

`claude agents` gives you a fleet view. `ccmux` gives you a workspace: a
persistent session explorer pinned to the **left** of a tmux window, with the
real Claude Code TUIs — full fidelity, full transcript — living in splits to its
right.

tmux does the hard part (it is the PTY substrate, so sessions survive detach and
SSH drops). ccmux adds the sidebar, the "open this session into a split" verb,
and the layout orchestration that makes the two feel like one app.

```
┌──────────────────┬───────────────────────┬───────────────────────┐
│ ccmux   18       │                       │                       │
│ ── Working (2) ──│                       │                       │
│ ▌● bt/reg-update │   live Claude TUI     │   live Claude TUI     │
│  ◐ kernel bugs   │                       │                       │
│ ── Idle (4) ─────│   ✶ Thundering… 3m    │   ✶ Bunning… 16m      │
│  ○ alpha/opt    │   >                   │   >                   │
│ ── Completed ────│                       │                       │
│  ✓ alpha/axioma │                       │                       │
├──────────────────┤                       │                       │
│ name  af/reg-…   │                       │                       │
│ id    1c45d64f   │                       │                       │
│ cwd   ~/…/bin    │                       │                       │
│ j/k move ⏎ open  │                       │                       │
└──────────────────┴───────────────────────┴───────────────────────┘
      34 cols                    splits fill the rest
```

## Install

```sh
cargo install --path .
```

Requires `tmux` and the `claude` CLI on `PATH`. Rust 1.94+ (edition 2024).

## Launch

```sh
ccmux                       # create-or-attach the `ccmux` tmux session
ccmux --session work        # a differently named session
ccmux --width 40 --dark     # wider sidebar, dark palette
```

Running `ccmux` again from another terminal attaches the **same** session — no
second window, no second sidebar. Running it from *inside* its own session is a
no-op. If the sidebar was quit with `q`, re-running `ccmux` re-inserts it on the
left at the pinned width, and the panes it had opened are still mapped.

The sidebar is normally started for you. To run it directly (it works standalone
outside tmux, in degraded mode — see below):

```sh
ccmux sidebar --interval 2500
```

### Options

| Flag | Default | Meaning |
|---|---|---|
| `--session <NAME>` | `ccmux` | tmux session to create/attach. `[A-Za-z0-9_-]{1,64}` |
| `--width <COLS>` | `34` | Pinned sidebar width, clamped to `20..=120` |
| `--light` | on | Light gruvbox palette (the default) |
| `--dark` | off | Dark gruvbox palette, for a dark terminal background; mutually exclusive with `--light` |

### Theme

The palette is **not** auto-detected — `COLORFGBG` is unset under kitty and most
modern terminals, and an OSC 11 background query could not be verified end to end
through tmux, so ccmux does not gamble on one at startup.

It defaults to **light**, because a wrong guess is not symmetric: dark `fg`
(`#ebdbb2`) on a light ground is 1.21:1 and simply cannot be read, while light
`fg` (`#3c3836`) on a dark ground still resolves. The safer default is the one
whose failure mode is merely ugly.

On a dark terminal background:

```sh
export CCMUX_THEME=dark       # in your shell rc, or
ccmux --dark                  # per invocation
```

`--light`/`--dark` beat `CCMUX_THEME`, which beats the light default. The launcher
resolves the choice and forwards the answer to the sidebar pane, so it holds even
though the pane may not inherit the variable.
| `-L`, `--socket <NAME>` | tmux default | Use `tmux -L <NAME>`, a separate tmux server |
| `--interval <MS>` | `2500` | *(`sidebar` only)* `claude agents --json` poll interval |

`CCMUX_CLAUDE_BIN` overrides the `claude` binary. `CCMUX_TMUX_SOCKET` is an
environment-variable equivalent of `--socket`.

## Keymap

### Normal mode

| Key | Action | Destructive |
|---|---|---|
| `j`, `Down` | Next session (skips group headers) | no |
| `k`, `Up` | Previous session | no |
| `g` | First session | no |
| `G` | Last session | no |
| `Ctrl-d` | Down half a viewport | no |
| `Ctrl-u` | Up half a viewport | no |
| `Tab` | First row of the next non-empty group | no |
| `Shift-Tab` | Previous non-empty group | no |
| `Enter` | Jump to the session's pane, or open it in a vertical split | no |
| `o` | Open in a **vertical** split (vim geometry: side by side) | no |
| `s` | Open in a **horizontal** split (vim geometry: stacked) | no |
| `x` | Close the pane showing a session — the agent keeps running, because background agents are daemon-owned and outlive their pane | no |
| `S` | **Stop the session.** Asks `y`/`n` first | **yes** |
| `n` | Dispatch a new background session with a typed task | no |
| `c` | New interactive session in a chosen cwd. It opens in a pane but is **not listed** — see Which sessions are listed | no |
| `L` | `claude logs` for this session, ANSI-stripped, in an overlay | no |
| `d` | **Dismiss** the selected session from this list. A view filter: the agent keeps running and its pane stays open — dismissing a row that has a ccmux pane says so, because the row was the only way to reach `x` and `Enter` for it | no |
| `u` | Undo the most recent `d` | no |
| `/` | Filter by name, cwd, or short id | no |
| `a` | Toggle visibility of the Completed group | no |
| `r` | Force refresh | no |
| `?` | Help overlay | no |
| `q` | Quit the sidebar. Sessions and panes are untouched | no |
| `Esc` | Clear the filter if one is active; otherwise does nothing | no |

`o` and `s` are named for vim's geometry, not tmux's: `o` = vertical =
side-by-side, `s` = horizontal = stacked.

### Overlays and modes

| Mode | Keys |
|---|---|
| Filter (`/`) | Type to filter live · `Ctrl-w` word · `Ctrl-u` clear · `Enter` commit · `Esc` clear and leave |
| Confirm (`S`) | `y` stop · `n`, `Esc`, `Enter`, anything else cancels. A `y` within 250 ms of the modal opening is treated as type-ahead and cancels |
| Prompt (`n`, `c`) | `Tab` next field · `Enter` run · `Esc` cancel · `Home`/`End`/arrows/`Backspace`/`Delete` edit |
| Help (`?`) | `j`/`k` scroll · `Ctrl-d`/`Ctrl-u` page · `g`/`G` top/bottom · any other key closes |
| Logs (`L`) | `j`/`k` scroll · `Ctrl-d`/`Ctrl-u` page · `g`/`G` top/bottom · `q`/`Esc` close |

`Ctrl-c` quits from any mode; `q` quits from Normal. Pasted text is never
executed as keys — it is discarded in Normal and taken as literal text in the
filter and the prompts.

## Reading the list

Sessions are grouped exactly as `claude agents` groups them: **Working**,
**Idle**, **Completed**.

| Glyph | Meaning |
|---|---|
| `●` orange | Working, actively generating |
| `◐` blue | Working, waiting on input |
| `○` gray | Idle |
| `✓` green | Completed |
| `?` purple | A status or state this build does not recognize |
| `▌` aqua | Open in a ccmux pane right now (column 0) |

### Which sessions are listed

Only **background** sessions — the ones started with `claude --bg` or with `n`.

Interactive sessions are never listed. There is no `claude attach` for one, so
it cannot be opened into a split; and an interactive session hosted by Claude
Desktop has no tmux pane to jump to either, so such a row is permanently
un-openable. Rather than show rows that nothing can act on, ccmux excludes them
when a poll is applied.

One consequence worth knowing: `c` still starts an interactive session in a
pane, and that session will not appear in the sidebar.

### Dismissing a row

`claude` has no delete verb — `claude stop` parks a session, it does not remove
it — so `d` hides a row **from this view** and nothing more. It runs no
`claude` command, kills no pane, and the agent goes on working. `u` undoes the
most recent `d`; the header keeps counting the hidden session in its total, so
the list reads `5/6` while one row is dismissed.

The use it was built for is a session ccmux can never open: a Claude Desktop
session has no controlling tty and no tmux pane, so `Enter` can only tell you
so. Dismissing it clears the row for good without touching the session.

If the dismissed session has a pane ccmux opened, the footer says
`hidden — pane open, u to undo` in yellow rather than naming the row. The pane
is deliberately left alone, but the row was the only place `x` and `Enter`
could be reached from, so that pane now has no affordance until `u` brings the
row back. Its `@ccmux_map` entry is kept for exactly that reason.

Dismissals are kept in the `@ccmux_hidden` tmux user option, next to
`@ccmux_map`, for the same reason: their correct lifetime is exactly the tmux
session's. Kill the tmux session and they are gone; quit and relaunch the
sidebar and they are still there — the option is written on the next poll and
again when the sidebar exits, so a `d` or a `u` in the last poll interval
before `q` is not lost.

A dismissed session that ends and leaves the poll is dropped from the set once
**two consecutive complete polls** agree it is gone, so the set cannot grow
without bound. Two consecutive, because one poll is not proof: `claude agents`
can exit 0 and still under-report — a malformed row is skipped rather than
being fatal, and a hiccup can return `[]`. Dropping an id also drops the `u`
that would restore it, so a single bad poll must not be able to do it. A poll
that is known to have lost rows concludes nothing at all.

## Safety

- Closing a pane with `x` does **not** stop the agent: background sessions are
  daemon-owned and outlive their pane. Only background sessions are listed, so
  `x` can never reach a process that dies with its pane.
- `S` is the only verb that stops a session, and it always confirms first.
- `d` removes a row from the list only. It is not a stop, not a kill, and not a
  delete — nothing outside ccmux's own view state changes, and `u` puts it back.
- Every mutating tmux command carries a validated target and is scoped to the
  ccmux session. Panes in your other tmux sessions are never split, resized, or
  killed.

## Degraded mode

`ccmux sidebar` outside tmux still runs: polling, grouping, filtering, `L`, `n`,
`S`, `d`, `u`, and all navigation work; the header indicator turns yellow and
the pane-related verbs (`Enter`, `o`, `s`, `x`, `c`) refuse with a message. The
session→pane map and the dismissed set are kept in memory only. This is what makes the sidebar
developable without a tmux server.

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets
```

Two live tests are `#[ignore]`d so CI never spawns `tmux` or `claude`:

```sh
cargo test -- --ignored live_round_trip   # tmux, pinned to the `ccmux` socket
cargo test -- --ignored live_poll         # `claude agents --json`, read-only
```

Run them one at a time — the tmux socket is process-global.

Exercise the launcher against a throwaway tmux server, never your own:

```sh
ccmux --socket ccmux --session ccmux-test-a
tmux -L ccmux kill-server        # cleanup, reaches nothing else
```

`SPEC.md` is the implementation contract; `PROBE-FINDINGS.md` records the
environment behaviour it was derived from.
