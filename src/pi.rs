//! Discover Pi's saved sessions without spawning Pi or loading its extensions.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::model::{self, Kind, Provider, Session, Status};

pub struct Catalog {
    root: PathBuf,
    project_dirs: bool,
    files: BTreeMap<PathBuf, CachedSession>,
}

struct CachedSession {
    modified: SystemTime,
    len: u64,
    row: Option<Session>,
}

impl Catalog {
    pub fn from_env() -> Option<Self> {
        let (root, project_dirs) = session_dir()?;
        Some(Self::new(std::path::absolute(root).ok()?, project_dirs))
    }

    pub(crate) fn new(root: PathBuf, project_dirs: bool) -> Self {
        Self {
            root,
            project_dirs,
            files: BTreeMap::new(),
        }
    }

    pub fn poll(&mut self) -> io::Result<Vec<Session>> {
        let mut paths = Vec::new();
        collect_paths(&self.root, self.project_dirs, &mut paths)?;
        let mut files = BTreeMap::new();
        for (path, metadata) in paths {
            let modified = metadata.modified()?;
            let len = metadata.len();
            let previous = self.files.remove(&path);
            let cached = match previous {
                Some(cached) if cached.modified == modified && cached.len == len => cached,
                previous => {
                    let Ok(row) = read_session(&path, modified) else {
                        // A file can be replaced or removed between stat and open.
                        if let Some(cached) = previous {
                            files.insert(path, cached);
                        }
                        continue;
                    };
                    CachedSession { modified, len, row }
                }
            };
            files.insert(path, cached);
        }
        self.files = files;
        Ok(self
            .files
            .values()
            .filter_map(|cached| cached.row.clone())
            .collect())
    }
}

fn session_dir() -> Option<(PathBuf, bool)> {
    if let Some(path) = env_path("PI_CODING_AGENT_SESSION_DIR") {
        return Some((path, false));
    }
    let agent_dir = env_path("PI_CODING_AGENT_DIR")
        .or_else(|| std::env::home_dir().map(|home| home.join(".pi/agent")))?;
    if let Ok(settings) = fs::read(agent_dir.join("settings.json"))
        && let Ok(settings) = serde_json::from_slice::<Value>(&settings)
        && let Some(dir) = settings["sessionDir"]
            .as_str()
            .filter(|dir| !dir.is_empty())
    {
        return Some((expand_home(PathBuf::from(dir))?, false));
    }
    Some((agent_dir.join("sessions"), true))
}

fn expand_home(path: PathBuf) -> Option<PathBuf> {
    if path == Path::new("~") {
        std::env::home_dir()
    } else if let Ok(relative) = path.strip_prefix("~/") {
        std::env::home_dir().map(|home| home.join(relative))
    } else {
        Some(path)
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    expand_home(PathBuf::from(
        std::env::var_os(key).filter(|value| !value.is_empty())?,
    ))
}

fn collect_paths(
    root: &Path,
    project_dirs: bool,
    paths: &mut Vec<(PathBuf, fs::Metadata)>,
) -> io::Result<()> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if project_dirs && metadata.is_dir() {
            collect_paths(&path, false, paths)?;
        } else if metadata.is_file() && path.to_str().is_some_and(valid_session_path) {
            paths.push((path, metadata));
        }
    }
    Ok(())
}

pub fn valid_session_path(path: &str) -> bool {
    Path::new(path).is_absolute() && path.ends_with(".jsonl") && !path.chars().any(char::is_control)
}

/// Capture the file on the first press and check it again before unlinking.
pub fn file_identity(path: &str) -> io::Result<String> {
    if !valid_session_path(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Pi session path",
        ));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Pi session is not a regular file",
        ));
    }
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}

pub fn delete(path: &str, expected: &str) -> io::Result<()> {
    if file_identity(path)? != expected {
        return Err(io::Error::other("Pi session file was replaced"));
    }
    fs::remove_file(path)
}

#[derive(Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    cwd: String,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: String,
    name: Option<String>,
    message: Option<MessageRole>,
}

#[derive(Deserialize)]
struct MessageRole {
    role: String,
}

fn read_session(path: &Path, modified: SystemTime) -> io::Result<Option<Session>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    let Ok(header) = serde_json::from_slice::<Header>(&line) else {
        return Ok(None);
    };
    if header.kind != "session" || header.id.is_empty() {
        return Ok(None);
    }

    let mut name = None;
    let mut first_prompt = None;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(entry) = serde_json::from_slice::<Entry>(&line) else {
            continue;
        };
        if entry.kind == "session_info" {
            name = entry.name.as_deref().and_then(display_name);
        } else if first_prompt.is_none()
            && entry.kind == "message"
            && entry.message.is_some_and(|message| message.role == "user")
            && let Ok(entry) = serde_json::from_slice::<Value>(&line)
        {
            let content = &entry["message"]["content"];
            first_prompt = content.as_str().and_then(display_name).or_else(|| {
                content
                    .as_array()?
                    .iter()
                    .filter(|block| block["type"] == "text")
                    .find_map(|block| block["text"].as_str().and_then(display_name))
            });
        }
    }

    Ok(Some(Session {
        provider: Provider::Pi,
        codex: None,
        id: display_name(&header.id).map(|id| id.chars().take(8).collect()),
        session_id: path
            .to_str()
            .expect("validated UTF-8 session path")
            .to_owned(),
        pid: None,
        cwd: header.cwd,
        kind: Kind::Interactive,
        started_at: DateTime::<Utc>::from(modified).timestamp_millis(),
        name: name.or(first_prompt).unwrap_or_else(|| "Pi session".into()),
        status: Status::Idle,
        state: None,
    }))
}

fn display_name(raw: &str) -> Option<String> {
    let text = model::clean_display_text(raw);
    let text = text.trim();
    (!text.is_empty()).then(|| model::truncate_end(text, 120))
}
