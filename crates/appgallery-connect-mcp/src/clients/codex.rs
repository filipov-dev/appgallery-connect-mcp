//! Codex: запись `[mcp_servers.<имя>]` в `$CODEX_HOME/config.toml`.
//!
//! Файл правится через `toml_edit`, чтобы сохранить комментарии, форматирование и чужие записи.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::{
    Existing, RegisterOptions, Registration, Scope, ServerSpec, confirm_foreign_replacement,
};
use crate::ui::Ui;

/// Переменная окружения Codex с каталогом его конфигурации.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Каталог конфигурации Codex: `$CODEX_HOME`, иначе `~/.codex`.
pub fn codex_home(env_value: Option<OsString>, home: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = env_value.filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    home.map(|home| home.join(".codex"))
        .context("cannot determine the home directory; set CODEX_HOME")
}

/// Каталог конфигурации Codex текущего пользователя.
pub fn default_codex_home() -> anyhow::Result<PathBuf> {
    codex_home(std::env::var_os(CODEX_HOME_ENV), dirs::home_dir())
}

/// Путь к `config.toml` в каталоге Codex.
pub fn config_path(codex_home: &Path) -> PathBuf {
    codex_home.join("config.toml")
}

/// Прежняя запись с тем же именем.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousEntry {
    /// Имена ключей прежней записи. Значения не возвращаются и не показываются: в чужой
    /// записи в `env` мог лежать секрет.
    pub keys: Vec<String>,
    pub existing: Existing,
}

/// Записывает `command` и `args` сервера в документ. Остальные записи не меняются; если
/// `[mcp_servers.<имя>]` уже есть, таблица очищается и заполняется заново на том же месте
/// (с теми же комментариями над заголовком).
pub fn apply_entry(
    doc: &mut DocumentMut,
    spec: &ServerSpec,
) -> anyhow::Result<Option<PreviousEntry>> {
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut table = Table::new();
            // Без отдельного пустого заголовка `[mcp_servers]`.
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .context("`mcp_servers` in the Codex config is not a table; edit it manually")?;

    let mut args = Array::new();
    for arg in &spec.args {
        args.push(arg.as_str());
    }

    let previous = match servers.get_mut(&spec.name) {
        Some(Item::Table(table)) => {
            let old_args: Vec<String> = table
                .get("args")
                .and_then(Item::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|arg| arg.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let previous = PreviousEntry {
                keys: table.iter().map(|(key, _)| key.to_owned()).collect(),
                existing: Existing::classify(
                    table.get("command").and_then(Item::as_str),
                    old_args.iter().map(String::as_str),
                    &spec.profile,
                ),
            };
            table.clear();
            table.insert("command", value(spec.command.as_str()));
            table.insert("args", value(args));
            Some(previous)
        }
        Some(other) => {
            let previous = PreviousEntry {
                keys: vec![format!("<{}>", other.type_name())],
                existing: Existing::Foreign,
            };
            *other = Item::Table(new_entry(spec, args));
            Some(previous)
        }
        None => {
            servers.insert(&spec.name, Item::Table(new_entry(spec, args)));
            None
        }
    };
    Ok(previous)
}

fn new_entry(spec: &ServerSpec, args: Array) -> Table {
    let mut table = Table::new();
    table.insert("command", value(spec.command.as_str()));
    table.insert("args", value(args));
    table
}

/// Запись сервера в виде TOML для показа пользователю.
pub fn render_entry(spec: &ServerSpec) -> anyhow::Result<String> {
    let mut doc = DocumentMut::new();
    apply_entry(&mut doc, spec)?;
    Ok(doc.to_string())
}

/// Регистрирует сервер в Codex.
///
/// Если в файле уже ровно эта запись, ничего не спрашивается и не пишется. Чужая запись с
/// тем же именем или запись другого профиля заменяется только после явного ответа в
/// терминале или с `force`: `yes` её не перезаписывает. С `dry_run` файл не меняется.
pub fn register(
    ui: &mut dyn Ui,
    codex_home: &Path,
    spec: &ServerSpec,
    options: RegisterOptions,
) -> anyhow::Result<Registration> {
    let name = spec.name.as_str();
    let path = config_path(codex_home);
    let original = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let mut doc: DocumentMut = original
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let previous = apply_entry(&mut doc, spec)?;
    let updated = doc.to_string();
    if updated == original {
        ui.say(&format!(
            "Codex config {} already has MCP server `{name}` with this command; nothing to \
             change.",
            path.display()
        ));
        return Ok(Registration::UpToDate);
    }

    let mut message = format!(
        "Codex config {} will get:\n\n{}",
        path.display(),
        render_entry(spec)?.trim_end()
    );
    if let Some(previous) = &previous {
        message.push_str(&format!(
            "\n\nIt replaces the existing `{name}` entry ({}; keys: {}).",
            previous.existing.describe(),
            previous.keys.join(", ")
        ));
    }
    ui.say(&message);
    let replaces_other = previous
        .as_ref()
        .filter(|previous| previous.existing != Existing::SameProfile);

    if options.dry_run {
        if let Some(previous) = replaces_other
            && !options.force
        {
            ui.say(&format!(
                "The existing entry is {}: it would be replaced only after an explicit answer \
                 in the terminal or with --force.",
                previous.existing.describe()
            ));
        }
        return Ok(Registration::DryRun);
    }

    let write = match replaces_other {
        Some(previous) => {
            confirm_foreign_replacement(ui, "Codex config", name, &previous.existing, options)?
                && (options.yes
                    || !options.force
                    || ui.confirm("Write the Codex config?", false)?)
        }
        None => options.yes || ui.confirm("Write the Codex config?", false)?,
    };
    if !write {
        return Ok(Registration::Declined);
    }

    if !codex_home.exists() {
        fs::create_dir_all(codex_home)
            .with_context(|| format!("failed to create {}", codex_home.display()))?;
    }
    // Запись по исходному пути (а не через rename), чтобы не сломать symlink на dotfiles.
    fs::write(&path, updated).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(Registration::Changed)
}

/// Проверка, которую делаем до первого вопроса: Codex поддерживает только пользовательскую
/// область.
pub fn ensure_scope(scope: Scope) -> anyhow::Result<()> {
    if scope != Scope::User {
        bail!("Codex supports only `--scope user` (config in $CODEX_HOME/config.toml)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testing::ScriptedUi;

    fn spec() -> ServerSpec {
        ServerSpec {
            name: "appgallery".into(),
            profile: "default".into(),
            command: "/usr/local/bin/appgallery-connect-mcp".into(),
            args: vec!["serve".into(), "--profile".into(), "default".into()],
        }
    }

    fn yes() -> RegisterOptions {
        RegisterOptions {
            yes: true,
            ..Default::default()
        }
    }

    fn read(dir: &Path) -> String {
        fs::read_to_string(config_path(dir)).unwrap()
    }

    /// Конфиг с другим сервером, прежней своей записью и записью другого профиля.
    const EXISTING: &str = r#"# Codex config
model = "gpt-5"  # inline comment

[mcp_servers.other_agc]
command = "npx"
args = ["-y", "some-other-appgallery-server"]
env = { AGC_CLIENT_SECRET = "secret-value" }

# our server
[mcp_servers.appgallery]
command = "/opt/homebrew/bin/appgallery-connect-mcp"
args = ["serve", "--profile", "default"]
env = { SOME = "old-value" }

[mcp_servers.agc-work]
command = "/opt/homebrew/bin/appgallery-connect-mcp"
args = ["serve", "--profile", "work"]

[profiles.fast]
model = "gpt-5-mini"
"#;

    #[test]
    fn codex_home_resolution() {
        assert_eq!(
            codex_home(Some("/tmp/codex".into()), Some("/home/u".into())).unwrap(),
            PathBuf::from("/tmp/codex")
        );
        assert_eq!(
            codex_home(Some("".into()), Some("/home/u".into())).unwrap(),
            PathBuf::from("/home/u").join(".codex")
        );
        assert!(codex_home(None, None).is_err());
    }

    #[test]
    fn windows_path_round_trips_through_toml() {
        let exe =
            r"C:\Users\u\AppData\Local\Programs\appgallery-connect-mcp\appgallery-connect-mcp.exe";
        let spec = ServerSpec {
            command: exe.into(),
            ..spec()
        };
        let mut doc = DocumentMut::new();
        apply_entry(&mut doc, &spec).unwrap();
        let parsed: DocumentMut = doc.to_string().parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["appgallery"]["command"].as_str(),
            Some(exe)
        );
    }

    #[test]
    fn adds_entry_to_empty_config() {
        let mut doc = DocumentMut::new();
        assert_eq!(apply_entry(&mut doc, &spec()).unwrap(), None);
        assert_eq!(
            doc.to_string(),
            "[mcp_servers.appgallery]\ncommand = \"/usr/local/bin/appgallery-connect-mcp\"\nargs = [\"serve\", \"--profile\", \"default\"]\n"
        );
    }

    #[test]
    fn replaces_entry_and_preserves_everything_else() {
        let mut doc: DocumentMut = EXISTING.parse().unwrap();
        let previous = apply_entry(&mut doc, &spec()).unwrap().unwrap();
        assert_eq!(previous.keys, ["command", "args", "env"]);
        assert_eq!(previous.existing, Existing::SameProfile);
        let expected = EXISTING.replace(
            "command = \"/opt/homebrew/bin/appgallery-connect-mcp\"\nargs = [\"serve\", \"--profile\", \"default\"]\nenv = { SOME = \"old-value\" }\n",
            "command = \"/usr/local/bin/appgallery-connect-mcp\"\nargs = [\"serve\", \"--profile\", \"default\"]\n",
        );
        assert_ne!(expected, EXISTING);
        assert_eq!(doc.to_string(), expected);
    }

    #[test]
    fn register_writes_file_in_temp_codex_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex-home");
        let mut ui = ScriptedUi::answering(&["y"], &[]);
        let outcome = register(&mut ui, &home, &spec(), Default::default()).unwrap();
        assert_eq!(outcome, Registration::Changed);
        let text = read(&home);
        assert!(text.contains("[mcp_servers.appgallery]"));
        assert!(
            ui.text()
                .contains("args = [\"serve\", \"--profile\", \"default\"]")
        );

        // Повторный запуск ничего не спрашивает и не меняет, даже без --yes.
        let mut ui = ScriptedUi::default();
        let outcome = register(&mut ui, &home, &spec(), Default::default()).unwrap();
        assert_eq!(outcome, Registration::UpToDate);
        assert_eq!(read(&home), text);
        assert!(ui.questions.is_empty());
        assert!(ui.text().contains("nothing to change"));
    }

    #[test]
    fn register_keeps_file_when_declined() {
        for answer in ["n", ""] {
            let mut ui = ScriptedUi::answering(&[answer], &[]);
            let dir = tempfile::tempdir().unwrap();
            fs::write(config_path(dir.path()), EXISTING).unwrap();
            let outcome = register(&mut ui, dir.path(), &spec(), Default::default()).unwrap();
            assert_eq!(outcome, Registration::Declined);
            assert_eq!(read(dir.path()), EXISTING);
            let text = ui.text();
            assert!(
                text.contains(
                    "replaces the existing `appgallery` entry (appgallery-connect-mcp, the same \
                     profile; keys: command, args, env)"
                ),
                "{text}"
            );
            // Значения прежней записи (в том числе `env`) не показываются.
            assert!(!text.contains("old-value"), "{text}");
        }
    }

    #[test]
    fn dry_run_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex-home");
        let options = RegisterOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ui = ScriptedUi::default();
        let outcome = register(&mut ui, &home, &spec(), options).unwrap();
        assert_eq!(outcome, Registration::DryRun);
        assert!(!home.exists());
        assert!(ui.text().contains("[mcp_servers.appgallery]"));

        fs::create_dir_all(&home).unwrap();
        fs::write(config_path(&home), EXISTING).unwrap();
        let target = ServerSpec {
            name: "other_agc".into(),
            ..spec()
        };
        let mut ui = ScriptedUi::default();
        let outcome = register(&mut ui, &home, &target, options).unwrap();
        assert_eq!(outcome, Registration::DryRun);
        assert_eq!(read(&home), EXISTING);
        assert!(ui.text().contains("--force"), "{}", ui.text());
        assert!(ui.questions.is_empty());
    }

    #[test]
    fn foreign_and_other_profile_entries_need_explicit_answer_or_force() {
        for (name, describe) in [
            ("other_agc", "another server, not appgallery-connect-mcp"),
            ("agc-work", "another profile, `work`"),
        ] {
            let target = ServerSpec {
                name: name.into(),
                ..spec()
            };
            let dir = tempfile::tempdir().unwrap();
            fs::write(config_path(dir.path()), EXISTING).unwrap();

            // `--yes` такую запись не перезаписывает.
            let mut ui = ScriptedUi::default();
            let err = register(&mut ui, dir.path(), &target, yes()).unwrap_err();
            assert!(err.to_string().contains("--force"), "{err}");
            assert_eq!(read(dir.path()), EXISTING);
            assert!(ui.text().contains(describe), "{}", ui.text());
            assert!(!ui.text().contains("secret-value"), "{}", ui.text());

            let mut ui = ScriptedUi::answering(&["n"], &[]);
            let outcome = register(&mut ui, dir.path(), &target, Default::default()).unwrap();
            assert_eq!(outcome, Registration::Declined);
            assert_eq!(read(dir.path()), EXISTING);

            // `--force` без `--yes` — обычное подтверждение записи.
            let mut ui = ScriptedUi::answering(&["n"], &[]);
            let options = RegisterOptions {
                force: true,
                ..Default::default()
            };
            let outcome = register(&mut ui, dir.path(), &target, options).unwrap();
            assert_eq!(outcome, Registration::Declined);
            assert_eq!(ui.questions, ["Write the Codex config?"]);
            assert_eq!(read(dir.path()), EXISTING);

            let mut ui = ScriptedUi::answering(&["y"], &[]);
            let outcome = register(&mut ui, dir.path(), &target, Default::default()).unwrap();
            assert_eq!(outcome, Registration::Changed);
            let text = read(dir.path());
            assert!(text.contains(&format!("[mcp_servers.{name}]")), "{text}");
            assert!(text.contains("[profiles.fast]"), "{text}");
        }

        // `--yes --force` заменяет без вопросов.
        let dir = tempfile::tempdir().unwrap();
        fs::write(config_path(dir.path()), EXISTING).unwrap();
        let target = ServerSpec {
            name: "other_agc".into(),
            ..spec()
        };
        let mut ui = ScriptedUi::default();
        let options = RegisterOptions {
            force: true,
            ..yes()
        };
        let outcome = register(&mut ui, dir.path(), &target, options).unwrap();
        assert_eq!(outcome, Registration::Changed);
        assert!(!read(dir.path()).contains("some-other-appgallery-server"));
    }

    #[test]
    fn rejects_non_table_mcp_servers() {
        let mut doc: DocumentMut = "mcp_servers = 1\n".parse().unwrap();
        assert!(apply_entry(&mut doc, &spec()).is_err());
    }

    #[test]
    fn only_user_scope() {
        ensure_scope(Scope::User).unwrap();
        assert!(ensure_scope(Scope::Project).is_err());
        assert!(ensure_scope(Scope::Local).is_err());
    }
}
