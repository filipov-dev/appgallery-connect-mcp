//! Сценарии `auth login --oauth`, `auth status`, `auth logout --forget`, отказа `key exec` и
//! `serve` для профиля со входом пользователя: хранилище в памяти, файл профилей во
//! временном каталоге, HUAWEI ID и Connect API на `wiremock`, браузер — заглушка, которая
//! отправляет запрос на loopback-адрес 127.0.0.1. Значения выдуманы и собраны из частей.

use std::fs;
use std::time::Duration;

use appgallery_connect_api::{ApiClientCredentials, MemoryStore, Region};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::{LoginArgs, ProfileArg, StatusArgs, login as api_login, logout, status};
use super::*;
use crate::login::SecretSource;
use crate::oauth_login::testing::{BrowserReply, BrowserUi};
use crate::profiles::{ProfileKind, ProfileRepo, load_profile_with};
use crate::testing::{
    TEAM_LIST_PATH, client_secret, credentials, mount_user_sign_in, oauth_client_id, refresh_token,
    user_access_token,
};
use crate::ui::testing::ScriptedUi;
use crate::verify::Verifier;

const TAG: &str = "77";

fn teams() -> Value {
    json!([
        {"id": "5100", "name": "Home", "siteId": 1, "userType": 1},
        {"id": "5200", "name": "Client", "siteId": 8, "userType": 2},
    ])
}

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
        oauth: true,
        oauth_client_id: Some(oauth_client_id(TAG)),
        team_id: None,
        oauth_scopes: Vec::new(),
        redirect_uri: None,
        no_browser: false,
        secret_argument: None,
        stray: Vec::new(),
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    repo: ProfileRepo,
    store: MemoryStore,
    server: MockServer,
}

impl Fixture {
    async fn new(teams: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        mount_user_sign_in(&server, TAG, teams).await;
        Self {
            repo: ProfileRepo::in_dir(dir.path().join("config")),
            store: MemoryStore::new(),
            server,
            dir,
        }
    }

    fn verifier(&self) -> Verifier {
        Verifier::with_base_url(&self.server.uri())
    }

    fn secret_file(&self) -> std::path::PathBuf {
        let path = self.dir.path().join("oauth-secret.txt");
        fs::write(&path, format!("{}\n", client_secret(TAG))).unwrap();
        path
    }

    /// `auth login --oauth` с браузером-заглушкой; секрет — из файла.
    async fn login(&self, ui: &mut dyn Ui, args: &LoginArgs) -> anyhow::Result<()> {
        let file = self.secret_file();
        let env = SignInEnv {
            browser: BrowserDecision::Open,
            timeout: Duration::from_secs(20),
        };
        login(
            &self.store,
            &self.repo,
            &self.verifier(),
            ui,
            args,
            SecretSource::File(&file),
            env,
        )
        .await
    }
}

fn browser(answers: &[&str]) -> BrowserUi {
    BrowserUi::new(
        ScriptedUi::answering(answers, &[]),
        BrowserReply::Code("test-code".into()),
    )
}

/// Ни секрета OAuth-клиента, ни refresh и access token, ни кода в тексте.
fn assert_clean(text: &str) {
    for secret in [
        client_secret(TAG),
        refresh_token(TAG),
        user_access_token(),
        "test-code".to_owned(),
    ] {
        assert!(!text.contains(&secret), "{text}");
    }
}

#[tokio::test]
async fn login_lists_teams_asks_for_one_and_saves_the_sign_in() {
    let fx = Fixture::new(teams()).await;
    // У профиля был API-клиент: вход пользователя его заменяет.
    fx.store.set("guest", &credentials("1")).unwrap();
    let mut ui = browser(&["2"]);
    fx.login(&mut ui, &args("guest")).await.unwrap();

    let settings = fx.repo.get("guest").unwrap().unwrap();
    assert_eq!(settings.kind, ProfileKind::Oauth);
    assert_eq!(settings.team_id.as_deref(), Some("5200"));
    // Регион — по сайту команды (siteId 8).
    assert_eq!(settings.region, Region::Ru);
    let stored = fx.store.get_oauth("guest").unwrap();
    assert_eq!(stored.expose_refresh_token(), refresh_token(TAG));
    assert_eq!(stored.client().client_id().expose(), oauth_client_id(TAG));
    assert!(fx.store.get("guest").is_err());

    let out = ui.inner.text();
    assert!(out.contains("1. Home (5100, site cn)"), "{out}");
    assert!(out.contains("Team: 5200 (Client)"), "{out}");
    assert!(out.contains("guest/oauth"), "{out}");
    assert!(out.contains("API client credentials of profile `guest` were removed"));
    assert_eq!(ui.inner.questions, ["Team (number or ID): "]);
    assert_clean(&out);
    assert_clean(&fs::read_to_string(fx.repo.path()).unwrap());
    let opened = url::Url::parse(&ui.inner.opened[0]).unwrap();
    let scope = opened
        .query_pairs()
        .find(|(k, _)| k == "scope")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    assert!(scope.starts_with("openid "), "{scope}");
    // Браузер получил только ответ входа, чужие запросы вход не прервали.
    let statuses = ui.statuses.lock().unwrap().clone();
    assert_eq!(statuses.last().map(String::as_str), Some("HTTP/1.1 200 OK"));

    // Профиль загружается для `serve`: клиент API с командой, без API-клиента.
    let profile = load_profile_with(&fx.repo, &fx.store, "guest").unwrap();
    let config = profile.client_config();
    assert!(config.credentials.is_none());
    assert_eq!(
        config.oauth.as_ref().and_then(|s| s.team_id.as_deref()),
        Some("5200")
    );
}

#[tokio::test]
async fn team_flag_single_team_and_region_checks() {
    let fx = Fixture::new(teams()).await;
    // `--team-id` не из списка — ошибка, ничего не сохранено.
    let mut ui = browser(&[]);
    let err = fx
        .login(
            &mut ui,
            &LoginArgs {
                team_id: Some("9999".into()),
                ..args("guest")
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("team `9999` is not among"), "{err}");
    assert!(err.contains("5100 (Home)"), "{err}");
    assert!(fx.repo.get("guest").unwrap().is_none());
    assert!(fx.store.get_oauth("guest").is_err());

    // Регион расходится с сайтом команды.
    let err = fx
        .login(
            &mut browser(&[]),
            &LoginArgs {
                team_id: Some("5200".into()),
                region: Some(Region::Cn),
                ..args("guest")
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("site ru"), "{err}");
    assert!(fx.store.get_oauth("guest").is_err());

    // Несколько команд без терминала — нужна `--team-id`.
    let mut ui = BrowserUi::new(
        ScriptedUi::default(),
        BrowserReply::Code("test-code".into()),
    );
    let err = fx
        .login(&mut ui, &args("guest"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("pass --team-id"), "{err}");
    assert!(fx.store.get_oauth("guest").is_err());

    // Единственная команда выбирается без вопроса.
    let single = Fixture::new(json!([{"id": "5300", "name": "Only", "siteId": 7}])).await;
    let mut ui = BrowserUi::new(
        ScriptedUi::default(),
        BrowserReply::Code("test-code".into()),
    );
    single.login(&mut ui, &args("solo")).await.unwrap();
    let settings = single.repo.get("solo").unwrap().unwrap();
    assert_eq!(settings.team_id.as_deref(), Some("5300"));
    assert_eq!(settings.region, Region::De);
    assert!(ui.inner.questions.is_empty());
}

#[tokio::test]
async fn team_list_failure_needs_the_team_flag() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    // Список команд отклонён: монтируется раньше, поэтому совпадает первым.
    Mock::given(method("GET"))
        .and(path(TEAM_LIST_PATH))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"ret": {"code": 403, "msg": "denied"}})),
        )
        .mount(&server)
        .await;
    mount_user_sign_in(&server, TAG, teams()).await;
    let fx = Fixture {
        repo: ProfileRepo::in_dir(dir.path().join("config")),
        store: MemoryStore::new(),
        server,
        dir,
    };
    let err = fx
        .login(&mut browser(&[]), &args("guest"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("team list") && err.contains("--team-id"),
        "{err}"
    );
    assert!(fx.store.get_oauth("guest").is_err());

    let mut ui = browser(&[]);
    fx.login(
        &mut ui,
        &LoginArgs {
            team_id: Some("5100".into()),
            ..args("guest")
        },
    )
    .await
    .unwrap();
    assert!(ui.inner.text().contains("without checking it"));
    let settings = fx.repo.get("guest").unwrap().unwrap();
    assert_eq!(settings.team_id.as_deref(), Some("5100"));
}

#[tokio::test]
async fn expired_refresh_is_reported_and_nothing_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth2/v3/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": 1101,
            "sub_error": 12304,
            "error_description": "invalid client_secret",
        })))
        .mount(&server)
        .await;
    mount_user_sign_in(&server, TAG, teams()).await;
    let fx = Fixture {
        repo: ProfileRepo::in_dir(dir.path().join("config")),
        store: MemoryStore::new(),
        server,
        dir,
    };
    let err = fx
        .login(&mut browser(&[]), &args("guest"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("sub_error 12304"), "{err}");
    assert!(err.contains("Nothing was saved"), "{err}");
    assert_clean(&err);
    assert!(fx.repo.get("guest").unwrap().is_none());
}

#[tokio::test]
async fn status_logout_forget_and_switching_back() {
    let fx = Fixture::new(teams()).await;
    fx.login(&mut browser(&["1"]), &args("guest"))
        .await
        .unwrap();

    let mut ui = ScriptedUi::default();
    status(
        &fx.store,
        &fx.repo,
        Some(&fx.verifier()),
        &mut ui,
        &StatusArgs {
            profile: Some("guest".into()),
            check: true,
        },
    )
    .await
    .unwrap();
    let out = ui.text();
    assert!(
        out.contains("Sign-in:      user (OAuth client), team 5100"),
        "{out}"
    );
    assert!(
        out.contains("Scopes:       openid, publish, project"),
        "{out}"
    );
    assert!(out.contains("account `guest/oauth`"), "{out}");
    assert!(out.contains("Check:        ok"), "{out}");
    assert!(!out.contains(&oauth_client_id(TAG)), "{out}");
    assert_clean(&out);

    // Ключ этого профиля другим программам не отдаётся.
    let err = crate::cli::key::load_credentials(&fx.repo, &fx.store, "guest", None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("signs in as a HUAWEI ID user"), "{err}");

    // Отзыв не удался — ничего не удалено.
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth2/v3/revoke"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&failing)
        .await;
    let err = logout(
        &fx.store,
        &fx.repo,
        &Verifier::with_base_url(&failing.uri()),
        &mut ScriptedUi::default(),
        "guest",
        true,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("Nothing was removed"), "{err}");
    assert!(fx.store.get_oauth("guest").is_ok());
    assert!(fx.repo.get("guest").unwrap().is_some());

    Mock::given(method("POST"))
        .and(path("/oauth2/v3/revoke"))
        .and(body_string_contains(format!(
            "token={}",
            refresh_token(TAG)
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&fx.server)
        .await;
    let mut ui = ScriptedUi::default();
    logout(&fx.store, &fx.repo, &fx.verifier(), &mut ui, "guest", true)
        .await
        .unwrap();
    assert!(ui.text().contains("revoked at HUAWEI ID"), "{}", ui.text());
    assert!(fx.store.get_oauth("guest").is_err());
    assert!(fx.repo.get("guest").unwrap().is_none());
    assert_clean(&ui.text());

    // Вход пользователя, затем API-клиент: запись входа удаляется, тип профиля меняется.
    fx.login(&mut browser(&["1"]), &args("guest"))
        .await
        .unwrap();
    let file = fx.dir.path().join("api-secret.txt");
    fs::write(&file, client_secret("3")).unwrap();
    let mut ui = ScriptedUi::default();
    api_login(
        &fx.store,
        &fx.repo,
        None,
        &mut ui,
        &LoginArgs {
            oauth: false,
            oauth_client_id: None,
            client_id: Some(crate::testing::client_id("3")),
            no_verify: true,
            ..args("guest")
        },
        SecretSource::File(&file),
    )
    .await
    .unwrap();
    let settings = fx.repo.get("guest").unwrap().unwrap();
    assert_eq!(settings.kind, ProfileKind::ApiClient);
    assert_eq!(settings.team_id, None);
    assert!(fx.store.get_oauth("guest").is_err());
    let stored: ApiClientCredentials = fx.store.get("guest").unwrap();
    assert_eq!(stored, credentials("3"));
    assert!(
        ui.text()
            .contains("user sign-in of profile `guest` was removed")
    );

    // Без `--forget` вход удаляется, а пользователь получает подсказку об отзыве.
    fx.login(&mut browser(&["1"]), &args("other"))
        .await
        .unwrap();
    let mut ui = ScriptedUi::default();
    logout(&fx.store, &fx.repo, &fx.verifier(), &mut ui, "other", false)
        .await
        .unwrap();
    assert!(ui.text().contains("was not revoked"), "{}", ui.text());
    assert!(fx.store.get_oauth("other").is_err());
}
