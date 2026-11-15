//! Claude Code: регистрация через `claude mcp add`.
//!
//! Конфигурацию Claude Code (`~/.claude.json`, `.mcp.json`) напрямую не правим — только через
//! его CLI, чтобы не зависеть от внутреннего формата файлов и не задеть чужие записи.

use std::io;
use std::process::Command;

use anyhow::{Context, bail};

use super::{
    Existing, RegisterOptions, Registration, Scope, ServerSpec, confirm_foreign_replacement,
    shell_join,
};
use crate::ui::Ui;

/// Имя исполняемого файла Claude Code CLI.
pub const CLAUDE_PROGRAM: &str = "claude";

/// Результат внешней команды.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Запуск внешних команд; в тестах подменяется записью вызовов или поддельным `claude`.
pub trait CommandRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput>;
}

/// Запуск через `std::process::Command`.
#[derive(Debug, Default)]
pub struct SystemRunner;

/// Запасное имя программы, если `program` не нашлась.
///
/// На Windows `Command` ищет в PATH только `<имя>.exe`, а установка Claude Code через npm
/// кладёт обёртку `claude.cmd`. Rust экранирует аргументы для `.cmd` сам и отказывается
/// запускать, если безопасно экранировать нельзя. В других ОС и при других ошибках запасного
/// имени нет.
fn windows_cmd_fallback(program: &str, windows: bool, err: &io::Error) -> Option<String> {
    (windows && err.kind() == io::ErrorKind::NotFound && !program.contains('.'))
        .then(|| format!("{program}.cmd"))
}

impl CommandRunner for SystemRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
        let output = match Command::new(program).args(args).output() {
            Err(err) => match windows_cmd_fallback(program, cfg!(windows), &err) {
                Some(fallback) => Command::new(fallback).args(args).output()?,
                None => return Err(err),
            },
            Ok(output) => output,
        };
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Аргументы `claude mcp add --scope <scope> <name> -- <exe> serve --profile <p> ...`.
pub fn add_args(scope: Scope, spec: &ServerSpec) -> Vec<String> {
    let mut args = strings(&[
        "mcp",
        "add",
        "--scope",
        scope.as_str(),
        spec.name.as_str(),
        "--",
    ]);
    args.push(spec.command.clone());
    args.extend(spec.args.iter().cloned());
    args
}

/// Аргументы `claude mcp remove --scope <scope> <name>`.
pub fn remove_args(scope: Scope, name: &str) -> Vec<String> {
    strings(&["mcp", "remove", "--scope", scope.as_str(), name])
}

/// Аргументы `claude mcp get <name>`.
pub fn get_args(name: &str) -> Vec<String> {
    strings(&["mcp", "get", name])
}

fn display(args: &[String]) -> String {
    shell_join(std::iter::once(CLAUDE_PROGRAM).chain(args.iter().map(String::as_str)))
}

/// Проверяет, что Claude Code CLI доступен, до первого вопроса пользователю.
pub fn ensure_available(runner: &mut dyn CommandRunner) -> anyhow::Result<()> {
    match runner.run(CLAUDE_PROGRAM, &["--version".to_owned()]) {
        Ok(output) if output.success => Ok(()),
        Ok(output) => bail!(
            "`{CLAUDE_PROGRAM} --version` failed: {}",
            output.stderr.trim()
        ),
        Err(err) if err.kind() == io::ErrorKind::NotFound => bail!(
            "Claude Code CLI `{CLAUDE_PROGRAM}` was not found in PATH; install Claude Code first"
        ),
        Err(err) => Err(err).context("failed to run Claude Code CLI"),
    }
}

/// Сообщение Claude Code о том, что сервер с таким именем уже есть.
fn already_exists(output: &CommandOutput) -> bool {
    let text = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    text.contains("already exists")
}

/// Запись сервера по выводу `claude mcp get` (строки `Scope:`, `Command:`, `Args:`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// Область записи; `None`, если строку не разобрать.
    scope: Option<Scope>,
    command: Option<String>,
    /// Аргументы по пробелам: только для поиска `--profile`, у имени профиля пробелов нет.
    args: Vec<String>,
    /// Строка `Args:` как есть. Claude Code склеивает аргументы через пробел без кавычек,
    /// поэтому аргумент с пробелом (`--allow-path` в `C:\Users\Jane Doe\...`) по пробелам
    /// обратно не делится: команду сравниваем со склеенными так же аргументами.
    args_line: String,
}

impl Entry {
    /// Разбирает вывод `claude mcp get`; `None` — записи нет (команда не удалась).
    fn parse(output: &CommandOutput) -> Option<Self> {
        if !output.success {
            return None;
        }
        let field = |name: &str| {
            output
                .stdout
                .lines()
                .find_map(|line| line.trim().strip_prefix(name).map(str::trim))
        };
        // `Scope: User config (available in all your projects)`, `Local config (...)`,
        // `Project config (...)`.
        let scope = field("Scope:").and_then(|value| {
            let first = value.split_whitespace().next()?.to_ascii_lowercase();
            match first.as_str() {
                "user" => Some(Scope::User),
                "project" => Some(Scope::Project),
                "local" => Some(Scope::Local),
                _ => None,
            }
        });
        let args_line = field("Args:").unwrap_or_default();
        Some(Self {
            scope,
            command: field("Command:").map(str::to_owned),
            args: args_line.split_whitespace().map(str::to_owned).collect(),
            args_line: args_line.to_owned(),
        })
    }

    fn existing(&self, profile: &str) -> Existing {
        Existing::classify(
            self.command.as_deref(),
            self.args.iter().map(String::as_str),
            profile,
        )
    }

    /// Запись в той же области запускает ровно ту же команду. Если вывод разобрать не
    /// удалось, запись считается другой: её заменят по обычным правилам.
    fn is_same(&self, scope: Scope, spec: &ServerSpec) -> bool {
        self.scope == Some(scope)
            && self.command.as_deref() == Some(spec.command.as_str())
            && self.args_line == spec.args.join(" ")
    }
}

fn run_checked(runner: &mut dyn CommandRunner, args: &[String]) -> anyhow::Result<CommandOutput> {
    runner
        .run(CLAUDE_PROGRAM, args)
        .with_context(|| format!("failed to run `{}`", display(args)))
}

fn run_ok(runner: &mut dyn CommandRunner, args: &[String]) -> anyhow::Result<()> {
    let output = run_checked(runner, args)?;
    if !output.success {
        bail!("`{}` failed: {}", display(args), output.stderr.trim());
    }
    Ok(())
}

/// Регистрирует сервер в Claude Code.
///
/// Своя запись с тем же именем и профилем заменяется после подтверждения (или сразу с
/// `yes`); если она уже запускает ту же команду, ничего не меняется. Чужая запись или
/// запись другого профиля заменяется только после явного ответа в терминале или с `force`:
/// `yes` её не перезаписывает. С `dry_run` выполняется только `claude mcp get`.
pub fn register(
    runner: &mut dyn CommandRunner,
    ui: &mut dyn Ui,
    scope: Scope,
    spec: &ServerSpec,
    options: RegisterOptions,
) -> anyhow::Result<Registration> {
    let name = spec.name.as_str();
    let add = add_args(scope, spec);
    ui.say(&format!(
        "Claude Code ({} scope) will get MCP server `{name}`:\n  {}",
        scope.as_str(),
        display(&add)
    ));

    if options.dry_run {
        return dry_run(runner, ui, scope, spec, options);
    }
    if !options.yes && !ui.confirm("Register the server in Claude Code?", false)? {
        return Ok(Registration::Declined);
    }

    let output = run_checked(runner, &add)?;
    if output.success {
        return Ok(Registration::Changed);
    }
    if !already_exists(&output) {
        bail!("`{}` failed: {}", display(&add), output.stderr.trim());
    }

    let entry = Entry::parse(&run_checked(runner, &get_args(name))?);
    if entry
        .as_ref()
        .is_some_and(|entry| entry.is_same(scope, spec))
    {
        ui.say(&format!(
            "Claude Code already has MCP server `{name}` with this command; nothing to change."
        ));
        return Ok(Registration::UpToDate);
    }
    let existing = entry.map_or(Existing::Foreign, |entry| entry.existing(&spec.profile));
    let remove = remove_args(scope, name);
    ui.say(&format!(
        "Claude Code already has MCP server `{name}` ({}). It will be replaced:\n  {}\n  {}",
        existing.describe(),
        display(&remove),
        display(&add)
    ));
    let replace = if existing == Existing::SameProfile {
        options.yes || ui.confirm("Replace the existing entry?", false)?
    } else {
        confirm_foreign_replacement(ui, "Claude Code", name, &existing, options)?
    };
    if !replace {
        return Ok(Registration::Declined);
    }
    run_ok(runner, &remove)?;
    run_ok(runner, &add)?;
    Ok(Registration::Changed)
}

/// `--dry-run`: по `claude mcp get` показывает, добавится запись или заменит прежнюю.
fn dry_run(
    runner: &mut dyn CommandRunner,
    ui: &mut dyn Ui,
    scope: Scope,
    spec: &ServerSpec,
    options: RegisterOptions,
) -> anyhow::Result<Registration> {
    let name = spec.name.as_str();
    let Some(entry) = Entry::parse(&run_checked(runner, &get_args(name))?) else {
        ui.say(&format!(
            "Claude Code has no MCP server `{name}` yet: it would be added."
        ));
        return Ok(Registration::DryRun);
    };
    if entry.is_same(scope, spec) {
        ui.say(&format!(
            "Claude Code already has MCP server `{name}` with this command; nothing would change."
        ));
        return Ok(Registration::UpToDate);
    }
    let existing = entry.existing(&spec.profile);
    let condition = match existing {
        Existing::SameProfile => "after confirmation",
        _ if options.force => "because of --force",
        _ => "only after an explicit answer in the terminal or with --force",
    };
    ui.say(&format!(
        "Claude Code already has MCP server `{name}` ({}): it would be replaced {condition}.",
        existing.describe()
    ));
    Ok(Registration::DryRun)
}

#[cfg(test)]
pub mod testing {
    use std::collections::VecDeque;

    use super::*;

    /// Записывает вызовы и возвращает заранее заданные результаты; `claude` не запускается.
    #[derive(Debug, Default)]
    pub struct FakeRunner {
        pub calls: Vec<Vec<String>>,
        pub results: VecDeque<CommandOutput>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
            assert_eq!(program, CLAUDE_PROGRAM);
            self.calls.push(args.to_vec());
            Ok(self.results.pop_front().unwrap_or(CommandOutput {
                success: true,
                ..Default::default()
            }))
        }
    }

    /// Ответ `claude mcp add`, когда сервер с таким именем уже есть.
    pub fn exists() -> CommandOutput {
        CommandOutput {
            success: false,
            stderr: "MCP server appgallery already exists in user config".into(),
            ..Default::default()
        }
    }

    /// Ответ `claude mcp get` для записи пользовательской области с командой `command` и
    /// аргументами `args`.
    pub fn get_output(command: &str, args: &[&str]) -> CommandOutput {
        CommandOutput {
            success: true,
            stdout: format!(
                "appgallery:\n  Scope: User config (available in all your projects)\n  \
                 Status: ✓ Connected\n  Type: stdio\n  Command: {command}\n  \
                 Args: {}\n  Environment:\n",
                args.join(" ")
            ),
            ..Default::default()
        }
    }

    /// Ответ `claude mcp get`, когда записи нет.
    pub fn not_found() -> CommandOutput {
        CommandOutput {
            success: false,
            stderr: "No MCP server found with name: appgallery".into(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeRunner, exists, get_output, not_found};
    use super::*;
    use crate::clients::TEST_EXE;
    use crate::ui::testing::ScriptedUi;

    const OWN: &str = "/opt/homebrew/bin/appgallery-connect-mcp";

    fn spec() -> ServerSpec {
        ServerSpec {
            name: "appgallery".into(),
            profile: "default".into(),
            command: TEST_EXE.into(),
            args: vec!["serve".into(), "--profile".into(), "default".into()],
        }
    }

    fn yes() -> RegisterOptions {
        RegisterOptions {
            yes: true,
            ..Default::default()
        }
    }

    fn runner(results: Vec<CommandOutput>) -> FakeRunner {
        FakeRunner {
            results: results.into(),
            ..Default::default()
        }
    }

    #[test]
    fn windows_falls_back_to_cmd_wrapper() {
        let not_found = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(
            windows_cmd_fallback(CLAUDE_PROGRAM, true, &not_found).as_deref(),
            Some("claude.cmd")
        );
        assert_eq!(
            windows_cmd_fallback(CLAUDE_PROGRAM, false, &not_found),
            None
        );
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(windows_cmd_fallback(CLAUDE_PROGRAM, true, &denied), None);
        assert_eq!(windows_cmd_fallback("claude.cmd", true, &not_found), None);
    }

    #[test]
    fn builds_argv() {
        assert_eq!(
            add_args(Scope::Local, &spec()),
            [
                "mcp",
                "add",
                "--scope",
                "local",
                "appgallery",
                "--",
                TEST_EXE,
                "serve",
                "--profile",
                "default"
            ]
        );
        assert_eq!(
            remove_args(Scope::Project, "agc-x"),
            ["mcp", "remove", "--scope", "project", "agc-x"]
        );
        assert_eq!(get_args("agc-x"), ["mcp", "get", "agc-x"]);
    }

    #[test]
    fn parses_existing_entries() {
        let entry = |output: CommandOutput| Entry::parse(&output).unwrap();
        let own = entry(get_output(OWN, &["serve", "--profile", "default"]));
        assert_eq!(own.scope, Some(Scope::User));
        assert_eq!(own.existing("default"), Existing::SameProfile);
        assert_eq!(
            entry(get_output(OWN, &["serve", "--profile", "work"])).existing("default"),
            Existing::OtherProfile("work".into())
        );
        assert_eq!(
            entry(get_output("npx", &["serve"])).existing("default"),
            Existing::Foreign
        );
        assert_eq!(Entry::parse(&not_found()), None);

        let same = entry(get_output(TEST_EXE, &["serve", "--profile", "default"]));
        assert!(same.is_same(Scope::User, &spec()));
        // Та же команда в другой области — другая запись.
        assert!(!same.is_same(Scope::Local, &spec()));
        let local = Entry {
            scope: Some(Scope::Local),
            ..same.clone()
        };
        assert!(local.is_same(Scope::Local, &spec()));
        let unknown_scope = Entry {
            scope: None,
            ..same
        };
        assert!(!unknown_scope.is_same(Scope::User, &spec()));
    }

    #[test]
    fn same_command_with_spaces_in_paths() {
        let dir = r"C:\Users\Jane Doe\My Projects\app";
        let mut spec = spec();
        spec.args.extend(["--allow-path".into(), dir.into()]);
        let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
        let entry = Entry::parse(&get_output(TEST_EXE, &args)).unwrap();
        assert!(entry.is_same(Scope::User, &spec));
        assert_eq!(entry.existing("default"), Existing::SameProfile);

        // Другой каталог — другая команда, даже если части совпадают.
        let mut other = spec.clone();
        other.args.pop();
        other.args.push(r"C:\Users\Jane".into());
        assert!(!entry.is_same(Scope::User, &other));
        let mut split = spec.clone();
        split.args.pop();
        split
            .args
            .extend([r"C:\Users\Jane".into(), r"Doe\My Projects\app".into()]);
        // Склеенные строки совпадают: Claude Code не различает такие команды и сам.
        assert!(entry.is_same(Scope::User, &split));
    }

    #[test]
    fn register_with_confirmation() {
        let mut runner = FakeRunner::default();
        let mut ui = ScriptedUi::answering(&["y"], &[]);
        let outcome = register(
            &mut runner,
            &mut ui,
            Scope::User,
            &spec(),
            Default::default(),
        )
        .unwrap();
        assert_eq!(outcome, Registration::Changed);
        assert_eq!(runner.calls, [add_args(Scope::User, &spec())]);
        assert!(
            ui.text()
                .contains("claude mcp add --scope user appgallery --")
        );
    }

    #[test]
    fn declined_or_unanswered_registration_runs_nothing() {
        for answer in ["n", ""] {
            let mut ui = ScriptedUi::answering(&[answer], &[]);
            let mut runner = FakeRunner::default();
            let outcome = register(
                &mut runner,
                &mut ui,
                Scope::User,
                &spec(),
                Default::default(),
            )
            .unwrap();
            assert_eq!(outcome, Registration::Declined);
            assert!(runner.calls.is_empty());
        }
        // Нет ответа (EOF) — ошибка ввода, конфигурация не меняется.
        let mut runner = FakeRunner::default();
        let mut ui = ScriptedUi::default();
        assert!(
            register(
                &mut runner,
                &mut ui,
                Scope::User,
                &spec(),
                Default::default()
            )
            .is_err()
        );
        assert!(runner.calls.is_empty());
    }

    #[test]
    fn same_command_is_up_to_date() {
        let mut runner = runner(vec![
            exists(),
            get_output(TEST_EXE, &["serve", "--profile", "default"]),
        ]);
        let mut ui = ScriptedUi::default();
        let outcome = register(&mut runner, &mut ui, Scope::User, &spec(), yes()).unwrap();
        assert_eq!(outcome, Registration::UpToDate);
        assert_eq!(
            runner.calls,
            [add_args(Scope::User, &spec()), get_args("appgallery")]
        );
    }

    #[test]
    fn replaces_own_entry_of_the_same_profile() {
        let mut runner = runner(vec![
            exists(),
            get_output(OWN, &["serve", "--profile", "default"]),
        ]);
        let mut ui = ScriptedUi::default();
        let outcome = register(&mut runner, &mut ui, Scope::User, &spec(), yes()).unwrap();
        assert_eq!(outcome, Registration::Changed);
        assert_eq!(
            runner.calls,
            [
                add_args(Scope::User, &spec()),
                get_args("appgallery"),
                remove_args(Scope::User, "appgallery"),
                add_args(Scope::User, &spec())
            ]
        );
        assert!(ui.questions.is_empty());
    }

    #[test]
    fn foreign_or_other_profile_entry_needs_an_answer_or_force() {
        for existing in [
            get_output("node", &["server.js"]),
            get_output(OWN, &["serve", "--profile", "work"]),
        ] {
            // `--yes` такую запись не перезаписывает.
            let mut runner = runner(vec![exists(), existing.clone()]);
            let mut ui = ScriptedUi::default();
            let err = register(&mut runner, &mut ui, Scope::User, &spec(), yes()).unwrap_err();
            assert!(err.to_string().contains("--force"), "{err}");
            assert_eq!(
                runner.calls,
                [add_args(Scope::User, &spec()), get_args("appgallery")]
            );

            // Интерактивно — только после явного «да»; отказ ничего не удаляет.
            let mut runner = self::runner(vec![exists(), existing.clone()]);
            let mut ui = ScriptedUi::answering(&["y", "n"], &[]);
            let outcome = register(
                &mut runner,
                &mut ui,
                Scope::User,
                &spec(),
                Default::default(),
            )
            .unwrap();
            assert_eq!(outcome, Registration::Declined);
            assert_eq!(runner.calls.len(), 2);

            let mut runner = self::runner(vec![exists(), existing.clone()]);
            let mut ui = ScriptedUi::answering(&["y", "y"], &[]);
            let outcome = register(
                &mut runner,
                &mut ui,
                Scope::User,
                &spec(),
                Default::default(),
            )
            .unwrap();
            assert_eq!(outcome, Registration::Changed);
            assert_eq!(runner.calls.len(), 4);

            // `--yes --force` — без вопросов.
            let mut runner = self::runner(vec![exists(), existing]);
            let mut ui = ScriptedUi::default();
            let options = RegisterOptions {
                force: true,
                ..yes()
            };
            let outcome = register(&mut runner, &mut ui, Scope::User, &spec(), options).unwrap();
            assert_eq!(outcome, Registration::Changed);
            assert_eq!(runner.calls.len(), 4);
            assert!(ui.text().contains("because of --force"), "{}", ui.text());
        }
    }

    #[test]
    fn other_profile_is_named() {
        let mut runner = runner(vec![
            exists(),
            get_output(OWN, &["serve", "--profile", "work"]),
        ]);
        let mut ui = ScriptedUi::answering(&["y", "n"], &[]);
        register(
            &mut runner,
            &mut ui,
            Scope::User,
            &spec(),
            Default::default(),
        )
        .unwrap();
        assert!(
            ui.text().contains("another profile, `work`"),
            "{}",
            ui.text()
        );
    }

    #[test]
    fn dry_run_only_reads() {
        let options = RegisterOptions {
            dry_run: true,
            ..Default::default()
        };
        let cases = [
            (not_found(), Registration::DryRun, "would be added"),
            (
                get_output(TEST_EXE, &["serve", "--profile", "default"]),
                Registration::UpToDate,
                "nothing would change",
            ),
            (
                get_output(OWN, &["serve", "--profile", "default"]),
                Registration::DryRun,
                "replaced after confirmation",
            ),
            (
                get_output("npx", &["other"]),
                Registration::DryRun,
                "only after an explicit answer",
            ),
        ];
        for (output, expected, text) in cases {
            let mut runner = runner(vec![output]);
            let mut ui = ScriptedUi::default();
            let outcome = register(&mut runner, &mut ui, Scope::User, &spec(), options).unwrap();
            assert_eq!(outcome, expected);
            assert_eq!(runner.calls, [get_args("appgallery")]);
            assert!(ui.questions.is_empty());
            assert!(ui.text().contains(text), "{text}: {}", ui.text());
        }
    }

    #[test]
    fn other_failures_are_errors() {
        let mut runner = runner(vec![CommandOutput {
            success: false,
            stderr: "boom".into(),
            ..Default::default()
        }]);
        let mut ui = ScriptedUi::default();
        let err = register(&mut runner, &mut ui, Scope::User, &spec(), yes()).unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert_eq!(runner.calls.len(), 1);
    }

    /// Поддельный `claude`: shell-скрипт во временном каталоге хранит одну запись в файле и
    /// понимает `--version`, `mcp add`, `mcp get`, `mcp remove`. Запускается через `sh`,
    /// чтобы не зависеть от прав на исполнение и PATH.
    #[cfg(unix)]
    mod fake_cli {
        use std::fs;
        use std::path::{Path, PathBuf};

        use super::*;

        const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
case "$1 $2" in
  "--version ") echo "2.0.0 (Claude Code)" ;;
  "mcp add")
    if [ -f "$dir/entry" ]; then
      echo "MCP server $5 already exists in $4 config" >&2
      exit 1
    fi
    shift 6
    echo "$*" > "$dir/entry" ;;
  "mcp get")
    if [ ! -f "$dir/entry" ]; then
      echo "No MCP server found with name: $3" >&2
      exit 1
    fi
    read -r cmd args < "$dir/entry"
    printf '%s:\n  Scope: User config (available in all your projects)\n  Type: stdio\n  Command: %s\n  Args: %s\n' "$3" "$cmd" "$args" ;;
  "mcp remove") rm -f "$dir/entry" ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;

        /// Запускает скрипт `claude` из каталога вместо настоящего CLI.
        struct ScriptRunner {
            script: PathBuf,
        }

        impl CommandRunner for ScriptRunner {
            fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
                assert_eq!(program, CLAUDE_PROGRAM);
                let mut argv = vec![self.script.display().to_string()];
                argv.extend(args.iter().cloned());
                SystemRunner.run("sh", &argv)
            }
        }

        fn read_calls(dir: &Path) -> Vec<String> {
            fs::read_to_string(dir.join("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        #[test]
        fn registers_idempotently_with_a_fake_claude() {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join(CLAUDE_PROGRAM);
            fs::write(&script, SCRIPT).unwrap();
            let mut runner = ScriptRunner { script };
            let mut ui = ScriptedUi::default();

            ensure_available(&mut runner).unwrap();
            let outcome = register(&mut runner, &mut ui, Scope::User, &spec(), yes()).unwrap();
            assert_eq!(outcome, Registration::Changed);
            let entry = fs::read_to_string(dir.path().join("entry")).unwrap();
            assert_eq!(entry.trim(), format!("{TEST_EXE} serve --profile default"));

            // Повторный запуск ничего не меняет: ни remove, ни второго add.
            let outcome = register(&mut runner, &mut ui, Scope::User, &spec(), yes()).unwrap();
            assert_eq!(outcome, Registration::UpToDate);
            let calls = read_calls(dir.path());
            assert_eq!(calls.len(), 4, "{calls:?}");
            assert!(
                calls.iter().all(|call| !call.contains("remove")),
                "{calls:?}"
            );

            // Чужая запись: с --yes — отказ, с --force — замена.
            fs::write(dir.path().join("entry"), "npx some-other-server\n").unwrap();
            let err = register(&mut runner, &mut ui, Scope::User, &spec(), yes()).unwrap_err();
            assert!(err.to_string().contains("another server"), "{err}");
            let entry = fs::read_to_string(dir.path().join("entry")).unwrap();
            assert_eq!(entry.trim(), "npx some-other-server");

            let options = RegisterOptions {
                force: true,
                ..yes()
            };
            let outcome = register(&mut runner, &mut ui, Scope::User, &spec(), options).unwrap();
            assert_eq!(outcome, Registration::Changed);
            let entry = fs::read_to_string(dir.path().join("entry")).unwrap();
            assert_eq!(entry.trim(), format!("{TEST_EXE} serve --profile default"));
            assert!(
                read_calls(dir.path())
                    .iter()
                    .any(|call| call.contains("mcp remove"))
            );
        }
    }
}
