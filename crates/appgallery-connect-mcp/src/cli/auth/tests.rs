//! Сценарии `auth login`, `auth status` и `auth logout` с хранилищем в памяти, файлом
//! профилей во временном каталоге и методом получения токена на `wiremock`. Системный
//! keychain и сеть не используются; учётные данные выдуманы и собраны из частей.

use std::fs;
use std::path::Path;

use appgallery_connect_api::{ApiClientCredentials, MemoryStore, Region, StoreError};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::profiles::DEFAULT_PROFILE;
use crate::testing::{
    TOKEN_PATH, access_token, client_id, client_secret, credentials, token_server,
};
use crate::ui::testing::ScriptedUi;

fn args(profile: &str) -> LoginArgs {
    LoginArgs {
        profile: ProfileArg {
            profile: profile.into(),
        },
        region: None,
        client_id: None,
        secret_file: None,
        secret_stdin: false,
        default_apps: Vec::new(),
        default_packages: Vec::new(),
        no_verify: false,
        secret_argument: None,
        stray: Vec::new(),
    }
}

fn status_args(profile: Option<&str>, check: bool) -> StatusArgs {
    StatusArgs {
        profile: profile.map(str::to_owned),
        check,
    }
}

struct Setup {
    dir: tempfile::TempDir,
    repo: ProfileRepo,
    store: MemoryStore,
}

impl Setup {
    /// Файл с секретом `text` во временном каталоге.
    fn secret_file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, text).unwrap();
        path
    }
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let repo = ProfileRepo::in_dir(dir.path().join("config"));
    Setup {
        dir,
        repo,
        store: MemoryStore::new(),
    }
}

/// Нет ли в тексте секрета, полного идентификатора клиента и токена.
fn assert_clean(text: &str, tag: &str) {
    assert!(!text.contains(&client_secret(tag)), "{text}");
    assert!(!text.contains(&client_id(tag)), "{text}");
    assert!(!text.contains(&access_token()), "{text}");
}

#[tokio::test]
async fn login_status_logout() {
    let s = setup();
    let expected = credentials("7248");
    let server = token_server(&expected).await;
    let verifier = Verifier::with_base_url(&server.uri());

    // Пробелы и перевод строки вокруг значений обрезаются.
    let secret_file = s.secret_file("secret.txt", &format!("  {}\n", client_secret("7248")));
    let login_args = LoginArgs {
        region: Some(Region::Ru),
        client_id: Some(format!(" {} ", client_id("7248"))),
        secret_file: Some(secret_file.clone()),
        default_apps: vec!["100000001".into(), " 100000002".into()],
        default_packages: vec!["com.example.app".into()],
        ..args("work")
    };
    let mut ui = ScriptedUi::default();
    login(
        &s.store,
        &s.repo,
        Some(&verifier),
        &mut ui,
        &login_args,
        SecretSource::File(&secret_file),
    )
    .await
    .unwrap();
    let out = ui.text();
    assert!(ui.questions.is_empty(), "{:?}", ui.questions);
    assert!(out.contains("Region: ru (Russia"), "{out}");
    assert!(out.contains("Client ID: 10***48."), "{out}");
    assert!(out.contains("Credentials work"), "{out}");
    assert!(out.contains("API client credentials saved"), "{out}");
    assert_clean(&out, "7248");
    assert_eq!(s.store.get("work").unwrap(), expected);
    // Файл секрета не удаляется и не запоминается.
    assert!(secret_file.exists());

    let saved = s.repo.get("work").unwrap().unwrap();
    assert_eq!(saved.region, Region::Ru);
    assert_eq!(saved.apps, ["100000001", "100000002"]);
    assert_eq!(saved.packages, ["com.example.app"]);
    assert!(!saved.allow_write);
    let file = fs::read_to_string(s.repo.path()).unwrap();
    assert_clean(&file, "7248");
    assert!(!file.contains("secret"), "{file}");
    assert!(!file.contains(&secret_file.display().to_string()), "{file}");

    let mut ui = ScriptedUi::default();
    status(
        &s.store,
        &s.repo,
        Some(&verifier),
        &mut ui,
        &status_args(Some("work"), true),
    )
    .await
    .unwrap();
    let out = ui.text();
    assert!(out.contains("Keychain:     available"), "{out}");
    assert!(out.contains("Profile:      work"), "{out}");
    assert!(out.contains("Region:       ru (Russia"), "{out}");
    assert!(out.contains("Client ID:    10***48"), "{out}");
    assert!(out.contains("Credentials:  stored"), "{out}");
    assert!(out.contains("Check:        ok"), "{out}");
    assert!(out.contains("Default apps: 100000001, 100000002"), "{out}");
    assert!(out.contains("Write:        no"), "{out}");
    assert_clean(&out, "7248");

    // Без --check — без запроса.
    let before = server.received_requests().await.unwrap().len();
    let mut ui = ScriptedUi::default();
    status(&s.store, &s.repo, None, &mut ui, &status_args(None, false))
        .await
        .unwrap();
    assert!(
        ui.text().contains("Check:        not checked"),
        "{}",
        ui.text()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), before);

    let mut ui = ScriptedUi::default();
    logout(&s.store, &s.repo, &mut ui, "work").unwrap();
    assert!(ui.text().contains("removed from the system keychain"));
    assert!(ui.text().contains("Profile `work` removed"));
    assert!(!s.store.contains("work").unwrap());
    assert_eq!(s.repo.get("work").unwrap(), None);

    // После выхода профиля нет: статус по имени — не готов, без имени — профилей нет.
    let mut ui = ScriptedUi::default();
    let err = status(
        &s.store,
        &s.repo,
        Some(&verifier),
        &mut ui,
        &status_args(Some("work"), true),
    )
    .await
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("no API client credentials"), "{text}");
    assert!(text.contains("auth login --profile work"), "{text}");
    assert!(ui.text().contains("Check:        skipped"), "{}", ui.text());
    let err = status(
        &s.store,
        &s.repo,
        None,
        &mut ScriptedUi::default(),
        &status_args(None, false),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("no profiles yet"), "{err}");
}

#[tokio::test]
async fn second_login_replaces_the_credentials() {
    let s = setup();
    let first = s.secret_file("first.txt", &client_secret("0001"));
    let first_args = LoginArgs {
        client_id: Some(client_id("0001")),
        region: Some(Region::Sg),
        default_apps: vec!["100000001".into()],
        no_verify: true,
        ..args(DEFAULT_PROFILE)
    };
    let mut ui = ScriptedUi::default();
    login(
        &s.store,
        &s.repo,
        None,
        &mut ui,
        &first_args,
        SecretSource::File(&first),
    )
    .await
    .unwrap();
    assert!(ui.text().contains("Skipping the check"), "{}", ui.text());
    assert_eq!(s.store.get(DEFAULT_PROFILE).unwrap(), credentials("0001"));

    // Второй вход: новые учётные данные, регион и приложения из профиля сохраняются.
    let replacement = credentials("0002");
    let server = token_server(&replacement).await;
    let second = s.secret_file("second.txt", &client_secret("0002"));
    let second_args = LoginArgs {
        client_id: Some(client_id("0002")),
        ..args(DEFAULT_PROFILE)
    };
    let mut ui = ScriptedUi::default();
    login(
        &s.store,
        &s.repo,
        Some(&Verifier::with_base_url(&server.uri())),
        &mut ui,
        &second_args,
        SecretSource::File(&second),
    )
    .await
    .unwrap();
    let out = ui.text();
    assert!(out.contains("Region: sg (Singapore"), "{out}");
    assert!(!out.contains("Pass --region"), "{out}");
    assert!(out.contains("API client credentials replaced"), "{out}");
    assert_eq!(s.store.get(DEFAULT_PROFILE).unwrap(), replacement);
    let saved = s.repo.get(DEFAULT_PROFILE).unwrap().unwrap();
    assert_eq!(saved.region, Region::Sg);
    assert_eq!(saved.apps, ["100000001"]);
}

#[tokio::test]
async fn asks_in_the_terminal_or_reads_stdin() {
    let s = setup();
    // В терминале: идентификатор видимым вопросом, секрет — скрытым.
    let id = format!("  {}  ", client_id("1111"));
    let secret = format!(" {}\n", client_secret("1111"));
    let mut ui = ScriptedUi::answering(&[&id], &[&secret]);
    login(
        &s.store,
        &s.repo,
        None,
        &mut ui,
        &LoginArgs {
            no_verify: true,
            ..args("asked")
        },
        SecretSource::Prompt,
    )
    .await
    .unwrap();
    assert_eq!(ui.questions.len(), 2, "{:?}", ui.questions);
    assert!(ui.questions[0].contains("Client ID"));
    assert!(ui.questions[1].contains("input is hidden"));
    let out = ui.text();
    assert!(out.contains("Pass --region de, sg or ru"), "{out}");
    assert_clean(&out, "1111");
    assert_eq!(s.store.get("asked").unwrap(), credentials("1111"));

    // stdin: весь ввод до EOF, с обрезанными краями.
    let text = format!("{}\r\n", client_secret("2222"));
    let mut input = text.as_bytes();
    login(
        &s.store,
        &s.repo,
        None,
        &mut ScriptedUi::default(),
        &LoginArgs {
            client_id: Some(client_id("2222")),
            secret_stdin: true,
            no_verify: true,
            ..args("stdin")
        },
        SecretSource::Reader(&mut input),
    )
    .await
    .unwrap();
    assert_eq!(s.store.get("stdin").unwrap(), credentials("2222"));

    // Без терминала вопросов нет: нужны флаги.
    for (case, source, expected) in [
        (args("ci"), SecretSource::Prompt, "pass --client-id"),
        (
            LoginArgs {
                client_id: Some(client_id("3333")),
                ..args("ci")
            },
            SecretSource::Prompt,
            "pass --secret-file <PATH> or --secret-stdin",
        ),
    ] {
        let mut ui = ScriptedUi::default();
        let err = login(&s.store, &s.repo, None, &mut ui, &case, source)
            .await
            .unwrap_err();
        assert!(err.to_string().contains(expected), "{expected}: {err}");
        assert!(ui.questions.is_empty());
    }
    assert!(!s.store.contains("ci").unwrap());
}

/// Ни одна ошибка входа не показывает секрет и ничего не сохраняет.
#[tokio::test]
async fn failures_store_nothing_and_never_show_the_secret() {
    let s = setup();
    let server = token_server(&credentials("9999")).await;
    // HTTP 401 с телом, в котором отражены учётные данные.
    let unauthorized = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(401).set_body_string(format!(
            "invalid client {} / {}",
            client_id("4321"),
            client_secret("4321")
        )))
        .mount(&unauthorized)
        .await;

    let tag = "4321";
    let good = s.secret_file("good.txt", &client_secret(tag));
    // Секрет с пробелом внутри и с кавычкой — не та форма.
    let spaced = s.secret_file("spaced.txt", &format!("{} x", client_secret(tag)));
    let quoted = s.secret_file("quoted.txt", &format!("\"{}\"", client_secret(tag)));
    let empty = s.secret_file("empty.txt", " \n");
    let missing = s.dir.path().join("missing.txt");
    let with_id = |profile: &str| LoginArgs {
        client_id: Some(client_id(tag)),
        ..args(profile)
    };

    let rejected = Verifier::with_base_url(&server.uri());
    let http_401 = Verifier::with_base_url(&unauthorized.uri());
    // Порт 9 (discard) на loopback закрыт: соединение не устанавливается.
    let unreachable = Verifier::with_base_url("http://127.0.0.1:9");

    let cases: Vec<(LoginArgs, &Path, Option<&Verifier>, &str)> = vec![
        (with_id("ci"), &good, Some(&rejected), "code 203817914"),
        (with_id("ci"), &good, Some(&http_401), "HTTP 401"),
        (with_id("ci"), &good, Some(&unreachable), "could not reach"),
        (with_id("ci"), &spaced, None, "not allowed"),
        (with_id("ci"), &quoted, None, "not allowed"),
        (with_id("ci"), &empty, None, "client secret is empty"),
        (with_id("ci"), &missing, None, "cannot open the secret file"),
        (
            LoginArgs {
                client_id: Some(format!("{} 1", client_id(tag))),
                ..args("ci")
            },
            &good,
            None,
            "invalid API client ID",
        ),
        (
            LoginArgs {
                secret_argument: Some(Discarded),
                ..with_id("ci")
            },
            &good,
            None,
            "not accepted as a command-line argument",
        ),
        (
            LoginArgs {
                stray: vec![Discarded],
                ..with_id("ci")
            },
            &good,
            None,
            "takes no positional arguments",
        ),
        (
            LoginArgs {
                default_apps: vec!["com.example.app".into()],
                ..with_id("ci")
            },
            &good,
            None,
            "invalid app ID",
        ),
        (
            LoginArgs {
                default_packages: vec!["not a package".into()],
                ..with_id("ci")
            },
            &good,
            None,
            "invalid package name",
        ),
        (with_id("bad name"), &good, None, "invalid profile name"),
    ];
    for (case, file, verifier, expected) in cases {
        let mut ui = ScriptedUi::default();
        let err = login(
            &s.store,
            &s.repo,
            verifier,
            &mut ui,
            &case,
            SecretSource::File(file),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains(expected), "{expected}: {text}");
        assert_clean(&text, tag);
        assert_clean(&ui.text(), tag);
        if verifier.is_some() {
            assert!(text.contains("Nothing was stored"), "{text}");
            assert!(text.contains("--no-verify"), "{text}");
        }
    }
    assert!(!s.store.contains("ci").unwrap());
    assert!(!s.repo.path().exists());
}

/// Хранилище, которое отказывает в записи.
struct FailingStore;

impl CredentialStore for FailingStore {
    fn get(&self, profile: &str) -> Result<ApiClientCredentials, StoreError> {
        Err(StoreError::NotFound {
            profile: profile.to_owned(),
        })
    }

    fn set(&self, _: &str, _: &ApiClientCredentials) -> Result<(), StoreError> {
        Err(StoreError::Backend("access denied".into()))
    }

    fn delete(&self, _: &str) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("locked".into()))
    }

    fn check_available(&self) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("locked".into()))
    }
}

#[tokio::test]
async fn keychain_failures_leave_the_profile_untouched() {
    let s = setup();
    let good = s.secret_file("good.txt", &client_secret("5555"));
    let login_args = LoginArgs {
        client_id: Some(client_id("5555")),
        no_verify: true,
        ..args("locked")
    };
    let err = login(
        &FailingStore,
        &s.repo,
        None,
        &mut ScriptedUi::default(),
        &login_args,
        SecretSource::File(&good),
    )
    .await
    .unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("cannot save the API client credentials"),
        "{text}"
    );
    assert_clean(&text, "5555");
    assert!(!s.repo.path().exists());

    // Хранилище недоступно: статус показывает это, выход не трогает профиль.
    s.repo.upsert("locked", ProfileSettings::default()).unwrap();
    let mut ui = ScriptedUi::default();
    let err = status(
        &FailingStore,
        &s.repo,
        None,
        &mut ui,
        &status_args(None, false),
    )
    .await
    .unwrap_err();
    assert!(
        ui.text().contains("Keychain:     not available"),
        "{}",
        ui.text()
    );
    assert!(err.to_string().contains("not ready"), "{err}");
    let err = logout(&FailingStore, &s.repo, &mut ScriptedUi::default(), "locked").unwrap_err();
    assert!(
        format!("{err:#}").contains("the profile was kept"),
        "{err:#}"
    );
    assert!(s.repo.get("locked").unwrap().is_some());

    // Файл профилей не записывается — хранилище возвращается к прежнему состоянию.
    let previous = credentials("6666");
    s.store.set("locked", &previous).unwrap();
    let mut permissions = fs::metadata(s.repo.path()).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(s.repo.path(), permissions).unwrap();
    // root (CI в контейнере) пишет и в файл только для чтения: там сбой записи так не
    // воспроизвести, и эта часть проверки пропускается.
    if fs::OpenOptions::new()
        .write(true)
        .open(s.repo.path())
        .is_ok()
    {
        eprintln!("skipped: the read-only profiles file is still writable (running as root)");
        return;
    }
    for profile in ["locked", "fresh"] {
        let err = login(
            &s.store,
            &s.repo,
            None,
            &mut ScriptedUi::default(),
            &LoginArgs {
                client_id: Some(client_id("5555")),
                no_verify: true,
                ..args(profile)
            },
            SecretSource::File(&good),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("keychain was left unchanged"),
            "{err:#}"
        );
    }
    assert_eq!(s.store.get("locked").unwrap(), previous);
    assert!(!s.store.contains("fresh").unwrap());
}

#[tokio::test]
async fn status_of_all_profiles_and_logout_of_a_missing_one() {
    let s = setup();
    s.store.set("ready", &credentials("7777")).unwrap();
    s.repo
        .upsert(
            "ready",
            ProfileSettings {
                region: Region::De,
                allow_destructive: true,
                toolsets: vec!["apps".into(), "reviews".into()],
                ..ProfileSettings::default()
            },
        )
        .unwrap();
    s.repo.upsert("empty", ProfileSettings::default()).unwrap();

    let mut ui = ScriptedUi::default();
    let err = status(&s.store, &s.repo, None, &mut ui, &status_args(None, false))
        .await
        .unwrap_err();
    let out = ui.text();
    assert!(out.contains("Profile:      empty"), "{out}");
    assert!(out.contains("Profile:      ready"), "{out}");
    assert!(out.contains("Region:       de (Germany"), "{out}");
    assert!(out.contains("Write:        yes"), "{out}");
    assert!(out.contains("Delete:       yes"), "{out}");
    assert!(out.contains("Toolsets:     apps,reviews"), "{out}");
    assert_clean(&out, "7777");
    let text = err.to_string();
    assert!(text.contains("profile `empty`"), "{text}");
    assert!(!text.contains("profile `ready`"), "{text}");

    // Профиль не из файла показывается с настройками по умолчанию.
    s.store.set("loose", &credentials("8888")).unwrap();
    let mut ui = ScriptedUi::default();
    status(
        &s.store,
        &s.repo,
        None,
        &mut ui,
        &status_args(Some("loose"), false),
    )
    .await
    .unwrap();
    assert!(ui.text().contains("Settings:     none"), "{}", ui.text());

    // Выход из несуществующего профиля — не ошибка.
    let mut ui = ScriptedUi::default();
    logout(&s.store, &s.repo, &mut ui, "nobody").unwrap();
    let out = ui.text();
    assert!(out.contains("No API client credentials stored"), "{out}");
    assert!(out.contains("has no saved settings"), "{out}");
    assert!(logout(&s.store, &s.repo, &mut ScriptedUi::default(), "bad name").is_err());
    assert!(s.store.contains("ready").unwrap());
}
