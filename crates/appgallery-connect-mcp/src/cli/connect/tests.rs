//! Сценарии `connect` с хранилищем в памяти, файлом профилей и каталогом Codex во временном
//! каталоге, методом получения токена на `wiremock` и записью вызовов `claude` вместо
//! настоящего CLI. Системный keychain, сеть и настоящие клиенты не используются; учётные
//! данные выдуманы и собраны из частей.

use std::fs;

use appgallery_connect_api::MemoryStore;
use mcp_kit::approval::testing::{FakePresence, MemoryStore as MemoryApprovals};
use wiremock::MockServer;

use super::*;
use crate::clients::TEST_EXE;
use crate::clients::claude::testing::{FakeRunner, exists, get_output};
use crate::profiles::DEFAULT_PROFILE;
use crate::testing::{access_token, client_id, client_secret, credentials, token_server};
use crate::ui::testing::ScriptedUi;

/// Метка рабочих учётных данных: только их принимает метод получения токена.
const GOOD: &str = "7248";
/// Метка учётных данных, которые метод получения токена отклоняет.
const BAD: &str = "6666";

struct Fixture {
    dir: tempfile::TempDir,
    store: MemoryStore,
    repo: ProfileRepo,
    server: MockServer,
    no_verify: bool,
    runner: FakeRunner,
    ui: ScriptedUi,
    codex_home: PathBuf,
    /// Одобрение прав, как на macOS; `None` — как на Linux и Windows.
    approval: Option<(MemoryApprovals, FakePresence)>,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            store: MemoryStore::new(),
            repo: ProfileRepo::in_dir(dir.path().join("config")),
            server: token_server(&credentials(GOOD)).await,
            no_verify: false,
            runner: FakeRunner::default(),
            ui: ScriptedUi::default(),
            codex_home: dir.path().join("codex"),
            approval: None,
            dir,
        }
    }

    /// Профиль с уже сохранёнными учётными данными `tag` (как после `auth login`).
    fn with_stored(self, profile: &str, tag: &str) -> Self {
        self.store.set(profile, &credentials(tag)).unwrap();
        let settings = ProfileSettings {
            packages: vec!["com.example.app".into()],
            ..Default::default()
        };
        self.repo.upsert(profile, settings).unwrap();
        self
    }

    /// Файл с секретом учётных данных `tag`.
    fn secret_file(&self, tag: &str) -> PathBuf {
        let path = self.dir.path().join(format!("secret-{tag}.txt"));
        fs::write(&path, format!("  {}\n", client_secret(tag))).unwrap();
        path
    }

    async fn connect(&mut self, args: &ConnectArgs) -> anyhow::Result<()> {
        self.connect_with(args, None).await
    }

    /// `connect` с секретом из `stdin` (для `--secret-stdin`).
    async fn connect_with(
        &mut self,
        args: &ConnectArgs,
        stdin: Option<&str>,
    ) -> anyhow::Result<()> {
        let verifier = Verifier::with_base_url(&self.server.uri());
        let mut reader = stdin.unwrap_or_default().as_bytes();
        let source = match (&args.secret_file, args.secret_stdin) {
            (Some(path), _) => SecretSource::File(path),
            (None, true) => SecretSource::Reader(&mut reader),
            (None, false) => SecretSource::Prompt,
        };
        let approval = self
            .approval
            .as_ref()
            .map(|(store, presence)| (store as &dyn ApprovalStore, presence as &dyn Presence));
        let mut env = ConnectEnv {
            approval,
            store: &self.store,
            repo: &self.repo,
            verifier: (!self.no_verify).then_some(&verifier),
            runner: &mut self.runner,
            ui: &mut self.ui,
            exe: PathBuf::from(TEST_EXE),
            codex_home: self.codex_home.clone(),
        };
        connect(&mut env, args, source).await
    }

    fn codex_config(&self) -> Option<String> {
        fs::read_to_string(codex::config_path(&self.codex_home)).ok()
    }

    fn settings(&self, profile: &str) -> ProfileSettings {
        self.repo.get(profile).unwrap().unwrap()
    }

    /// Ничего не сохранено и не зарегистрировано: ни учётных данных, ни профиля, ни записи в
    /// клиенте (у Claude Code — ни одного вызова, кроме `--version`).
    fn assert_nothing_changed(&self, profile: &str) {
        assert!(self.store.get(profile).is_err());
        assert_eq!(self.repo.get(profile).unwrap(), None);
        assert!(self.codex_config().is_none());
        assert!(
            self.runner
                .calls
                .iter()
                .all(|call| call == &["--version".to_owned()]),
            "{:?}",
            self.runner.calls
        );
    }

    /// Секрета, полного идентификатора клиента и токена нет ни в выводе, ни в вопросах, ни в
    /// конфигурации клиента, ни в аргументах `claude`, ни в файле профилей.
    fn assert_clean(&self, tag: &str) {
        let mut texts = vec![self.ui.text(), self.ui.questions.join("\n")];
        texts.extend(self.codex_config());
        texts.extend(fs::read_to_string(self.repo.path()).ok());
        texts.extend(self.runner.calls.iter().map(|call| call.join(" ")));
        for text in texts {
            assert!(!text.contains(&client_secret(tag)), "{text}");
            assert!(!text.contains(&client_id(tag)), "{text}");
            assert!(!text.contains(&access_token()), "{text}");
        }
    }
}

fn args(client: ClientKind) -> ConnectArgs {
    ConnectArgs {
        client,
        profile: ProfileArg {
            profile: DEFAULT_PROFILE.into(),
        },
        name: None,
        scope: Scope::User,
        region: None,
        client_id: None,
        secret_file: None,
        secret_stdin: false,
        default_apps: Vec::new(),
        default_packages: Vec::new(),
        allow_write: false,
        allow_destructive: false,
        toolsets: Vec::new(),
        no_dynamic_toolsets: false,
        allow_paths: Vec::new(),
        yes: false,
        force: false,
        dry_run: false,
        no_verify: false,
        silent: false,
        secret_argument: None,
        stray: Vec::new(),
        oauth: false,
    }
}

/// Аргументы с учётными данными `tag`: идентификатор флагом, секрет из файла.
fn with_flags(mut args: ConnectArgs, fx: &Fixture, tag: &str) -> ConnectArgs {
    args.client_id = Some(client_id(tag));
    args.secret_file = Some(fx.secret_file(tag));
    args
}

/// Серверы из конфига Codex. Конфиг разбирается как TOML, а не ищется подстрокой: на
/// Windows пути с обратной косой записываются литеральными строками.
fn codex_servers(config: &str) -> toml::Table {
    let table: toml::Table = toml::from_str(config).unwrap();
    table
        .get("mcp_servers")
        .and_then(|servers| servers.as_table())
        .cloned()
        .unwrap_or_default()
}

/// Команда и аргументы сервера `name` в конфиге Codex.
fn codex_entry(config: &str, name: &str) -> (String, Vec<String>) {
    let servers = codex_servers(config);
    let server = servers[name].as_table().unwrap();
    let args = server["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    (server["command"].as_str().unwrap().to_owned(), args)
}

#[tokio::test]
async fn first_codex_run_with_flags_stores_credentials_and_writes_config() {
    let mut fx = Fixture::new().await;
    let builds = fx.dir.path().join("builds");
    fs::create_dir(&builds).unwrap();
    let mut args = with_flags(args(ClientKind::Codex), &fx, GOOD);
    args.region = Some(Region::Ru);
    args.allow_write = true;
    args.toolsets = vec!["apps".into(), " ".into(), "apps".into(), "releases".into()];
    args.allow_paths = vec![builds.clone(), builds.clone()];
    args.default_apps = vec!["100000001".into()];
    args.yes = true;

    fx.connect(&args).await.unwrap();

    assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap(), credentials(GOOD));
    let settings = fx.settings(DEFAULT_PROFILE);
    assert_eq!(settings.region, Region::Ru);
    assert_eq!(settings.apps, ["100000001"]);
    assert_eq!(settings.client.as_deref(), Some("codex"));
    assert!(settings.allow_write && !settings.allow_destructive);
    // Каталоги — в правах профиля, канонизированные и без повторов.
    assert_eq!(settings.allow_path, [builds.canonicalize().unwrap()]);
    assert_eq!(settings.toolsets, ["apps", "releases"]);
    assert!(settings.dynamic_toolsets);

    let config = fx.codex_config().unwrap();
    let (command, serve_args) = codex_entry(&config, "appgallery");
    assert_eq!(command, TEST_EXE);
    assert_eq!(
        serve_args,
        [
            "serve",
            "--profile",
            "default",
            "--toolsets",
            "apps,releases",
            "--dynamic-toolsets"
        ]
    );
    // В записи клиента только команда и аргументы: ни `env`, ни секрета.
    assert_eq!(
        codex_servers(&config)["appgallery"]
            .as_table()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["args", "command"]
    );
    assert!(fx.ui.questions.is_empty(), "{:?}", fx.ui.questions);
    let out = fx.ui.text();
    assert!(out.contains("Region: ru (Russia"), "{out}");
    assert!(out.contains("Credentials work"), "{out}");
    assert!(out.contains("saved in the system keychain"), "{out}");
    fx.assert_clean(GOOD);
    assert!(fx.runner.calls.is_empty());
}

#[tokio::test]
async fn interactive_run_explains_how_to_create_an_api_client() {
    let mut fx = Fixture::new().await;
    let id = client_id(GOOD);
    let secret = client_secret(GOOD);
    // Идентификатор, затем запись конфига — да; секрет — скрытым вводом.
    fx.ui = ScriptedUi::answering(&[&id, "y"], &[&secret]);
    let mut args = args(ClientKind::Codex);
    args.profile.profile = "work".into();

    fx.connect(&args).await.unwrap();

    assert_eq!(
        fx.ui.questions,
        [
            "Client ID: ",
            "Client secret (input is hidden): ",
            "Write the Codex config?"
        ]
    );
    let out = fx.ui.text();
    for step in [
        "Users and permissions",
        "API key > Connect API",
        "API client tab",
        "Project as N/A",
        "Administrator",
        crate::login::AGC_CONSOLE_URL,
    ] {
        assert!(out.contains(step), "{step}: {out}");
    }
    assert_eq!(fx.store.get("work").unwrap(), credentials(GOOD));
    let config = fx.codex_config().unwrap();
    assert!(config.contains("[mcp_servers.agc-work]"), "{config}");
    assert_eq!(
        codex_entry(&config, "agc-work").1,
        ["serve", "--profile", "work", "--dynamic-toolsets"]
    );
    fx.assert_clean(GOOD);
}

#[tokio::test]
async fn secret_from_stdin() {
    let mut fx = Fixture::new().await;
    let mut args = args(ClientKind::Codex);
    args.client_id = Some(client_id(GOOD));
    args.secret_stdin = true;
    args.yes = true;
    let stdin = format!("{}\n", client_secret(GOOD));
    fx.connect_with(&args, Some(&stdin)).await.unwrap();
    assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap(), credentials(GOOD));
    fx.assert_clean(GOOD);
}

#[tokio::test]
async fn failed_check_stores_nothing_and_registers_nothing() {
    for client in [ClientKind::Codex, ClientKind::ClaudeCode] {
        let mut fx = Fixture::new().await;
        let mut args = with_flags(args(client), &fx, BAD);
        args.yes = true;
        let err = fx.connect(&args).await.unwrap_err().to_string();
        assert!(err.contains("could not get an access token"), "{err}");
        assert!(err.contains("Nothing was stored"), "{err}");
        assert!(!err.contains(&client_secret(BAD)), "{err}");
        fx.assert_nothing_changed(DEFAULT_PROFILE);
        fx.assert_clean(BAD);
    }

    // Неработающие новые учётные данные не трогают прежние рабочие.
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    let before = fx.settings(DEFAULT_PROFILE);
    let mut args = with_flags(args(ClientKind::Codex), &fx, BAD);
    args.yes = true;
    assert!(fx.connect(&args).await.is_err());
    assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap(), credentials(GOOD));
    assert_eq!(fx.settings(DEFAULT_PROFILE), before);
    assert!(fx.codex_config().is_none());
}

#[tokio::test]
async fn rerun_reuses_working_credentials_and_changes_nothing_the_second_time() {
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    fx.ui = ScriptedUi::answering(&["y"], &[]);
    let mut args = args(ClientKind::Codex);
    args.allow_destructive = true;
    fx.connect(&args).await.unwrap();

    // Ни идентификатора, ни секрета: только запись конфига.
    assert_eq!(fx.ui.questions, ["Write the Codex config?"]);
    let out = fx.ui.text();
    assert!(out.contains("already has an API client"), "{out}");
    assert!(out.contains("The stored credentials work"), "{out}");
    assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap(), credentials(GOOD));
    let settings = fx.settings(DEFAULT_PROFILE);
    assert!(settings.allow_write && settings.allow_destructive);
    // Несвязанные настройки профиля сохраняются.
    assert_eq!(settings.packages, ["com.example.app"]);
    let config = fx.codex_config().unwrap();
    assert_eq!(
        codex_entry(&config, "appgallery").1,
        ["serve", "--profile", "default", "--dynamic-toolsets"]
    );

    // Ещё раз без терминала и без --yes: без вопросов, конфиг не меняется.
    fx.ui = ScriptedUi::default();
    fx.connect(&args).await.unwrap();
    assert!(fx.ui.questions.is_empty());
    assert!(fx.ui.text().contains("already runs"), "{}", fx.ui.text());
    assert_eq!(fx.codex_config().unwrap(), config);
    fx.assert_clean(GOOD);
}

#[tokio::test]
async fn broken_stored_credentials_are_replaced() {
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, BAD);
    let id = client_id(GOOD);
    let secret = client_secret(GOOD);
    fx.ui = ScriptedUi::answering(&[&id, "y"], &[&secret]);
    fx.connect(&args(ClientKind::Codex)).await.unwrap();
    let out = fx.ui.text();
    assert!(out.contains("do not work"), "{out}");
    assert!(out.contains("replaced in the system keychain"), "{out}");
    assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap(), credentials(GOOD));
}

#[tokio::test]
async fn non_interactive_run_without_credentials_explains_what_to_pass() {
    let mut fx = Fixture::new().await;
    let err = fx
        .connect(&args(ClientKind::Codex))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("--client-id"), "{err}");
    assert!(err.contains("--secret-file"), "{err}");
    assert!(err.contains("API key > Connect API"), "{err}");
    fx.assert_nothing_changed(DEFAULT_PROFILE);
}

#[tokio::test]
async fn no_verify_stores_unchecked_credentials() {
    let mut fx = Fixture::new().await;
    fx.no_verify = true;
    let mut args = with_flags(args(ClientKind::Codex), &fx, BAD);
    args.no_verify = true;
    args.yes = true;
    fx.connect(&args).await.unwrap();
    assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap(), credentials(BAD));
    assert!(fx.ui.text().contains("--no-verify"));
}

#[tokio::test]
async fn access_flags_change_the_profile_not_the_registration() {
    let cases = [(false, false), (true, false), (false, true), (true, true)];
    for (write, destructive) in cases {
        let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
        let mut args = args(ClientKind::Codex);
        args.allow_write = write;
        args.allow_destructive = destructive;
        args.yes = true;
        fx.connect(&args).await.unwrap();

        let settings = fx.settings(DEFAULT_PROFILE);
        assert_eq!(settings.allow_write, write || destructive);
        assert_eq!(settings.allow_destructive, destructive);

        let config = fx.codex_config().unwrap();
        assert_eq!(
            codex_entry(&config, "appgallery").1,
            ["serve", "--profile", "default", "--dynamic-toolsets"]
        );
    }

    // Без флагов права профиля не меняются.
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    let mut settings = fx.settings(DEFAULT_PROFILE);
    settings.allow_write = true;
    settings.allow_path = vec![fx.dir.path().to_path_buf()];
    fx.repo.upsert(DEFAULT_PROFILE, settings.clone()).unwrap();
    let mut args = args(ClientKind::Codex);
    args.yes = true;
    fx.connect(&args).await.unwrap();
    let after = fx.settings(DEFAULT_PROFILE);
    assert!(after.allow_write && !after.allow_destructive);
    assert_eq!(after.allow_path, settings.allow_path);

    // Только --allow-path: уровень прежний, каталоги новые.
    let builds = fx.dir.path().join("builds");
    fs::create_dir(&builds).unwrap();
    args.allow_paths = vec![builds.clone()];
    fx.connect(&args).await.unwrap();
    let after = fx.settings(DEFAULT_PROFILE);
    assert!(after.allow_write && !after.allow_destructive);
    assert_eq!(after.allow_path, [builds.canonicalize().unwrap()]);
}

#[tokio::test]
async fn raising_access_needs_approval_on_macos() {
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    fx.approval = Some((MemoryApprovals::default(), FakePresence::new(false)));
    let mut args = args(ClientKind::Codex);
    args.allow_destructive = true;
    args.yes = true;
    fx.connect(&args).await.unwrap();
    // Отказ: права прежние, но профиль и регистрация сохранены.
    let settings = fx.settings(DEFAULT_PROFILE);
    assert!(!settings.allow_write && !settings.allow_destructive);
    let text = fx.ui.text();
    assert!(text.contains("The access was not changed"), "{text}");
    assert!(
        text.contains(&format!(
            "`appgallery-connect-mcp ui` (profile `{DEFAULT_PROFILE}`)"
        )),
        "{text}"
    );
    assert_eq!(settings.client.as_deref(), Some("codex"));
    assert!(fx.codex_config().is_some());

    fx.approval = Some((MemoryApprovals::default(), FakePresence::new(true)));
    fx.ui = ScriptedUi::default();
    fx.connect(&args).await.unwrap();
    assert!(fx.settings(DEFAULT_PROFILE).allow_destructive);
    let (approvals, presence) = fx.approval.as_ref().unwrap();
    assert_eq!(presence.asked.get(), 1);
    assert_eq!(
        approvals.load(DEFAULT_PROFILE).unwrap().unwrap().level,
        AccessLevel::Destructive
    );
    assert!(
        fx.ui.text().contains("approved on this Mac"),
        "{}",
        fx.ui.text()
    );

    // Понижение — без запроса.
    args.allow_destructive = false;
    args.allow_write = true;
    fx.ui = ScriptedUi::default();
    fx.connect(&args).await.unwrap();
    let settings = fx.settings(DEFAULT_PROFILE);
    assert!(settings.allow_write && !settings.allow_destructive);
    assert_eq!(fx.approval.as_ref().unwrap().1.asked.get(), 1);
}

#[tokio::test]
async fn no_dynamic_toolsets_lists_every_tool_at_start() {
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    let mut args = args(ClientKind::Codex);
    args.no_dynamic_toolsets = true;
    args.yes = true;
    fx.connect(&args).await.unwrap();
    assert!(!fx.settings(DEFAULT_PROFILE).dynamic_toolsets);
    let config = fx.codex_config().unwrap();
    assert_eq!(
        codex_entry(&config, "appgallery").1,
        ["serve", "--profile", "default"]
    );

    // Повторный запуск по умолчанию включает динамические наборы снова.
    args.no_dynamic_toolsets = false;
    fx.connect(&args).await.unwrap();
    assert!(fx.settings(DEFAULT_PROFILE).dynamic_toolsets);
    assert!(fx.codex_config().unwrap().contains("--dynamic-toolsets"));
}

#[tokio::test]
async fn silent_mode_asks_nothing_and_writes_the_config() {
    let mut fx = Fixture::new().await;
    let mut silent = args(ClientKind::Codex);
    silent.silent = true;
    let verifier = Verifier::with_base_url(&fx.server.uri());
    let mut ui = SilentUi;

    // Без учётных данных — ошибка с подсказкой, ничего не записано.
    let err = {
        let mut env = ConnectEnv {
            store: &fx.store,
            repo: &fx.repo,
            verifier: Some(&verifier),
            runner: &mut fx.runner,
            ui: &mut ui,
            exe: PathBuf::from(TEST_EXE),
            codex_home: fx.codex_home.clone(),
            approval: None,
        };
        connect(&mut env, &silent, SecretSource::Prompt)
            .await
            .unwrap_err()
            .to_string()
    };
    assert!(err.contains("without --silent"), "{err}");
    fx.assert_nothing_changed(DEFAULT_PROFILE);

    // С флагами: конфиг записан без вопросов (как с --yes).
    let silent = with_flags(silent, &fx, GOOD);
    assert!(silent.register_options().yes);
    let path = silent.secret_file.clone().unwrap();
    let mut env = ConnectEnv {
        store: &fx.store,
        repo: &fx.repo,
        verifier: Some(&verifier),
        runner: &mut fx.runner,
        ui: &mut ui,
        exe: PathBuf::from(TEST_EXE),
        codex_home: fx.codex_home.clone(),
        approval: None,
    };
    connect(&mut env, &silent, SecretSource::File(&path))
        .await
        .unwrap();
    assert!(
        fx.codex_config()
            .unwrap()
            .contains("[mcp_servers.appgallery]")
    );
    assert_eq!(fx.store.get(DEFAULT_PROFILE).unwrap(), credentials(GOOD));
}

#[tokio::test]
async fn declined_config_write_keeps_credentials_and_profile() {
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    fx.ui = ScriptedUi::answering(&["n"], &[]);
    fx.connect(&args(ClientKind::Codex)).await.unwrap();
    assert!(fx.codex_config().is_none());
    assert!(fx.store.get(DEFAULT_PROFILE).is_ok());
    assert_eq!(
        fx.settings(DEFAULT_PROFILE).client.as_deref(),
        Some("codex")
    );
    assert!(fx.ui.text().contains("config was not changed"));
}

#[tokio::test]
async fn existing_codex_config_is_preserved() {
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    fs::create_dir_all(&fx.codex_home).unwrap();
    let existing =
        "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"npx\"  # keep\n";
    fs::write(codex::config_path(&fx.codex_home), existing).unwrap();
    let mut args = args(ClientKind::Codex);
    args.yes = true;
    fx.connect(&args).await.unwrap();
    let config = fx.codex_config().unwrap();
    assert!(config.starts_with(existing), "{config}");
    assert!(config.contains("[mcp_servers.appgallery]"), "{config}");
}

#[tokio::test]
async fn foreign_codex_entry_needs_force() {
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    fs::create_dir_all(&fx.codex_home).unwrap();
    let existing = "[mcp_servers.appgallery]\ncommand = \"npx\"\nargs = [\"other\"]\n";
    fs::write(codex::config_path(&fx.codex_home), existing).unwrap();
    let mut args = args(ClientKind::Codex);
    args.yes = true;
    let err = fx.connect(&args).await.unwrap_err();
    assert!(err.to_string().contains("--force"), "{err}");
    assert_eq!(fx.codex_config().unwrap(), existing);

    args.force = true;
    fx.connect(&args).await.unwrap();
    let config = fx.codex_config().unwrap();
    assert_eq!(codex_entry(&config, "appgallery").0, TEST_EXE);
    assert!(!config.contains("npx"), "{config}");
}

#[tokio::test]
async fn dry_run_changes_nothing() {
    // Без учётных данных: ничего не спрашивается и не сохраняется.
    let mut fx = Fixture::new().await;
    let mut args = args(ClientKind::Codex);
    args.dry_run = true;
    args.allow_write = true;
    fx.connect(&args).await.unwrap();
    let text = fx.ui.text();
    assert!(
        text.contains("connect would ask for the client ID and secret"),
        "{text}"
    );
    assert!(text.contains("would get access `write`"), "{text}");
    assert!(text.contains("[mcp_servers.appgallery]"), "{text}");
    assert!(text.contains("Dry run: nothing was changed"), "{text}");
    assert!(fx.ui.questions.is_empty());
    assert!(!fx.repo.path().exists());
    fx.assert_nothing_changed(DEFAULT_PROFILE);

    // С флагами: секрет из файла читается, но не проверяется и не сохраняется.
    let args = with_flags(args, &fx, GOOD);
    fx.ui = ScriptedUi::default();
    fx.connect(&args).await.unwrap();
    assert!(
        fx.ui.text().contains("would be checked and stored"),
        "{}",
        fx.ui.text()
    );
    fx.assert_nothing_changed(DEFAULT_PROFILE);
    fx.assert_clean(GOOD);

    // Claude Code с рабочими учётными данными: только `--version` и `mcp get`.
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    let settings = fx.settings(DEFAULT_PROFILE);
    fx.runner.results = [
        claude::CommandOutput {
            success: true,
            ..Default::default()
        },
        get_output("npx", &["other"]),
    ]
    .into();
    let mut args = self::args(ClientKind::ClaudeCode);
    args.dry_run = true;
    fx.connect(&args).await.unwrap();
    assert_eq!(
        fx.runner.calls,
        [vec!["--version".to_owned()], claude::get_args("appgallery")]
    );
    let text = fx.ui.text();
    assert!(text.contains("The stored credentials work"), "{text}");
    assert!(text.contains("--force"), "{text}");
    assert_eq!(fx.settings(DEFAULT_PROFILE), settings);
}

#[tokio::test]
async fn claude_flow_registers_only_the_serve_command() {
    let mut fx = Fixture::new().await;
    let id = client_id(GOOD);
    let secret = client_secret(GOOD);
    fx.ui = ScriptedUi::answering(&[&id, "y"], &[&secret]);
    let mut args = args(ClientKind::ClaudeCode);
    args.profile.profile = "work".into();
    args.scope = Scope::Local;
    args.allow_write = true;
    args.toolsets = vec!["reviews".into()];
    fx.connect(&args).await.unwrap();

    assert_eq!(
        fx.runner.calls,
        [
            vec!["--version".to_owned()],
            claude::add_args(
                Scope::Local,
                &ServerSpec {
                    name: "agc-work".into(),
                    profile: "work".into(),
                    command: TEST_EXE.into(),
                    args: [
                        "serve",
                        "--profile",
                        "work",
                        "--toolsets",
                        "reviews",
                        "--dynamic-toolsets"
                    ]
                    .map(String::from)
                    .to_vec(),
                }
            )
        ]
    );
    assert_eq!(fx.settings("work").client.as_deref(), Some("claude-code"));
    assert!(fx.codex_config().is_none());
    fx.assert_clean(GOOD);
}

#[tokio::test]
async fn claude_custom_name_and_existing_entries() {
    // Своё имя сервера.
    let mut fx = Fixture::new().await.with_stored("work", GOOD);
    let mut args = args(ClientKind::ClaudeCode);
    args.profile.profile = "work".into();
    args.name = Some("huawei".into());
    args.yes = true;
    fx.connect(&args).await.unwrap();
    assert_eq!(fx.runner.calls[1][4], "huawei");

    // Та же команда уже зарегистрирована — ничего не меняется.
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    fx.runner.results = [
        claude::CommandOutput {
            success: true,
            ..Default::default()
        },
        exists(),
        get_output(
            TEST_EXE,
            &["serve", "--profile", "default", "--dynamic-toolsets"],
        ),
    ]
    .into();
    let mut args = self::args(ClientKind::ClaudeCode);
    args.yes = true;
    fx.connect(&args).await.unwrap();
    assert_eq!(fx.runner.calls.len(), 3, "{:?}", fx.runner.calls);
    assert!(fx.ui.text().contains("already runs"), "{}", fx.ui.text());

    // Чужая запись с --yes не заменяется.
    let mut fx = Fixture::new().await.with_stored(DEFAULT_PROFILE, GOOD);
    fx.runner.results = [
        claude::CommandOutput {
            success: true,
            ..Default::default()
        },
        exists(),
        get_output("npx", &["other"]),
    ]
    .into();
    let err = fx.connect(&args).await.unwrap_err().to_string();
    assert!(err.contains("--force"), "{err}");
    assert!(
        fx.runner
            .calls
            .iter()
            .all(|call| call.get(1).map(String::as_str) != Some("remove")),
        "{:?}",
        fx.runner.calls
    );
}

#[tokio::test]
async fn claude_missing_cli_fails_before_questions() {
    let mut fx = Fixture::new().await;
    fx.runner.results = [claude::CommandOutput {
        success: false,
        stderr: "broken".into(),
        ..Default::default()
    }]
    .into();
    fx.ui = ScriptedUi::answering(&[], &[]);
    let err = fx.connect(&args(ClientKind::ClaudeCode)).await.unwrap_err();
    assert!(err.to_string().contains("broken"), "{err}");
    assert!(fx.ui.output.is_empty());
    assert!(fx.ui.questions.is_empty());
}

#[test]
fn relative_allow_path_becomes_absolute() {
    let paths = normalize_allow_paths(&[PathBuf::from(".")]).unwrap();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].is_absolute(), "{paths:?}");
}

#[tokio::test]
async fn invalid_input_fails_before_questions() {
    let probe = Fixture::new().await;
    let missing_dir = probe.dir.path().join("missing");
    let mut cases = Vec::new();
    let mut codex_project = args(ClientKind::Codex);
    codex_project.scope = Scope::Project;
    cases.push((codex_project, "--scope user"));
    let mut codex_local = args(ClientKind::Codex);
    codex_local.scope = Scope::Local;
    cases.push((codex_local, "--scope user"));
    let mut toolset = args(ClientKind::Codex);
    toolset.toolsets = vec!["Bad Set".into()];
    cases.push((toolset, "invalid toolset"));
    let mut unknown = args(ClientKind::Codex);
    unknown.toolsets = vec!["nope".into()];
    cases.push((unknown, "unknown toolset `nope`; available: apps"));
    let mut profile = args(ClientKind::Codex);
    profile.profile.profile = "--evil".into();
    cases.push((profile, "invalid profile name"));
    let mut name = args(ClientKind::Codex);
    name.name = Some("a b".into());
    cases.push((name, "invalid server name"));
    let mut long_name = args(ClientKind::Codex);
    long_name.name = Some("appgallery-production".into());
    cases.push((long_name, "longer than 17"));
    let mut long_profile = args(ClientKind::ClaudeCode);
    long_profile.profile.profile = "production-eu1".into();
    cases.push((
        long_profile,
        "profile name `production-eu1` is too long for the default server name: \
         `agc-production-eu1` is longer than 17 characters",
    ));
    let mut allow_path = args(ClientKind::Codex);
    allow_path.allow_paths = vec![missing_dir];
    cases.push((allow_path, "is not an existing directory"));
    let mut app = args(ClientKind::Codex);
    app.default_apps = vec!["com.example.app".into()];
    cases.push((app, "invalid app ID"));
    let mut package = args(ClientKind::Codex);
    package.default_packages = vec!["app".into()];
    cases.push((package, "invalid package name"));
    let mut secret = args(ClientKind::Codex);
    secret.secret_argument = Some(Discarded);
    cases.push((secret, "not accepted as a command-line argument"));
    let mut stray = args(ClientKind::Codex);
    stray.stray = vec![Discarded];
    cases.push((stray, "takes no positional arguments after the client name"));
    let mut bad_id = args(ClientKind::Codex);
    bad_id.client_id = Some("1 2".into());
    cases.push((bad_id, "invalid API client ID"));

    for (args, expected) in cases {
        let mut fx = Fixture::new().await;
        fx.ui = ScriptedUi::answering(&[], &[]);
        let err = fx.connect(&args).await.unwrap_err();
        assert!(format!("{err:#}").contains(expected), "{expected}: {err:#}");
        // Ошибка флага `--client-id` случается после строки о регионе, остальные — до
        // любого вывода.
        if expected != "invalid API client ID" {
            assert!(fx.ui.output.is_empty(), "{expected}: {:?}", fx.ui.output);
            assert!(fx.runner.calls.is_empty());
        }
        assert!(fx.ui.questions.is_empty());
        fx.assert_nothing_changed(DEFAULT_PROFILE);
    }
}

#[test]
fn profile_of_13_characters_fits_the_default_name() {
    let mut args = args(ClientKind::Codex);
    args.profile.profile = "production-eu".into();
    assert_eq!(
        server_name(&args, "production-eu").unwrap(),
        "agc-production-eu"
    );
    assert_eq!(
        server_name(&self::args(ClientKind::Codex), DEFAULT_PROFILE).unwrap(),
        "appgallery"
    );
    assert!(server_name(&args, "production-eu1").is_err());
    args.name = Some("agc-prod".into());
    assert_eq!(server_name(&args, "production-eu1").unwrap(), "agc-prod");
}

/// Профиль со входом пользователя (как после `auth login --oauth`): `connect` проверяет
/// вход и пишет конфиг, но не трогает вход и отклоняет флаги API-клиента.
#[tokio::test]
async fn user_sign_in_profile_is_connected_with_its_stored_sign_in() {
    use crate::profiles::ProfileKind;
    use crate::testing::{mount_user_sign_in, oauth_client_id, refresh_token, sign_in};
    use appgallery_connect_api::Region;
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    let mut fx = Fixture::new().await;
    // Обновление по старому refresh token выдаёт новый: его нужно сохранить.
    Mock::given(method("POST"))
        .and(path("/oauth2/v3/token"))
        .and(body_string_contains(format!(
            "refresh_token={}",
            refresh_token("31")
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": crate::testing::user_access_token(),
            "expires_in": 3600,
            "refresh_token": refresh_token("32"),
        })))
        .mount(&fx.server)
        .await;
    mount_user_sign_in(
        &fx.server,
        "32",
        json!([{"id": "5100", "name": "Home", "siteId": 8}]),
    )
    .await;
    fx.store.set_oauth(DEFAULT_PROFILE, &sign_in("31")).unwrap();
    fx.repo
        .upsert(
            DEFAULT_PROFILE,
            ProfileSettings {
                kind: ProfileKind::Oauth,
                team_id: Some("5100".into()),
                region: Region::Ru,
                ..Default::default()
            },
        )
        .unwrap();

    // Флаги API-клиента и чужой регион отклоняются до вопросов.
    let err = fx
        .connect(&with_flags(args(ClientKind::Codex), &fx, GOOD))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("signs in as a HUAWEI ID user"), "{err}");
    let mut wrong_region = args(ClientKind::Codex);
    wrong_region.region = Some(Region::Cn);
    let err = fx.connect(&wrong_region).await.unwrap_err().to_string();
    assert!(err.contains("site of its team (ru)"), "{err}");
    let mut oauth = args(ClientKind::Codex);
    oauth.oauth = true;
    let err = fx.connect(&oauth).await.unwrap_err().to_string();
    assert!(err.contains("auth login --oauth"), "{err}");
    assert!(fx.codex_config().is_none());

    let mut run = args(ClientKind::Codex);
    run.yes = true;
    fx.connect(&run).await.unwrap();
    let out = fx.ui.text();
    assert!(out.contains("signs in as a HUAWEI ID user"), "{out}");
    assert!(out.contains("The stored sign-in works"), "{out}");
    assert!(!out.contains(&oauth_client_id("31")), "{out}");
    let config = fx.codex_config().unwrap();
    assert_eq!(
        codex_entry(&config, "appgallery").1,
        ["serve", "--profile", "default", "--dynamic-toolsets"]
    );
    let settings = fx.settings(DEFAULT_PROFILE);
    assert_eq!(settings.kind, ProfileKind::Oauth);
    assert_eq!(settings.team_id.as_deref(), Some("5100"));
    // Повёрнутый refresh token сохранён, API-клиента у профиля нет.
    let stored = fx.store.get_oauth(DEFAULT_PROFILE).unwrap();
    assert_eq!(stored.expose_refresh_token(), refresh_token("32"));
    assert!(fx.store.get(DEFAULT_PROFILE).is_err());
    for text in [
        out,
        config.clone(),
        fs::read_to_string(fx.repo.path()).unwrap(),
    ] {
        assert!(!text.contains(&refresh_token("31")), "{text}");
        assert!(!text.contains(&refresh_token("32")), "{text}");
    }

    // Без записи входа в keychain — ошибка с подсказкой, ничего не меняется.
    fx.store.delete_oauth(DEFAULT_PROFILE).unwrap();
    let err = fx.connect(&run).await.unwrap_err().to_string();
    assert!(
        err.contains("auth login --oauth --profile default"),
        "{err}"
    );
    assert!(err.contains("nothing was changed"), "{err}");
    assert_eq!(fx.codex_config().unwrap(), config);
}
