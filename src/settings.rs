//! Resolved, non-secret launch settings shared by panes and restart paths.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{codex::{CodexConfig, CodexDiagnostic, CodexFailureKind}, tmux};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexSettings {
    pub url: String,
    pub token_file: PathBuf,
    pub bin: String,
}

impl Default for CodexSettings {
    fn default() -> Self {
        Self { url: String::new(), token_file: PathBuf::new(), bin: "codex".into() }
    }
}

impl CodexSettings {
    /// Resolution reads no credentials and does no DNS. Empty CLI values
    /// override the environment, especially the explicit OFF passed to tmux.
    pub fn resolve(
        url: Option<String>, token: Option<String>,
        env: impl Fn(&str) -> Option<String>, cwd: Option<&Path>,
    ) -> Self {
        let url = url.or_else(|| env("CCMUX_CODEX_URL")).unwrap_or_default();
        let token = token.or_else(|| env("CCMUX_CODEX_TOKEN_FILE")).unwrap_or_default();
        let mut token_file = PathBuf::from(token);
        if !token_file.as_os_str().is_empty() && token_file.is_relative()
            && let Some(cwd) = cwd
        {
            token_file = cwd.join(token_file);
        }
        Self {
            url, token_file,
            bin: env("CCMUX_CODEX_BIN").filter(|s| !s.is_empty()).unwrap_or_else(|| "codex".into()),
        }
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

    pub fn command(&self, exe: &Path, args: impl IntoIterator<Item = OsString>) -> Option<String> {
        let args = self.args(args);
        let bin_env = format!("CCMUX_CODEX_BIN={}", self.bin);
        // env treats even an absolute program path containing '=' as an
        // assignment. A fixed shell executable ends that scan; the launch
        // target and arguments stay positional data, not shell source.
        let mut words = vec!["env", bin_env.as_str(), "/bin/sh", "-c", "exec \"$0\" \"$@\"", exe.to_str()?];
        for arg in &args { words.push(arg.to_str()?); }
        Some(tmux::sh_join(&words))
    }
}

#[cfg(test)]
mod tests;
