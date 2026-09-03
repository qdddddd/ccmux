# ccmux — verified environment findings

Everything below was **empirically verified** on this machine (devhost, Claude Code
2.1.241/2.1.245, tmux 3.4) before design started. Do not contradict it. Do not
re-test destructively against live sessions.

## 1. Data source: `claude agents --json`

Returns a JSON array. Add `--all` to include completed sessions. Cost: **0.21s** per
invocation (measured 3x) — polling every 2–3s is fine.

Element shape (keys union across all observed rows):

```json
{
  "pid": 2877291,                 // UNSTABLE - changes across attach/detach. NEVER key on this.
  "id": "1c45d64f",               // 8-hex short id. ABSENT for kind=="interactive".
  "cwd": "/home/dev/projects/...",
  "kind": "background",           // "background" | "interactive"
  "startedAt": 1787626475282,     // epoch ms
  "sessionId": "1c45d64f-9bba-4038-8de7-d5f112c92360",  // UUID, stable
  "name": "bt/reg-update",        // live-updating; Claude renames sessions as work evolves
  "status": "busy",               // "busy" | "idle" | "waiting". OPTIONAL:
                                  // the key is absent on some rows.
  "waitingFor": "input needed",   // OPTIONAL, and only seen alongside
                                  // status=="waiting". Free text; observed
                                  // "permission prompt" and "input needed"
                                  // (verified 2026-08-31).
  "state": "working"              // "working" | "done" | "stopped" | "blocked";
                                  // ABSENT for kind=="interactive".
                                  // "stopped" = halted by `claude stop`; the
                                  // conversation is kept and `claude attach`
                                  // resumes it (verified). Do NOT treat it as
                                  // an error state.
                                  // "blocked" = running, but stopped at a
                                  // permission prompt or a question and
                                  // WAITING ON THE OPERATOR (verified). It is
                                  // the most urgent row on the screen, not an
                                  // error and not an unknown.
}
```

**`state` and `status` are two INDEPENDENT axes. Do not infer either from the
other.** It is tempting to read `blocked` and `waiting` as two spellings of one
fact, and ccmux was briefly written as though they were. They are not. A census
of the live fleet on 2026-08-31 (17 sessions, all `kind: "background"`):

| `state` | `status` | rows |
|---|---|---|
| `done` | *(absent)* | 9 |
| `done` | `idle` | 5 |
| `blocked` | `waiting` | 1 |
| `blocked` | `idle` | 1 |
| `working` | `busy` | 1 |

Two consequences, both of which ccmux has had to be corrected for:

* **A `blocked` row need not carry `status: "waiting"`.** One of the two live
  blocked rows (`629da7fc Kernel bugs investigation`) reported `status: "idle"`
  and carried no `waitingFor` key at all. Anything that keys "needs a human" off
  `status` alone will miss half of them.
* **`status` is not absent on every `done` row.** 5 of the 14 `done` rows
  carried `status: "idle"`. What is true is the weaker statement: `status` is
  absent on *some* rows (9 of 17 here), and an absent `status` is not an
  unrecognised one — it must not be reported as drift.

**This key list has been INCOMPLETE twice.** The original §1 said
`"working" | "done"`; `stopped` was added after it shipped unmodelled, and
`blocked` after it shipped unmodelled a second time — both blocked sessions were
rendering as a purple `?` under **Idle** when it was found. Treat the vocabulary
here as what has been OBSERVED, never as what the CLI can emit. ccmux's guard
against the third occurrence is `App::note_drift` (a named warning in the
footer, repeated until it has actually been on screen uncovered) plus
`cargo test -- --ignored live_state_and_status`, which re-asks the running fleet
this exact question.

Grouping used by the stock fleet view: **Working** (state=working), **Idle**,
**Completed** (state=done). ccmux mirrors it with one deliberate addition:
**Blocked** sorted ABOVE Working, because it is the only group that cannot make
progress without a human. Its rule is `state == blocked`, OR `status ==
waiting` at any state that is not already terminal — the second clause is
forward-compat for a CLI that renames the state value again, and the terminal
exclusion is why a `done` row carrying a stale status word is not hoisted.

## 2. Session verbs (hidden subcommands — NOT in `claude --help` Commands list)

| Command | Behavior | Verified |
|---|---|---|
| `claude attach <id>` | Opens the background session in the current terminal, full TUI fidelity, transcript restored. `←` returns to agent view, `Ctrl+Z` **exits the process** (see below). Session keeps running either way. | YES |
| `claude logs <id>` | Prints recent terminal output as a **raw ANSI/PTY dump** (includes alt-screen setup, cursor moves). Needs VT stripping before it can be shown in a sidebar preview. | YES |
| `claude stop <id>` | Stops the session; conversation kept; resume later with `claude attach <id>`. | YES |
| `claude kill <id>` | Alias of `stop`. | YES |
| `claude rm <id>` | `claude rm --help`, verbatim: "Delete a background session and its worktree. Unlike `stop`, works on already-exited sessions." **IRREVERSIBLE** — it removes the git worktree, so uncommitted work in it is gone. Verified on 2.1.246. | YES |

`claude resume` and `claude list` are **NOT** subcommands — they fall through to the
generic help (verified against a `bogus123` control). Only `attach`, `logs`, `stop`,
`kill`, `rm` are real.

**`rm` REFUSES rather than destroying unpushed work, and it explains itself on
STDOUT.** Verified on 2.1.246: against a worktree holding commits that are not
pushed anywhere, or uncommitted changes, `claude rm <id>` prints

```
kept 35f940dd — worktree has commits that are not pushed anywhere
  worktree kept at /home/dev/.local/tmp/ccmux-cx-live/.claude/worktrees/haiku-notes-a
  resolve that (commit/push, or remove the worktree), then run 'claude rm 35f940dd' again
```

on **stdout**, leaves **stderr empty**, and exits **1**. Anything that surfaces a
failed `rm` by reading stderr alone therefore renders `exit 1` and drops the only
sentence that says the work is safe. Read stdout when stderr is blank. A `rm`
whose worktree is clean (or which has none) deletes the session and removes the
worktree from `git worktree list`, verified both ways.

`rm` is hidden from `claude --help`'s Commands list exactly as the other four are.
It was added to this table on 2026-08-27, after four implementers had already built
against a version of §2 that listed only `attach`/`logs`/`stop`/`kill`: that list was
INCOMPLETE, not exhaustive. `stop` and `rm` are not interchangeable — `stop` is the
recoverable verb (the conversation survives, `claude attach <id>` resumes it) and `rm`
is the one that cannot be undone. Anything that offers both must make which is which
unmistakable at the moment of the keypress.

## 3. Safety properties (both verified — these make the design safe)

- **Killing the tmux pane does NOT kill the agent — FOR BACKGROUND SESSIONS ONLY.**
  Attached session in a pane, `tmux kill-pane`, agent survived with the same pid still
  `state=working`. This was tested against a **daemon-owned `kind: background`**
  session, and the result holds *because* the agent process is owned by the daemon and
  is not a child of the pane.
  **It does NOT generalize to `kind: interactive` sessions.** Per §4, an interactive
  session's process IS a descendant of its pane's pid, so `kill-pane` WOULD kill it and
  destroy in-flight work. Treat "close pane" as safe for background sessions and as
  DESTRUCTIVE — refuse it, or require confirmation — for interactive ones.
- **Double-attach is allowed.** The same session attached in two panes simultaneously
  works; both render live, no error, agent unaffected. So "jump to existing pane" is a
  UX preference, not a correctness requirement.
- **`Ctrl+Z` in an attached pane is an EXIT, not a suspend.** Corrected 2026-09-03,
  after the earlier wording ("drops back to shell", "detaches cleanly") was read as
  a job-control suspend and was not. `claude attach` holds the tty in RAW MODE, so
  the terminal never generates `SIGTSTP`: `Ctrl+Z` arrives as the byte `0x1A`, claude
  treats it as detach, and the process **exits with rc 0**. Verified under a full
  interactive shell with job control — no stopped job appears, `jobs` is empty, and
  there is nothing for `fg` to resume. The session keeps running either way, and
  `claude attach <id>` re-opens it with the transcript restored.
  The signal path itself is fine and is not what is missing: sending `SIGTSTP`
  directly DOES stop the process (state `T`), and `SIGCONT` resumes it. claude simply
  never receives one.
  **Consequence for ccmux**: whatever the pane command runs after the attach runs on
  the operator's most ordinary keypress, not only on a crash or a vanished session.
  It parks the pane on a prompt (SPEC §3.3) where enter re-runs the same attach,
  `s` hands over to the operator's shell and `q` closes the pane; "resume" is
  re-running `claude attach <id>` — never `fg`.
- **An interactive shell in a ccmux pane runs the operator's rc against CCMUX'S tmux
  server.** Verified 2026-09-03 on tmux 3.4, throwaway socket: a server holding one
  session and two panes was split once with `[ -x "${SHELL:-}" ] || SHELL=/bin/sh;
  exec "$SHELL" -l` and, eight seconds later, held **three sessions and fifteen
  panes**. The pane's `$TMUX` names ccmux's server, `~/.zshrc` reaches a helper that
  runs `tmux new -s dev -d` followed by tmux-resurrect's `restore.sh`, and the saved
  workspace was restored over the live session — window renamed, panes injected,
  layout and size overwritten. Reproduced with `exec "$SHELL"` (no `-l`) too: an
  interactive zsh reads `~/.zshrc` either way, so the login flag is not the trigger.
  **Consequence for ccmux**: it does not start an interactive shell on its own
  initiative anywhere. Every pane it creates is created WITH a command, and the one
  handover to `$SHELL` is behind the operator's `s` (SPEC §3.3).
- **A pane parked on `read` cannot be frozen by `Ctrl+Z`.** Verified on 3.4: the byte
  reaches the tty in canonical mode, but the pane command's process group is orphaned
  (its parent is the tmux server, in another session), and POSIX has the kernel
  DISCARD stop signals for an orphaned process group. Measured `STAT=S`, never `T`,
  and the following enter was read normally. So the resume prompt cannot be left
  hung by the same key that reached it.

## 4. Session -> tmux pane resolution

- **Background sessions are daemon-owned.** Their pid is NOT a descendant of any tmux
  pane pid. They can only be opened via `claude attach <id>`.
- **Interactive sessions ARE descendants of their pane's pid.** Resolve by walking
  `/proc/<pid>/stat` ppid chain up and matching against `#{pane_pid}` from
  `tmux list-panes -a`. Verified: interactive session pid 2936154 resolved to pane
  `agents:3.1`.
- **`#{pane_current_command}` cannot even see the attach.** tmux resolves it from
  the pane's FOREGROUND PROCESS GROUP LEADER (`tcgetpgrp`), and tmux runs a pane
  command as `$SHELL -c '<string>'`, which — being non-interactive — never hands the
  terminal to its child. Measured on tmux 3.4, same window, same second:

  | pane command | `#{pane_current_command}` |
  |---|---|
  | `claude attach <id>; rc=$?; …` (TUI live on screen) | `zsh` |
  | `sleep 60; read _` | `zsh` |
  | `exec sleep 60` | `sleep` |
  | `exec /usr/bin/zsh -l` | `zsh` |

  `set -m` does not rescue it: zsh puts the child in its own process group but never
  `tcsetpgrp`s, so the child is stopped on `SIGTTIN` (observed `STAT=T`, `TPGID` still
  the wrapper's) and the pane hangs blank. So a pane running `claude attach` and a
  pane that has fallen through to a shell are INDISTINGUISHABLE by that format. What
  distinguishes them is a PANE-scoped user option the pane sets about itself on the
  way out (`@ccmux_detached`, SPEC §3.3/§5.3). `set-option -p` with no `-t` resolves
  to the session's ACTIVE pane — verified, it marked the sidebar — so it must be
  `-t "$TMUX_PANE"`. An EMPTY `-t` is the same case and, measured, does NOT fail:
  `set-option -p -t "" @ccmux_detached 1` returns rc 0 and marks the active pane. A
  `2>/dev/null` therefore hides nothing there, and the template guards the call with
  `[ -n "$TMUX_PANE" ]` so that an absent pane id means no latch at all.
  An interactive shell DOES hand the terminal over, so a command the operator runs at
  a prompt is visible there. ccmux still does not read it: see the next bullet.
- **A pane's cmdline does NOT identify which session it displays.** Panes opened from
  the stock fleet view all show cmdline `claude agents` (one process that switches
  surfaces). Therefore **ccmux must own its own `pane_id -> session` map**, keyed on
  tmux `#{pane_id}` (the `%N` form — stable across window/pane renumbering).
  Reconcile the map each tick against `tmux list-panes -a -F '#{pane_id}'`.

## 5. Hard constraint: documented surfaces only

Build ONLY on: `claude agents --json`, `claude attach|logs|stop`, `claude --bg`,
`claude --resume`, `claude --session-id`, `claude --fork-session`, and tmux commands.

Do NOT build on these internal, version-churning surfaces:
`~/.claude/daemon/roster.json`, `ptySock`, `rendezvousSock`, `~/.claude/daemon/dispatch/`,
`~/.claude/daemon/attach-journal/`, `CLAUDE_CODE_MESSAGING_SOCKET`.
Three CLI versions already sit side by side (2.1.241, 2.1.243, 2.1.245); internals move.

## 6. Host environment

- tmux 3.4. Prefix `C-q`. `mode-keys vi`. `mouse on`. `base-index 1`, `pane-base-index 1`.
  `renumber-windows on`. vim-tmux-navigator style `bind-key h/j/k/l` pane nav already configured.
- Rust 1.94, edition 2024. Match the user's existing TUI stack (`~/projects/slurm-tui`):
  `ratatui 0.29`, `crossterm 0.28`, `clap 4 (derive)`, `chrono 0.4`, `dirs 6`.
- Terminal is 274x76 in the reference window.

## 7. Reference UI (the target — user pointed at their tmux window `agents:2`)

Layout string: `274x76,0,0{103x76,0,0, 102x76,104,0, 67x76,207,0[67x37, 67x38]}`

```
+----------+---------------------+---------------------+
| SESSIONS | prediction analy... | bt/reg-update       |
| (sidebar)|  live claude TUI    |  live claude TUI    |
| Working  |                     |                     |
|  run-a   |  * Thundering... 3m |  * Bunning... 16m   |
|  kernel  |  >                  |  >                  |
| Idle     |                     |                     |
|  alpha   |                     |                     |
+----------+---------------------+---------------------+
```

The user wants the session explorer **on the LEFT** (their hand-built reference has it
on the right; left is the explicit request), persistently visible, with the live
Claude TUIs filling the remaining space as splits.

---

## 8. TESTING POLICY — MANDATORY. READ TWICE. THIS SECTION OVERRIDES YOUR PROMPT.

### What already went wrong — the incident this rule exists to prevent

During spec authoring an agent ran this, intending to work in its own throwaway session:

```sh
SIDEBAR=$(tmux list-panes -t "$S:cc" -F '#{pane_id}' | head -1)   # <-- FAILED, printed nothing
P1=$(tmux split-window -h -t "$SIDEBAR" ...)                       # <-- -t "" 
```

The target lookup failed, so `SIDEBAR` was **empty**. `tmux split-window -t ""` does
not error — **tmux silently defaults an empty or omitted `-t` to the caller's current
pane**, and the agent's shell inherits `$TMUX` from the user's live session. Three
stray panes were created inside `agents:2`, the user's live window running three real
Claude sessions. The agent's attempt to clean them up was blocked, and **the user had
to delete them by hand.** They then instructed: do not manipulate my panes.

Naming the right target is NOT sufficient protection. The bug was an empty variable,
not a wrong name. So the rule below is structural, not a matter of care.

### RULE T1 — all testing runs on a SEPARATE TMUX SERVER

Every mutating tmux command you run for testing MUST carry `-L ccmux`:

```sh
tmux -L ccmux new-session -d -s test -x 200 -y 50
tmux -L ccmux split-window -h -t test:1
tmux -L ccmux capture-pane -p -t test:1.1
tmux -L ccmux kill-server          # your cleanup: safe, wipes only YOUR server
```

`-L ccmux` selects a different socket. That server contains **only panes you created**.
An empty `-t`, a typo'd name, a failed lookup, `kill-server` — none of it can reach the
user's panes, because the user's panes live on a different socket entirely. This makes
the incident above impossible by construction rather than by discipline.

**Never run a mutating bare `tmux ...` command** (no `-L`) — that is the user's server.
Forbidden on the default socket: `split-window`, `kill-pane`, `kill-window`,
`kill-session`, `kill-server`, `resize-pane`, `respawn-pane`, `send-keys`,
`select-pane`, `select-window`, `select-layout`, `switch-client`, `set-option`,
`new-window`, `new-session`.

### RULE T2 — read-only on the default socket is allowed

ccmux must resolve real sessions on the user's server, so these remain permitted
WITHOUT `-L`: `list-panes`, `list-windows`, `list-sessions`, `display-message -p`,
`capture-pane -p`, `show-options -v`, `has-session`. Nothing else.

### RULE T3 — never inherit the ambient session

At the top of every test script, unset the inherited context so a missing `-t` cannot
resolve to one of the user's panes:

```sh
env -u TMUX -u TMUX_PANE tmux -L ccmux <command>
```

### RULE T4 — always capture and verify the target before mutating

Never pass an unvalidated variable to `-t`. Guard every one:

```sh
[ -n "$TARGET" ] || { echo "FATAL: empty target, refusing"; exit 1; }
```

### The user's scratch tab

`agents:ccmux-test` exists on the user's server (created for visibility). You may NOT
split, kill, or send-keys into it — it is on the default socket. It is there so the
*user* can watch your test server by running `tmux -L ccmux attach` in it themselves.

### Off limits, absolutely

Session `agents` (windows 1, 2, 3, and the `ccmux-test` tab) and session `dev`, on the
default socket. Never steal focus: no `select-window`, `switch-client`, or
`attach-session` against the attached client.

### Testing ccmux's own launcher

`ccmux` must accept a socket override (`-L/--socket`, default none) precisely so it can
be tested here. Test it as `ccmux --socket ccmux ...`, never against the default socket.
