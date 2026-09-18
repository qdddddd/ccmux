use super::*;

fn env(key: &str) -> Option<String> {
    match key {
        "CCMUX_CODEX_URL" => Some("ws://localhost:8965".into()),
        "CCMUX_CODEX_TOKEN_FILE" => Some("secrets/token file".into()),
        "CCMUX_CODEX_BIN" => Some("/opt/codex custom".into()),
        "HOME" => Some("/home/fixture".into()),
        _ => None,
    }
}

#[test]
fn explicit_settings_win_and_relative_paths_resolve_once() {
    let cwd = Path::new("/workspace");
    let s = CodexSettings::resolve(None, None, env, Some(cwd),
        |_| panic!("explicit environment settings must not probe defaults"));
    assert_eq!(s.url, "ws://localhost:8965");
    assert_eq!(s.token_file, Path::new("/workspace/secrets/token file"));
    assert_eq!(s.bin, "/opt/codex custom");
    let explicit = CodexSettings::resolve(Some("ws://127.0.0.2:42".into()),
        Some("/other/token".into()), env, Some(cwd),
        |_| panic!("explicit settings must not probe defaults"));
    assert_eq!(explicit.url, "ws://127.0.0.2:42");
    assert_eq!(explicit.token_file, Path::new("/other/token"));
    assert!(explicit.config().is_ok());
}

#[test]
fn absent_options_resolve_the_home_or_xdg_defaults_independently() {
    let home = |key: &str| match key {
        "HOME" => Some("/home/fixture".into()),
        _ => None,
    };
    let s = CodexSettings::resolve(None, None, home, None, |path| {
        assert_eq!(path, Path::new("/home/fixture/.config/agents/codex-serve.token"));
        true
    });
    assert_eq!(s.url, DEFAULT_CODEX_URL);
    assert_eq!(s.token_file, Path::new("/home/fixture/.config/agents/codex-serve.token"));

    let xdg = |key: &str| match key {
        "XDG_CONFIG_HOME" => Some("/xdg/config".into()),
        "HOME" => Some("/ignored/home".into()),
        _ => None,
    };
    let s = CodexSettings::resolve(None, None, xdg, None, |path| {
        assert_eq!(path, Path::new("/xdg/config/agents/codex-serve.token"));
        true
    });
    assert_eq!(s.url, DEFAULT_CODEX_URL);
    assert_eq!(s.token_file, Path::new("/xdg/config/agents/codex-serve.token"));

    let empty_xdg = |key: &str| match key {
        "XDG_CONFIG_HOME" => Some(String::new()),
        "HOME" => Some("/fallback/home".into()),
        _ => None,
    };
    let s = CodexSettings::resolve(None, None, empty_xdg, None, |_| true);
    assert_eq!(s.token_file, Path::new("/fallback/home/.config/agents/codex-serve.token"));
}

#[test]
fn missing_fully_defaulted_token_is_quiet_off_but_explicit_values_are_loud() {
    use std::cell::RefCell;
    let home = |key: &str| match key {
        "HOME" => Some("/home/fixture".into()),
        _ => None,
    };
    let probed = RefCell::new(Vec::new());
    let quiet = CodexSettings::resolve(None, None, home, None, |path| {
        probed.borrow_mut().push(path.to_path_buf());
        false
    });
    assert_eq!(quiet, CodexSettings::default());
    assert_eq!(&*probed.borrow(), &[PathBuf::from(
        "/home/fixture/.config/agents/codex-serve.token")]);

    let explicit_url = CodexSettings::resolve(Some("ws://localhost:8965".into()), None,
        home, None, |_| panic!("an explicit URL must stay loud without a default probe"));
    assert!(explicit_url.enabled());
    assert_eq!(explicit_url.token_file,
        Path::new("/home/fixture/.config/agents/codex-serve.token"));

    let explicit_token = CodexSettings::resolve(None, Some("/missing/explicit-token".into()),
        home, None, |_| panic!("an explicit token must stay loud without a default probe"));
    assert!(explicit_token.enabled());
    assert_eq!(explicit_token.url, DEFAULT_CODEX_URL);

    let no_root = |key: &str| match key {
        "HOME" | "XDG_CONFIG_HOME" => Some(String::new()),
        _ => None,
    };
    let quiet = CodexSettings::resolve(None, None, no_root, None,
        |_| panic!("no default token path means there is nothing to probe"));
    assert_eq!(quiet, CodexSettings::default());
}

#[test]
fn empty_flag_or_environment_url_is_off_before_any_existence_probe() {
    use std::cell::Cell;
    let probes = Cell::new(0);
    let explicit = CodexSettings::resolve(Some(String::new()), None, env,
        Some(Path::new("/workspace")), |_: &Path| {
            probes.set(probes.get() + 1);
            true
        });
    assert_eq!(explicit, CodexSettings {
        url: String::new(), token_file: PathBuf::new(), bin: "/opt/codex custom".into(),
        show_imports: false,
    });
    assert_eq!(probes.get(), 0);

    let empty_env = |key: &str| match key {
        "CCMUX_CODEX_URL" => Some(String::new()),
        "HOME" => Some("/home/fixture".into()),
        _ => None,
    };
    let inherited = CodexSettings::resolve(None, None, empty_env, None, |_: &Path| {
        probes.set(probes.get() + 1);
        true
    });
    assert_eq!(inherited, CodexSettings::default());
    assert_eq!(probes.get(), 0, "set-but-empty URL must perform no filesystem probe");
}

#[test]
fn canonical_args_pin_on_and_off_over_conflicting_tmux_environment() {
    for url in ["", "ws://localhost:8965"] {
        let s = CodexSettings { url: url.into(), token_file: "/resolved/token".into(),
            bin: "codex".into(), show_imports: false };
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
        bin: "/bin/a; $(false)".into(), show_imports: false,
    };
    let cmd = s.command(Path::new("/opt/ccmux build"), [OsString::from("sidebar")]).unwrap();
    let bin = format!("CCMUX_CODEX_BIN={}", s.bin);
    let imports = format!("CCMUX_CODEX_IMPORTS={}", s.imports_value());
    assert_eq!(cmd, tmux::sh_join(&["env", &bin, &imports, "/bin/sh", "-c", "exec \"$0\" \"$@\"", "/opt/ccmux build",
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
            bin: "/opt/a b' $(false) codex".into(), show_imports: false,
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

#[test]
fn only_an_explicit_show_lists_claude_transcript_imports() {
    for (value, shown) in [(None, false), (Some(""), false), (Some("show"), true),
        (Some("hide"), false), (Some("Show"), false), (Some("1"), false), (Some("true"), false)]
    {
        let env = |key: &str| match key {
            "CCMUX_CODEX_IMPORTS" => value.map(str::to_owned),
            "HOME" => Some("/home/fixture".into()),
            _ => None,
        };
        let on = CodexSettings::resolve(Some("ws://127.0.0.1:8965".into()),
            Some("/fixture/token".into()), env, None, |_| true);
        assert_eq!(on.show_imports, shown, "{value:?}");
        assert_eq!(on.config().unwrap().show_imports, shown, "{value:?}");
        assert_eq!(on.imports_value(), if shown { "show" } else { "hide" });
        // The resolved value is carried, so a pane never re-decides from its
        // own environment — the rule the URL and token path already follow.
        let cmd = on.command(Path::new("/opt/ccmux"), [OsString::from("sidebar")]).unwrap();
        assert!(cmd.contains(&format!("CCMUX_CODEX_IMPORTS={}", on.imports_value())), "{cmd}");
        // Explicit OFF still answers the question it was asked.
        let off = CodexSettings::resolve(Some(String::new()), None, env, None,
            |_: &Path| panic!("explicit OFF must not probe"));
        assert_eq!(off.show_imports, shown, "{value:?} with codex off");
    }
}
