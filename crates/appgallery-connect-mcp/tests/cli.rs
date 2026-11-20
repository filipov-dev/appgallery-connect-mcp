//! Настоящий бинарник: ошибки `auth login`, `connect`, `serve` и `key exec`, которые
//! случаются до обращения к системному хранилищу и к сети. Каталог настроек — временный;
//! системный keychain не трогается: каждый сценарий завершается ошибкой раньше, чем команда
//! доходит до хранилища.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn secret() -> String {
    ["C0FF", "EE12", "34AB", "CDEF"].concat().repeat(3)
}

fn run(config: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_appgallery-connect-mcp"))
        .args(args)
        .env("APPGALLERY_CONNECT_MCP_CONFIG_DIR", config)
        .env_remove("APPGALLERY_CONNECT_MCP_PROFILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut pipe = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            pipe.write_all(text.as_bytes()).unwrap();
        }
    }
    child.wait_with_output().unwrap()
}

/// Команда завершилась ошибкой, в stderr есть `expected`, секрета нет нигде, настройки не
/// записаны.
fn assert_rejected(config: &Path, output: &Output, expected: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains(expected), "{expected}: {stderr}");
    assert!(!stdout.contains(&secret()), "{stdout}");
    assert!(!stderr.contains(&secret()), "{stderr}");
    assert!(!config.join("profiles.toml").exists());
}

#[test]
fn secret_is_never_accepted_as_an_argument() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path();
    let secret = secret();
    let with_value = format!("--client-secret={secret}");
    for (args, expected) in [
        (
            vec!["auth", "login", "--client-id", "123", "--secret", &secret],
            "not accepted as a command-line argument",
        ),
        (
            vec!["auth", "login", "--client-id", "123", &with_value],
            "not accepted as a command-line argument",
        ),
        (
            vec!["auth", "login", "--client-id", "123", "--key", &secret],
            "not accepted as a command-line argument",
        ),
        (
            vec!["auth", "login", "--client-id", "123", &secret],
            "takes no positional arguments",
        ),
        (
            vec![
                "connect",
                "codex",
                "--client-id",
                "123",
                "--secret",
                &secret,
            ],
            "not accepted as a command-line argument",
        ),
        (
            vec!["connect", "claude-code", &with_value],
            "not accepted as a command-line argument",
        ),
        (
            vec!["connect", "codex", "--client-id", "123", &secret],
            "takes no positional arguments after the client name",
        ),
    ] {
        let output = run(config, &args, None);
        assert_rejected(config, &output, expected);
    }
}

#[test]
fn bad_input_is_rejected_without_echoing_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path();

    let output = run(config, &["auth", "login", "--region", "us"], None);
    assert_rejected(config, &output, "unknown region `us`");

    // Секрет из stdin не той формы (пробел внутри): ошибка до хранилища и сети.
    let output = run(
        config,
        &["auth", "login", "--client-id", "123", "--secret-stdin"],
        Some(&format!("{} {}\n", secret(), secret())),
    );
    assert_rejected(config, &output, "not allowed");

    // Без терминала секрет не спрашивается.
    let output = run(config, &["auth", "login", "--client-id", "123"], None);
    assert_rejected(config, &output, "--secret-file <PATH> or --secret-stdin");

    let output = run(config, &["auth", "logout", "--profile", "a b"], None);
    assert_rejected(config, &output, "invalid profile name");

    // Слишком длинное имя профиля для имени сервера — ошибка до любых действий.
    let output = run(
        config,
        &["connect", "codex", "--profile", "production-eu1"],
        None,
    );
    assert_rejected(config, &output, "profile name `production-eu1` is too long");
}

#[test]
fn help_lists_no_secret_argument() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), &["auth", "login", "--help"], None);
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--secret-file"), "{help}");
    assert!(help.contains("--secret-stdin"), "{help}");
    assert!(help.contains("--region"), "{help}");
    assert!(!help.contains("--secret "), "{help}");
    assert!(!help.contains("--client-secret"), "{help}");

    let output = run(dir.path(), &["connect", "--help"], None);
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--secret-file",
        "--secret-stdin",
        "--dry-run",
        "--allow-write",
    ] {
        assert!(help.contains(flag), "{flag}: {help}");
    }
    assert!(!help.contains("--secret "), "{help}");
    assert!(!help.contains("--client-secret"), "{help}");
}

/// Ошибка запуска `serve` — в stderr, ненулевой код, stdout (канал протокола MCP) пуст,
/// без паники.
#[test]
fn serve_reports_startup_errors_in_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path();
    for (args, expected) in [
        (
            vec!["serve", "--toolsets", "nope"],
            "unknown toolset `nope`",
        ),
        (vec!["serve", "--profile", "bad name"], "profile"),
        (
            vec!["serve", "--timeout", "0"],
            "--timeout must be greater than 0",
        ),
        (vec!["serve", "--log-level", "=["], "invalid --log-level"),
    ] {
        let output = run(config, &args, None);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(stdout.is_empty(), "{args:?}: {stdout}");
        assert!(stderr.starts_with("error: "), "{args:?}: {stderr}");
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
        assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
    }
}

/// Ошибки `key exec` до обращения к хранилищу: ненулевой код, команда не запускается.
#[test]
fn key_exec_rejects_bad_input_before_the_keychain() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path();
    let marker = dir.path().join("ran");
    let marker = marker.to_string_lossy();
    for (args, expected) in [
        (
            vec!["key", "exec", "--env", "1BAD", "--", "touch", &marker],
            "invalid environment variable name",
        ),
        (
            vec![
                "key",
                "exec",
                "--env",
                "HUAWEI_CLIENT_SECRET",
                "--",
                "touch",
                &marker,
            ],
            "holds a credential",
        ),
        (
            vec![
                "key",
                "exec",
                "--profile",
                "bad name",
                "--",
                "touch",
                &marker,
            ],
            "profile",
        ),
        (vec!["key", "exec"], "COMMAND"),
    ] {
        let output = run(config, &args, None);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}: {stderr}");
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
        assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
        assert!(!dir.path().join("ran").exists(), "{args:?}");
    }
    let output = run(config, &["key", "exec", "--help"], None);
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    for expected in [
        "APPGALLERY_CONNECT_CREDENTIALS_FILE",
        "--format",
        "dotenv",
        "0600",
    ] {
        assert!(help.contains(expected), "{expected}: {help}");
    }
}
