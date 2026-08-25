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
ccmux --width 40 --light    # wider sidebar, light palette
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
| `--light` | off | Light gruvbox palette instead of dark |
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
| `x` | Close the pane showing a **background** session — the agent keeps running. Refused for interactive sessions, whose pane owns the process | no |
| `S` | **Stop the session.** Asks `y`/`n` first | **yes** |
| `n` | Dispatch a new background session with a typed task | no |
| `c` | New interactive session in a chosen cwd | no |
| `L` | `claude logs` for this session, ANSI-stripped, in an overlay | no |
| `/` | Filter by name, cwd, or short id | no |
| `a` | Toggle visibility of the Completed group | no |
| `r` | Force refresh | no |
| `?` | Help overlay | no |
| `q` | Quit the sidebar. Sessions and panes are untouched | no |
| `Esc` | Clear the filter if one is active, otherwise quit | no |

`o` and `s` are named for vim's geometry, not tmux's: `o` = vertical =
side-by-side, `s` = horizontal = stacked.

### Overlays and modes

| Mode | Keys |
|---|---|
| Filter (`/`) | Type to filter live · `Ctrl-w` word · `Ctrl-u` clear · `Enter` commit · `Esc` clear and leave |
| Confirm (`S`) | `y` stop · `n`, `Esc`, `Enter`, anything else cancels |
| Prompt (`n`, `c`) | `Tab` next field · `Enter` run · `Esc` cancel · `Home`/`End`/arrows/`Backspace`/`Delete` edit |
| Help (`?`) | `j`/`k` scroll · `Ctrl-d`/`Ctrl-u` page · `g`/`G` top/bottom · any other key closes |
| Logs (`L`) | `j`/`k` scroll · `Ctrl-d`/`Ctrl-u` page · `g`/`G` top/bottom · `q`/`Esc` close |

`Ctrl-c` quits from any mode.

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

Interactive sessions render their name in purple. They have no short id, so
`claude attach`, `claude stop`, and `claude logs` do not apply to them; ccmux
jumps to their existing pane instead, resolving it through `/proc` ancestry.

## Safety

- Closing a pane with `x` does **not** stop the agent. Background sessions are
  daemon-owned and outlive their pane. Interactive sessions are not, so `x`
  refuses them.
- `S` is the only verb that stops a session, and it always confirms first.
- Every mutating tmux command carries a validated target and is scoped to the
  ccmux session. Panes in your other tmux sessions are never split, resized, or
  killed.

## Degraded mode

`ccmux sidebar` outside tmux still runs: polling, grouping, filtering, `L`, `n`,
`S`, and all navigation work; the header indicator turns yellow and the
pane-related verbs (`Enter`, `o`, `s`, `x`, `c`) refuse with a message. The
session→pane map is kept in memory only. This is what makes the sidebar
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
