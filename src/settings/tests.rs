use super::*;

fn env(key: &str) -> Option<String> {
    match key {
        "CCMUX_CODEX_URL" => Some("ws://localhost:8965".into()),
        "CCMUX_CODEX_TOKEN_FILE" => Some("secrets/token file".into()),
        "CCMUX_CODEX_BIN" => Some("/opt/codex custom".into()),
        _ => None,
    }
}

#[test]
fn explicit_settings_win_and_relative_paths_resolve_once() {
    let cwd = Path::new("/workspace");
    let s = CodexSettings::resolve(None, None, env, Some(cwd));
    assert_eq!(s.url, "ws://localhost:8965");
    assert_eq!(s.token_file, Path::new("/workspace/secrets/token file"));
    assert_eq!(s.bin, "/opt/codex custom");
    let explicit = CodexSettings::resolve(Some("ws://127.0.0.2:42".into()),
        Some("/other/token".into()), env, Some(cwd));
    assert_eq!(explicit.url, "ws://127.0.0.2:42");
    assert_eq!(explicit.token_file, Path::new("/other/token"));
    assert!(explicit.config().is_ok());
    assert!(!CodexSettings::resolve(Some(String::new()), None, env, Some(cwd)).enabled());
    assert!(!CodexSettings::resolve(None, None, |_| None, None).enabled());
}

#[test]
fn absent_and_unresolvable_token_paths_never_get_an_implicit_default() {
    let missing = CodexSettings::resolve(Some("ws://localhost".into()), None, |_| None, None);
    assert!(missing.token_file.as_os_str().is_empty());
    let relative = CodexSettings::resolve(None, None, env, None);
    let error = relative.config().err().expect("unresolvable relative path");
    assert_eq!(error.kind, CodexFailureKind::Configuration);
}

#[test]
fn canonical_args_pin_on_and_off_over_conflicting_tmux_environment() {
    for url in ["", "ws://localhost:8965"] {
        let s = CodexSettings { url: url.into(), token_file: "/resolved/token".into(), bin: "codex".into() };
        let original = ["sidebar", "--socket", "ccmux-smoke", "--codex-url=ws://stale",
            "--codex-token-file", "stale", "--interval", "9000"].map(Into::into);
        let args = s.args(original);
        let expected: Vec<OsString> = ["sidebar", "--socket", "ccmux-smoke", "--interval", "9000",
            &format!("--codex-url={url}"), "--codex-token-file=/resolved/token"].map(Into::into).into();
        assert_eq!(args, expected);
        assert_eq!(s.args(args.clone()), args, "canonicalization is idempotent");
    }
}

#[test]
fn command_quotes_each_nonsecret_word_without_reading_token_path() {
    let s = CodexSettings {
        url: "ws://127.0.0.1:8965".into(), token_file: "/never read/a'b $(false)".into(),
        bin: "/bin/a; $(false)".into(),
    };
    let cmd = s.command(Path::new("/opt/ccmux build"), [OsString::from("sidebar")]).unwrap();
    let bin = format!("CCMUX_CODEX_BIN={}", s.bin);
    assert_eq!(cmd, tmux::sh_join(&["env", &bin, "/bin/sh", "-c", "exec \"$0\" \"$@\"", "/opt/ccmux build",
        "sidebar", &format!("--codex-url={}", s.url),
        &format!("--codex-token-file={}", s.token_file.to_str().unwrap())]));
    assert!(!cmd.contains("CODEX_REMOTE_TOKEN"));
}

#[test]
fn every_allowed_host_and_invalid_url_uses_the_client_validator() {
    for url in ["ws://localhost:8965", "ws://127.9.8.7:99", "ws://[::1]:8965"] {
        let s = CodexSettings { url: url.into(), ..CodexSettings::default() };
        assert!(s.config().is_ok());
    }
    for url in ["wss://localhost", "ws://192.0.2.1", "ws://localhost?token=secret"] {
        let s = CodexSettings { url: url.into(), ..CodexSettings::default() };
        let error = s.config().err().unwrap();
        assert_eq!(error.kind, CodexFailureKind::Configuration);
        assert!(!error.message.contains("secret"));
    }
}


#[test]
fn commands_execute_paths_containing_equals_and_preserve_bin_and_argv() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }
    let root = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/tmp")
        .join(format!("ccmux-settings-{}", std::process::id()));
    let dir = Fixture(root);
    let build = dir.0.join("build=rel ' $(false)");
    std::fs::create_dir_all(&build).unwrap();
    let exe = build.join("ccmux");
    // An existing shell image avoids races from execing a just-written script.
    std::os::unix::fs::symlink("/bin/sh", &exe).unwrap();
    for url in ["", "ws://127.0.0.1:8965"] {
        let settings = CodexSettings {
            url: url.into(), token_file: "/fixture/token ' file".into(),
            bin: "/opt/a b' $(false) codex".into(),
        };
        let probe = r#"printf '%s\0' "$CCMUX_CODEX_BIN" "$0" "$@""#;
        let args = ["-c", probe, "sidebar", "--session", "scratch"].map(OsString::from);
        let cmd = settings.command(&exe, args).unwrap();
        let output = std::process::Command::new("/bin/sh").args(["-c", &cmd]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let expected = [&settings.bin, "sidebar", "--session", "scratch",
            &format!("--codex-url={url}"), "--codex-token-file=/fixture/token ' file"].join("\0") + "\0";
        assert_eq!(output.stdout, expected.as_bytes());
    }
}

#[test]
fn rejected_urls_are_forwarded_as_invalid_without_copying_private_values() {
    for url in ["ws://user:fixture-secret@127.0.0.1:8965",
        "ws://127.0.0.1:8965/?token=fixture-secret", "-h", "--", "--dark"]
    {
        let settings = CodexSettings { url: url.into(), ..CodexSettings::default() };
        let args = settings.args(["sidebar"].map(Into::into));
        assert_eq!(args, ["sidebar", "--codex-url=invalid", "--codex-token-file="].map(OsString::from));
        let command = settings.command(Path::new("/ccmux"), ["sidebar"].map(Into::into)).unwrap();
        assert!(!command.contains("fixture-secret"));
        assert!(command.ends_with("--codex-url=invalid --codex-token-file="));
        let child = CodexSettings { url: "invalid".into(), ..CodexSettings::default() };
        assert!(child.enabled());
        assert_eq!(child.config().err().unwrap().message, settings.config().err().unwrap().message);
    }
}
