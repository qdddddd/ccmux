//! App-server observations and the guarded archive operation (SPEC §12). No
//! daemon, session, turn, or thread is created here. Preparation owns
//! DNS/credential IO; each wire operation owns one connection and one deadline.

use std::{
    collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher},
    fmt,
    fs::File,
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tungstenite::{
    Message, WebSocket,
    client::{IntoClientRequest, client_with_config},
    handshake::HandshakeError,
    http::{HeaderValue, Uri, header::AUTHORIZATION},
    protocol::WebSocketConfig,
};

use crate::model::{CodexMeta, CodexStatus, Kind, Provider, Session, State, Status, valid_codex_id};

pub const POLL_TIMEOUT: Duration = Duration::from_millis(1000);
pub const HISTORY_DAYS: i64 = 7;
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
const MAX_TOKEN: u64 = 16 * 1024;
// Short kernel waits avoid coarse timer-wheel slack on a one-second timeout.
// Each retry still spends the original whole-poll budget.
const IO_SLICE: Duration = Duration::from_millis(50);
const URL_ERROR: &str =
    "codex url must be loopback ws:// (localhost, 127.0.0.0/8, or [::1])";

pub struct CodexConfig {
    pub url: String,
    pub token_file: PathBuf,
    pub bin: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexFailureKind {
    Configuration, Credential, Connection, Authentication,
    Timeout, Protocol, Incomplete,
}

#[derive(Debug, Clone)]
pub struct CodexDiagnostic {
    pub kind: CodexFailureKind,
    pub message: String,
}

/// Only locally authored messages enter diagnostics. Never retain an IO,
/// HTTP, WebSocket, or RPC error: all can contain server/credential bytes.
#[derive(Debug)]
pub struct CodexError {
    diagnostic: CodexDiagnostic,
    rpc_response: bool,
}

impl CodexError {
    fn new(kind: CodexFailureKind, message: &'static str) -> Self {
        Self {
            diagnostic: CodexDiagnostic { kind, message: message.chars().take(120).collect() },
            rpc_response: false,
        }
    }

    pub fn diagnostic(&self) -> &CodexDiagnostic {
        &self.diagnostic
    }

    fn timeout() -> Self {
        Self::new(CodexFailureKind::Timeout, "codex poll timed out after 1000 ms")
    }

    fn protocol() -> Self {
        Self::new(CodexFailureKind::Protocol, "codex protocol response invalid")
    }

    fn incomplete() -> Self {
        Self::new(CodexFailureKind::Incomplete, "codex listing incomplete")
    }
}

impl fmt::Display for CodexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diagnostic.message)
    }
}

impl std::error::Error for CodexError {}

pub struct CodexObservation {
    pub sessions: Vec<Session>,
    pub complete: bool,
    pub cutoff_ms: i64,
    pub history_metadata_ids: BTreeSet<String>,
    pub diagnostic: Option<CodexDiagnostic>,
    /// Source drift has no Session to carry it. Kept separate from the bounded
    /// provider diagnostic for the app's eventual note_drift delivery.
    pub source_drift: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveOutcome {
    /// `updated_at` is the fresh read's `updatedAt`: the value the local
    /// tombstone recognizes a lagging copy of the archived thread by.
    Archived { updated_at: i64 },
    StateChanged,
}

impl CodexObservation {
    fn new(now_ms: i64) -> Self {
        Self {
            sessions: Vec::new(),
            complete: true,
            cutoff_ms: now_ms.saturating_sub(HISTORY_DAYS * 24 * 60 * 60 * 1000),
            history_metadata_ids: BTreeSet::new(),
            diagnostic: None,
            source_drift: BTreeSet::new(),
        }
    }

    fn fail(&mut self, error: CodexError) {
        self.complete = false;
        if self.diagnostic.is_none() || error.diagnostic.kind == CodexFailureKind::Timeout {
            self.diagnostic = Some(error.diagnostic);
        }
    }
}

// These types deliberately have no Debug/Serialize implementation.
struct Endpoint {
    uri: Uri,
    host: String,
    port: u16,
    literal: Option<IpAddr>,
}

struct Prepared {
    endpoint: Endpoint,
    addresses: Vec<SocketAddr>,
    authorization: HeaderValue,
    token: String,
}

pub struct CodexClient {
    prepared: Prepared,
    exclusions: BTreeMap<String, Exclusion>,
    attempted: BTreeMap<String, u64>,
    sequence: u64,
}

fn endpoint(url: &str) -> Result<Endpoint, CodexError> {
    let invalid = || CodexError::new(CodexFailureKind::Configuration, URL_ERROR);
    // http::Uri intentionally does not implement all URL normalization. Reject
    // forbidden syntax before parsing, including fragments it would discard.
    if !url.starts_with("ws://") || url.contains(['@', '?', '#']) {
        return Err(invalid());
    }
    let uri: Uri = url.parse().map_err(|_| invalid())?;
    let authority = uri.authority().ok_or_else(invalid)?;
    let host = authority.host();
    let bracketed = host.strip_prefix('[').and_then(|s| s.strip_suffix(']'));
    let bare = bracketed.unwrap_or(host);
    let literal = bare.parse::<IpAddr>().ok();
    let valid_host = if bracketed.is_some() {
        matches!(literal, Some(IpAddr::V6(ip)) if ip.is_loopback())
    } else {
        bare.eq_ignore_ascii_case("localhost")
            || matches!(literal, Some(IpAddr::V4(ip)) if ip.is_loopback())
    };
    if !valid_host { return Err(invalid()); }
    // Reject an invalid/empty explicit port instead of silently using 80.
    let suffix = authority.as_str().strip_prefix(host).ok_or_else(invalid)?;
    let port = if suffix.is_empty() { 80 } else {
        suffix.strip_prefix(':').filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|p| *p != 0).ok_or_else(invalid)?
    };
    Ok(Endpoint { host: bare.to_owned(), port, literal, uri })
}

/// Syntax validation does no DNS, file, or socket IO.
pub fn validate_url(url: &str) -> Result<(), CodexError> {
    endpoint(url).map(|_| ())
}

pub fn prepare(config: &CodexConfig) -> Result<CodexClient, CodexError> {
    prepare_with(config, |host, port| {
        (host, port).to_socket_addrs().map(|a| a.collect())
    }, |path| {
        if !std::fs::metadata(path)?.is_file() {
            return Err(io::Error::other("not a regular credential file"));
        }
        let mut file = File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("not a regular credential file"));
        }
        let mut token = String::new();
        (&mut file).take(MAX_TOKEN + 1).read_to_string(&mut token)?;
        Ok(token)
    })
}

fn prepare_with(
    config: &CodexConfig,
    resolve: impl FnOnce(&str, u16) -> io::Result<Vec<SocketAddr>>,
    read_token: impl FnOnce(&Path) -> io::Result<String>,
) -> Result<CodexClient, CodexError> {
    let endpoint = endpoint(&config.url)?;
    let mut addresses = if let Some(ip) = endpoint.literal {
        vec![SocketAddr::new(ip, endpoint.port)]
    } else {
        resolve(&endpoint.host, endpoint.port).map_err(|_| {
            CodexError::new(CodexFailureKind::Connection, "codex localhost resolution failed")
        })?
    };
    if addresses.is_empty() || addresses.iter().any(|a| !a.ip().is_loopback()) {
        return Err(CodexError::new(CodexFailureKind::Configuration,
            "codex localhost resolved outside loopback — open unavailable"));
    }
    addresses.sort_unstable();
    addresses.dedup();
    let credential = || CodexError::new(CodexFailureKind::Credential,
        "codex token file unreadable or invalid");
    if config.token_file.as_os_str().is_empty() {
        return Err(credential());
    }
    let token = read_token(&config.token_file).map_err(|_| credential())?;
    if token.len() as u64 > MAX_TOKEN { return Err(credential()); }
    let token = token.trim_end_matches('\n').to_owned();
    if token.is_empty() || token.len() as u64 > MAX_TOKEN || token.contains(['\r', '\n']) {
        return Err(credential());
    }
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| credential())?;
    authorization.set_sensitive(true);
    Ok(CodexClient {
        prepared: Prepared { endpoint, addresses, authorization, token },
        exclusions: BTreeMap::new(),
        attempted: BTreeMap::new(),
        sequence: 0,
    })
}

// ── One monotonic deadline, including library-internal IO loops ────────────

trait Clock {
    fn now(&self) -> Duration;
}

struct MonotonicClock(Instant);

impl Clock for MonotonicClock {
    fn now(&self) -> Duration { self.0.elapsed() }
}

#[derive(Clone, Copy)]
struct Deadline<'a> {
    clock: &'a dyn Clock,
    end: Duration,
}

impl<'a> Deadline<'a> {
    fn new(clock: &'a dyn Clock) -> Self {
        Self { clock, end: clock.now() + POLL_TIMEOUT }
    }

    fn remaining(self) -> io::Result<Duration> {
        self.end.checked_sub(self.clock.now()).filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    }

    fn check(self) -> Result<(), CodexError> {
        self.remaining().map(|_| ()).map_err(|_| CodexError::timeout())
    }
}

trait SocketIo: Read + Write {
    fn read_timeout(&self, timeout: Duration) -> io::Result<()>;
    fn write_timeout(&self, timeout: Duration) -> io::Result<()>;
}

impl SocketIo for TcpStream {
    fn read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(timeout))
    }
    fn write_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.set_write_timeout(Some(timeout))
    }
}

struct TimedStream<'a, S> {
    inner: S,
    deadline: Deadline<'a>,
}

fn retry_io(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

impl<S: SocketIo> Read for TimedStream<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.inner.read_timeout(self.deadline.remaining()?.min(IO_SLICE))?;
            let result = self.inner.read(buf);
            self.deadline.remaining()?;
            match result {
                Err(error) if retry_io(&error) => continue,
                result => return result,
            }
        }
    }
}

impl<S: SocketIo> Write for TimedStream<'_, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            self.inner.write_timeout(self.deadline.remaining()?.min(IO_SLICE))?;
            let result = self.inner.write(buf);
            self.deadline.remaining()?;
            match result {
                Err(error) if retry_io(&error) => continue,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        loop {
            self.inner.write_timeout(self.deadline.remaining()?.min(IO_SLICE))?;
            let result = self.inner.flush();
            self.deadline.remaining()?;
            match result {
                Err(error) if retry_io(&error) => continue,
                result => return result,
            }
        }
    }
}

fn io_error(error: io::Error) -> CodexError {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => CodexError::timeout(),
        _ => CodexError::new(CodexFailureKind::Connection, "codex connection failed"),
    }
}

fn ws_error(error: tungstenite::Error) -> CodexError {
    match error {
        tungstenite::Error::Io(e) => io_error(e),
        tungstenite::Error::Http(r) if matches!(r.status().as_u16(), 401 | 403) =>
            CodexError::new(CodexFailureKind::Authentication, "codex authentication failed"),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed =>
            CodexError::new(CodexFailureKind::Connection, "codex connection closed"),
        _ => CodexError::protocol(),
    }
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default().read_buffer_size(16 * 1024)
        .write_buffer_size(0).max_write_buffer_size(MAX_MESSAGE + 1024)
        .max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE))
}

trait Transport {
    // The ignored harness scopes individual observation waits within its
    // longer connection lifetime. Production polls retain one fixed deadline.
    #[cfg(test)]
    fn set_deadline(&mut self, _end: Duration) {}
    fn send(&mut self, value: Value) -> Result<(), CodexError>;
    fn receive(&mut self) -> Result<Value, CodexError>;
    fn close(&mut self) -> Result<(), CodexError>;
}

struct WsTransport<'a, S> {
    socket: WebSocket<TimedStream<'a, S>>,
    deadline: Deadline<'a>,
}

impl<S: SocketIo> Transport for WsTransport<'_, S> {
    #[cfg(test)]
    fn set_deadline(&mut self, end: Duration) {
        self.deadline.end = end;
        self.socket.get_mut().deadline.end = end;
    }

    fn send(&mut self, value: Value) -> Result<(), CodexError> {
        self.deadline.check()?;
        self.socket.send(Message::Text(value.to_string().into())).map_err(ws_error)?;
        self.deadline.check()
    }

    fn receive(&mut self) -> Result<Value, CodexError> {
        loop {
            self.deadline.check()?;
            let message = self.socket.read().map_err(ws_error)?;
            self.deadline.check()?;
            match message {
                Message::Text(text) => {
                    let value = serde_json::from_str(&text).map_err(|_| CodexError::protocol())?;
                    self.deadline.check()?;
                    return Ok(value);
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(_) => return Err(CodexError::new(
                    CodexFailureKind::Connection, "codex connection closed")),
                _ => return Err(CodexError::protocol()),
            }
        }
    }

    fn close(&mut self) -> Result<(), CodexError> {
        self.deadline.check()?;
        self.socket.close(None).map_err(ws_error)?;
        self.socket.flush().map_err(ws_error)?;
        self.deadline.check()
    }
}

trait Connector {
    fn connect<'a>(&mut self, prepared: &Prepared, deadline: Deadline<'a>)
        -> Result<Box<dyn Transport + 'a>, CodexError>;
}

struct TcpConnector;

fn connect_addresses<S>(
    addresses: &[SocketAddr], deadline: Deadline<'_>,
    mut connect: impl FnMut(&SocketAddr, Duration) -> io::Result<S>,
) -> Result<S, CodexError> {
    let mut last = CodexError::new(CodexFailureKind::Connection, "codex connection failed");
    for address in addresses {
        let remaining = deadline.remaining().map_err(|_| CodexError::timeout())?;
        let result = connect(address, remaining);
        deadline.check()?;
        match result {
            Ok(stream) => return Ok(stream),
            Err(error) => last = io_error(error),
        }
    }
    Err(last)
}

impl Connector for TcpConnector {
    fn connect<'a>(&mut self, prepared: &Prepared, deadline: Deadline<'a>)
        -> Result<Box<dyn Transport + 'a>, CodexError>
    {
        let stream = connect_addresses(&prepared.addresses, deadline, TcpStream::connect_timeout)?;
        let stream = TimedStream { inner: stream, deadline };
        let mut request = prepared.endpoint.uri.clone().into_client_request()
            .map_err(ws_error)?;
        request.headers_mut().insert(AUTHORIZATION, prepared.authorization.clone());
        let (socket, _) = client_with_config(request, stream, Some(websocket_config()))
            .map_err(|error| match error {
                HandshakeError::Failure(error) => ws_error(error),
                HandshakeError::Interrupted(_) => CodexError::timeout(),
            })?;
        deadline.check()?;
        Ok(Box::new(WsTransport { socket, deadline }))
    }
}

// Only this enum can construct a request. There is no public arbitrary RPC.
enum Method<'a> {
    Initialize,
    Loaded(Option<&'a str>),
    History(Option<&'a str>),
    Read(&'a str),
    Archive(&'a str),
}

impl Method<'_> {
    fn request(&self) -> (&'static str, Value) {
        match self {
            Self::Initialize => ("initialize", json!({
                "clientInfo":{"name":"ccmux","version":env!("CARGO_PKG_VERSION")},
                "capabilities":{"experimentalApi":true}
            })),
            Self::Loaded(cursor) => ("thread/loaded/list", json!({"limit":100,"cursor":cursor})),
            Self::History(cursor) => ("thread/list", json!({
                "limit":100, "cursor":cursor, "sortKey":"updated_at", "sortDirection":"desc",
                "archived":false, "sourceKinds":["cli","vscode","exec","appServer","unknown"],
                "modelProviders":[], "useStateDbOnly":true
            })),
            Self::Read(id) => ("thread/read", json!({"threadId":id,"includeTurns":false})),
            Self::Archive(id) => ("thread/archive", json!({"threadId":id})),
        }
    }

    fn rejection(&self) -> &'static str {
        match self {
            Self::Archive(_) => "codex archive RPC rejected",
            _ => "codex read-only RPC rejected",
        }
    }
}

struct Rpc<'a> {
    transport: Box<dyn Transport + 'a>,
    deadline: Deadline<'a>,
    next_id: u64,
    attempted: bool,
}

impl Rpc<'_> {
    fn request(&mut self, method: Method<'_>) -> Result<Value, CodexError> {
        self.attempted = false;
        self.deadline.check()?;
        let rejection = method.rejection();
        let (method, params) = method.request();
        // Exercise both protocol ID types: initialize is 0, all later requests
        // have string IDs. Server request IDs never enter this counter.
        let id = if self.next_id == 0 { json!(0) } else { json!(format!("ccmux-{}", self.next_id)) };
        self.next_id += 1;
        self.attempted = true;
        self.transport.send(json!({"id":id,"method":method,"params":params}))?;
        loop {
            self.deadline.check()?;
            let value = self.transport.receive()?;
            self.deadline.check()?;
            let object = value.as_object().ok_or_else(CodexError::protocol)?;
            if object.contains_key("method") {
                if !object["method"].is_string() { return Err(CodexError::protocol()); }
                // Field presence is intentional, including id:0 and id:null.
                // A response, even -32601, can deny someone else's approval.
                continue;
            }
            if object.get("id") != Some(&id) {
                return Err(CodexError::protocol());
            }
            match (object.get("result"), object.get("error")) {
                (Some(result), None) => return Ok(result.clone()),
                (None, Some(error)) if error.get("code").and_then(Value::as_i64).is_some()
                    && error.get("message").and_then(Value::as_str).is_some() =>
                {
                    let mut error = CodexError::new(CodexFailureKind::Protocol, rejection);
                    error.rpc_response = true;
                    return Err(error);
                }
                _ => return Err(CodexError::protocol()),
            }
        }
    }

    fn initialized(&mut self) -> Result<(), CodexError> {
        self.deadline.check()?;
        self.transport.send(json!({"method":"initialized","params":{}}))?;
        self.deadline.check()
    }
}

// ── Thread parsing and observation union ──────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exclusion { Ephemeral, Parent, SubAgent, Custom }

impl Exclusion {
    fn contradicted(self, row: &Value) -> bool {
        match self {
            Self::Ephemeral => row.get("ephemeral") == Some(&Value::Bool(false)),
            Self::Parent => row.get("parentThreadId").is_none_or(Value::is_null),
            Self::SubAgent | Self::Custom => match source(row.get("source")) {
                Source::Eligible => true,
                Source::Excluded(other) => self != other,
                Source::Unrecognized => false,
            },
        }
    }
}

enum Source { Eligible, Excluded(Exclusion), Unrecognized }

fn source(value: Option<&Value>) -> Source {
    match value {
        Some(Value::String(s)) if ["cli","vscode","exec","appServer","unknown"].contains(&s.as_str()) =>
            Source::Eligible,
        Some(Value::Object(o)) if o.len() == 1 && o.contains_key("subAgent") =>
            Source::Excluded(Exclusion::SubAgent),
        Some(Value::Object(o)) if o.len() == 1 && o.get("custom").is_some_and(Value::is_string) =>
            Source::Excluded(Exclusion::Custom),
        _ => Source::Unrecognized,
    }
}

enum Parsed { Excluded(Exclusion), Eligible(Box<Session>) }

enum RowError { Invalid, Source }

fn thread_id(row: &Value) -> Option<&str> {
    row.get("id").and_then(Value::as_str).filter(|id| valid_codex_id(id))
}

fn timestamp(row: &Value, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)?.checked_mul(1000)
}

fn parse_thread(row: &Value) -> Result<Parsed, RowError> {
    let id = thread_id(row).ok_or(RowError::Invalid)?;
    if row.get("ephemeral") == Some(&Value::Bool(true)) {
        return Ok(Parsed::Excluded(Exclusion::Ephemeral));
    }
    if row.get("parentThreadId").is_some_and(|p| !p.is_null()) {
        return Ok(Parsed::Excluded(Exclusion::Parent));
    }
    match source(row.get("source")) {
        Source::Excluded(exclusion) => return Ok(Parsed::Excluded(exclusion)),
        Source::Unrecognized => return Err(RowError::Source),
        Source::Eligible => {}
    }
    if row.get("ephemeral") != Some(&Value::Bool(false)) {
        return Err(RowError::Invalid);
    }
    let cwd = row.get("cwd").and_then(Value::as_str).filter(|s| {
        Path::new(s).is_absolute() && !s.contains('\0')
    }).ok_or(RowError::Invalid)?;
    let started_at = timestamp(row, "createdAt").ok_or(RowError::Invalid)?;
    let updated_at = timestamp(row, "updatedAt").ok_or(RowError::Invalid)?;
    let preview = row.get("preview").and_then(Value::as_str).ok_or(RowError::Invalid)?;
    let name = match row.get("name") {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s,
        _ => return Err(RowError::Invalid),
    };
    let short: String = id.chars().filter(|c| *c != '-').rev().take(8)
        .collect::<Vec<_>>().into_iter().rev().collect::<String>().to_ascii_lowercase();
    let name = clean_text(name).replace('\n', " ");
    let preview = clean_text(preview);
    let name = if !name.trim().is_empty() { name.trim().to_owned() } else {
        preview.lines().find(|s| !s.trim().is_empty()).map(|s| s.trim().to_owned())
            .unwrap_or_else(|| format!("Codex {short}"))
    };
    let runtime = row.get("status").and_then(Value::as_object).ok_or(RowError::Invalid)?;
    let tag = runtime.get("type").and_then(Value::as_str).ok_or(RowError::Invalid)?;
    let (runtime, status, state) = match tag {
        "notLoaded" => (CodexStatus::NotLoaded, Status::Idle, Some(State::Unloaded)),
        "idle" => (CodexStatus::Idle, Status::Idle, None),
        "systemError" => (CodexStatus::SystemError, Status::Unknown(tag.into()), None),
        "active" => {
            let mut flags: Vec<String> = runtime.get("activeFlags").and_then(Value::as_array)
                .ok_or(RowError::Invalid)?.iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or(RowError::Invalid))
                .collect::<Result<_, _>>()?;
            flags.sort();
            flags.dedup();
            let (status, state) = if flags.iter().any(|f| matches!(f.as_str(),
                "waitingOnApproval" | "waitingOnUserInput"))
            {
                (Status::Waiting, State::Blocked)
            } else if flags.is_empty() {
                (Status::Busy, State::Working)
            } else {
                (Status::Unknown(flags.join(", ")), State::Working)
            };
            (CodexStatus::Active { flags }, status, Some(state))
        }
        _ => (CodexStatus::Unknown(tag.into()), Status::Unknown(tag.into()), None),
    };
    Ok(Parsed::Eligible(Box::new(Session {
        provider: Provider::Codex,
        codex: Some(CodexMeta { updated_at, runtime }),
        id: Some(short), session_id: id.into(), pid: None, cwd: cwd.into(),
        kind: Kind::Background, started_at, name, status, state,
    })))
}

/// Strip ANSI controls without depending on agents (SPEC §12.3). Preserve
/// newlines only for preview-line selection; all other terminal controls go.
fn clean_text(raw: &str) -> String {
    let mut output = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() { if ('@'..='~').contains(&c) { break; } }
                }
                Some(']') | Some('P') | Some('^') | Some('_') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) { break; }
                    }
                }
                Some(c) if (' '..='/').contains(&c) => {
                    for c in chars.by_ref() { if ('0'..='~').contains(&c) { break; } }
                }
                _ => {}
            },
            '\n' => output.push(c),
            c if c.is_control() => {}
            c => output.push(c),
        }
    }
    output
}

/// Stable fleet projection. Timestamps/provenance never reset idle backoff.
pub fn fingerprint(sessions: &[Session]) -> u64 {
    let mut rows: Vec<_> = sessions.iter().collect();
    rows.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    let mut hash = DefaultHasher::new();
    for row in rows {
        (&row.provider, &row.session_id, &row.name, &row.cwd, &row.status, &row.state).hash(&mut hash);
        row.codex.as_ref().map(|m| &m.runtime).hash(&mut hash);
    }
    hash.finish()
}

struct Union {
    observation: CodexObservation,
    loaded: BTreeSet<String>,
    metadata: BTreeMap<String, Value>,
    conflicted: BTreeSet<String>,
    sessions: BTreeMap<String, Session>,
}

impl Union {
    fn new(now_ms: i64) -> Self {
        Self {
            observation: CodexObservation::new(now_ms),
            loaded: BTreeSet::new(), metadata: BTreeMap::new(),
            conflicted: BTreeSet::new(), sessions: BTreeMap::new(),
        }
    }

    fn ingest(&mut self, row: Value, history: bool, client: &mut CodexClient) -> bool {
        let Some(id) = thread_id(&row).map(str::to_owned) else {
            self.observation.fail(CodexError::incomplete());
            return false;
        };
        if history { self.observation.history_metadata_ids.insert(id.clone()); }
        if self.conflicted.contains(&id) { return false; }
        if let Some(previous) = self.metadata.get(&id) {
            if previous == &row { return true; }
            self.conflicted.insert(id.clone());
            self.sessions.remove(&id);
            client.exclusions.remove(&id);
            self.observation.fail(CodexError::incomplete());
            return false;
        }
        let contradiction = client.exclusions.get(&id).is_some_and(|v| v.contradicted(&row));
        if contradiction {
            client.exclusions.remove(&id);
            self.observation.fail(CodexError::incomplete());
        }
        let valid = match parse_thread(&row) {
            Ok(Parsed::Excluded(exclusion)) => {
                if self.loaded.contains(&id) && !contradiction {
                    client.exclusions.insert(id.clone(), exclusion);
                }
                true
            }
            Ok(Parsed::Eligible(session)) => {
                if self.loaded.contains(&id) || session.codex.as_ref().unwrap().updated_at >= self.observation.cutoff_ms {
                    self.sessions.insert(id.clone(), *session);
                }
                true
            }
            Err(error) => {
                if matches!(error, RowError::Source) {
                    let raw = row.get("source").map(Value::to_string).unwrap_or_else(|| "<missing>".into());
                    self.observation.source_drift.insert(client.redact(&raw));
                }
                self.observation.fail(CodexError::incomplete());
                false
            }
        };
        self.metadata.insert(id, row);
        valid && !contradiction
    }

    fn finish(mut self) -> CodexObservation {
        self.observation.sessions = self.sessions.into_values().collect();
        self.observation
    }
}

struct Page<'a> {
    data: &'a [Value],
    next: Option<&'a str>,
    valid_cursor: bool,
}

fn page(value: &Value) -> Result<Page<'_>, CodexError> {
    let data = value.get("data").and_then(Value::as_array).ok_or_else(CodexError::protocol)?;
    let (next, valid_cursor) = match value.get("nextCursor") {
        None | Some(Value::Null) => (None, true), // Optional in the measured schema.
        Some(Value::String(s)) if !s.is_empty() => (Some(s.as_str()), true),
        _ => (None, false),
    };
    Ok(Page { data, next, valid_cursor })
}

impl CodexClient {
    pub fn replace_prepared(&mut self, prepared: CodexClient) {
        // The workspace endpoint is fixed (§12.2); the caller re-prepares its
        // same effective config. Replacement carries no observations over.
        self.prepared = prepared.prepared;
    }

    /// Forget provider-local metadata for a thread removed by an archive.
    /// A later unarchive is rediscovered from the authoritative history list.
    pub fn forget(&mut self, id: &str) {
        self.exclusions.remove(id);
        self.attempted.remove(id);
    }

    fn redact(&self, text: &str) -> String {
        clean_text(&text.replace(&self.prepared.token, "[redacted]"))
    }

    pub fn poll(&mut self, now_ms: i64) -> CodexObservation {
        let clock = MonotonicClock(Instant::now());
        self.poll_with(now_ms, &clock, &mut TcpConnector)
    }

    pub fn archive(&mut self, id: &str, expected: &CodexStatus) -> Result<ArchiveOutcome, CodexError> {
        let clock = MonotonicClock(Instant::now());
        let result = self.archive_with(id, expected, &clock, &mut TcpConnector);
        result.map_err(|mut error| {
            if error.diagnostic.kind == CodexFailureKind::Timeout {
                error.diagnostic.message = "codex archive timed out after 1000 ms".into();
            }
            error
        })
    }

    /// `expected` is the runtime of the row as re-checked at settle. Only
    /// `idle` for an Idle row or `notLoaded` for an Unloaded one passes:
    /// `notLoaded` -> `idle` is the one visible sign that a client outside
    /// ccmux attached since the poll, and an attached TUI breaks silently.
    fn archive_with(
        &mut self, id: &str, expected: &CodexStatus, clock: &dyn Clock, connector: &mut impl Connector,
    ) -> Result<ArchiveOutcome, CodexError> {
        if !valid_codex_id(id) { return Err(CodexError::protocol()); }
        let deadline = Deadline::new(clock);
        let transport = connector.connect(&self.prepared, deadline)?;
        deadline.check()?;
        let mut rpc = Rpc { transport, deadline, next_id: 0, attempted: false };
        rpc.request(Method::Initialize)?.get("userAgent").and_then(Value::as_str)
            .ok_or_else(CodexError::protocol)?;
        rpc.initialized()?;
        let value = rpc.request(Method::Read(id))?;
        let row = value.get("thread").ok_or_else(CodexError::protocol)?;
        let tag = row.get("status").and_then(|status| status.get("type")).and_then(Value::as_str);
        let eligible = thread_id(row) == Some(id) && matches!((expected, tag),
            (CodexStatus::Idle, Some("idle")) | (CodexStatus::NotLoaded, Some("notLoaded")));
        let outcome = if eligible {
            let updated_at = timestamp(row, "updatedAt").ok_or_else(CodexError::protocol)?;
            let result = rpc.request(Method::Archive(id))?;
            if !result.as_object().is_some_and(serde_json::Map::is_empty) {
                return Err(CodexError::protocol());
            }
            ArchiveOutcome::Archived { updated_at }
        } else {
            ArchiveOutcome::StateChanged
        };
        deadline.check()?;
        rpc.transport.close()?;
        deadline.check()?;
        Ok(outcome)
    }

    fn poll_with(&mut self, now_ms: i64, clock: &dyn Clock, connector: &mut impl Connector)
        -> CodexObservation
    {
        let deadline = Deadline::new(clock);
        let mut union = Union::new(now_ms);
        let result = (|| {
            let transport = connector.connect(&self.prepared, deadline)?;
            deadline.check()?;
            let mut rpc = Rpc { transport, deadline, next_id: 0, attempted: false };
            rpc.request(Method::Initialize)?.get("userAgent").and_then(Value::as_str)
                .ok_or_else(CodexError::protocol)?;
            rpc.initialized()?;
            self.collect(&mut rpc, &mut union)?;
            deadline.check()?;
            rpc.transport.close()?;
            deadline.check()
        })();
        if let Err(error) = result { union.observation.fail(error); }
        union.finish()
    }

    fn collect(&mut self, rpc: &mut Rpc<'_>, union: &mut Union) -> Result<(), CodexError> {
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        let mut loaded_complete = true;
        loop {
            let value = rpc.request(Method::Loaded(cursor.as_deref()))?;
            let page = page(&value)?;
            if !page.valid_cursor {
                loaded_complete = false;
                union.observation.fail(CodexError::incomplete());
            }
            for id in page.data {
                rpc.deadline.check()?;
                if let Some(id) = id.as_str().filter(|id| valid_codex_id(id)) {
                    union.loaded.insert(id.to_owned());
                } else {
                    loaded_complete = false;
                    union.observation.fail(CodexError::incomplete());
                }
            }
            let Some(next) = page.next else { break; };
            if !seen.insert(next.to_owned()) {
                loaded_complete = false;
                union.observation.fail(CodexError::incomplete());
                break;
            }
            cursor = Some(next.to_owned());
        }
        if loaded_complete {
            self.exclusions.retain(|id, _| union.loaded.contains(id));
            self.attempted.retain(|id, _| union.loaded.contains(id));
        }

        cursor = None;
        seen.clear();
        let mut previous_time = None;
        let mut ordered = true;
        loop {
            let value = rpc.request(Method::History(cursor.as_deref()))?;
            let page = page(&value)?;
            let mut crosses = false;
            let mut valid_page = page.valid_cursor;
            if !page.valid_cursor { union.observation.fail(CodexError::incomplete()); }
            for row in page.data {
                rpc.deadline.check()?;
                if let Some(time) = timestamp(row, "updatedAt") {
                    if previous_time.is_some_and(|previous| time > previous) {
                        ordered = false;
                        union.observation.fail(CodexError::incomplete());
                    }
                    previous_time = Some(time);
                    crosses |= time < union.observation.cutoff_ms;
                } else {
                    // An excluded row needn't have usable timestamps, but it
                    // cannot justify stopping the history walk early.
                    ordered = false;
                }
                valid_page &= union.ingest(row.clone(), true, self);
            }
            let Some(next) = page.next else { break; };
            if !seen.insert(next.to_owned()) {
                union.observation.fail(CodexError::incomplete());
                break;
            }
            if ordered && valid_page && crosses { break; }
            cursor = Some(next.to_owned());
        }

        let mut missing: Vec<_> = union.loaded.iter().filter(|id| {
            !union.metadata.contains_key(*id) && !self.exclusions.contains_key(*id)
        }).cloned().collect();
        missing.sort_by(|a, b| self.attempted.get(a).cmp(&self.attempted.get(b)).then(a.cmp(b)));
        for id in missing {
            rpc.deadline.check()?; // An unattempted read must retain its place.
            let result = rpc.request(Method::Read(&id));
            if rpc.attempted {
                self.sequence += 1;
                self.attempted.insert(id.clone(), self.sequence);
            }
            match result {
                Ok(value) => {
                    let row = value.get("thread").filter(|row| thread_id(row) == Some(id.as_str()));
                    if let Some(row) = row {
                        union.ingest(row.clone(), false, self);
                    } else {
                        union.observation.fail(CodexError::incomplete());
                    }
                }
                Err(error) if error.rpc_response => union.observation.fail(error),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// Prepare a fresh client, then perform the read-before-archive transaction.
/// Preparation may do DNS and credential IO; the connection, initialize,
/// fresh read, optional archive, and close share one 1000 ms deadline.
pub fn archive(config: &CodexConfig, id: &str, expected: &CodexStatus)
    -> Result<ArchiveOutcome, CodexDiagnostic>
{
    let mut client = prepare(config).map_err(|error| error.diagnostic().clone())?;
    client.archive(id, expected).map_err(|error| error.diagnostic().clone())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod live_tests;
