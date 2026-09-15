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
                                  // PRESENT-OR-NULL IS A DIFFERENT QUESTION, and
                                  // it IS reliable - see below.
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

**`pid` PRESENCE IS THE LIVENESS FLAG, AND IT IS THE ONLY ONE. Its VALUE is
still forbidden.** These are two different readings of one key and the ban
covers exactly one of them.

* **Forbidden, unchanged:** the pid as an IDENTITY. Do not remember one, compare
  two, match a row to a process by one, or carry one across a poll. It changes
  across attach/detach, so anything keyed on the value is wrong the moment a
  pane opens. `sessionId` is the only key there is.
* **Sound, and now relied on:** `pid` present ⟺ a worker process is running for
  that session. Measured on the live fleet 2026-09-09 — 16 rows, **7 with a
  `pid` and 9 with `null`**, and that 7 matched the daemon's worker count
  exactly. Re-measured the same day at 17 rows: 8 live, 9 `null`. A `null` row
  is not stale, it is **dormant** — the session finished and its worker exited;
  it is running no `claude` at all, old or new.

Verified directly rather than inferred: a `done` session's own worker was ended
(`SIGTERM`, pid taken from its own row) and the row went `pid: 1230016,
status: "idle"` -> `pid: null, status` absent, with `state` still `done`. The
same shape the 9 dormant rows are in.

This is what `R`'s paneless population is scoped on (SPEC §8.11). A rule that
looked only at `state` would have called all 14 idle-or-done rows restartable
and, on one keypress, SPAWNED nine workers for sessions that had none — the
opposite of what the verb is for. `model::Session::pid` is therefore an
`Option` read only through `has_worker()`, and the crate reads the value
nowhere. That includes the one place it would have been tempting: when a
`claude respawn` returns an error, `R` polls and asks `has_worker()` whether the
halt landed — never whether the pid CHANGED, which would have been the
forbidden reading arriving through the back door for the sake of one footer
word.

**The presence flag is about the WORKER, not about a client.** A live
`claude attach` neither creates a `pid` nor changes one, and `claude agents
--json` carries no field at all that says a session has a client attached
(the row is exactly `id, cwd, kind, startedAt, sessionId, name, state, status,
pid` — checked 2026-09-09, 18 rows). So "is anyone looking at this session"
cannot be answered from the payload, and `R` answers it from the client process
instead — see §2, *who is attached*.

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
| `claude --bg --resume <sessionId>` | Documented on `--bg`: "With --resume &lt;session-id&gt;, continues that session in the background". Resumes a stopped session as a background worker with **no client and no pane**. Takes the **UUID**, not the short id. Verified on 2.1.266 — see below. | YES |

`claude resume` and `claude list` are **NOT** subcommands — they fall through to the
generic help (verified against a `bogus123` control). Only `attach`, `logs`, `stop`,
`kill`, `rm` are real SUBCOMMANDS. `--resume` is a different thing: a documented
**flag** on `--bg`, not a verb of its own, and `claude --bg --resume <uuid>` works
where `claude resume <uuid>` does not.

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

**`stop` + a RESUME IS HOW AN AGENT CHANGES VERSION, and there is no other way.**
There are two resumes: `attach`, below, which needs a terminal to attach in, and
`--bg --resume`, further down, which needs nothing. Verified 2026-09-04 on
2.1.260. A background worker is a separate,
daemon-owned process (§3) and it keeps the `claude` it was launched with for its
whole life; restarting the attach CLIENT changes nothing about it. `claude stop`
followed by `claude attach` respawns it as a genuinely NEW process, which
resolves `claude` at launch and therefore comes up on whatever the CLI is now:

```
before stop : pid=1941112 replPid=1941127 cliVersion=2.1.260
after stop  : (absent from the roster)
after attach: pid=1956984 replPid=1957001 cliVersion=2.1.260   <- new process
```

**`claude respawn <id>` IS THE RESTART THAT NEEDS NO PANE, and it is the other
half of the version story.** `stop` + `attach` above needs a terminal to attach
in, which is why ccmux could only ever refresh the agents it had open in a pane.
`respawn` is a real subcommand, in the `claude --help` Commands list on 2.1.266:
"Restart a background session, or all of them with `--all`, so it runs the
current Claude Code version."

Verified 2026-09-09 on 2.1.266, on throwaways:

```
before respawn   name='PLFIX-A: reply with…'  startedAt=…072902  pid=1486602  state=done
claude respawn e77b0ddc   -> stdout "respawned e77b0ddc", rc=0, stderr empty
after respawn    name='e77b0ddc'              startedAt=…083178  pid=1491316  state=done
fleet size 18 before and 18 after                            <- no fork, no duplicate
claude logs e77b0ddc  -> the original prompt and its answer  <- conversation intact
```

Same short id, same `sessionId`, one row throughout, a fresh worker that
resolved `claude` at launch.

**IT REPLACED `claude stop` + `claude --bg --resume <sessionId>`, and the reason
is a fork branch, measured.** `--bg --resume` is documented as "continues that
session in the background under the same ID, **or starts a copy and says so when
the session is already running**", and the copy branch exits **0** with the
notice on stdout — so a caller that reads only the exit status counts a
duplicated conversation as a success. ccmux fired the resume immediately after
its own `claude stop`, i.e. inside exactly the window where the daemon may still
call the session running, and roughly one resume in twenty came back as a copy:
an extra live agent on the machine, an extra row in the fleet, silently. Three
different notice texts were observed for the one branch, so text-matching it
would have been a guess:

```
note: session <id> is already running in the background, so this started a copy as <new>.
note: started a copy of that conversation as <new>. To continue a session under its own id, …
note: background session <id> keeps its own saved options, so the flags you passed started a copy as <new>.
```

`respawn` has no such branch, and it takes the same 8-hex short id that
`stop`/`kill`/`rm`/`attach` take, so the uuid/short-id pairing hazard goes with
it. Two calls also meant a stop could land with its partner never sent; one call
cannot.

**`respawn` CHECKS NOTHING, so the caller's gates are load-bearing.** Measured
the same day:

* on a **stopped** session it UN-STOPS it — `pid: null` -> a live pid. `R` must
  refuse `state: "stopped"` itself, or a keypress named "restart" undoes a
  deliberate `Ctrl+X`.
* on a **working** session it interrupts the work mid-flight —
  `state=working status=busy` -> `state=blocked status=idle`.
* it KILLS the session's attach client, exactly as `claude stop` does: a pane
  running `claude attach <id>` printed `Session <id> has exited.` and dropped to
  its post-attach prompt.
* `respawn --all` is therefore never built: it would hit every dormant, stopped
  and working session at once.

**A RESUMED WORKER LOSES ITS DISPLAY NAME AND ITS `startedAt`.** Not specific to
`respawn` — `stop` + `attach` and `--bg --resume` do the same — and not a lost
record. While the resumed worker runs, `name` reads as the bare 8-hex short id
and `startedAt` is the moment of the resume; the persisted name comes BACK as
soon as the session stops again:

```
dispatched       name='PLFIX-A: reply with exactly the word ALPHA…'   startedAt=…072902
after respawn    name='e77b0ddc'                                      startedAt=…083178
after claude stop name='PLFIX-A: reply with exactly the word ALPHA…'  startedAt=…072665
```

There is no way to set it back. The CLI has no rename verb, and `-n/--name`
passed alongside `--resume` does not set one — it triggers the fork branch above
("…keeps its own saved options, so the flags you passed started a copy as…"), so
buying the name back would cost a duplicated conversation. ccmux states the cost
in the README instead (§8.11, *The agents*).

**WHO IS ATTACHED: `/proc/<pid>/cmdline`, because the payload cannot say.**
`claude agents --json` is machine-wide and carries no attached/client field
(§1), while every tmux record ccmux holds is scoped to its own session — so
"does anyone have this session open" has no answer inside either source. The
attach CLIENT is a plain process with argv `["claude", "attach", "<id>"]`
(observed live: six of them, one per ccmux pane the operator had open), and it
is the thing `stop`/`respawn` kill. A flat scan of `/proc` for that argv shape
therefore answers the question across ccmux workspaces, tmux servers and bare
terminals alike. It is read-only, it can only make `R` refuse to act, and it is
NOT §5.4's deleted ancestry walk: no ppid chain, no `list-panes -a`, no
resolving a session to a pane. It cannot see an attach on another machine
against a shared daemon.

**Agents go stale, and by a lot.** Measured on this host with 2.1.258 / 2.1.259 /
2.1.260 installed and 2.1.260 current: of 7 live workers, 4 were on 2.1.251 and 1
on 2.1.247 — only 2 were current. Corroborated from `ps` alone on 2026-09-04: a
`claude bg-pty-host … -- ~/.local/share/claude/versions/2.1.247 --bg-spare …`
started eight days earlier was still hosting a session that was actively running
shell tools, while the CLI on `PATH` was 2.1.260.

**A worker's pid and version are visible without touching any internal file.**
The daemon spawns `claude bg-pty-host … -- <…/versions/X.Y.Z> [--resume <jsonl>|
--bg-spare …]` with the worker as its child, and the worker's `/proc/<pid>/cwd`
is the session's own directory. So `ps` + `/proc` answer both "which process is
this session's worker" and "which version was it launched from". Use that for
verification. `~/.claude/daemon/roster.json` answers the same questions and is
still banned by §5: it is an internal file, and the CLI moved through six
versions in five days.

**`claude stop` on a session that has already finished is a no-op that SUCCEEDS.**
Verified 2026-09-04 on a `state: "done"` session: `claude stop <id>` printed
`stopped <id>`, exited **0**, took 0.60 s, and the session's `state` stayed
`done` — it did not become `stopped`. So a caller that stops a finished session
gets no error to report and nothing changes; `state: "stopped"` is only ever
produced by stopping a session that was RUNNING.

**Stopping a session KILLS its attach clients.** Verified 2026-09-04 in a ccmux
pane: `claude stop <id>` against a session with a live `claude attach` made the
attach print `Session <id> has exited.` and exit rc 0, exactly as `Ctrl+Z` does.
Two consequences. First, anything that stops a session must expect the pane
command wrapped around the attach to run its post-attach path. Second, combined
with the next finding, **`state: "stopped"` and a live `claude attach` cannot
coexist**: attaching a stopped session RESUMES it, so it is `working` or `done`
by the time the pane has drawn. Measured both ways — a session stopped mid-tool
came back `working` on attach, and one stopped after its task had finished came
back `done`.

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

## 9. Codex app-server probes — 2026-09-16

These findings apply to **codex-cli 0.154.0 connecting to app-server 0.153.4**
on this host, with tmux 3.4. All measurements below were made on **2026-09-16
(Asia/Taipei, UTC+08:00)**; clock times in this section are local. The temporary
stdio contention probe also used 0.153.4. These version and date qualifiers
apply to every finding below.

### Method and isolation

Only the explicitly authorized `ccmux-probe` tmux socket was used, overriding
the older socket example in §8 for this run. The harness removed `TMUX` and
`TMUX_PANE` from the environment of every tmux invocation and initialized:

```sh
env -u TMUX -u TMUX_PANE tmux -L ccmux-probe -f /dev/null \
  new-session -d -s probe -x 180 -y 48 '/usr/bin/sleep 7200'
```

Every new window used `tmux -L ccmux-probe new-window -d -t =probe:`, with a
command, never an interactive shell. Returned `%N` pane IDs were recorded and
checked against this server before capture, input, or destruction. Each TUI ran
this command inside a noninteractive wrapper; the wrapper recorded its exit
code and parked on `sleep`:

```sh
CODEX_REMOTE_TOKEN="$(cat /home/qdu/.config/agents/codex-serve.token)" \
  codex --remote ws://127.0.0.1:8965 \
  --remote-auth-token-env CODEX_REMOTE_TOKEN resume "$THREAD_ID"
```

The token was read into process memory only, never printed or placed in argv,
probe results, or this repository. RPC probes used Python
`~/.venv/bin/python` with `websockets.sync.client.connect`, a Bearer authorization
header, and proxy bypass for the loopback endpoint. Temporary scripts and
timestamped evidence are under
`~/.local/tmp/ccmux-probe-20260916/` (`probe.py`, `scenarios.py`,
`extra.py`, `watch_grace.py`, `events.jsonl`, and the ownership registry).
The methods and relevant wire parameters are recorded here so the findings
do not depend on retaining those temporary files.

Each connection sent `initialize` with
`clientInfo={"name":"ccmux_probe","version":"2026-09-16"}`,
`capabilities={"experimentalApi":true}`, then the `initialized` notification.
Messages were dispatched by field presence: `method` denotes a server
notification/request; a response has `id` without `method`. Only requests
belonging to owned probe threads could be held or answered.

Probe threads used `thread/start` with
`cwd="/home/qdu/.local/tmp/ccmux-probe-20260916/work"`,
`model="gpt-5.6-luna"`, `config={"model_reasoning_effort":"low"}`,
`ephemeral=false`, `sandbox="read-only"`, `approvalPolicy="never"`,
and `approvalsReviewer="user"`. Restrictive developer instructions limited
them to the exact probe task. Each returned `Thread.id` was registered before
any further mutation, then named with `thread/name/set` and a
`ccmux-probe-` prefix. The approval probe alone used `"untrusted"`.
The TUI-created fork/new threads were identified from that owned pane's
`/status` and registered separately. Every model turn used luna/low; the
`/new` materialization turn explicitly overrode the inherited model, effort,
and turn cwd; the thread's own cwd remained the default described below.

### Versions and protocol surface

- **The installed CLI and running server differ.** Commands:
  `codex --version` returned `codex-cli 0.154.0`;
  `/proc/4714/exe --version` returned `codex-cli 0.153.4`.
  PID 4714 was identified read-only by its `app-server` and
  `--listen ws://0.0.0.0:8965` argv. RPC `initialize` reported a user agent
  beginning `meterboard/0.153.4`, and the attached TUI's `/status` showed
  `Remote: ws://127.0.0.1:8965/ (v0.153.4)`. The executable was an old,
  deleted-on-disk binary still running. **Consequence:** generated bindings
  from the installed CLI alone do not establish the server's capabilities.

- **The planned read parameters exist in the running server's schema.**
  Commands:
  `codex app-server generate-json-schema --out ~/.local/tmp/ccmux-codex-schema-20260916`
  and
  `/proc/4714/exe app-server generate-json-schema --out ~/.local/tmp/ccmux-codex-server-schema-20260916`.
  Both schemas contain `thread/list.useStateDbOnly`, `sourceKinds`,
  `sortKey`, `sortDirection`, `thread/read.includeTurns`, and
  `thread/loaded/list.cursor/limit`. The installed schema additionally has
  `thread/list.originators`; the running server's does not.
  **Consequence:** v1 can use the measured read subset without that newer filter.

### Attach-safety gate and client lifecycle

**GATE: PASS.**

- **Two TUIs can attach to one thread and receive the same live turn.**
  Method: launch the wrapper twice for `attach-idle`, producing panes
  `%1` and `%2`; send
  `Reply with exactly multi-attach-ok. Do not use tools.` in `%1`.
  Both panes displayed that prompt and the final `multi-attach-ok`.
  `/status` identified the original full thread ID.
  **Consequence:** same-server multi-attach is supported in this version pair.

- **Active work survives loss of its last subscribed client.**
  Method: send `turn/start` with luna/low and the prompt
  `Run /usr/bin/sleep 90 using the shell execution tool in the foreground.
  Do not background it or shorten the duration. After it exits, reply exactly
  done. Do not inspect files or run any other commands.`
  Wait for `item/started` containing an in-progress
  `/bin/zsh -c '/usr/bin/sleep 90'`, attach a TUI, verify its native client
  process and active display, then close the creating RPC connection.
  The following action removed the last TUI. Fresh observers used only
  `thread/read(includeTurns:false)` and
  `thread/turns/list(limit:2,itemsView:"full")`; they did not resume/subscribe.

  | Owned thread label | Last-client action | Turn ID | Result on the same turn |
  |---|---|---|---|
  | `active-pane-valid` | `tmux -L ccmux-probe kill-pane -t %9`, 01:13:10 | `01a0a60e-b8f4-77f2-8c43-ae49400d6260` | completed 01:14:37; command exit 0; final `done` |
  | `active-sigkill` | `os.kill(2300282, signal.SIGKILL)`, 01:07:54 | `01a0a609-ecfc-76c1-95c1-96bf6a58b588` | completed 01:09:21; command exit 0; final `done` |
  | `active-graceful-valid` | type `/quit`, wait 0.7 s, Enter in `%10`, 01:13:57 | `01a0a60f-7105-72a1-b4d1-6baa0a087335` | completed 01:15:23; command exit 0; final `done` |

  The SIGKILL PID was first verified as the sole native Codex descendant of
  the owned pane's PID; no existing client was targeted. Each fresh observer
  still saw `active{activeFlags:[]}` after client loss. The graceful wrapper
  reported exit code 0 before the turn completed, and the TUI printed
  `Disconnected from this task. Any running work continues.`
  **Consequence:** remote attach and ccmux's pane-closing `x` pass the active-work
  survival requirement for the measured server.

- **Pending approval survives disconnect and is shown on reattach.**
  Method: on `approval-last-client`, use per-thread
  `approvalPolicy:"untrusted"`, `approvalsReviewer:"user"`, and read-only
  sandbox; ask only for
  `/usr/bin/touch /home/qdu/.local/tmp/ccmux-probe-20260916/work/approval-marker`.
  Hold the `item/commandExecution/requestApproval` server request unanswered
  (request ID `0`, turn
  `01a0a608-4c13-7090-afaf-9837b1c4fa96`).
  At 01:06:04, `thread/read` showed
  `active{activeFlags:["waitingOnApproval"]}`; close the last client.
  A fresh observer three seconds later saw the same pending turn and flag.
  Resume in `%3`: the TUI showed the command approval menu.
  Kill `%3` at 01:06:37; resume in `%4`: the menu appeared again.
  Choose the one-time Yes, not the persistent rule option.
  The same command item
  `exec-194ac023-fc41-4879-89d1-e738cbbd0ded` completed with exit 0,
  the turn completed at 01:07:42, and its final answer was `done`.
  **Consequence:** an approval remains actionable after the last client leaves;
  the read-only sidebar does not need to own or answer it.

- **Graceful exit leaves an idle thread loaded.** Method: after moving `%2`
  to another owned thread, send `/quit` to the remaining `attach-idle` TUI
  in `%1`; repeat for `tui-new` in `%2`.
  Both wrappers exited 0, native client PID sets became empty, and immediate
  `thread/read` results remained `idle` with unchanged timestamps.
  **Consequence:** client exit is not a stopped/completed-work status signal.

- **An empty new thread can be loaded but not yet resumable.**
  Method: the first `active-pane` attempt opened a TUI immediately after
  `thread/start`, before its first turn. The TUI exited 1 with
  `invalid paginated history lineage ... missing source rollout`
  (`thread/resume` error `-32600`). After a first turn had started,
  ordinary remote resume succeeded.
  That initial pane kill is excluded from the active-TUI gate evidence.
  That initial turn (`01a0a609-12a6-7560-82dd-e95c5e8d24d4`) made progress
  through two model rounds with zero clients after its creator closed at
  01:06:52. Pane `%7` reattached at 01:08:11; the rollout's `task_complete`
  was at 01:08:23, about 12 s after reattach. It did not complete with zero
  clients. No RPC `completion_check` exists for this turn; review correlated
  the client events with the server rollout. The three gate rows above are
  the separate, supported completion tests.
  A separate first graceful-quit attempt left `/quit` in the composer;
  only the delayed-input repeat with a verified exited client counts above.
  **Consequence:** retain the deferral of Codex `n`; loaded membership alone
  does not guarantee that a brand-new, empty thread has resumable history.

### Idle unload, read side effects, and attach recency

- **Unsubscribed idle threads unloaded after approximately 30 minutes.**
  Method: complete one trivial turn in each of `grace-control` and
  `grace-read`; explicitly `thread/unsubscribe` and close the creating
  connection. Neither had a TUI. The control had no subsequent
  `thread/read` or resume until the unload check. Both were observed through
  `thread/loaded/list` and historical listings; `grace-read` additionally
  received periodic `thread/read(includeTurns:false)`, every 15 seconds from
  01:23:51 through the boundary.

  | Thread | Unsubscribe time | Last loaded sample | First absent sample | Elapsed-time bound |
  |---|---|---|---|---|
  | `grace-control` | 01:02:07.818 | 01:32:07.196 | 01:32:22.211 | (1799.38, 1814.39] s |
  | `grace-read` | 01:02:35.013 | 01:32:22.212 | 01:32:37.226 | (1787.20, 1802.21] s |

  The latter read at 01:32:37 returned `status:{"type":"notLoaded"}`.
  Both threads remained in DB-only historical listing with their original
  timestamps. **Consequence:** recent unloaded rows are necessary to preserve
  the “done, reopen for review” workflow. Metadata reads did not prevent
  the measured idle unload; `notLoaded` does not mean stopped or deleted.

- **The planned reads do not load an unloaded persistent thread.**
  Method: after both controls disappeared from loaded membership, use a fresh
  connection for each of `thread/list` (DB-only, isolated cwd),
  `thread/loaded/list`, and `thread/read(includeTurns:false)`.
  Check loaded membership before and after each, then issue the owned
  control's `thread/unsubscribe` as an assertion.
  At 01:32:49 all three left both controls absent from loaded membership;
  reads/listing returned `notLoaded`, and unsubscribe returned
  `{"status":"notLoaded"}`. No server request arrived.
  **Consequence:** the read-only loaded-plus-history union does not itself
  reload those threads.

- **Ordinary remote resume reopens an unloaded thread without refreshing
  its activity timestamp.** Method: launch the standard TUI wrapper for
  `grace-control` in `%11` at 01:32:49; verify a live native client and the
  original `ready` transcript. The next read returned `idle`, with
  `createdAt=1789491722`, `updatedAt=1789491727`,
  `recencyAt=1789491722` unchanged from the `notLoaded` read.
  `tmux -L ccmux-probe kill-pane -t %11` then left the thread idle.
  **Consequence:** the same attach command supports loaded and unloaded rows;
  attach alone does not extend this thread's recent-history eligibility.

- **Read-only polling does not subscribe to a loaded thread.**
  Method: on a fresh connection, separately call
  `thread/list(limit:3,sourceKinds:[cli,vscode,exec,appServer,unknown],useStateDbOnly:true)`,
  `thread/loaded/list(limit:100)`, and
  `thread/read(threadId:attach-idle,includeTurns:false)`.
  After each, call `thread/unsubscribe` for that owned thread on the same
  connection. Every response was `{"status":"notSubscribed"}`.
  Loaded population remained 12 before/after; the owned thread's metadata
  did not change. The observer received no server requests, only the unrelated
  connection notification `remoteControl/status/changed`.
  **Consequence:** these reads can form a short-lived, non-subscribing poll.
  `thread/unsubscribe` was a probe assertion, not a proposed v1 poll method.

- **Attaching an already-loaded persistent thread did not advance recency.**
  Method: read `attach-idle`, open its two TUIs without sending a turn, then
  read again. Before and after:
  `createdAt=1789491790`, `updatedAt=1789491795`,
  `recencyAt=1789491790`. A real new turn later advanced `updatedAt` to
  `1789491982`. **Consequence:** do not assume viewing a thread renews its
  recent-history window; the loaded union remains necessary.

### Listing freshness, scope, pagination, and cost

- **DB-only listing was fresh for completed probe turns.**
  Method: create `empty-index` and list the isolated cwd in the order
  `useStateDbOnly:true`, `false`, `true`, with the source allowlist below.
  Before any turn, all three omitted it (nine other probe rows).
  Run `Reply with exactly indexed. Do not use tools.`; immediately after
  `turn/completed`, repeat. All three included it (ten rows), with the same
  ID, name, and timestamps. The first post-completion DB-only query took
  2.8 ms and already found it, before the scan query ran.
  Full-population DB-only and scan queries at 01:13:11 also returned identical
  sets of 32 IDs and no differences in
  `updatedAt/createdAt/name/cwd/source/ephemeral`.
  **Consequence:** the measured ordinary write path does not require a
  scan-and-repair poll. Recovery after an external index fault is
  **UNMEASURED**: no live storage was altered to manufacture one.

- **Source is not a reliable “created through this endpoint” label.**
  Method: inspect `Thread.source` on the owned `thread/start`, fork, and new
  results and on listing metadata. All probe threads reported `"vscode"`,
  including those created directly over WebSocket RPC.
  The poll used
  `sourceKinds:["cli","vscode","exec","appServer","unknown"]` and
  `modelProviders:[]`. The logged loaded-outside-listing breakdown reports
  four IDs: three `vscode`, one structured `subAgent`, two ephemeral, all idle
  and recent. **Provenance gap — UNVERIFIED:** the 01:16:18 run predates the
  surviving `extra.py` (mtime 01:18:31), and its JSON-encoded counter keys do
  not match that file's code. The breakdown and its counts cannot be reproduced
  from the retained script and must not be used as acceptance evidence.
  **Consequence:** query loaded IDs separately, read missing metadata,
  and apply the persistent/top-level filters to that union too. Do not narrow
  the historical list to `appServer` source alone.

- **Both list methods paginate with opaque string cursors.**
  Method: compare a `limit:100` baseline with repeated `limit:3` calls,
  passing each returned `nextCursor` unchanged until null. Historical calls
  used `sortKey:"updated_at"`, `sortDirection:"desc"`, the source allowlist,
  and `useStateDbOnly:true`. At 01:16:18, `thread/list` returned 34 unique
  rows across 12 pages (eleven of three, one of one), in descending update
  order. `thread/loaded/list` returned 16 unique IDs across six pages.
  Neither traversal had duplicates or omissions versus its baseline.
  **Consequence:** follow cursors for both methods; a single page is not a
  complete provider observation. These live traversals do not prove an atomic
  snapshot while other clients mutate rows.

- **A complete cold-connection poll cost about 0.42 s here.**
  Method: `scenarios.py budget`, 20 consecutive fresh WebSocket connections,
  each timed from before connect through initialize, loaded-ID pagination,
  historical pagination, missing-ID metadata reads, and close.
  Historical requests used `limit:100`, the filters above, and a client-side
  seven-day cutoff (`updatedAt < now - 7*86400`); traversal stops after
  the first page crossing the cutoff, or after the final page.
  Every sample saw 14 loaded IDs, 32 historical rows in one page, and required
  four `thread/read(includeTurns:false)` calls for loaded IDs absent from
  history. Measured median **412.8 ms**, nearest-rank p95 **422.5 ms**, maximum
  **422.5 ms**; initialize/connect was about 10–22 ms.
  The separate full-population DB query took **60.1 ms**, scan query
  **112.9 ms**. **Consequence:** 1.5 s has headroom for this measured Codex poll,
  but roughly 0.42 s still adds to input latency when run synchronously after
  Claude. Seven days was a measurement parameter, not a newly fixed product
  default. Enforce one deadline across connection, writes, all pages/reads,
  and close in the implementation: the probe's per-request remaining-time
  check does not validate hard-deadline behavior under stalled connect/close.
  Large-history, saturated-server, and failure-path latency are **UNMEASURED**;
  the live server was not overloaded, stopped, or fault-injected.

### Cross-runtime contention and in-TUI identity

- **A second runtime cannot acquire this thread's active writer.**
  Method: while `attach-idle` still had its remote TUIs, start
  `/proc/4714/exe app-server --listen stdio://` as one owned subprocess,
  with a 15 s whole-operation cap and guaranteed process cleanup.
  Initialize it and request
  `thread/resume({"threadId":<attach-idle ID>,"excludeTurns":true})`.
  It returned `-32600`:
  `thread <ID> already has an active writer`.
  Close stdin: the temporary server exited 0; total lifetime was 0.212 s.
  Existing-server status and timestamps were unchanged.
  **Consequence:** same-version writer contention fails explicitly rather than
  silently creating a second writer. A desktop runtime or a differently
  versioned writer is **UNMEASURED**; this test did not launch either.
  Surface remote resume errors instead of declaring a `notLoaded` row
  globally free of writers.

- **All three in-TUI navigation commands can invalidate launch identity.**
  Method: in `%2`, native client PID 2280314, run `/status`,
  `/resume 01a0a60f-f2b6-7680-beee-f25c46a8345f`, `/status`,
  `/fork`, `/status`, `/new`, `/status`.
  The displayed IDs changed from `attach-idle` to `empty-index`, then
  `tui-fork`, then `tui-new` (full IDs below), while the PID and pane stayed
  unchanged. The remote endpoint remained the same.
  Reading that owned process's `/proc/2280314/cmdline` afterwards still showed
  `resume 01a0a605-c1c8-7960-a4c1-c32f0c4b3002`, the original launch target.
  `/new` also reset cwd from the probe directory to `/home/qdu` and model
  from luna/low to the server default, astra/max; no turn was sent using that
  default. `register_new.py` applied `cwd=<probe directory>`, luna, and low
  effort to its materialization `turn/start`, not to thread configuration.
  Model and effort persisted back, but **Thread.cwd stayed `/home/qdu`**:
  live `thread/read` and rollout `session_meta` agree, and listing filtered to
  the probe cwd omits `tui-new`. This was a turn-level override, not a cleanup
  turn that moved the thread.
  **Consequence:** document the accepted v1 limitation that the pane map
  records the launch target. It cannot track current identity through argv,
  and `/new` must not be assumed to inherit the selected thread's settings.
  Its listed cwd is the TUI/server default, not the pane's launch thread's cwd
  or a later turn's cwd override; ccmux must display `Thread.cwd`.

- **Unmaterialized thread timestamps need caution.**
  Method: read the empty `tui-new` twice before its first turn.
  `createdAt/updatedAt/recencyAt` were all `1789492894` at 01:21:34 and
  all `1789492927` at 01:22:07. After its first persisted turn,
  `createdAt` became `1789492788` (the actual `/new` time).
  The empty fork was likewise absent from DB-only history until its first
  new turn, though it was loaded and had a visible inherited transcript.
  **Consequence:** do not interpret fallback metadata for empty loaded threads
  as durable activity timestamps. This does not change the measured stable
  timestamps of persisted threads.

### Cleanup and probe thread inventory

At 01:33:58 all 12 owned threads were checked for their probe names and absence
of active turns. `tmux -L ccmux-probe kill-server` removed the throwaway server;
a subsequent `list-sessions` returned 1 with “no server running”, and its
remaining tracked native client was gone. Each registered thread then received
`thread/archive({"threadId":<owned ID>})`, the owned fork before its parent.
All 12 calls succeeded. Paginated DB-only listings with `archived:true` and
`archived:false` found all 12 in the former and none in the latter.
No thread was deleted.

The original server was still PID 4714 and reported `codex-cli 0.153.4` after
cleanup. No default tmux command, existing-client kill, service-management
command, or global configuration change was performed. The bounded stdio process had
already exited 0.

All names have the `ccmux-probe-` prefix. The following are **Thread.id**
values, not transport/session IDs.

| Label after the prefix | Thread.id | Final state |
|---|---|---|
| `grace-control` | `01a0a604-b81e-7970-862e-1b57affdcf1c` | archived |
| `grace-read` | `01a0a605-2256-7892-b440-34956927be79` | archived |
| `attach-idle` | `01a0a605-c1c8-7960-a4c1-c32f0c4b3002` | archived |
| `approval-last-client` | `01a0a608-49ee-7c03-8adc-dd20d358b8d5` | archived |
| `active-pane` | `01a0a609-04bf-75f2-8507-6714b009d9fa` | archived |
| `active-sigkill` | `01a0a609-eada-7b83-9709-14281a2c69f6` | archived |
| `active-graceful` | `01a0a60a-ba5c-7c91-852b-3dfe989b6363` | archived |
| `active-pane-valid` | `01a0a60e-b6cc-7dc1-ae92-64fce28cdfd7` | archived |
| `active-graceful-valid` | `01a0a60f-6edf-7d61-9869-cb25e480adb4` | archived |
| `empty-index` | `01a0a60f-f2b6-7680-beee-f25c46a8345f` | archived |
| `tui-fork` | `01a0a613-17cd-7d81-bdd5-e028a714dcde` | archived |
| `tui-new` | `01a0a614-fbfa-7ec3-b3bf-ee9c88ef2dd9` | archived |

### Decision for the next stage

**PASS — the attach-safety gate is satisfied for CLI 0.154.0 / server 0.153.4.**
The same active turns completed after pane kill, SIGKILL, and graceful quit;
pending approval reappeared on reattach; an idle thread unloaded naturally and
resumed through the ordinary remote command.

The approved read-only listing + attach + `x` scope remains viable. The SPEC
amendment must carry the measured version skew, empty-thread resume failure,
unchanged attach timestamps, approximately 30-minute unload, and launch-target
pane-map limitation. The 1.5 s poll budget has measured headroom here, not a
general latency guarantee. The explicitly UNMEASURED cases above remain
evidence limits. This stage changes only this findings document.
