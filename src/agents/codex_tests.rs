//! Pane scripts run only against private fixture executables, never tmux or Codex.
use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const ID: &str = "01a0a609-12a6-7000-8000-123456789abc";
const SECRET: &str = "fixture-only-remote-token";

fn row() -> Session {
    let mut row = crate::model::parse_sessions(
        r#"[{"id":"789abc00","sessionId":"fixture","name":"probe","cwd":"/unused"}]"#
    ).unwrap().sessions.remove(0);
    row.provider = Provider::Codex;
    row.session_id = ID.into();
    row
}

fn config() -> CodexConfig {
    CodexConfig { url: "ws://127.0.0.1:8965".into(),
        token_file: "/fixture/token".into(), bin: "codex".into(), show_imports: false }
}

fn entry(row: &Session) -> PaneEntry {
    PaneEntry { provider: row.provider, session_id: row.session_id.clone(),
        short_id: row.id.clone().unwrap_or_default(), name: row.name.clone(), opened_at: 1 }
}

#[test]
fn codex_facade_refuses_before_short_id_lookup_or_any_claude_process() {
    test_spawn::reset();
    for id in [None, Some(""), Some("789abc00"), Some(ID)] {
        let row = Session { id: id.map(str::to_owned), ..row() };
        for (verb, result) in [("stop", stop(&row)), ("delete", delete(&row)),
            ("restart", respawn(&row)), ("logs", logs(&row, 10).map(|_| ()))]
        {
            let error = result.unwrap_err();
            assert!(matches!(error, AgentsError::UnsupportedProvider { provider: Provider::Codex, verb: v } if v == verb));
            if verb == "restart" { assert_eq!(error.to_string(), "Codex restart unavailable in v1"); }
        }
    }
    assert!(matches!(dispatch_background(Provider::Codex, "/unused", "task"),
        Err(AgentsError::UnsupportedProvider { provider: Provider::Codex, verb: "dispatch" })));
    assert!(test_spawn::calls().is_empty());

    let row = Session { provider: Provider::Claude, ..row() };
    stop(&row).unwrap();
    respawn(&row).unwrap();
    delete(&row).unwrap();
    logs(&row, 10).unwrap();
    dispatch_background(Provider::Claude, "/unused", "task").unwrap();
    assert_eq!(test_spawn::joined(), ["stop 789abc00", "respawn 789abc00",
        "rm 789abc00", "logs 789abc00", "--bg task"]);
}

#[test]
fn pane_and_entry_facades_address_codex_by_full_launch_id_and_claude_by_short_id() {
    let config = config();
    let mut row = row();
    row.id = None; // display id is not an address or an attach gate
    row.name = "name must not enter a shell command".into();
    let cmd = attach_pane_cmd(&row, Some(&config)).unwrap();
    assert_eq!(cmd, attach_entry_cmd(&entry(&row), Some(&config)).unwrap());
    assert!(cmd.contains(&format!("resume {ID}; rc=$?")));
    assert!(!cmd.contains(&row.name));
    assert!(!cmd.contains("claude"));
    assert!(!cmd.contains(" --cwd ") && !cmd.contains(" --model "));
    row.provider = Provider::Claude;
    row.id = Some("c1a0de00".into());
    let cmd = attach_pane_cmd(&row, None).unwrap();
    assert_eq!(cmd, attach_entry_cmd(&entry(&row), None).unwrap());
    assert!(cmd.contains("claude attach c1a0de00; rc=$?"));
    assert!(!cmd.contains(ID));
}

#[test]
fn attach_configuration_checks_do_no_preparation_or_fallback() {
    let row = row();
    assert!(matches!(attach_pane_cmd(&row, None), Err(AgentsError::NotConfigured(Provider::Codex))));
    for url in ["", "ws://example.com:8965", "wss://localhost:8965", "ws://[127.0.0.1]:8965", "ws://localhost:+8965"] {
        let mut config = config();
        config.url = url.into();
        let error = attach_entry_cmd(&entry(&row), Some(&config)).unwrap_err();
        assert_eq!(error.to_string(), "codex not configured — open unavailable");
    }
    for path in ["", "relative/token"] {
        let mut config = config();
        config.token_file = path.into();
        assert!(matches!(attach_pane_cmd(&row, Some(&config)), Err(AgentsError::NotConfigured(_))));
    }
    // An absolute missing token remains the TUI's retry problem: building does no IO.
    let mut config = config();
    assert!(attach_pane_cmd(&row, Some(&config)).is_ok());
    {
        use std::os::unix::ffi::OsStringExt;
        config.token_file = std::ffi::OsString::from_vec(vec![b'/', 0xff]).into();
        assert!(matches!(attach_pane_cmd(&row, Some(&config)), Err(AgentsError::NotConfigured(_))));
    }
    config.token_file = "/fixture/token".into();
    let malformed = Session { session_id: "not-a-thread".into(), ..row };
    assert!(matches!(attach_pane_cmd(&malformed, Some(&config)), Err(AgentsError::NotAttachable)));
}

#[test]
fn codex_wrapper_snapshot_preserves_literal_resume_and_configured_socket() {
    let cmd = codex_attach_cmd(ID, &config(), Some("ccmux-probe")).unwrap();
    assert_eq!(cmd, concat!(
        "while :; do ",
        "case \"${TMUX_PANE:-}\" in %*) case \"${TMUX_PANE#%}\" in ''|*[!0-9]*) exit 2;; esac;; *) exit 2;; esac; ",
        "tmux -L ccmux-probe set-option -p -u -t \"$TMUX_PANE\" @ccmux_detached 2>/dev/null; ",
        "CODEX_REMOTE_TOKEN=\"$(cat /fixture/token)\" codex --remote ws://127.0.0.1:8965 --remote-auth-token-env CODEX_REMOTE_TOKEN resume 01a0a609-12a6-7000-8000-123456789abc; rc=$?; ",
        "tmux -L ccmux-probe set-option -p -t \"$TMUX_PANE\" @ccmux_detached 1 2>/dev/null; ",
        "printf '\\n[ccmux] Codex attach exited (rc=%s). resume: %s\\n' \"$rc\" 'CODEX_REMOTE_TOKEN=\"$(cat /fixture/token)\" codex --remote ws://127.0.0.1:8965 --remote-auth-token-env CODEX_REMOTE_TOKEN resume 01a0a609-12a6-7000-8000-123456789abc'; ",
        "printf '[ccmux] enter=resume  s=shell  q=close pane: '; ",
        "read ans || ans=q; case \"$ans\" in s|S) break;; q|Q) exit \"$rc\";; esac; done; ",
        "tmux -L ccmux-probe set-option -p -t \"$TMUX_PANE\" @ccmux_detached shell 2>/dev/null; ",
        "printf '\\n'; [ -x \"${SHELL:-}\" ] || SHELL=/bin/sh; exec \"$SHELL\" -l"
    ));
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/tmp");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("ccmux-attach-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&path).unwrap();
        let fixture = Self(path);
        fixture.executable("tmux", "#!/bin/sh\nprintf 'tmux' >> \"$TRACE\"\nprintf ' <%s>' \"$@\" >> \"$TRACE\"\nprintf '\\n' >> \"$TRACE\"\n");
        fixture.executable("codex", &format!(
            "#!/bin/sh\n[ \"$CODEX_REMOTE_TOKEN\" = '{SECRET}' ] || exit 65\nprintf 'codex' >> \"$TRACE\"\nprintf ' <%s>' \"$@\" >> \"$TRACE\"\nprintf '\\n' >> \"$TRACE\"\nexit 47\n"));
        fixture.executable("shell", "#!/bin/sh\n[ -z \"${CODEX_REMOTE_TOKEN+x}\" ] || exit 98\nprintf 'shell <%s>\\n' \"$*\" >> \"$TRACE\"\nexit 19\n");
        std::os::unix::fs::symlink("/bin/cat", fixture.0.join("cat")).unwrap();
        std::fs::write(fixture.0.join("token"), format!("{SECRET}\n")).unwrap();
        fixture
    }

    fn executable(&self, name: &str, text: &str) {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A parallel fork may briefly inherit the writer fd. Prove direct exec
        // is past Linux's ETXTBSY window before a wrapper test uses this path.
        for _ in 0..100 {
            let result = Command::new(&path)
                .env("TRACE", "/dev/null").env_remove("CODEX_REMOTE_TOKEN")
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
                .status();
            match result {
                Ok(_) => return,
                Err(error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("fixture executable did not settle: {error}"),
            }
        }
        panic!("fixture executable remained busy");
    }

    fn config(&self) -> CodexConfig {
        CodexConfig { token_file: self.0.join("token"), bin: self.0.join("codex").to_str().unwrap().into(), ..config() }
    }

    fn run(&self, cmd: &str, pane: Option<&str>, input: &str) -> std::process::Output {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", cmd]).current_dir(&self.0)
            .env("PATH", &self.0).env("SHELL", self.0.join("shell"))
            .env("TRACE", self.0.join("trace")).env_remove("CODEX_REMOTE_TOKEN")
            .env_remove("TMUX").env_remove("TMUX_PANE")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(pane) = pane { command.env("TMUX_PANE", pane); }
        let mut child = command.spawn().unwrap();
        // Invalid pane targets exit before reading; that can close the pipe
        // before this write. Exit/output/trace assertions still check the result.
        if let Err(error) = child.stdin.take().unwrap().write_all(input.as_bytes()) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("pane fixture exceeded watchdog");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        child.wait_with_output().unwrap()
    }

    fn trace(&self) -> String {
        std::fs::read_to_string(self.0.join("trace")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

#[test]
fn fixture_tolerates_a_child_closing_stdin_before_the_input_is_written() {
    let fixture = Fixture::new();
    let out = fixture.run("exec 0<&-; exit 2", None, &"s\n".repeat(65_536));
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert!(fixture.trace().is_empty());
}

#[test]
fn invalid_tmux_pane_exits_before_latching_or_reading_the_token() {
    let fixture = Fixture::new();
    let mut config = fixture.config();
    config.token_file = fixture.0.join("must-not-be-read");
    let cmd = codex_attach_cmd(ID, &config, Some("ccmux-probe")).unwrap();
    for pane in [None, Some(""), Some("%"), Some("12"), Some("%%12"), Some("%12x"), Some("%1\n"), Some("% 1"), Some("%1; touch sentinel")] {
        let out = fixture.run(&cmd, pane, "s\n");
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
        assert!(fixture.trace().is_empty());
    }
}

#[test]
fn parked_retries_clear_the_latch_and_keep_the_launch_id_and_exit_code() {
    let fixture = Fixture::new();
    let cmd = codex_attach_cmd(ID, &fixture.config(), Some("ccmux-probe")).unwrap();
    let out = fixture.run(&cmd, Some("%27"), "\nanything\nQ\n");
    assert_eq!(out.status.code(), Some(47));
    let trace = fixture.trace();
    let lines: Vec<_> = trace.lines().collect();
    assert_eq!(lines.len(), 9);
    for attempt in lines.chunks(3) {
        assert_eq!(attempt[0], "tmux <-L> <ccmux-probe> <set-option> <-p> <-u> <-t> <%27> <@ccmux_detached>");
        assert_eq!(attempt[1], format!("codex <--remote> <ws://127.0.0.1:8965> <--remote-auth-token-env> <CODEX_REMOTE_TOKEN> <resume> <{ID}>"));
        assert_eq!(attempt[2], "tmux <-L> <ccmux-probe> <set-option> <-p> <-t> <%27> <@ccmux_detached> <1>");
    }
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.matches("[ccmux] Codex attach exited (rc=47). resume: ").count(), 3);
    assert_eq!(text.matches("CODEX_REMOTE_TOKEN=\"$(cat ").count(), 3);
    assert!(!text.contains(SECRET) && !cmd.contains(SECRET) && !trace.contains(SECRET));
    assert!(out.stderr.is_empty());
}

#[test]
fn parked_eof_closes_and_only_s_explicitly_hands_off_to_a_shell() {
    for answer in ["", "q\n", "s\n", "S\n"] {
        let fixture = Fixture::new();
        let cmd = codex_attach_cmd(ID, &fixture.config(), Some("ccmux-probe")).unwrap();
        let out = fixture.run(&cmd, Some("%27"), answer);
        let shell = answer.eq_ignore_ascii_case("s\n");
        assert_eq!(out.status.code(), Some(if shell { 19 } else { 47 }));
        let trace = fixture.trace();
        assert_eq!(trace.contains("shell <-l>"), shell);
        assert_eq!(trace.contains("<@ccmux_detached> <shell>"), shell);
        if shell {
            assert!(trace.find("<@ccmux_detached> <shell>").unwrap() < trace.find("shell <-l>").unwrap());
        }
    }
}

#[test]
fn hostile_paths_are_single_words_and_displayed_resume_is_inert() {
    let fixture = Fixture::new();
    let mut config = fixture.config();
    let bin = "codex ' ; $(touch injected-bin)";
    let token = "token ' ; $(touch injected-token) `touch injected-backtick` %s";
    std::fs::rename(&config.bin, fixture.0.join(bin)).unwrap();
    std::fs::rename(&config.token_file, fixture.0.join(token)).unwrap();
    config.bin = fixture.0.join(bin).to_str().unwrap().into();
    config.token_file = fixture.0.join(token);
    config.url.push_str("/a';$(false)");
    let mut row = row();
    row.name = "name $(touch injected-name)".into();
    let cmd = attach_pane_cmd(&row, Some(&config)).unwrap();
    assert_eq!(cmd, attach_entry_cmd(&entry(&row), Some(&config)).unwrap());
    let out = fixture.run(&cmd, Some("%27"), "q\n");
    assert_eq!(out.status.code(), Some(47));
    let printed = String::from_utf8(out.stdout).unwrap();
    assert!(printed.contains(&format!("$(cat {})", sh_quote(config.token_file.to_str().unwrap()))));
    assert!(fixture.trace().contains(&format!("<{}>", config.url)));
    assert!(!printed.contains(SECRET) && !cmd.contains(SECRET) && !fixture.trace().contains(SECRET));
    assert!(!cmd.contains(&row.name));
    for name in ["injected-bin", "injected-token", "injected-backtick", "injected-name"] {
        assert!(!Path::new(&fixture.0).join(name).exists());
    }
}

#[test]
fn failed_attach_keeps_the_park_path_and_its_rc() {
    for missing_binary in [false, true] {
        let fixture = Fixture::new();
        let mut config = fixture.config();
        if missing_binary { config.bin = fixture.0.join("missing-bin").to_str().unwrap().into(); }
        else { config.token_file = fixture.0.join("missing-token"); }
        let cmd = attach_pane_cmd(&row(), Some(&config)).unwrap();
        let out = fixture.run(&cmd, Some("%27"), "q\n");
        let rc = if missing_binary { 127 } else { 65 };
        assert_eq!(out.status.code(), Some(rc));
        assert!(String::from_utf8(out.stdout).unwrap().contains(&format!("Codex attach exited (rc={rc})")));
        assert!(fixture.trace().contains("<@ccmux_detached> <1>"));
        assert!(!fixture.trace().contains("<shell>"));
    }
}
