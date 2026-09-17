//! Provider-local polling and diagnostic state (SPEC §12.5–6).

use super::*;
use crate::codex::{self, CodexConfig, CodexDiagnostic, CodexFailureKind, CodexObservation};
use crate::model::{CodexStatus, Status};
use crate::settings::CodexSettings;

pub const DIAGNOSTIC_COOLDOWN: Duration = Duration::from_secs(30);

/// The app owns scheduling; the observation client owns its transport/cache.
/// This seam keeps event-loop tests free of credentials and network IO.
pub trait PollClient {
    fn poll(&mut self, now_ms: i64) -> CodexObservation;
    fn reload(&mut self, config: &CodexConfig) -> Result<(), CodexDiagnostic>;
    fn forget(&mut self, id: &str);
}

impl PollClient for codex::CodexClient {
    fn poll(&mut self, now_ms: i64) -> CodexObservation { self.poll(now_ms) }
    fn reload(&mut self, config: &CodexConfig) -> Result<(), CodexDiagnostic> {
        let prepared = codex::prepare(config).map_err(|e| e.diagnostic().clone())?;
        self.replace_prepared(prepared);
        Ok(())
    }
    fn forget(&mut self, id: &str) { codex::CodexClient::forget(self, id); }
}

fn prepare(config: &CodexConfig) -> Result<Box<dyn PollClient>, CodexDiagnostic> {
    codex::prepare(config).map(|c| Box::new(c) as Box<dyn PollClient>)
        .map_err(|e| e.diagnostic().clone())
}

pub struct CodexPoll {
    pub settings: CodexSettings,
    client: Option<Box<dyn PollClient>>,
    prepared: bool,
    prepare_error: Option<CodexDiagnostic>,
    // Retained after a rejected reload even when an old client still polls.
    pub open_rejected: bool,
    pub last_attempt: Option<Instant>,
    pub force: bool,
    pub startup: bool,
    pub reload: bool,
    pub fail_streak: u32,
    pub idle_streak: u32,
    fingerprint: Option<u64>,
    rows: BTreeMap<String, Session>,
    /// Successful local archives, keyed to the fresh read's `updatedAt`.
    /// `thread/read` remains valid after archive and a lagging loaded-list row
    /// must not resurrect the just-removed row, so the ID stays suppressed
    /// until a complete observation omits it or the authoritative
    /// `archived:false` history lists it with a different `updatedAt`.
    archived: BTreeMap<String, i64>,
    pub prepare: fn(&CodexConfig) -> Result<Box<dyn PollClient>, CodexDiagnostic>,
}

impl Default for CodexPoll {
    fn default() -> Self {
        Self {
            settings: CodexSettings::default(), client: None, prepared: false,
            prepare_error: None, open_rejected: false, last_attempt: None,
            force: false, startup: true, reload: false, fail_streak: 0,
            idle_streak: 0, fingerprint: None, rows: BTreeMap::new(),
            archived: BTreeMap::new(), prepare,
        }
    }
}

impl CodexPoll {
    pub fn enabled(&self) -> bool { self.settings.enabled() }

    pub fn interval(&self, base: Duration) -> Duration {
        let steps = self.idle_streak.saturating_sub(IDLE_BACKOFF_AT - 1);
        let idle = if steps == 0 { base } else {
            base.saturating_mul(1 << steps.min(5)).min(IDLE_MAX)
        };
        idle.max(if self.fail_streak >= FAIL_BACKOFF_AT { BACKOFF } else { base })
    }

    pub fn due(&self, watched: bool, base: Duration, now: Instant) -> bool {
        self.enabled() && (self.force || (watched && (self.startup
            || self.last_attempt.is_none_or(|t| now.saturating_duration_since(t) >= self.interval(base)))))
    }

    fn attempt(&mut self, now_ms: i64) -> Result<CodexObservation, CodexDiagnostic> {
        if !self.prepared || self.reload {
            self.prepared = true;
            let result = self.settings.config().and_then(|config| match &mut self.client {
                Some(client) => client.reload(&config),
                None => (self.prepare)(&config).map(|client| self.client = Some(client)),
            });
            match result {
                Ok(()) => {
                    self.prepare_error = None;
                    self.open_rejected = false;
                }
                Err(error) => {
                    self.open_rejected |= error.kind == CodexFailureKind::Configuration;
                    self.prepare_error = Some(error.clone());
                    return Err(error);
                }
            }
        }
        match &mut self.client {
            Some(client) => Ok(client.poll(now_ms)),
            None => Err(self.prepare_error.clone().expect("failed preparation has a diagnostic")),
        }
    }

    fn apply(&mut self, observation: &CodexObservation) {
        // Rediscovery. Archive leaves `updatedAt` unchanged and drops the ID
        // from `archived:false` history at once; unarchive bumps it. A history
        // row with another `updatedAt` is the thread back, complete poll or
        // not. A loaded-list/read row with the archived value stays hidden.
        for row in &observation.sessions {
            if observation.history_metadata_ids.contains(&row.session_id)
                && self.archived.get(&row.session_id).is_some_and(|archived_at|
                    row.codex.as_ref().map(|meta| meta.updated_at) != Some(*archived_at))
            {
                self.archived.remove(&row.session_id);
            }
        }
        let mut fresh = BTreeMap::new();
        for row in &observation.sessions {
            if self.archived.contains_key(&row.session_id) { continue; }
            let mut row = row.clone();
            if !observation.history_metadata_ids.contains(&row.session_id)
                && let Some(old) = self.rows.get(&row.session_id)
            {
                row.started_at = row.started_at.min(old.started_at);
            }
            fresh.insert(row.session_id.clone(), row);
        }
        if observation.complete {
            let observed: BTreeSet<_> = observation.sessions.iter()
                .map(|row| row.session_id.as_str()).collect();
            self.archived.retain(|id, _| observed.contains(id.as_str()));
            self.rows = fresh;
            let sessions: Vec<_> = self.rows.values().cloned().collect();
            let fp = codex::fingerprint(&sessions);
            self.idle_streak = if self.fingerprint == Some(fp) {
                self.idle_streak.saturating_add(1)
            } else { 0 };
            self.fingerprint = Some(fp);
            self.fail_streak = 0;
        } else {
            self.rows.extend(fresh);
            self.failed();
        }
    }

    fn failed(&mut self) {
        self.fail_streak = self.fail_streak.saturating_add(1);
        self.idle_streak = 0;
    }

    pub(super) fn archived(&mut self, id: &str, updated_at: i64) {
        self.rows.remove(id);
        self.archived.insert(id.to_owned(), updated_at);
        if let Some(client) = &mut self.client { client.forget(id); }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureCategory {
    CommandUnavailable, CommandFailed, InvalidPayload, Codex(CodexFailureKind),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    NotYetObserved,
    Healthy,
    Degraded(FailureCategory, String),
}

struct PendingDiagnostic {
    reason: String,
    explicit: bool,
}

pub struct ProviderDiagnostic {
    pub health: Health,
    pending: Option<PendingDiagnostic>,
    last_posted: Option<Instant>,
    pub explicit_refresh: bool,
}

impl Default for ProviderDiagnostic {
    fn default() -> Self {
        Self { health: Health::NotYetObserved, pending: None, last_posted: None, explicit_refresh: false }
    }
}

impl ProviderDiagnostic {
    pub fn error(&self) -> Option<&str> {
        match &self.health { Health::Degraded(_, reason) => Some(reason), _ => None }
    }

    pub(super) fn success(&mut self) {
        self.health = Health::Healthy;
        if self.pending.as_ref().is_some_and(|p| !p.explicit) { self.pending = None; }
        self.explicit_refresh = false;
    }

    fn failure(&mut self, category: FailureCategory, reason: String) {
        let explicit = std::mem::take(&mut self.explicit_refresh);
        let event = !matches!(&self.health, Health::Degraded(old, _) if *old == category);
        self.health = Health::Degraded(category, reason.clone());
        if explicit || (self.pending.as_ref().is_none_or(|p| !p.explicit)
            && (event || self.pending.is_some()))
        {
            self.pending = Some(PendingDiagnostic { reason, explicit });
        }
    }

    fn take_ready(&mut self, now: Instant, provider: Provider) -> Option<String> {
        let pending = self.pending.as_ref()?;
        if !pending.explicit && self.last_posted.is_some_and(|t|
            now.saturating_duration_since(t) < DIAGNOSTIC_COOLDOWN)
        { return None; }
        let pending = self.pending.take().unwrap();
        self.last_posted = Some(now);
        let label = match (provider, pending.explicit) {
            (Provider::Claude, true) => "agents refresh failed",
            (Provider::Claude, false) => "agents",
            (Provider::Codex, true) => "codex refresh failed",
            (Provider::Codex, false) => "codex degraded",
        };
        Some(format!("{label}: {}", pending.reason))
    }
}

#[derive(Default)]
pub struct Diagnostics {
    pub claude: ProviderDiagnostic,
    pub codex: ProviderDiagnostic,
    runtime: BTreeMap<String, RuntimeWarning>,
    next_episode: u64,
    runtime_flash: Option<RuntimeFlash>,
}

struct RuntimeWarning { episode: u64, seen: bool }
struct RuntimeFlash { id: String, episode: u64, text: String, deadline: Instant }

impl App {
    pub fn codex_enabled(&self) -> bool { self.codex.enabled() }
    pub fn codex_error(&self) -> Option<&str> {
        if self.codex_enabled() { self.diagnostics.codex.error() } else { None }
    }

    pub(super) fn poll_providers(&mut self) -> bool {
        let claude = self.poll_step(self.agents_poll);
        self.poll_codex();
        claude
    }

    pub fn poll_codex(&mut self) -> bool {
        if !self.codex.due(self.watchers().polls(), self.interval, Instant::now()) { return false; }
        let explicit = self.codex.reload;
        let result = self.codex.attempt(self.now_ms);
        self.codex.reload = false;
        self.codex.force = false;
        self.codex.startup = false;
        self.codex.last_attempt = Some(Instant::now());
        self.diagnostics.codex.explicit_refresh = explicit;
        match result {
            Ok(observation) => {
                self.codex.apply(&observation);
                self.sessions.retain(|s| s.provider != Provider::Codex);
                self.sessions.extend(self.codex.rows.values().cloned());
                for raw in observation.source_drift {
                    let label = format!("codex source {raw}");
                    if !self.drift_seen.contains(&label) { self.drift_pending.insert(label); }
                }
                self.note_drift(&observation.sessions);
                if observation.complete {
                    self.diagnostics.codex.success();
                } else {
                    let diagnostic = observation.diagnostic.unwrap_or(CodexDiagnostic {
                        kind: CodexFailureKind::Incomplete, message: "incomplete listing".into(),
                    });
                    self.diagnostics.codex.failure(FailureCategory::Codex(diagnostic.kind), diagnostic.message);
                }
                self.reconcile_hidden(Provider::Codex, observation.complete);
            }
            Err(error) => {
                self.codex.failed();
                self.diagnostics.codex.failure(FailureCategory::Codex(error.kind), error.message);
            }
        }
        true
    }

    pub(super) fn note_claude_failure(&mut self, error: &AgentsError) {
        let category = match error {
            AgentsError::NotFound(_) => FailureCategory::CommandUnavailable,
            AgentsError::Cmd { .. } => FailureCategory::CommandFailed,
            AgentsError::Parse(_) => FailureCategory::InvalidPayload,
            AgentsError::NotAttachable | AgentsError::UnsupportedProvider { .. }
                | AgentsError::NotConfigured(_) => FailureCategory::CommandFailed,
        };
        self.diagnostics.claude.failure(category, model::truncate_end(&agents_msg(error), POLL_ERR_MAX));
    }

    /// Called beside expiry/drift on every event-loop iteration, even while
    /// polls are gated. Posting, not queuing, starts the cooldown and TTL.
    pub fn tick_diagnostics(&mut self) -> bool {
        self.deliver_diagnostics(Instant::now())
    }

    fn deliver_diagnostics(&mut self, now: Instant) -> bool {
        if self.message.is_some() || self.footer_is_covered() { return false; }
        let mut parts = Vec::new();
        if let Some(text) = self.diagnostics.claude.take_ready(now, Provider::Claude) { parts.push(text); }
        if self.codex_enabled()
            && let Some(text) = self.diagnostics.codex.take_ready(now, Provider::Codex)
        { parts.push(text); }
        if parts.is_empty() { return false; }
        self.flash(parts.join("; "), MsgLevel::Error);
        self.msg_deadline = now.checked_add(MSG_TTL);
        true
    }

    pub(super) fn note_codex_drift(&mut self, session: &Session) {
        let runtime = session.codex.as_ref().map(|meta| &meta.runtime);
        if runtime == Some(&CodexStatus::SystemError) {
            if !self.diagnostics.runtime.contains_key(&session.session_id) {
                self.diagnostics.next_episode += 1;
                self.diagnostics.runtime.insert(session.session_id.clone(),
                    RuntimeWarning { episode: self.diagnostics.next_episode, seen: false });
            }
            return;
        }
        self.diagnostics.runtime.remove(&session.session_id);
        if self.diagnostics.runtime_flash.as_ref().is_some_and(|f| f.id == session.session_id) {
            let flight = self.diagnostics.runtime_flash.take().unwrap();
            if self.message.as_ref().is_some_and(|(text, _)| text == &flight.text) {
                self.message = None;
                self.msg_deadline = None;
            }
        }
        let label = match runtime {
            Some(CodexStatus::Active { flags }) => {
                let unknown: Vec<_> = flags.iter().filter(|flag|
                    !matches!(flag.as_str(), "waitingOnApproval" | "waitingOnUserInput")).collect();
                if unknown.is_empty() { None } else { Some(format!("codex active flags {unknown:?}")) }
            }
            Some(CodexStatus::Unknown(raw)) => Some(format!("codex status {raw:?}")),
            _ => match &session.status {
                Status::Unknown(raw) if !raw.is_empty() => Some(format!("codex status {raw:?}")),
                _ => None,
            },
        };
        if let Some(label) = label
            && !self.drift_seen.contains(&label)
        { self.drift_pending.insert(label); }
    }

    pub(super) fn settle_runtime(&mut self, now: Instant) {
        let covered = self.footer_is_covered();
        if let Some(flight) = &self.diagnostics.runtime_flash {
            let ours = self.message.as_ref().is_some_and(|(text, _)| text == &flight.text);
            if ours && !covered && now < flight.deadline { return; }
            let flight = self.diagnostics.runtime_flash.take().unwrap();
            if !covered && now >= flight.deadline && (ours || self.message.is_none())
                && let Some(warning) = self.diagnostics.runtime.get_mut(&flight.id)
                && warning.episode == flight.episode
            { warning.seen = true; }
        }
    }

    pub(super) fn post_runtime(&mut self, now: Instant) {
        if self.message.is_some() || self.footer_is_covered() { return; }
        let Some((id, warning)) = self.diagnostics.runtime.iter().find(|(id, warning)|
            !warning.seen && self.sessions.iter().any(|row| &row.session_id == *id
                && row.provider == Provider::Codex
                && row.codex.as_ref().is_some_and(|m| m.runtime == CodexStatus::SystemError)))
        else { return; };
        let row = self.sessions.iter().find(|s| &s.session_id == id && s.provider == Provider::Codex).unwrap();
        let text = format!("codex runtime error: {}", row.id.as_deref().unwrap_or("—"));
        self.diagnostics.runtime_flash = Some(RuntimeFlash {
            id: id.clone(), episode: warning.episode, text: text.clone(), deadline: now + MSG_TTL,
        });
        self.flash(text, MsgLevel::Warn);
        self.msg_deadline = now.checked_add(MSG_TTL);
    }

    /// Opening uses non-secret settings even when polling or credentials fail.
    /// A known unsafe resolution remains refused until a successful reload.
    pub(super) fn refuse_codex_open(&mut self) -> bool {
        if !self.selected_session().is_some_and(|s| s.provider == Provider::Codex) {
            return false;
        }
        let settings = &self.codex.settings;
        let reason = if !settings.enabled() || settings.config().is_err()
            || !settings.token_file.is_absolute() || settings.bin.is_empty()
        {
            Some("codex not configured — open unavailable")
        } else if self.codex.open_rejected {
            Some("codex localhost resolved outside loopback — open unavailable")
        } else { None };
        if let Some(reason) = reason {
            self.flash(reason, MsgLevel::Warn);
            true
        } else { false }
    }
}

#[cfg(test)]
mod tests;
