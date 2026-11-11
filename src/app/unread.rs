//! The unread mark (SPEC §6.4, §11.6): a row whose agent finished a turn that
//! nobody has looked at since.
//!
//! Three parts, each in memory except the log:
//!   * DETECTION — `last_running` remembers, per session, the wall-clock ms of
//!     the latest poll that saw it running. The poll that first sees it
//!     finished turns that stamp into an unread ADD.
//!   * THE SHARED SET — every tab's sidebar writes its own `@ccmux_tab_unread`
//!     fragment, and the set is the fold of all of them, exactly as the
//!     dismissal log works (§11.3). Only the on-screen sidebar polls, so a mark
//!     must be visible in every other tab and survive a sidebar restart.
//!   * THE SEEN RULE — a session on screen anywhere, or opened from the
//!     sidebar, gets a SEEN op (`add: false`) that outranks the add.
//!
//! STAMPS are what make the set safe against a sidebar that comes back on
//! screen holding a stale observation. An add carries the BACK-DATED stamp of
//! the last poll that saw the turn running (raised past a look at the running
//! row, `raise_past_look`), never a Lamport bump; a seen carries
//! a Lamport stamp, strictly above every add folded so far. A sidebar whose last
//! look at a session predates the operator's look therefore mints an add that
//! loses to the seen already in the log.

use super::*;
use crate::model::{CodexStatus, State, Status};
use crate::tmux::UnreadLog;
use std::collections::HashMap;

/// Everything the unread mark keeps, as one field of `App`.
#[derive(Debug, Default)]
pub struct Unread {
    /// session id -> (provider, ms of the latest poll that saw it running).
    /// In memory only: a fresh process has observed nothing, so it never
    /// invents a mark at startup.
    pub last_running: BTreeMap<String, (Provider, u64)>,
    /// MY window's fragment — the only thing this process writes to
    /// `@ccmux_tab_unread`.
    pub log: UnreadLog,
    /// The fold of every fragment: the unread ids, with the provider each was
    /// minted for. What `ui` draws the mark from.
    pub set: BTreeMap<String, Provider>,
    /// Flushed by the next tick, or by `shutdown`.
    pub dirty: bool,
    /// Per provider, the ids in my fragment (or in `last_running`) missing
    /// from that provider's previous complete observation — the first of the
    /// two strikes that retire them. Keyed by provider so one provider's poll
    /// never resets the other's strikes.
    pub absent: HashMap<Provider, BTreeSet<String>>,
    /// Every other tab's fragment as of the previous refresh, for orphan
    /// adoption.
    pub last_frags: BTreeMap<u64, UnreadLog>,
    /// Ids last known finished WHILE ON SCREEN — by a poll, or by a tick that
    /// found the look begun against a finished snapshot. The edge detector for
    /// the watermark seen (`observe_turns`, `tick_unread_seen`): one seen per
    /// look, not one per poll or tick. An id leaves when its pane leaves the
    /// screen (`tick_unread_seen`).
    pub viewed_done: BTreeMap<String, Provider>,
    /// Per provider, the ids its most recent poll returned while this sidebar
    /// was polling. Emptied by that provider's failed poll and by a quiesced
    /// tick. Only these rows' snapshot is trusted to say what a look saw
    /// (`snapshot_trusted`); any other row may be minutes old or a cached copy
    /// an incomplete Codex observation did not re-read.
    pub fresh: HashMap<Provider, BTreeSet<String>>,
    /// Codex threads whose running turn `Ctrl+X` interrupted. The operator
    /// ended that turn, so its end is no mark — Claude reports the same as
    /// `state: stopped`, Codex only as `idle`. An id leaves at the first
    /// observation that finds it not running; until then a lagging `active`
    /// row does not re-arm `last_running`.
    pub stopped: BTreeSet<String>,
}

/// What one observation says about a row's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// Working right now.
    Running,
    /// The turn ended on its own and the agent is waiting for the next one.
    Finished,
    /// Anything else: stopped by the operator, blocked on a question (already
    /// surfaced by `▲` at the top of the list), or a value this build does not
    /// model. Ends the tracked turn without a mark.
    Other,
}

/// Classify one row. `group() == Working` is NOT "running": it also holds
/// `state: working, status: idle`, which is a session between turns.
pub fn turn_of(s: &Session) -> Turn {
    if s.provider == Provider::Codex {
        return match s.codex.as_ref().map(|m| &m.runtime) {
            // Unknown non-waiting flags still normalise to `Working`.
            Some(CodexStatus::Active { .. }) if s.state == Some(State::Working) => Turn::Running,
            Some(CodexStatus::Idle | CodexStatus::NotLoaded | CodexStatus::SystemError) => Turn::Finished,
            _ => Turn::Other,
        };
    }
    match &s.state {
        Some(State::Done) => Turn::Finished,
        Some(State::Stopped | State::Blocked | State::Unloaded) => Turn::Other,
        Some(State::Working) | Some(State::Unknown(_)) | None => match s.status {
            Status::Busy => Turn::Running,
            Status::Idle => Turn::Finished,
            Status::Waiting | Status::Unknown(_) => Turn::Other,
        },
    }
}

/// Is this pane on somebody's screen right now? The window must be rendered —
/// the same expression `App::watchers` reads off the sidebar's own row — and
/// in a zoomed window only the zoomed (active) pane is drawn.
pub fn pane_on_screen(info: &PaneInfo) -> bool {
    let rendered = info.window_viewers.map_or(info.session_clients > 0 && info.window_active, |v| v > 0);
    rendered && (!info.window_zoomed || info.active)
}

impl App {
    /// Is `id` unread? What `ui` draws the mark from.
    pub fn is_unread(&self, provider: Provider, id: &str) -> bool {
        self.unread.set.get(id) == Some(&provider)
    }

    /// Fold one provider's fresh observation into the turn tracker, minting
    /// marks for turns that ended. `rows` must be rows THIS observation of
    /// `provider` actually returned: an absent row, a failed poll or a skipped
    /// one says nothing about a turn and leaves its stamp alone. They also
    /// become `provider`'s fresh set.
    pub(super) fn observe_turns(&mut self, provider: Provider, rows: &[Session]) {
        let now = self.now_ms.max(0) as u64;
        let mut changed = false;
        self.unread.fresh.insert(provider, rows.iter().map(|s| s.session_id.clone()).collect());
        for s in rows {
            let id = &s.session_id;
            let turn = turn_of(s);
            if turn != Turn::Finished {
                self.unread.viewed_done.remove(id);
            }
            if turn != Turn::Running {
                self.unread.stopped.remove(id);
            }
            match turn {
                Turn::Running if self.unread.stopped.contains(id) => {}
                Turn::Running => {
                    // Never lowered: a look may have raised it past `now`
                    // (`raise_past_look`).
                    let ms = self.unread.last_running.get(id).map_or(now, |&(_, ms)| ms.max(now));
                    self.unread.last_running.insert(id.clone(), (s.provider, ms));
                }
                Turn::Finished => {
                    let stamp = self.unread.last_running.remove(id);
                    if self.session_on_screen(s.provider, id) {
                        // On screen with the turn over: no mark, and a SEEN
                        // rather than nothing. It is the watermark that keeps
                        // a sidebar in another tab, still holding an older
                        // running stamp for this session, from marking a turn
                        // the operator has already read — whether THIS process
                        // saw the turn end (a stamp) or only sees it ended (a
                        // fresh look, edge-triggered so a long look writes
                        // once).
                        let fresh = self.unread.viewed_done.insert(id.clone(), s.provider).is_none();
                        if stamp.is_some() || fresh {
                            self.mint_seen(s.provider, id);
                            changed = true;
                        }
                    } else {
                        self.unread.viewed_done.remove(id);
                        if let Some((_, ms)) = stamp {
                            self.mint_add(s.provider, id, ms);
                            changed = true;
                        }
                    }
                }
                Turn::Other => {
                    self.unread.last_running.remove(id);
                }
            }
        }
        if changed {
            self.refold_unread();
        }
    }

    /// An unread ADD, stamped with the last-running ms, never Lamport-bumped.
    fn mint_add(&mut self, provider: Provider, id: &str, seq: u64) {
        let org = self.own_org();
        if self.unread.log.push(HiddenOp { provider, id: id.to_string(), add: true, seq, org }) {
            self.unread.dirty = true;
        }
        // Folded into the clock at push time, so a seen minted on this same
        // tick cannot tie with it — a tie on `seq` would let the add win.
        self.seq_seen = self.seq_seen.max(seq);
    }

    /// A SEEN op, Lamport-stamped: strictly above every stamp folded so far,
    /// unread adds included.
    fn mint_seen(&mut self, provider: Provider, id: &str) {
        let (seq, org) = (self.next_seq(), self.own_org());
        if self.unread.log.push(HiddenOp { provider, id: id.to_string(), add: false, seq, org }) {
            self.unread.dirty = true;
        }
        self.raise_past_look(provider, id);
    }

    /// A look at a row whose trusted snapshot has it RUNNING reads the mark
    /// of an earlier turn, not the end of this one. Raise its running stamp
    /// strictly above every stamp folded so far — the look's own seen, and
    /// any other tab's seen for the same look — so the add that closes this
    /// turn still outranks them. A stale snapshot raises nothing: an
    /// off-screen tab's old running stamp must keep losing to the seen.
    fn raise_past_look(&mut self, provider: Provider, id: &str) {
        if !self.snapshot_trusted(provider, id) {
            return;
        }
        let floor = self.seq_seen.saturating_add(1);
        if let Some((_, ms)) = self.unread.last_running.get_mut(id) {
            *ms = (*ms).max(floor);
        }
    }

    /// Does this sidebar's snapshot of `id` say what is on screen now? Only
    /// while it is polling, and only for a row the provider's most recent
    /// poll succeeded and returned.
    fn snapshot_trusted(&self, provider: Provider, id: &str) -> bool {
        !self.quiesced && self.unread.fresh.get(&provider).is_some_and(|ids| ids.contains(id))
    }

    /// `provider`'s poll failed: none of its rows' snapshots is trusted until
    /// one succeeds.
    pub(super) fn unread_poll_failed(&mut self, provider: Provider) {
        self.unread.fresh.remove(&provider);
    }

    /// `Ctrl+X` interrupted a running Codex turn: the operator ended it, so it
    /// is not marked (`Unread::stopped`).
    pub(super) fn unread_turn_stopped(&mut self, id: &str) {
        self.unread.last_running.remove(id);
        self.unread.stopped.insert(id.to_string());
    }

    /// Is any live pane mapped to this session, in ANY tab's map, on screen?
    /// A pane latched `@ccmux_detached` (parked by `Ctrl+Z`, or handed to a
    /// shell) does NOT count: the attach has exited and the pane shows the
    /// parked prompt or a shell, never the turn that ends after it — and
    /// "park it, let it run, mark it when it finishes" is the case the mark
    /// exists for. `rebuild_open` treats such a pane as not attached for the
    /// same reason. Never answers yes from a stale inventory.
    pub(super) fn session_on_screen(&self, provider: Provider, id: &str) -> bool {
        if self.degraded || !self.panes_fresh {
            return false;
        }
        let mine = self.own_window.as_ref();
        self.tabs
            .iter()
            .filter(|t| Some(&t.window) != mine)
            .map(|t| &t.map)
            .chain(std::iter::once(&self.map))
            .flat_map(|m| m.panes_for_session(provider, id))
            .any(|pane| self.panes.iter().find(|i| i.id == pane).is_some_and(|i| !i.detached && pane_on_screen(i)))
    }

    /// Tick step 4c: every unread session that is on screen anywhere is read,
    /// and a fresh look at a finished row leaves a watermark seen. No tmux
    /// call: it reads the inventory `refresh_panes` already took.
    pub(super) fn tick_unread_seen(&mut self) {
        // A look that has ended re-arms the watermark for the next one.
        let gone: Vec<String> = self
            .unread
            .viewed_done
            .iter()
            .filter(|(id, provider)| !self.session_on_screen(**provider, id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            self.unread.viewed_done.remove(&id);
        }
        // A look that BEGAN since the last poll at a row whose latest snapshot
        // is already finished: the same watermark `observe_turns` mints for a
        // fresh look, taken at the tick so a look shorter than the poll gap
        // still leaves one. Only a sidebar that is polling has a snapshot
        // worth trusting; an off-screen one's may be minutes old. A look at a
        // row last seen RUNNING mints nothing: whether its turn ended during
        // the look is unknown until the next poll (SPEC §11.6 Limits). A row
        // whose provider's last poll failed, or that an incomplete Codex
        // observation did not return, is skipped without consuming its edge,
        // so the next good poll takes the look.
        if self.quiesced {
            self.unread.fresh.clear();
        } else {
            let looks: Vec<(String, Provider)> = self
                .sessions
                .iter()
                .filter(|s| !self.unread.viewed_done.contains_key(&s.session_id))
                .filter(|s| self.snapshot_trusted(s.provider, &s.session_id))
                .filter(|s| turn_of(s) == Turn::Finished && self.session_on_screen(s.provider, &s.session_id))
                .map(|s| (s.session_id.clone(), s.provider))
                .collect();
            let mut minted = false;
            for (id, provider) in looks {
                // An id in the set gets its seen just below.
                if !self.unread.set.contains_key(&id) {
                    self.mint_seen(provider, &id);
                    minted = true;
                }
                self.unread.viewed_done.insert(id, provider);
            }
            if minted {
                self.refold_unread();
            }
        }
        let viewed: Vec<(String, Provider)> = self
            .unread
            .set
            .iter()
            .filter(|(id, provider)| self.session_on_screen(**provider, id))
            .map(|(id, provider)| (id.clone(), *provider))
            .collect();
        if !viewed.is_empty() {
            for (id, provider) in viewed {
                self.mint_seen(provider, &id);
            }
            self.refold_unread();
        }
        // A look at a row still running: whichever tab minted the seen for an
        // earlier turn's mark, the turn in progress keeps its mark.
        let running: Vec<(String, Provider)> = self
            .unread
            .last_running
            .iter()
            .filter(|(id, (provider, _))| self.session_on_screen(*provider, id))
            .map(|(id, (provider, _))| (id.clone(), *provider))
            .collect();
        for (id, provider) in running {
            self.raise_past_look(provider, &id);
        }
    }

    /// `Enter`, `v`, `s` and `t` read the selected row's mark at the keypress,
    /// whatever the verb then manages to do — including in degraded mode,
    /// where that keypress is the only way a mark is ever cleared. In memory;
    /// the write waits for the next tick, like every other keypress.
    pub(super) fn unread_seen_selected(&mut self) {
        let Some((provider, id)) = self.selected_session().map(|s| (s.provider, s.session_id.clone())) else {
            return;
        };
        if self.is_unread(provider, &id) {
            self.mint_seen(provider, &id);
            self.refold_unread();
        }
    }

    /// Recompute the shared unread set and garbage-collect my fragment —
    /// `refold_hidden`'s twin, over every OTHER tab's stored fragment plus my
    /// own in-memory log. Also advances the Lamport clock past every unread
    /// stamp, which is what keeps a seen above every add it is meant to beat.
    pub fn refold_unread(&mut self) {
        let mine = self.own_window.clone();
        let others: Vec<&UnreadLog> = self
            .tabs
            .iter()
            .filter(|t| Some(&t.window) != mine.as_ref())
            .map(|t| &t.unread)
            .collect();
        self.seq_seen = others
            .iter()
            .map(|l| l.max_seq())
            .chain([self.unread.log.max_seq(), self.seq_seen])
            .max()
            .unwrap_or(0);
        if tmux::prune_unread_log(&mut self.unread.log, &others) {
            self.unread.dirty = true;
        }
        self.unread.set = tmux::fold_unread(others.into_iter().chain(std::iter::once(&self.unread.log)));
    }

    /// `/clear` re-keys a session under the same short id: its running stamp
    /// and its winning op move to the new id. The op is copied VERBATIM —
    /// stamp and origin kept — so two sidebars carrying the same rename write
    /// one identity, and a stale tab's late carry cannot outrank a seen.
    pub(super) fn carry_unread(&mut self, old: &str, new: &str) {
        if let Some(stamp) = self.unread.last_running.remove(old) {
            self.unread.last_running.insert(new.to_string(), stamp);
        }
        let mine = self.own_window.clone();
        let winner = tmux::fold_winners(
            self.tabs
                .iter()
                .filter(|t| Some(&t.window) != mine.as_ref())
                .map(|t| &t.unread)
                .chain(std::iter::once(&self.unread.log)),
        )
        .remove(old);
        if let Some(op) = winner
            && self.unread.log.push(HiddenOp { id: new.to_string(), ..op })
        {
            self.unread.dirty = true;
            self.refold_unread();
        }
    }

    /// Retire the unread ops — marks and seen tombstones alike — and the
    /// running stamps and `Ctrl+X` stops of `provider`'s sessions absent from TWO consecutive
    /// complete observations of that provider, the rule `reconcile_hidden`
    /// uses for Claude. A tombstone is otherwise kept for as long as its
    /// session is listed (see `prune_unread_log`). An incomplete, failed or
    /// skipped observation neither adds nor clears a strike.
    ///
    /// Codex ids retire too, unlike Codex dismissals (§12.9): an unread op
    /// whose thread left the listing window costs at most a mark on a row
    /// nobody can see, while keeping them would fill the fragment with
    /// tombstones. The live set is the provider's rows after `apply`, so a
    /// deleted thread the server still returns counts as absent, and hidden
    /// imports are never listed at all.
    pub(super) fn reconcile_unread(&mut self, provider: Provider, complete: bool) {
        if !complete {
            return;
        }
        let live: BTreeSet<&str> = self
            .sessions
            .iter()
            .filter(|s| s.provider == provider)
            .map(|s| s.session_id.as_str())
            .collect();
        let absent: BTreeSet<String> = self
            .unread
            .log
            .ops
            .iter()
            .filter(|o| o.provider == provider)
            .map(|o| &o.id)
            .chain(self.unread.last_running.iter().filter(|(_, (p, _))| *p == provider).map(|(id, _)| id))
            .chain(self.unread.stopped.iter().filter(|_| provider == Provider::Codex))
            .filter(|id| !live.contains(id.as_str()))
            .cloned()
            .collect();
        let strikes = self.unread.absent.insert(provider, absent.clone()).unwrap_or_default();
        let retired: BTreeSet<&String> = absent.intersection(&strikes).collect();
        if retired.is_empty() {
            return;
        }
        self.unread.last_running.retain(|id, (p, _)| *p != provider || !retired.contains(id));
        self.unread.stopped.retain(|id| !retired.contains(id));
        let before = self.unread.log.ops.len();
        self.unread.log.ops.retain(|o| o.provider != provider || !retired.contains(&o.id));
        if self.unread.log.ops.len() != before {
            self.unread.dirty = true;
        }
        self.refold_unread();
    }

    /// Flush `@ccmux_tab_unread`: `save_hidden_now`'s contract exactly — no
    /// write from an un-adopted base, dirty cleared on failure too, and the
    /// operator told. A failed write costs a mark only its survival across a
    /// sidebar restart and its visibility in other tabs.
    pub(super) fn save_unread_now(&mut self) {
        if self.degraded || !self.unread.dirty || !self.own_state_loaded {
            return;
        }
        let (Some(pane), Some(win)) = (self.own_pane.clone(), self.own_window.clone()) else {
            return;
        };
        match tmux::save_tab_unread(&self.tmux_session, &pane, &win, &self.unread.log) {
            Ok(()) => self.unread.dirty = false,
            Err(e) => {
                self.unread.dirty = false;
                self.flash(format!("unread marks not saved: {}", tmux_msg(&e)), MsgLevel::Warn);
            }
        }
    }
}

#[cfg(test)]
mod tests;
