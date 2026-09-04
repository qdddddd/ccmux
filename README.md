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
│ ── Blocked (1) ──│                       │                       │
│  ▲ client statem… │                       │                       │
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
| `--interval <MS>` | `2500` | *(`sidebar` only)* `claude agents --json` poll interval, when something is watching |

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
| `Enter` | Jump to the session's pane, or open it in a vertical split. A pane you have detached from is no longer showing the session, so `Enter` opens a fresh one instead of jumping there | no |
| `o` | Open in a **vertical** split (vim geometry: side by side), then spread the panes evenly across the width | no |
| `s` | Open in a **horizontal** split (vim geometry: stacked), then spread the panes evenly down the height | no |
| `t` | Open in a **new tab** — a tmux window with its own pinned sidebar — and switch to it | no |
| `x` | Close the pane showing a session — the agent keeps running, because background agents are daemon-owned and outlive their pane. Still closes a pane you have detached from, even though nothing marks it as open any more | no |
| `Ctrl-x` | **Stop the session** — immediately, no confirmation. Press it **again within two seconds** to **delete** the session and its git worktree. A second press inside 750 ms is read as a held key or a buffered burst and ignored — the window stays open, so press again | **yes** |
| `n` | Dispatch a new background session with a typed task — the cwd field takes `~` paths, `Tab`-completes directories, and offers to create a missing directory (see below) | no |
| `L` | `claude logs` for this session, ANSI-stripped, in an overlay | no |
| `d` | **Dismiss** the selected session from this list. A view filter: the agent keeps running and its pane stays open — dismissing a row that has a ccmux pane says so, because the row was the only way to reach `x` and `Enter` for it | no |
| `u` | Undo the most recent `d` | no |
| `/` | Filter by name, cwd, or short id | no |
| `a` | Toggle visibility of the Completed group | no |
| `r` | Force refresh | no |
| `R` | **Restart ccmux in place** after an upgrade — this sidebar, every other tab's sidebar, every pane ccmux opened, and the **agents** behind those panes that are not busy. Windows, panes and layout are kept exactly as they are (see *Restarting after an upgrade*) | no |
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

### Detaching: `Ctrl-Z` parks the pane, enter puts you back

`Ctrl+Z` inside an attached session is **not** a suspend. `claude attach` holds
the terminal in raw mode, so the tty never generates `SIGTSTP`; claude reads the
keystroke as "detach" and exits. There is no stopped job and nothing for `fg` to
resume — the agent itself is unaffected, because it is daemon-owned and was
never a child of the pane.

So the pane is yours the moment you press it. Instead of dying, or sitting on a
"press enter to close" prompt, it prints one line and waits:

```
[ccmux] attach exited (rc=0). resume: claude attach 1c45d64f
[ccmux] enter=resume  s=shell  q=close pane:
```

- **enter** re-runs that exact attach, in the same pane, with the whole
  transcript back. It is the closest thing to `fg` that is honest here, and it
  is one key rather than a retyped command.
- **`s`** hands the pane to your login shell (`$SHELL -l`), in the same
  directory, with the resume command in the scrollback above the prompt.
- **`q`** (or `Ctrl-D`) closes the pane, carrying out the attach's own exit
  code.

The exit code is always shown, so an attach that failed for real — the session
was stopped between the poll and the keypress, say — says `rc=1` above the same
prompt instead of vanishing.

**Why the shell is behind a key.** An interactive shell runs your startup files,
and it runs them in a pane whose `$TMUX` points at the tmux server ccmux is
using. A startup file that touches tmux therefore touches *that* server: on the
machine this was measured on, one unconditional login shell took a throwaway
server from 1 session and 2 panes to 3 sessions and 15, because the rc chain
ends up calling `tmux new -s dev -d` and tmux-resurrect's `restore.sh`, which
restored a saved workspace straight over the live ccmux session. ccmux cannot
vet your dotfiles, so it does not run them on its own initiative — `s` is your
say-so, exactly like typing `zsh` would be. (If your own rc does something like
that, guarding it with a check for `$TMUX` is worth doing anyway.)

The sidebar stops claiming that pane the moment you detach. The `▌` goes out,
the tab badge disappears, `Enter` opens a fresh pane rather than sending you
back to a prompt you left, and `R` will not respawn it. What does not change is
ownership: it is still a pane ccmux opened, so `x` still closes it. Resume it
with **enter** and the sidebar takes it back — the pane says so itself. Attach
by hand from the `s` shell and it does **not**: from the outside a `claude`
process does not say which session it is showing, so that pane stays yours, and
`Enter` gives the session a pane of its own.

### Overlays and modes

| Mode | Keys |
|---|---|
| Filter (`/`) | Type to filter live · `Ctrl-w` word · `Ctrl-u` clear · `Enter` commit · `Esc` clear and leave |
| Prompt (`n`) | `Tab` to the cwd field — and, once there, directory completion · `Shift-Tab` back to the task · `Enter` run (from either field) · `Esc` cancel · `Home`/`End`/arrows/`Backspace`/`Delete` edit |
| Help (`?`) | `j`/`k` scroll · `Ctrl-d`/`Ctrl-u` page · `g`/`G` top/bottom · any other key closes |
| Logs (`L`) | `j`/`k` scroll · `Ctrl-d`/`Ctrl-u` page · `g`/`G` top/bottom · `q`/`Esc` close |

`Ctrl-c` quits from any mode; `q` quits from Normal. Pasted text is never
executed as keys — it is discarded in Normal and taken as literal text in the
filter and the prompts.

### Dispatching into a new directory

`n` prefills the cwd with the selected row's — accept it, type the task, and
one `Enter` dispatches, same as ever. To send the session somewhere else,
`Tab` jumps to the cwd field: type a path (`~` and `~/x` expand; a tilde
anywhere else is a literal file name; `~user` is refused), and `Tab` completes
directory names as you go — unique prefixes complete through to `.../name/`,
ambiguous ones extend to the common prefix or report `N matches`.

If the directory does not exist, `Enter` does not reject it outright: the
footer says so, the hint line switches to `⏎ create cwd + run`, and a second
`Enter` creates it and dispatches — the flash names the created path. Only ONE
level is ever created: the parent must already exist, so a typo'd deep path is
refused (`parent does not exist: …`) rather than materialised as a tree.
Anything already occupying the name — a file, a dangling symlink — is refused,
never overwritten. Editing either field, `Esc`, or leaving the prompt drops
the offer. This `mkdir` is the only write ccmux ever makes to the filesystem.

## Reading the list

Sessions are grouped as `claude agents` groups them, with **Blocked** hoisted to
the top: **Blocked**, **Working**, **Idle**, **Completed**. A blocked session is
stopped at a permission prompt or a question and cannot advance until you answer
it, so it is the first thing on screen.

| Glyph | Meaning |
|---|---|
| `▲` yellow | **Blocked — waiting on you.** A permission prompt or a question |
| `●` orange | Working, actively generating |
| `◐` blue | Working, waiting on input |
| `○` gray | Idle |
| `✓` green | Completed |
| `■` gray | Stopped with `Ctrl-x`; the conversation is kept, `o` resumes it |
| `?` purple | A status or state this build does not recognize |
| `▌` aqua | Open in a ccmux pane in the tab you are looking at (column 1) |
| `▌` emphasised aqua | Open, but in **another** tab. The emphasised shade is the theme's more prominent aqua — deeper on the light theme, paler on the dark one, since a dark ground emphasises upwards — because the pane you cannot see is the one worth pointing at |
| `5` | Which tab (column 2), in the same shade as the `▌` beside it, so `▌5` reads as one token. Blank means the tab you are looking at; `+` means a tab number of ten or more |

When `claude agents` reports a `state` or `status` this build has no variant for,
the row still renders (`?` purple) and still groups — and the footer says so by
name: `unmodelled state "…" — update ccmux`. Two such values, `stopped` and
`blocked`, shipped unnoticed before that warning existed.

Each value is announced once, but "once" means once it has actually been on
screen: if a keypress message overwrites the warning, or the help overlay is
covering the footer when it lands, it comes back on a later poll rather than
being spent unread. `cargo test -- --ignored live_state_and_status` asks the
running fleet the same question, and is worth running after a `claude` upgrade.

### Tabs

`t` puts a session in its own tmux window and takes you there. Every tab gets
its own pinned sidebar, so the list is on screen wherever you are. Only the tab
you are looking at polls (see *Polling* below), so the extra tabs are close to
free.

The header says `tab 2` — tmux's own window number, so `prefix-2` goes there —
once a second tab exists, and the badge in the second gutter column says which
tab each open session is in. Nothing is shown while there is only one tab.

The open marker `▌` says the same thing in colour: today's aqua for a session
open in this tab, the emphasised aqua for one parked in another. Both cells take
one ink, decided once from the badge, so the shade and the digit cannot end up
disagreeing about where a session is.

A session you open in two tabs is "here" in both: each tab's badge stays blank,
its marker keeps the unemphasised aqua, `Enter` does not move you, and `x`
closes the pane in the tab you are actually looking at.

There is no close-tab key: a tab ends when its last pane does, the way tmux
already ends windows. If you quit a tab's sidebar with `q` while its Claude
panes are still there, that tab has no sidebar until you detach and run `ccmux`
again, which heals every tab that is missing one. A window you made yourself
with `prefix-c` is never touched, and neither is a tab `t` is still building.

Dismissals (`d`/`u`) are shared across tabs, because the session list is the
same list in every tab. Two tabs dismissing different rows in the same interval
both stick: each sidebar writes only its own window's state, so neither can
revert the other.

### Restarting after an upgrade

`cargo install --path .` replaces the binary on disk, but the sidebar you are
looking at is still the old image, and so is every `claude attach` client in
every pane. A `claude` upgrade is worse: a background agent keeps the version it
was launched with for its whole life, so it can be days behind the CLI you just
installed. `R` restarts all of them **in place**: every window, every pane,
every pane id and the whole layout stay exactly as they are — only the processes
change. There is no confirmation, and none is needed (see *Safety*).

What it restarts, and how:

| | mechanism | what survives |
|---|---|---|
| this sidebar | `exec(2)` — the process image is replaced | the pane, by construction: tmux is never told |
| the other tabs' sidebars | `respawn-pane -k` | the pane id, its geometry, the layout |
| the panes ccmux opened, still attached | `respawn-pane -k` with `claude attach <id>` | the **agent** — it is daemon-owned and outlives its client |
| the **agents** behind those panes, when idle, done or stopped | `claude stop <id>`, and the pane's own `claude attach` is the resume | the conversation and the transcript |
| an agent that is **working** or **blocked** | nothing — skipped and counted | the work in flight |
| a pane you detached from | nothing — skipped and counted | whatever you have been doing in it |

**Nothing else is touched.** A pane is restarted only if ccmux can prove it
created it — it is a tab's recorded sidebar, or it is in that tab's pane map. A
pane you opened yourself inside the ccmux session, with `prefix-"` or
`prefix-%`, is in neither, and `R` leaves it alone: whatever is running in it
keeps running, with the same pid.

**The agents.** Restarting an attach client changes nothing about the version
doing the work: the agent is a separate, daemon-owned process, and it runs
whatever `claude` was current when it was dispatched until it dies. `R` refreshes
it the only way there is — `claude stop <id>`, after which the pane's respawned
`claude attach <id>` resumes it as a genuinely new process on the binary that is
installed now. The conversation is kept; the transcript comes back.

It only ever touches sessions **ccmux has open in a pane** — the same ownership
rule the panes follow. A session you never opened here is none of ccmux's
business, and it could not be resumed anyway: the pane's attach is the resume,
so ccmux stops only what it is about to re-attach.

And it never touches an agent that is **working** or **blocked** — the two top
groups in the list. A working agent is mid-task, a blocked one is holding a
question for you, and `claude stop` would take either. Those keep their old
version until they finish, which is the trade that lets `R` stay safe to press
at any moment. The footer names them: `2 busy`. State is re-read from
`claude agents --json` immediately before each stop, so a session that picks up
work while `R` is running is left alone from the next one onwards.

A pane you **detached from** is skipped for the same reason, even though ccmux
did open it. Once `Ctrl-Z` has parked it — and especially once `s` has left a
shell in it — the map entry says who created the pane and nothing about what is
in it, and respawning a shell you have been working in for an hour with
`claude attach` would destroy it. The footer counts those, one per pane:
`restarted 2 sidebars, 1 pane (1 skipped)`. Resume the pane with enter and it
becomes a restart target again.

The footer then says what happened, from the new image: `restarted 3 sidebars,
4 panes, 2 agents (1 busy, 1 skipped)`. Successes are in the head, exceptions in
the parenthesis, and each gets its own word — `failed` is a pane that did not
come back, `not restarted` an agent still on the old binary, `left stopped` an
agent that was stopped and whose pane never came back to resume it, `busy` an
agent deliberately left alone, `skipped` a pane deliberately left alone. A line
too wide for the sidebar wraps rather than truncating.

`left stopped` is the only one of those that asks anything of you, and it is
rare: it needs a pane to be killed or to fail its respawn in the moment between
the stop and the re-attach. The agent is halted and ccmux has nothing left that
would resume it, so press `Enter` on the row to open it again. An agent is
counted as restarted only once a pane has actually come back holding it — a
stop is half a restart, and the footer does not claim the other half before it
has happened. It is a count of what was
actually restarted, not a plan announced in advance — the sidebar you pressed
`R` in restarts first, and the image that comes up is the one that restarts
everything else.

One agent that will not stop does not cost the others theirs: the stop is
attempted for each in turn, a failure is counted, and the pass carries on. Every
stop lands **before** any pane is respawned, because the respawned
`claude attach` is what resumes the session — the other order would leave you
looking at a pane whose agent had just been halted underneath it.

The pass is not instant — about a second per agent, and up to 75 s against a
`claude` that has wedged — so the restarted sidebar **draws a frame first** and
says `restarting the session…` while it works. Keys struck at it during that
window are discarded rather than replayed when it finishes: they were aimed at
a screen that had not been updated yet, and one of them could be a `Ctrl-x`
landing on a row you never chose.

That order is the safety property. `respawn-pane -k` has no undo: it kills the
pane, runs the new command, and if that command exits the pane closes and the
window layout collapses with it. So `R` spends none of them until the new binary
has proven it runs, by running. If the binary is broken, or the path is wrong,
or the `exec` fails anyway, the sidebar re-enters its screen, says
`restart failed: …`, and every pane in the session is exactly where it was.

Two things do not survive, both by nature. The panes' **scrollback** is gone,
because the commands in them were restarted — the agents' transcripts are not,
and `L` still shows them. And a `d`/`u` pressed in **another** tab within the
last poll interval reverts, because that sidebar is killed before it flushes;
dismissals in the tab you press `R` in are flushed first, and everything already
written to tmux — the pane map and the dismissal set are tmux *window* options —
comes back untouched.

`R` re-resolves the binary from `argv[0]` rather than from `current_exe()`, and
that is not a detail: once `cargo install` has renamed a new file over the old
path, `current_exe()` answers `…/ccmux (deleted)` and `/proc/self/exe` still
opens the **old** image, so the obvious implementation would either fail or
restart the very binary you just replaced while reporting success.

Then it runs it. `<ccmux> --version` has to spawn, exit 0 and print `ccmux`
before `R` will hand the session over to it — an execute bit is not proof that a
file can be executed, and "the binary was replaced seconds ago" is exactly the
situation that produces a truncated download, a wrong-architecture build or one
linked against a library that is no longer installed. A candidate that fails
that check is refused with a message and nothing is restarted.

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
- `R` restarts processes but destroys nothing: it only respawns panes ccmux
  itself created, and every pane comes straight back. Nothing is killed until
  the new binary is proven to run — it is run, and then it is the process doing
  the respawning — so a failed upgrade costs a message rather than your
  sidebars. That is why it has no confirmation: an arm belongs on a verb with no
  undo, and `Ctrl-x`'s second press is the only one of those.
- `R` also runs `claude stop` — the same **recoverable** verb `Ctrl-x`'s first
  press runs, and never `claude rm`. It reaches only sessions ccmux has open in
  a pane, and only those that are neither working nor blocked; the conversation
  is kept and the pane's own `claude attach` resumes it a moment later. A busy
  agent keeps its old version until it finishes, which is the whole reason `R`
  is safe to press at any moment.
- `Ctrl-x` is the only verb that can **delete** a session, and the only stop
  that reaches a **working or blocked** one — `R`'s stop refuses those, and puts
  everything it does stop straight back. The first press **stops** —
  recoverable: the conversation is kept and `Enter` resumes it. Only a **second press within two seconds** deletes: it
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

## Polling

The sidebar refreshes by running `claude agents --json --all` every 2.5 s. It
does that **only while something is watching it**:

- A sidebar in a tmux window that no client is rendering polls nothing. With
  three tabs, one polls and two are silent.
- A session with no attached client — you detached, or went home — polls
  nothing at all.
- A listing that comes back identical four times running widens the gap, 5 s
  to 10 s to 20 s to 30 s, for as long as nothing moves.

"Watching" is counted per WINDOW, not per session, so a second client attached
through a grouped session (`tmux new-session -t ccmux`) keeps the window it is
displaying on the fast path even though the original session shows no clients
of its own.

All of it is answered by the same `tmux list-panes` the sidebar already runs
each tick, so the check costs nothing, and none of it can make the sidebar slow
when it matters: switching back to the tab refreshes it on that tick, any
keypress puts it back on 2.5 s, and `r` always polls right now. The tick itself
never slows down — not for the gate and not for a failing `claude` — because it
is what notices you coming back. Nothing runs in a background thread; a paused
sidebar is paused, not queued.

If tmux cannot say who is watching — the sidebar is outside tmux, or a
`list-panes` failed this tick — it polls. The gate closes on evidence, never on
a guess.

While polling is paused the header dot goes hollow (`○` dim) instead of solid,
so the frame tmux replays when you switch back tells you the list is a moment
stale rather than pretending it is live. It is not an error: a failed poll is
still a red dot and a footer message.

To keep a tab polling regardless, keep it on screen. There is no flag to
disable the gate — a sidebar nobody can see has nothing to show.

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
