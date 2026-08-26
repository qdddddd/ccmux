//! Pure data + parsing + grouping + formatting for `claude agents --json`.
//!
//! SPEC §3.1. No IO, no process spawning, no tmux, no ratatui. Everything here
//! is unit-testable without a terminal or a `claude` binary.

use serde::Deserialize;

// ── Enums ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Background,
    Interactive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Busy,
    Idle,
    /// Forward-compatibility: CLI versions churn; unknown strings are preserved.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Working,
    /// Finished on its own.
    Done,
    /// Halted by `claude stop`. NOT a failure and NOT unknown: the conversation
    /// is kept and `claude attach <id>` resumes it (verified). Modelled
    /// explicitly because otherwise it lands in `Unknown` and renders as a
    /// purple `?` under Idle — reading as broken when it is merely parked.
    Stopped,
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Working = 0,
    Idle = 1,
    Completed = 2,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Working => "Working",
            Group::Idle => "Idle",
            Group::Completed => "Completed",
        }
    }

    pub fn all() -> [Group; 3] {
        [Group::Working, Group::Idle, Group::Completed]
    }

    /// SPEC §3.1 surface. `App::cycle_group` walks `rows`' header positions
    /// instead, because only that skips groups the current filter emptied.
    #[allow(dead_code)]
    pub fn next(self) -> Group {
        match self {
            Group::Working => Group::Idle,
            Group::Idle => Group::Completed,
            Group::Completed => Group::Working,
        }
    }

    /// See `next`.
    #[allow(dead_code)]
    pub fn prev(self) -> Group {
        match self {
            Group::Working => Group::Completed,
            Group::Idle => Group::Working,
            Group::Completed => Group::Idle,
        }
    }
}

// ── Session ─────────────────────────────────────────────────────────────────

/// One row from `claude agents --json`. Field names mirror the CLI's JSON
/// exactly. Identity is `session_id` and nothing else: the CLI's `pid` is
/// UNSTABLE across attach/detach, and with the /proc ancestry walk gone
/// nothing reads it, so it is no longer carried.
#[derive(Debug, Clone)]
pub struct Session {
    /// 8-hex short id. `None` for `kind == Interactive`.
    pub id: Option<String>,
    /// UUID. Stable. THE primary key everywhere in ccmux.
    pub session_id: String,
    pub cwd: String,
    pub kind: Kind,
    /// epoch milliseconds
    pub started_at: i64,
    pub name: String,
    pub status: Status,
    /// `None` for `kind == Interactive`.
    pub state: Option<State>,
}

impl Session {
    /// Identity key. Always `&self.session_id`.
    ///
    /// SPEC §3.1 surface; call sites reach for `session_id` directly.
    #[allow(dead_code)]
    pub fn key(&self) -> &str {
        &self.session_id
    }

    /// Grouping rule, mirroring the stock fleet view:
    ///   Some(Working)                 -> Group::Working
    ///   Some(Done)                    -> Group::Completed
    ///   None | Some(Unknown(_))       -> Busy => Working, otherwise => Idle
    /// Interactive sessions have no `state`, so they fall through to the
    /// status rule and never land in Completed.
    pub fn group(&self) -> Group {
        match &self.state {
            Some(State::Working) => Group::Working,
            // Stopped is finished-and-not-running, like Done.
            Some(State::Done) | Some(State::Stopped) => Group::Completed,
            None | Some(State::Unknown(_)) => match self.status {
                Status::Busy => Group::Working,
                _ => Group::Idle,
            },
        }
    }

    /// True when `id.is_some()`. Gates `attach` / `stop` / `logs`.
    pub fn is_attachable(&self) -> bool {
        self.id.is_some()
    }

    /// Lowercased haystack for `/` filtering: name + " " + cwd + " " + short id.
    pub fn filter_haystack(&self) -> String {
        let mut out = String::with_capacity(self.name.len() + self.cwd.len() + 12);
        out.push_str(&self.name);
        out.push(' ');
        out.push_str(&self.cwd);
        out.push(' ');
        if let Some(id) = &self.id {
            out.push_str(id);
        }
        out.to_lowercase()
    }
}

// ── Parsing ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ParseError {
    /// serde_json failed; carries a truncated excerpt of the offending input.
    Json { msg: String, excerpt: String },
    /// Top-level JSON was not an array.
    NotAnArray,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Json { msg, excerpt } => {
                write!(f, "invalid JSON from `claude agents --json`: {msg} (near: {excerpt})")
            }
            ParseError::NotAnArray => {
                write!(f, "`claude agents --json` did not return a JSON array")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Raw wire shape. EVERY field is optional so that a row with a missing or
/// wrongly-typed key is a per-row failure, never a whole-payload failure.
/// `id` and `state` are genuinely absent for `kind == "interactive"`
/// (PROBE-FINDINGS §1), which is why they map to `Option` in `Session` too.
#[derive(Debug, Deserialize)]
struct RawSession {
    id: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    kind: Option<String>,
    #[serde(rename = "startedAt")]
    started_at: Option<i64>,
    name: Option<String>,
    status: Option<String>,
    state: Option<String>,
}

impl RawSession {
    /// `None` means "skip this row": `sessionId`, `name`, or `cwd` was absent.
    ///
    /// Defaults for the remaining absent fields (the CLI always emits them
    /// today; these exist so a future version cannot blank the sidebar):
    ///   startedAt  -> 0        (renders as a very old age, never panics)
    ///   kind       -> inferred from `id` presence: an `id` means background
    ///   status     -> Status::Unknown("") -> the `?` glyph, groups as Idle
    fn into_session(self) -> Option<Session> {
        let session_id = self.session_id.filter(|s| !s.is_empty())?;
        let name = self.name?;
        let cwd = self.cwd?;

        let id = self.id.filter(|s| !s.is_empty());

        let kind = match self.kind.as_deref() {
            Some("interactive") => Kind::Interactive,
            Some("background") => Kind::Background,
            _ if id.is_some() => Kind::Background,
            _ => Kind::Interactive,
        };

        let status = match self.status.as_deref() {
            Some("busy") => Status::Busy,
            Some("idle") => Status::Idle,
            Some(other) => Status::Unknown(other.to_string()),
            None => Status::Unknown(String::new()),
        };

        let state = self.state.map(|s| match s.as_str() {
            "working" => State::Working,
            "done" => State::Done,
            "stopped" => State::Stopped,
            _ => State::Unknown(s),
        });

        Some(Session {
            id,
            session_id,
            cwd,
            kind,
            started_at: self.started_at.unwrap_or(0),
            name,
            status,
            state,
        })
    }
}

/// One parsed `claude agents --json` payload: the rows that survived, and how
/// many the CLI emitted that did not.
///
/// `dropped > 0` means the payload was well-formed JSON but INCOMPLETE — some
/// sessions the CLI knows about are missing from `sessions` through no fault of
/// the CLI call, which still exited 0. Callers that reason about a session
/// being *gone* (only `App::apply_poll`, for the dismissed set) must treat a
/// lossy payload as no evidence at all: an absent row may simply be one of the
/// dropped ones.
#[derive(Debug, Clone, Default)]
pub struct Payload {
    pub sessions: Vec<Session>,
    pub dropped: usize,
}

impl Payload {
    /// True when every row the CLI emitted became a `Session`. An empty payload
    /// is complete: `[]` means the CLI really does know of no sessions.
    pub fn is_complete(&self) -> bool {
        self.dropped == 0
    }
}

/// Parse the whole `claude agents --json` payload.
/// Rows missing `sessionId`, `name`, or `cwd` are SKIPPED, not fatal — a single
/// malformed row must never blank the sidebar. Unknown extra keys are ignored.
/// Returns rows in the order the CLI emitted them; ordering is imposed later.
///
/// Every skipped row is COUNTED, because silently dropping rows and silently
/// concluding those sessions ended are two different things. See `Payload`.
pub fn parse_sessions(json: &str) -> Result<Payload, ParseError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| ParseError::Json {
        msg: e.to_string(),
        excerpt: truncate_end(json.trim(), 120),
    })?;

    let arr = match value {
        serde_json::Value::Array(a) => a,
        _ => return Err(ParseError::NotAnArray),
    };

    let mut out = Vec::with_capacity(arr.len());
    let mut dropped = 0usize;
    for elem in arr {
        // Per-row tolerance: a type error in one row skips that row only.
        match serde_json::from_value::<RawSession>(elem).ok().and_then(RawSession::into_session) {
            Some(sess) => out.push(sess),
            None => dropped += 1,
        }
    }
    Ok(Payload { sessions: out, dropped })
}

// ── Rows: the display model shared by app.rs and ui.rs ──────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Header { group: Group, count: usize },
    /// A blank line separating a group from the one above it. Emitted before
    /// every header EXCEPT the first, so the list never opens on a wasted line.
    /// Never selectable: `App::is_session_row` excludes it exactly as it
    /// excludes a header, so `j`/`k` and `Tab` step straight over it.
    Spacer,
    /// Index into the `&[Session]` slice that was passed to `build_rows`.
    Session { idx: usize },
}

/// Build the flat render list: for each group in Working, Idle, Completed
/// order, emit a `Header` (only if the group has >=1 matching session) followed
/// by its `Session` rows. Groups after the first are preceded by a `Spacer`.
///
/// Filtering: case-insensitive substring of `filter` against
/// `Session::filter_haystack()`. An empty `filter` matches everything.
/// `show_completed == false` omits the Completed group entirely.
/// `hidden` holds `session_id`s dismissed with `d`; they are omitted too.
///
/// All three filters live here, in the one pure function, so they cannot
/// disagree: `/`, `a` and `d` compose by construction and every caller —
/// including the header's `matching/total` count, which counts the `Row`s this
/// returns — sees the same answer. Dismissal is deliberately NOT a mutation of
/// `sessions`: the full poll stays intact, so `total` still counts the
/// dismissed session and reconciliation can still see that it is alive.
///
/// Within a group, sessions sort by `started_at` DESCENDING (newest first),
/// tie-broken by `session_id` ASCENDING so the order is total and stable.
pub fn build_rows(
    sessions: &[Session],
    filter: &str,
    show_completed: bool,
    hidden: &[String],
) -> Vec<Row> {
    let needle = filter.trim().to_lowercase();
    let matching: Vec<usize> = (0..sessions.len())
        .filter(|&i| !hidden.iter().any(|h| h == &sessions[i].session_id))
        .filter(|&i| needle.is_empty() || sessions[i].filter_haystack().contains(&needle))
        .collect();

    // 3 headers + 2 spacers in the worst case.
    let mut rows = Vec::with_capacity(matching.len() + 5);
    for group in Group::all() {
        if group == Group::Completed && !show_completed {
            continue;
        }
        let mut in_group: Vec<usize> = matching
            .iter()
            .copied()
            .filter(|&i| sessions[i].group() == group)
            .collect();
        if in_group.is_empty() {
            continue;
        }
        in_group.sort_by(|&a, &b| {
            sessions[b]
                .started_at
                .cmp(&sessions[a].started_at)
                .then_with(|| sessions[a].session_id.cmp(&sessions[b].session_id))
        });
        if !rows.is_empty() {
            rows.push(Row::Spacer);
        }
        rows.push(Row::Header { group, count: in_group.len() });
        rows.extend(in_group.into_iter().map(|idx| Row::Session { idx }));
    }
    rows
}

// ── Formatting helpers (pure; used by ui.rs) ────────────────────────────────

/// Relative age. `now_ms` is epoch ms (chrono::Utc::now().timestamp_millis()).
///   < 60s      -> "12s"
///   < 60m      -> "2m"
///   < 24h      -> "17h"
///   otherwise  -> "3d"
/// Negative or future timestamps clamp to "0s". Never returns >5 chars.
pub fn format_age(started_at_ms: i64, now_ms: i64) -> String {
    let secs = now_ms.saturating_sub(started_at_ms).max(0) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        // Cap so the result is never wider than the 5-column age budget.
        format!("{}d", (secs / 86_400).min(9_999))
    }
}

/// `home` is `std::env::var("HOME").ok()`, passed in so this stays pure.
/// 1. Replace a leading `home` with `~`.
/// 2. If the result fits `max`, return it.
/// 3. Otherwise drop leading path components, replacing them with `…/`, until
///    it fits, always keeping at least the final component.
/// 4. If the final component alone exceeds `max`, end-truncate it with `…`.
///
/// Returns a string of at most `max` display COLUMNS. `max == 0` returns "".
///
/// SPEC NOTE: §3.1 step 3 and §10.1's assertion
/// (`shorten_cwd("/home/dev/projects/shared/Foundation", Some("/home/dev"), 24)`
/// "starts with `~`") only agree if a leading `~` survives the elision, so the
/// `~` root marker is kept and components are dropped after it:
/// `~/…/shared/Foundation`. §6.5's sample rendering shows the same shape.
pub fn shorten_cwd(cwd: &str, home: Option<&str>, max: usize) -> String {
    if max == 0 {
        return String::new();
    }

    let mut s = cwd.to_string();
    if let Some(h) = home.filter(|h| !h.is_empty()) {
        if s == h {
            s = "~".to_string();
        } else if let Some(rest) = s.strip_prefix(h)
            && rest.starts_with('/')
        {
            s = format!("~{rest}");
        }
    }

    if display_width(&s) <= max {
        return s;
    }

    let parts: Vec<&str> = s.split('/').collect();
    let keep_root = parts.first() == Some(&"~");
    let root = if keep_root { "~/" } else { "" };
    // Elide at least one component: start after the retained root marker.
    let first = if keep_root { 2 } else { 1 };
    for start in first..parts.len() {
        let cand = format!("{root}…/{}", parts[start..].join("/"));
        if display_width(&cand) <= max {
            return cand;
        }
    }
    truncate_end(parts.last().copied().unwrap_or(""), max)
}

/// Terminal columns one char occupies. Zero for combining marks and controls,
/// two for East Asian Wide/Fullwidth and emoji, one otherwise.
///
/// SPEC AMENDMENT (§6.6): the spec pinned char-count budgets and called the
/// resulting overflow cosmetic. It is not — a CJK session name overflows the
/// pane and ratatui clips the age and pane-badge segments off the row entirely.
/// A local table is used rather than the `unicode-width` crate §6.6 excluded;
/// it is deliberately coarse, but only ever in the safe direction — charging a
/// column too many shortens a name, charging one too few overflows the row.
///
/// Per-char only: the U+FE0F widening rule needs the NEXT char, so
/// `display_width` / `truncate_end` are the oracle, not this.
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    // Controls and format/combining characters occupy no cell.
    if u < 0x20 || (0x7f..0xa0).contains(&u) {
        return 0;
    }
    if matches!(u,
        0x0300..=0x036f      // combining diacritics
        | 0x200b..=0x200f    // zero width space/joiners, bidi marks
        | 0x20d0..=0x20ff    // combining marks FOR SYMBOLS (keycap enclosure)
        | 0xfe00..=0xfe0f    // variation selectors
        | 0xfeff
    ) {
        return 0;
    }
    if matches!(u,
        0x1100..=0x115f      // Hangul Jamo
        | 0x2e80..=0x303e    // CJK radicals, Kangxi, CJK symbols
        | 0x3041..=0x33ff    // kana, Hangul compat, CJK compat
        | 0x3400..=0x4dbf    // CJK ext A
        | 0x4e00..=0x9fff    // CJK unified
        | 0xa000..=0xa4cf    // Yi
        | 0xa960..=0xa97f    // Hangul Jamo ext A
        | 0xac00..=0xd7a3    // Hangul syllables
        | 0xf900..=0xfaff    // CJK compat ideographs
        | 0xfe10..=0xfe19
        | 0xfe30..=0xfe6f    // CJK compat forms
        | 0xff00..=0xff60    // fullwidth forms
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f  // emoji, pictographs
        | 0x1f680..=0x1f6ff
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd  // CJK ext B and later
    ) {
        return 2;
    }
    // The Wide symbol islands scattered BELOW the emoji planes. Omitting them
    // was the one direction of error this table cannot afford: a name holding
    // `✅` was charged one column and drawn in two, so the row ran to W+1 and
    // pushed the age onto the margin. Every range here was measured against
    // tmux 3.4 with `printf` + `#{cursor_x}`; see the test of the same name.
    if matches!(u,
        0x231a..=0x231b      // ⌚⌛
        | 0x2329..=0x232a    // 〈〉
        | 0x23e9..=0x23ec | 0x23f0 | 0x23f3
        | 0x25fd..=0x25fe
        | 0x2614..=0x2615
        | 0x2648..=0x2653    // zodiac
        | 0x267f | 0x2693 | 0x26a1
        | 0x26aa..=0x26ab
        | 0x26bd..=0x26be
        | 0x26c4..=0x26c5
        | 0x26ce | 0x26d4 | 0x26ea
        | 0x26f2..=0x26f3
        | 0x26f5 | 0x26fa | 0x26fd
        | 0x2705
        | 0x270a..=0x270b
        | 0x2728 | 0x274c | 0x274e
        | 0x2753..=0x2755
        | 0x2757
        | 0x2795..=0x2797
        | 0x27b0 | 0x27bf
        | 0x2b1b..=0x2b1c
        | 0x2b50 | 0x2b55
        | 0x1f004 | 0x1f0cf | 0x1f18e
        | 0x1f191..=0x1f19a
        | 0x1f200..=0x1f2ff  // enclosed CJK/ideographic supplement
        | 0x1f7e0..=0x1f7eb  // colour circles and squares
        | 0x1fa70..=0x1faff  // symbols and pictographs extended-A
    ) {
        return 2;
    }
    1
}

/// Each char of `s` paired with the columns it occupies, applying the one
/// SEQUENCE rule a per-char table cannot see: a char immediately followed by
/// U+FE0F (VARIATION SELECTOR-16, "draw the preceding char as emoji") is drawn
/// two cells wide whatever its own width — tmux 3.4 widens `⚠️` (U+26A0 U+FE0F)
/// to two columns while `⚠` alone stays at one. U+FE0E (the TEXT selector) does
/// not widen, and neither selector occupies a cell of its own.
///
/// This is the single walk behind BOTH `display_width` and `truncate_end`, so
/// the two can never disagree about where a string ends. `truncate_end` stops
/// at the first char that does not fit, and the selector is zero-width, so a
/// base/selector pair is never split across the cut.
fn char_widths(s: &str) -> impl Iterator<Item = (char, usize)> + '_ {
    let mut it = s.chars().peekable();
    std::iter::from_fn(move || {
        let c = it.next()?;
        let w = if it.peek() == Some(&'\u{fe0f}') { 2 } else { char_width(c) };
        Some((c, w))
    })
}

/// Terminal columns `s` occupies. The single source of truth for every row
/// budget in `ui.rs`.
pub fn display_width(s: &str) -> usize {
    char_widths(s).map(|(_, w)| w).sum()
}

/// End-truncate to `max` COLUMNS, appending '…' when truncation occurred.
/// `max == 0` -> ""; a `max` too small for even one column of content -> "…".
///
/// SPEC AMENDMENT (§6.6): budgets are display columns, not chars. A wide
/// character that would straddle the limit is dropped, so the result never
/// exceeds `max` columns.
pub fn truncate_end(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }
    // One column is reserved for the ellipsis.
    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for (c, w) in char_widths(s) {
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The real payload shape from PROBE-FINDINGS §1: three live background
    /// sessions plus one interactive session. The interactive row physically
    /// LACKS the `id` and `state` keys — that absence is the thing under test.
    const SAMPLE: &str = r#"[
      {
        "pid": 2877291,
        "id": "1c45d64f",
        "cwd": "/home/dev/projects/shared/Foundation",
        "kind": "background",
        "startedAt": 1787626475282,
        "sessionId": "1c45d64f-9bba-4038-8de7-d5f112c92360",
        "name": "bt/reg-update",
        "status": "busy",
        "state": "working"
      },
      {
        "pid": 2877300,
        "id": "629da7fc",
        "cwd": "/home/dev",
        "kind": "background",
        "startedAt": 1787626000000,
        "sessionId": "629da7fc-1111-4038-8de7-d5f112c92361",
        "name": "Kernel bugs investigation",
        "status": "idle",
        "state": "working"
      },
      {
        "pid": 2877400,
        "id": "674b1d29",
        "cwd": "/home/dev/projects/shared/Foundation/price-sanitize",
        "kind": "background",
        "startedAt": 1787620000000,
        "sessionId": "674b1d29-2222-4038-8de7-d5f112c92362",
        "name": "prediction analysis ab_1_2_cd",
        "status": "idle",
        "state": "done"
      },
      {
        "pid": 2936154,
        "cwd": "/home/dev/projects/ccmux",
        "kind": "interactive",
        "startedAt": 1787630000000,
        "sessionId": "aaaaaaaa-3333-4038-8de7-d5f112c92363",
        "name": "ccmux scaffold",
        "status": "busy"
      }
    ]"#;

    fn sample() -> Vec<Session> {
        parse_sessions(SAMPLE).expect("sample payload parses").sessions
    }

    fn sess(id: Option<&str>, status: Status, state: Option<State>) -> Session {
        Session {
            id: id.map(str::to_string),
            session_id: format!("uuid-{}", id.unwrap_or("interactive")),
            cwd: "/tmp".into(),
            kind: if id.is_some() { Kind::Background } else { Kind::Interactive },
            started_at: 0,
            name: "n".into(),
            status,
            state,
        }
    }

    #[test]
    fn parses_the_probe_findings_payload() {
        let s = sample();
        assert_eq!(s.len(), 4);
        assert_eq!(s[0].id.as_deref(), Some("1c45d64f"));
        assert_eq!(s[0].session_id, "1c45d64f-9bba-4038-8de7-d5f112c92360");
        assert_eq!(s[0].kind, Kind::Background);
        assert_eq!(s[0].status, Status::Busy);
        assert_eq!(s[0].state, Some(State::Working));
        assert_eq!(s[0].started_at, 1787626475282);
        assert!(s[0].is_attachable());

        let interactive = &s[3];
        assert_eq!(interactive.kind, Kind::Interactive);
        assert_eq!(interactive.id, None);
        assert_eq!(interactive.state, None);
        assert!(!interactive.is_attachable());
        assert_eq!(interactive.key(), "aaaaaaaa-3333-4038-8de7-d5f112c92363");
    }

    #[test]
    fn skips_rows_missing_required_keys() {
        let json = r#"[
          {"sessionId":"a","name":"ok","cwd":"/tmp","kind":"background","id":"aaaaaaaa"},
          {"name":"no session id","cwd":"/tmp"},
          {"sessionId":"c","cwd":"/tmp"},
          {"sessionId":"d","name":"no cwd"}
        ]"#;
        let p = parse_sessions(json).unwrap();
        assert_eq!(p.sessions.len(), 1);
        assert_eq!(p.sessions[0].session_id, "a");
        // The three unusable rows are COUNTED, not silently forgotten: a poll
        // that lost rows is not evidence that those sessions ended.
        assert_eq!(p.dropped, 3);
        assert!(!p.is_complete());
    }

    #[test]
    fn skips_rows_with_wrong_types_without_failing_the_payload() {
        let json = r#"[
          {"sessionId":"a","name":"ok","cwd":"/tmp"},
          {"sessionId":"b","name":"bad","cwd":"/tmp","startedAt":"not-a-number"}
        ]"#;
        let p = parse_sessions(json).unwrap();
        assert_eq!(p.sessions.len(), 1);
        assert_eq!(p.sessions[0].session_id, "a");
        assert_eq!(p.dropped, 1, "the wrong-typed row is counted");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let json = r#"[{"sessionId":"a","name":"n","cwd":"/tmp","brandNewKey":42}]"#;
        let p = parse_sessions(json).unwrap();
        assert_eq!(p.sessions.len(), 1);
        assert!(p.is_complete(), "an unknown key does not make a payload lossy");
    }

    #[test]
    fn non_array_and_garbage_payloads() {
        assert!(matches!(parse_sessions("{}"), Err(ParseError::NotAnArray)));
        assert!(matches!(parse_sessions("oops"), Err(ParseError::Json { .. })));
        let empty = parse_sessions("[]").unwrap();
        assert_eq!(empty.sessions.len(), 0);
        // `[]` is COMPLETE, not lossy: the CLI really does know of no sessions.
        assert!(empty.is_complete());
    }

    #[test]
    fn grouping_rules() {
        // state wins over status
        assert_eq!(
            sess(Some("a"), Status::Idle, Some(State::Working)).group(),
            Group::Working
        );
        assert_eq!(
            sess(Some("a"), Status::Busy, Some(State::Done)).group(),
            Group::Completed
        );
        // interactive: no state, falls through to status
        assert_eq!(sess(None, Status::Busy, None).group(), Group::Working);
        assert_eq!(sess(None, Status::Idle, None).group(), Group::Idle);
        // unknown state falls through to status too
        assert_eq!(
            sess(Some("a"), Status::Busy, Some(State::Unknown("nope".into()))).group(),
            Group::Working
        );
        // unknown status is not busy => Idle
        assert_eq!(
            sess(None, Status::Unknown(String::new()), None).group(),
            Group::Idle
        );
    }

    #[test]
    fn interactive_sessions_never_land_in_completed() {
        for st in [Status::Busy, Status::Idle, Status::Unknown("x".into())] {
            assert_ne!(sess(None, st, None).group(), Group::Completed);
        }
    }

    #[test]
    fn group_helpers() {
        assert_eq!(Group::Working.title(), "Working");
        assert_eq!(Group::Idle.title(), "Idle");
        assert_eq!(Group::Completed.title(), "Completed");
        assert_eq!(Group::all(), [Group::Working, Group::Idle, Group::Completed]);
        assert_eq!(Group::Working.next(), Group::Idle);
        assert_eq!(Group::Completed.next(), Group::Working);
        assert_eq!(Group::Working.prev(), Group::Completed);
        assert!(Group::Working < Group::Idle && Group::Idle < Group::Completed);
    }

    #[test]
    fn build_rows_groups_headers_and_order() {
        let s = sample();
        let rows = build_rows(&s, "", true, &[]);
        // Working: bt/reg-update + GNOME (both state=working)
        // Working also gets the interactive busy session.
        // Idle: none. Completed: prediction analysis.
        assert_eq!(rows[0], Row::Header { group: Group::Working, count: 3 });
        // newest first: interactive (1787630000000), bt/reg-update, GNOME
        let names: Vec<&str> = rows[1..4]
            .iter()
            .map(|r| match r {
                Row::Session { idx } => s[*idx].name.as_str(),
                _ => panic!("expected session row"),
            })
            .collect();
        assert_eq!(names, ["ccmux scaffold", "bt/reg-update", "Kernel bugs investigation"]);
        // A Spacer separates Completed from the group above it.
        assert_eq!(rows[4], Row::Spacer);
        assert_eq!(rows[5], Row::Header { group: Group::Completed, count: 1 });
        assert_eq!(rows.len(), 7);
        // The list never opens on a blank line.
        assert_ne!(rows[0], Row::Spacer);
        // no Idle header: the group is empty
        assert!(!rows.iter().any(|r| matches!(r, Row::Header { group: Group::Idle, .. })));
    }

    #[test]
    fn build_rows_hides_completed_when_asked() {
        let s = sample();
        let rows = build_rows(&s, "", false, &[]);
        assert!(!rows.iter().any(|r| matches!(r, Row::Header { group: Group::Completed, .. })));
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn build_rows_filtering() {
        let s = sample();
        assert!(build_rows(&s, "zzz-no-such-thing", true, &[]).is_empty());
        // case-insensitive across name, cwd and short id
        assert_eq!(build_rows(&s, "KERNEL", true, &[]).len(), 2); // header + row
        assert_eq!(build_rows(&s, "kernel", true, &[]).len(), 2);
        // 2 groups, 2 rows, 1 spacer between them
        assert_eq!(build_rows(&s, "foundation", true, &[]).len(), 5);
        assert_eq!(build_rows(&s, "674b1d29", true, &[]).len(), 2);
    }

    #[test]
    fn build_rows_sort_is_total_and_stable() {
        let mut a = sess(Some("a"), Status::Busy, Some(State::Working));
        let mut b = sess(Some("b"), Status::Busy, Some(State::Working));
        a.session_id = "bbb".into();
        b.session_id = "aaa".into();
        a.started_at = 100;
        b.started_at = 100;
        let rows = build_rows(&[a, b], "", true, &[]);
        // equal started_at => session_id ascending: "aaa" (index 1) first
        assert_eq!(rows[1], Row::Session { idx: 1 });
        assert_eq!(rows[2], Row::Session { idx: 0 });
    }

    #[test]
    fn build_rows_on_empty_input() {
        assert!(build_rows(&[], "", true, &[]).is_empty());
        assert!(build_rows(&[], "x", false, &[]).is_empty());
    }

    #[test]
    fn format_age_table() {
        assert_eq!(format_age(0, 0), "0s");
        assert_eq!(format_age(0, 59_000), "59s");
        assert_eq!(format_age(0, 120_000), "2m");
        assert_eq!(format_age(0, 61_200_000), "17h");
        assert_eq!(format_age(0, 259_200_000), "3d");
        // future / negative clamps to 0s
        assert_eq!(format_age(1_000_000, 0), "0s");
        // boundaries
        assert_eq!(format_age(0, 60_000), "1m");
        assert_eq!(format_age(0, 3_600_000), "1h");
        assert_eq!(format_age(0, 86_400_000), "1d");
    }

    #[test]
    fn format_age_never_exceeds_five_chars() {
        for ms in [0i64, 1, 59_999, 60_000, 86_399_000, i64::MAX / 2] {
            assert!(format_age(0, ms).chars().count() <= 5, "{ms}");
        }
    }

    #[test]
    fn shorten_cwd_keeps_home_marker_and_budget() {
        let out = shorten_cwd("/home/dev/projects/shared/Foundation", Some("/home/dev"), 24);
        assert!(out.starts_with('~'), "{out}");
        assert!(out.chars().count() <= 24, "{out}");
        assert!(out.ends_with("Foundation"), "{out}");

        // fits without elision
        assert_eq!(shorten_cwd("/home/dev", Some("/home/dev"), 24), "~");
        assert_eq!(shorten_cwd("/home/dev/ccmux", Some("/home/dev"), 24), "~/ccmux");
        // home not a prefix
        assert_eq!(shorten_cwd("/var/log", Some("/home/dev"), 24), "/var/log");
        assert_eq!(shorten_cwd("/home/devxx/a", Some("/home/dev"), 24), "/home/devxx/a");
        // no home given
        assert_eq!(shorten_cwd("/home/dev/a", None, 24), "/home/dev/a");
    }

    #[test]
    fn shorten_cwd_elides_and_respects_max() {
        for max in 1..=40usize {
            let out = shorten_cwd("/home/dev/projects/shared/Foundation", Some("/home/dev"), max);
            assert!(out.chars().count() <= max, "max={max} out={out}");
        }
        assert_eq!(shorten_cwd("/a/b/c", None, 0), "");
        // final component alone longer than max -> end-truncated
        let out = shorten_cwd("/home/dev/averyveryverylongdirectoryname", Some("/home/dev"), 8);
        assert!(out.chars().count() <= 8, "{out}");
        assert!(out.ends_with('…'), "{out}");
    }

    #[test]
    fn truncate_end_edges() {
        assert_eq!(truncate_end("abc", 0), "");
        assert_eq!(truncate_end("abc", 1), "…");
        assert_eq!(truncate_end("abc", 3), "abc");
        assert_eq!(truncate_end("abc", 9), "abc");
        assert_eq!(truncate_end("abcdef", 4), "abc…");
        assert_eq!(truncate_end("", 0), "");
        assert_eq!(truncate_end("", 5), "");
    }

    #[test]
    fn truncate_end_respects_char_boundaries() {
        // multi-byte input: must not panic and must not split a codepoint
        assert_eq!(truncate_end("日本語テキスト", 1), "…");
        assert_eq!(truncate_end("日本語テキスト", 0), "");
        // Width semantics: one wide char (2 cols) + the ellipsis fills 3.
        assert_eq!(truncate_end("日本語テキスト", 3), "日…");
        assert_eq!(truncate_end("日本語テキスト", 5), "日本…");
        assert_eq!(truncate_end("héllo wörld", 4), "hél…");
        // A wide char cannot straddle the budget, so 2 columns hold "…" alone.
        assert_eq!(truncate_end("🙂🙂🙂", 2), "…");
        assert_eq!(truncate_end("🙂🙂🙂", 3), "🙂…");
    }

    #[test]
    fn truncate_end_never_exceeds_the_column_budget() {
        for s in ["回归模型数据清洗与因子测试流水线重构任务", "bt/reg-update", "🙂ok", "ｆｕｌｌ"] {
            for max in 0..24usize {
                let out = truncate_end(s, max);
                assert!(
                    display_width(&out) <= max,
                    "{out:?} is wider than {max} columns"
                );
            }
        }
    }

    #[test]
    fn display_width_charges_two_columns_for_wide_text() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width(""), 0);
        assert_eq!(display_width("回归模型"), 8);
        assert_eq!(display_width("af/回归"), 7);
        assert_eq!(display_width("héllo"), 5);
        // combining acute after "e" occupies no cell of its own
        assert_eq!(display_width("e\u{301}llo"), 4);
        assert_eq!(display_width("🙂"), 2);
        assert_eq!(display_width("ｆｕｌｌ"), 8);
    }

    /// Every number here was MEASURED, not derived from this table: each char
    /// was written to a tmux 3.4 pane with `printf` and the resulting
    /// `#{cursor_x}` recorded. That matters because `display_width` is the
    /// oracle every row budget in `ui.rs` is checked against, so a test that
    /// asked `display_width` what the width should be would agree with itself
    /// on exactly the inputs that break the grid.
    ///
    /// The Wide symbol islands below U+1F300 were the gap: `✅` was charged one
    /// column and drawn in two, so a session name holding one ran its row to
    /// W+1 and pushed the age off the rail onto the margin column.
    #[test]
    fn wide_symbols_below_the_emoji_planes_are_two_columns() {
        for (s, want) in [
            ("✅", 2usize), // U+2705
            ("⭐", 2),      // U+2B50
            ("⌚", 2),      // U+231A
            ("❌", 2),      // U+274C
            ("⏳", 2),      // U+23F3
            ("❓", 2),      // U+2753
            ("❗", 2),      // U+2757
            ("⚡", 2),      // U+26A1
            ("⛔", 2),      // U+26D4
            ("⬛", 2),      // U+2B1B
            ("⭕", 2),      // U+2B55
            ("〈", 2),      // U+2329
            ("🀄", 2),      // U+1F004
            ("🈁", 2),      // U+1F201
            ("🟠", 2),      // U+1F7E0
            ("🩰", 2),      // U+1FA70
            // Narrow neighbours in the same blocks, which must NOT be charged
            // two — over-charging shortens a name for no reason.
            ("⚠", 1),      // U+26A0
            ("❤", 1),      // U+2764
            ("✔", 1),      // U+2714
            ("⬆", 1),      // U+2B06
            ("✓", 1),      // U+2713, the Completed glyph
            ("●", 1),      // U+25CF
            ("▌", 1),      // U+258C, the open marker
            ("▏", 1),      // U+258F, the selection cap
            ("─", 1),      // U+2500
            ("emoji ✅ name", 13),
        ] {
            assert_eq!(display_width(s), want, "{s:?}");
        }
    }

    /// U+FE0F asks for the emoji rendering of the char BEFORE it, and tmux 3.4
    /// answers by drawing that char two columns wide whatever its own width —
    /// `⚠️` is two cells where `⚠` is one. A per-char table cannot see this, so
    /// `display_width` and `truncate_end` share one sequence-aware walk. U+FE0E
    /// (the text selector) does not widen; both selectors are themselves
    /// zero-width, as is the keycap enclosure U+20E3. All measured in tmux 3.4.
    #[test]
    fn the_emoji_variation_selector_widens_the_char_before_it() {
        assert_eq!(display_width("\u{26a0}"), 1, "bare ⚠");
        assert_eq!(display_width("\u{26a0}\u{fe0f}"), 2, "⚠ + VS16");
        assert_eq!(display_width("\u{2764}\u{fe0f}"), 2, "❤ + VS16");
        assert_eq!(display_width("9\u{fe0f}"), 2, "any base widens, even ASCII");
        assert_eq!(display_width("9\u{fe0f}\u{20e3}"), 2, "the keycap encloses");
        assert_eq!(display_width("\u{2705}\u{fe0f}"), 2, "already wide, stays 2");
        assert_eq!(display_width("\u{26a0}\u{fe0e}"), 1, "VS15 does not widen");
        assert_eq!(display_width("\u{2705}\u{fe0e}"), 2, "VS15 does not narrow");
    }

    /// The cut never lands between a char and its variation selector: the pair
    /// is charged as one two-column unit and `truncate_end` stops at the first
    /// unit that does not fit, so a truncated name can never render wider than
    /// `display_width` says it is.
    #[test]
    fn truncate_end_never_splits_a_variation_selector_pair() {
        let s = "ab\u{26a0}\u{fe0f}cd";
        assert_eq!(display_width(s), 6);
        // budget 4 => "ab" + "…"; the pair needs 2 and only 1 column is left.
        let out = truncate_end(s, 4);
        assert_eq!(out, "ab…");
        assert!(!out.contains('\u{fe0f}'), "a bare selector survived: {out:?}");
        // budget 5 => the pair fits whole.
        assert_eq!(truncate_end(s, 5), "ab\u{26a0}\u{fe0f}…");
        for max in 0..10usize {
            let out = truncate_end(s, max);
            assert!(display_width(&out) <= max, "{out:?} exceeds {max}");
            assert_eq!(
                out.contains('\u{26a0}'),
                out.contains('\u{fe0f}'),
                "base and selector must survive together: {out:?}"
            );
        }
        // The same for a Wide symbol that used to be charged one column.
        for max in 0..14usize {
            let out = truncate_end("emoji ✅ name", max);
            assert!(display_width(&out) <= max, "{out:?} exceeds {max}");
        }
    }

    #[test]
    fn filter_haystack_is_lowercased_and_includes_id() {
        let s = &sample()[0];
        let h = s.filter_haystack();
        assert!(h.contains("bt/reg-update"));
        assert!(h.contains("/home/dev/projects/shared/foundation"));
        assert!(h.contains("1c45d64f"));
        assert_eq!(h, h.to_lowercase());
        // interactive row has no short id, and must not panic
        assert!(!sample()[3].filter_haystack().is_empty());
    }

    /// `claude agents --json` emits a THIRD state value beyond working/done:
    /// a session halted by `claude stop` reports `state: "stopped"`. It must not
    /// fall into `Unknown`, which renders as a purple `?` under Idle.
    #[test]
    fn stopped_state_is_modelled_and_groups_as_completed() {
        let raw = r#"[{
            "id": "a2b509dd",
            "sessionId": "a2b509dd-0000-0000-0000-000000000000",
            "cwd": "/home/dev",
            "kind": "background",
            "startedAt": 1787640000000,
            "name": "halted probe",
            "state": "stopped"
        }]"#;
        let parsed = parse_sessions(raw).expect("parse").sessions;
        assert_eq!(parsed.len(), 1);
        let s = &parsed[0];
        assert_eq!(s.state, Some(State::Stopped));
        assert_eq!(s.group(), Group::Completed);
        // A stopped session is still openable: `claude attach` resumes it.
        assert!(s.is_attachable());
    }

}
