//! Resolved, non-secret launch settings shared by panes and restart paths.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{codex::{CodexConfig, CodexDiagnostic, CodexFailureKind}, tmux};

pub const DEFAULT_CODEX_URL: &str = "ws://127.0.0.1:8965";
const DEFAULT_TOKEN_NAME: &str = "agents/codex-serve.token";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexSettings {
    pub url: String,
    pub token_file: PathBuf,
    pub bin: String,
    /// `CCMUX_CODEX_IMPORTS=show` lists Codex Desktop's Claude-transcript
    /// imports; anything else, including unset, hides them (§12.2).
    pub show_imports: bool,
}

impl Default for CodexSettings {
    fn default() -> Self {
        Self { url: String::new(), token_file: PathBuf::new(), bin: "codex".into(), show_imports: false }
    }
}

impl CodexSettings {
    /// Resolution reads no credentials and does no DNS. It only probes whether
    /// the fully defaulted token path exists, so a machine without codex-serve
    /// stays quietly OFF. Empty CLI/environment URL values are explicit OFF
    /// and return before that probe.
    pub fn resolve(
        url: Option<String>, token: Option<String>,
        env: impl Fn(&str) -> Option<String>, cwd: Option<&Path>,
        exists: impl Fn(&Path) -> bool,
    ) -> Self {
        let bin = env("CCMUX_CODEX_BIN").filter(|s| !s.is_empty()).unwrap_or_else(|| "codex".into());
        let show_imports = env("CCMUX_CODEX_IMPORTS").as_deref() == Some("show");
        let (url, url_defaulted) = match url {
            Some(url) => (url, false),
            None => match env("CCMUX_CODEX_URL") {
                Some(url) => (url, false),
                None => (DEFAULT_CODEX_URL.into(), true),
            },
        };
        if url.is_empty() {
            return Self { url, token_file: PathBuf::new(), bin, show_imports };
        }

        let (token_file, token_defaulted) = match token {
            Some(token) => (PathBuf::from(token), false),
            None => match env("CCMUX_CODEX_TOKEN_FILE") {
                Some(token) => (PathBuf::from(token), false),
                None => {
                    let root = env("XDG_CONFIG_HOME").filter(|s| !s.is_empty())
                        .map(PathBuf::from)
                        .or_else(|| env("HOME").filter(|s| !s.is_empty())
                            .map(|home| PathBuf::from(home).join(".config")));
                    (root.map(|root| root.join(DEFAULT_TOKEN_NAME)).unwrap_or_default(), true)
                }
            },
        };
        let mut token_file = token_file;
        if !token_file.as_os_str().is_empty() && token_file.is_relative()
            && let Some(cwd) = cwd
        {
            token_file = cwd.join(token_file);
        }
        if url_defaulted && token_defaulted
            && (token_file.as_os_str().is_empty() || !exists(&token_file))
        {
            return Self { url: String::new(), token_file: PathBuf::new(), bin, show_imports };
        }
        Self { url, token_file, bin, show_imports }
    }

    pub fn enabled(&self) -> bool { !self.url.is_empty() }

    pub fn config(&self) -> Result<CodexConfig, CodexDiagnostic> {
        crate::codex::validate_url(&self.url).map_err(|e| e.diagnostic().clone())?;
        if !self.token_file.as_os_str().is_empty() && !self.token_file.is_absolute() {
            return Err(CodexDiagnostic {
                kind: CodexFailureKind::Configuration,
                message: "codex token file needs an absolute local path".into(),
            });
        }
        Ok(CodexConfig {
            url: self.url.clone(), token_file: self.token_file.clone(), bin: self.bin.clone(),
            show_imports: self.show_imports,
        })
    }

    /// Replace inherited flags instead of appending duplicates. The same argv
    /// feeds self-exec and other-sidebar respawns, including explicit OFF.
    pub fn args(&self, args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
        let mut out = Vec::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let s = arg.to_str().unwrap_or("");
            if s == "--codex-url" || s == "--codex-token-file" {
                args.next();
            } else if !s.starts_with("--codex-url=") && !s.starts_with("--codex-token-file=") {
                out.push(arg);
            }
        }
        // Invalid URLs may contain credentials. Keep the child's configuration
        // invalid without copying the rejected value into argv or tmux options.
        let url = if self.url.is_empty() || crate::codex::validate_url(&self.url).is_ok() {
            self.url.as_str()
        } else { "invalid" };
        let mut token = OsString::from("--codex-token-file=");
        token.push(&self.token_file);
        out.extend([OsString::from(format!("--codex-url={url}")), token]);
        out
    }

    pub fn imports_value(&self) -> &'static str {
        if self.show_imports { "show" } else { "hide" }
    }

    pub fn command(&self, exe: &Path, args: impl IntoIterator<Item = OsString>) -> Option<String> {
        let args = self.args(args);
        let bin_env = format!("CCMUX_CODEX_BIN={}", self.bin);
        // Resolved once, then carried: a pane must not re-decide from its own
        // environment, the same rule the URL and token path follow.
        let imports_env = format!("CCMUX_CODEX_IMPORTS={}", self.imports_value());
        // env treats even an absolute program path containing '=' as an
        // assignment. A fixed shell executable ends that scan; the launch
        // target and arguments stay positional data, not shell source.
        let mut words = vec!["env", bin_env.as_str(), imports_env.as_str(),
            "/bin/sh", "-c", "exec \"$0\" \"$@\"", exe.to_str()?];
        for arg in &args { words.push(arg.to_str()?); }
        Some(tmux::sh_join(&words))
    }
}

#[cfg(test)]
mod tests;
