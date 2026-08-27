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
| `o` | Open in a **vertical** split (vim geometry: side by side), then spread the panes evenly across the width | no |
| `s` | Open in a **horizontal** split (vim geometry: stacked), then spread the panes evenly down the height | no |
| `t` | Open in a **new tab** — a tmux window with its own pinned sidebar — and switch to it | no |
| `x` | Close the pane showing a session — the agent keeps running, because background agents are daemon-owned and outlive their pane | no |
| `Ctrl-x` | **Stop the session** — immediately, no confirmation. Press it **again within two seconds** to **delete** the session and its git worktree. A second press inside 750 ms is read as a held key or a buffered burst and ignored — the window stays open, so press again | **yes** |
| `n` | Dispatch a new background session with a typed task | no |
| `L` | `claude logs` for this session, ANSI-stripped, in an overlay | no |
| `d` | **Dismiss** the selected session from this list. A view filter: the agent keeps running and its pane stays open — dismissing a row that has a ccmux pane says so, because the row was the only way to reach `x` and `Enter` for it | no |
| `u` | Undo the most recent `d` | no |
| `/` | Filter by name, cwd, or short id | no |
| `a` | Toggle visibility of the Completed group | no |
| `r` | Force refresh | no |
| `?` | Help overlay | no |
| `q` | Quit the sidebar. Sessions and panes are untouched | no |
| `Esc` | Close an open delete window if there is one; else clear the filter if one is active; otherwise does nothing | no |

`o` and `s` are named for vim's geometry, not tmux's: `o` = vertical =
side-by-side, `s` = horizontal = stacked.

tmux's own `split-window` halves whichever pane it lands on, so a fourth `o`
would leave a 20-column sliver next to a 42-column pane. ccmux re-lays the
window after a split, and after an `x` that closed a pane **in the tab you are
looking at**, so the panes it opened share the axis evenly — the sidebar keeps
its pinned width, and the leftover cells are handed out one apiece rather than
piled on one pane, so no two panes differ by more than a column (83 content
columns over three panes is 28 + 28 + 27). Evening happens **only** on those two
keys, never on the poll tick, so a border you drag with the mouse stays where
you put it. A window holding a mix of `o` and `s` is a tree rather than a row or
a column, and is left exactly as you built it — as is the other tab's geometry
when `x` reaches across tabs to close a pane there.

`Enter` and `x` reach across tabs: `Enter` on a session open in another tab
switches to that window and selects its pane, and `x` closes a pane wherever it
lives — except a sidebar, which no tab will let you close.

### Overlays and modes

| Mode | Keys |
|---|---|
| Filter (`/`) | Type to filter live · `Ctrl-w` word · `Ctrl-u` clear · `Enter` commit · `Esc` clear and leave |
| Prompt (`n`) | `Tab` next field · `Enter` run · `Esc` cancel · `Home`/`End`/arrows/`Backspace`/`Delete` edit |
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
| `▌` aqua | Open in a ccmux pane right now (column 1) |
| `5` aqua | Open in **tab 5** (column 2). Blank means open in the tab you are looking at; `+` means a tab number of ten or more |

### Tabs

`t` puts a session in its own tmux window and takes you there. Every tab gets
its own pinned sidebar, so the list is on screen wherever you are — the cost is
one `claude agents` poll per tab, which is the trade the layout is for.

The header says `tab 2` — tmux's own window number, so `prefix-2` goes there —
once a second tab exists, and the badge in the second gutter column says which
tab each open session is in. Nothing is shown while there is only one tab.

A session you open in two tabs is "here" in both: each tab's badge stays blank,
`Enter` does not move you, and `x` closes the pane in the tab you are actually
looking at.

There is no close-tab key: a tab ends when its last pane does, the way tmux
already ends windows. If you quit a tab's sidebar with `q` while its Claude
panes are still there, that tab has no sidebar until you detach and run `ccmux`
again, which heals every tab that is missing one. A window you made yourself
with `prefix-c` is never touched, and neither is a tab `t` is still building.

Dismissals (`d`/`u`) are shared across tabs, because the session list is the
same list in every tab. Two tabs dismissing different rows in the same interval
both stick: each sidebar writes only its own window's state, so neither can
revert the other.

### Which sessions are listed

Only **background** sessions — the ones started with `claude --bg` or with `n`.

Interactive sessions are never listed. There is no `claude attach` for one, so
it cannot be opened into a split; and an interactive session hosted by Claude
Desktop has no tmux pane to jump to either, so such a row is permanently
un-openable. Rather than show rows that nothing can act on, ccmux excludes them
when a poll is applied.

ccmux has no verb for starting one either: start interactive Claude in a tmux
pane yourself, the ordinary way.

### Dismissing a row

`claude` has no delete verb — `claude stop` parks a session, it does not remove
it — so `d` hides a row **from this view** and nothing more. It runs no
`claude` command, kills no pane, and the agent goes on working. `u` undoes the
most recent `d`; the header keeps counting the hidden session in its total, so
the list reads `5/6` while one row is dismissed.

The use it was built for is the row you are finished with but `claude` will not
stop reporting. A completed or stopped session keeps coming back in every poll,
and `a` only hides the whole Completed group at once. `d` takes out the one row
you named.

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
- `Ctrl-x` is the only verb that stops a session, and the only one that can
  delete one. The first press **stops** — recoverable: the conversation is kept
  and `Enter` resumes it. Only a **second press within two seconds** deletes: it
  runs `claude rm <id>`, which removes the session **and its git worktree**, so
  uncommitted work in that worktree goes with it. Nothing undoes that; `u`
  undoes a dismissal, never a delete.
- `claude rm` refuses rather than destroying unpushed work: a worktree holding
  unpushed commits or uncommitted changes is **kept**, and ccmux shows the
  refusal in full. That is a backstop, not the safety model — a worktree with
  nothing outstanding is deleted without further question.
- While that two-second window is open the footer says so, names the session,
  and says what delete takes. It outranks every other footer message, so the
  warning cannot be pushed off screen while the verb is loaded.
- The window is closed by: two seconds passing, `Esc`, `q`, a second press
  (whether it deleted or refused), or anything that leaves the list (`/`, `n`,
  `?`, `L`). Moving the cursor does **not** close it — the warning keeps naming
  the session it is loaded against, and the next press refuses.
- The second press deletes **only** the session the first press stopped. The id
  is captured at the first press and never re-read, so a poll that re-sorts the
  list in between cannot change the target — and if the cursor has moved to a
  different row, the second press deletes nothing and stops nothing.
- Mis-fire is defended in three layers. Pasted text is discarded in Normal mode
  and cannot produce a Ctrl chord anyway. A `Ctrl-x` arriving within 750 ms of
  the previous one is ignored — it says `too fast — press Ctrl+X again` and
  leaves the window open — and every press restarts that clock, including when
  it returns from the `claude stop` it blocked in, so a buffered burst of any
  length performs exactly one stop. And a qualifying second press does not
  delete on the spot: it settles for a beat first, and a further `Ctrl-x` inside
  that beat cancels it.
- 750 ms rather than something snappier because of the held key. A terminal
  reports key presses but not releases, so a hold's first auto-repeat is
  indistinguishable from a deliberate second press — and every stock auto-repeat
  delay (GNOME 500 ms, KDE 600 ms, X11 660 ms) is under 750 ms, so at any of
  them a hold never even schedules a delete. Holding `Ctrl-x` down stops one
  session and deletes nothing. The deliberate second press has the rest of the
  two seconds; it is meant to be a press you made after reading the warning.
- With the Completed group hidden (`a`), the row you just stopped leaves the
  list and the cursor falls to its neighbour. The second press then refuses —
  `<name> left the list — nothing deleted` — because the cursor is no longer on
  the session it stopped. This is deliberate: the alternative would stop the
  neighbour. The same happens under a `/` filter written against the worktree
  path, because `claude stop` moves a session's reported directory back to the
  parent. Clear the filter (or press `a`) and press `Ctrl-x` twice on the
  stopped row: on an already-stopped session the first press only arms.
- `d` removes a row from the list only. It is not a stop, not a kill, and not a
  delete — nothing outside ccmux's own view state changes, and `u` puts it back.
- Every tmux command that names a pane carries a validated target and is scoped
  to the ccmux session, without exception. Panes in your other tmux sessions are
  never split, resized, killed — or even focused. ccmux does not enumerate the
  tmux server at all; it lists only its own session's panes.

## Degraded mode

`ccmux sidebar` outside tmux still runs: polling, grouping, filtering, `L`, `n`,
`Ctrl-x`, `d`, `u`, and all navigation work; the header indicator turns yellow and
the pane-related verbs (`Enter`, `o`, `s`, `x`) refuse with a message. The
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
