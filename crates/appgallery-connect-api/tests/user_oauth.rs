//! Вход пользователя (режим OAuth-клиента) на `wiremock`: обмен кода с PKCE, обновление
//! и кэш access token, заголовки `oauth2Token`/`teamId`, операции только для API-клиента,
//! список команд, отзыв, вырезание секретов. Сеть и системное хранилище не используются;
//! значения выдуманы и собраны из частей.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use appgallery_connect_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, RetryPolicy,
};
use appgallery_connect_api::token::{ManualClock, REFRESH_MARGIN, TokenError, TokenSource};
use appgallery_connect_api::user_oauth::{
    self, OAuthClient, OAuthCredentials, OAuthEndpoints, OAuthSession, Pkce, UserTokens,
};
use appgallery_connect_api::{ClientId, ClientSecret, HttpMethod, Region, spec};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN_PATH: &str = "/oauth2/v3/token";
const REVOKE_PATH: &str = "/oauth2/v3/revoke";
const START: u64 = 1_800_000_000;

fn client_id() -> String {
    ["1088", "0001", "77"].concat()
}

fn client_secret() -> String {
    ["9f3A", "c21B", "77dE", "0a4F"].concat().repeat(4)
}

fn refresh_token(n: usize) -> String {
    [
        "DQ",
        "Ej",
        "rT",
        "-refresh-",
        &n.to_string(),
        "-0123456789abcdef",
    ]
    .concat()
}

fn access_token(n: usize) -> String {
    [
        "CV",
        "user",
        "-access-",
        &n.to_string(),
        "-0123456789abcdef",
    ]
    .concat()
}

fn team() -> String {
    ["7700", "1234", "56"].concat()
}

fn oauth_client(secret: bool) -> OAuthClient {
    OAuthClient::new(
        ClientId::new(client_id()).unwrap(),
        secret.then(|| ClientSecret::new(client_secret()).unwrap()),
    )
}

fn sign_in(secret: bool) -> OAuthCredentials {
    OAuthCredentials::new(
        oauth_client(secret),
        &refresh_token(0),
        user_oauth::requested_scopes(&[]).unwrap(),
    )
    .unwrap()
}

fn endpoints(server: &MockServer) -> OAuthEndpoints {
    OAuthEndpoints::huawei()
        .with_token_url(&format!("{}{TOKEN_PATH}", server.uri()))
        .unwrap()
        .with_revoke_url(&format!("{}{REVOKE_PATH}", server.uri()))
        .unwrap()
}

fn session(server: &MockServer, team_id: Option<String>) -> OAuthSession {
    OAuthSession {
        endpoints: endpoints(server),
        ..OAuthSession::new(sign_in(true), team_id)
    }
}

fn config(server: &MockServer, session: OAuthSession) -> ClientConfig {
    let mut config = ClientConfig::with_oauth(Region::Ru, session);
    config.base_url = Some(server.uri());
    config.retry = RetryPolicy {
        max_retries: 1,
        default_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(100),
    };
    config
}

fn form(request: &Request) -> HashMap<String, String> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

/// Сервер входа: каждое обновление выдаёт следующий access token; запросы запоминаются.
struct TokenEndpoint {
    calls: Arc<AtomicUsize>,
    forms: Arc<Mutex<Vec<HashMap<String, String>>>>,
    delay: Duration,
    /// Выдавать ли новый refresh token при обновлении.
    rotate: bool,
}

impl Respond for TokenEndpoint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.forms.lock().unwrap().push(form(request));
        let mut body = json!({
            "access_token": access_token(n),
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": "openid https://www.huawei.com/auth/agc/publish",
        });
        if self.rotate {
            body["refresh_token"] = json!(refresh_token(n));
        }
        ResponseTemplate::new(200)
            .set_delay(self.delay)
            .set_body_json(body)
    }
}

struct Mounted {
    calls: Arc<AtomicUsize>,
    forms: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

async fn mount_tokens(server: &MockServer, delay: Duration, rotate: bool) -> Mounted {
    let calls = Arc::new(AtomicUsize::new(0));
    let forms = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .respond_with(TokenEndpoint {
            calls: calls.clone(),
            forms: forms.clone(),
            delay,
            rotate,
        })
        .mount(server)
        .await;
    Mounted { calls, forms }
}

fn ok_body() -> Value {
    json!({"ret": {"code": 0, "msg": "success"}, "appInfo": {"releaseState": 0}})
}

fn no_api_client_headers(request: &Request) -> bool {
    !request.headers.contains_key("authorization") && !request.headers.contains_key("client_id")
}

fn assert_clean(text: &str) {
    for secret in [
        client_secret(),
        refresh_token(0),
        refresh_token(1),
        access_token(1),
        access_token(2),
    ] {
        assert!(!text.contains(&secret), "{text}");
    }
}

// ---------- обмен кода ----------

#[tokio::test]
async fn code_exchange_sends_pkce_and_keeps_the_refresh_token() {
    let server = MockServer::start().await;
    let forms = Arc::new(Mutex::new(Vec::new()));
    let seen = forms.clone();
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(move |request: &Request| {
            seen.lock().unwrap().push(form(request));
            ResponseTemplate::new(200).set_body_json(json!({
                "access_token": access_token(1),
                "expires_in": 3600,
                "refresh_token": refresh_token(1),
                "scope": "openid https://www.huawei.com/auth/agc/publish",
                "token_type": "Bearer",
                "id_token": "header.payload.signature",
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let client = oauth_client(true);
    let pkce = Pkce::generate().unwrap();
    let state = user_oauth::random_state().unwrap();
    let scopes = user_oauth::requested_scopes(&[]).unwrap();
    let redirect = "http://127.0.0.1:45678/";
    let url = user_oauth::authorization_url(
        &client,
        &endpoints(&server),
        redirect,
        &scopes,
        &pkce,
        &state,
    )
    .unwrap();
    let challenge = url
        .query_pairs()
        .find(|(key, _)| key == "code_challenge")
        .map(|(_, value)| value.into_owned())
        .unwrap();

    let code = ["auth", "-code-", "42"].concat();
    let grant = user_oauth::exchange_code(
        &client,
        &endpoints(&server),
        &code,
        &pkce,
        redirect,
        &scopes,
    )
    .await
    .unwrap();
    assert_eq!(grant.token.expose(), access_token(1));
    assert_eq!(grant.expires_in, Duration::from_secs(3600));
    assert_eq!(grant.credentials.expose_refresh_token(), refresh_token(1));
    // Области — выданные сервером, а не запрошенные.
    assert_eq!(
        grant.credentials.scopes(),
        ["openid", "https://www.huawei.com/auth/agc/publish"]
    );
    assert_eq!(
        grant.credentials.missing_scopes(&scopes),
        ["https://www.huawei.com/auth/agc/project"]
    );

    let forms = forms.lock().unwrap();
    let sent = &forms[0];
    assert_eq!(sent["grant_type"], "authorization_code");
    assert_eq!(sent["code"], code);
    assert_eq!(sent["client_id"], client_id());
    assert_eq!(sent["client_secret"], client_secret());
    assert_eq!(sent["redirect_uri"], redirect);
    // Сервер входа может проверить PKCE: challenge из адреса — SHA-256 от verifier.
    let verifier = &sent["code_verifier"];
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        challenge
    );
    assert!(!url.as_str().contains(verifier.as_str()));
    assert!(!format!("{grant:?}").contains(&refresh_token(1)));
}

#[tokio::test]
async fn code_exchange_without_secret_or_refresh_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(|request: &Request| !form(request).contains_key("client_secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": access_token(1), "expires_in": 3600})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let err = user_oauth::exchange_code(
        &oauth_client(false),
        &endpoints(&server),
        "code",
        &Pkce::generate().unwrap(),
        "http://127.0.0.1:1/",
        &[],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("no refresh token"), "{err}");
    assert!(!err.contains(&access_token(1)), "{err}");
}

#[tokio::test]
async fn code_exchange_errors_explain_huawei_codes_without_secrets() {
    let server = MockServer::start().await;
    let code = ["leaked", "-code-", "77"].concat();
    let echoed = format!(
        "redirect_uri mismatch for {code} and {} and {}",
        client_secret(),
        refresh_token(0)
    );
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": 1102,
            "sub_error": 20023,
            "error_description": echoed,
        })))
        .mount(&server)
        .await;
    let pkce = Pkce::generate().unwrap();
    let err = user_oauth::exchange_code(
        &oauth_client(true),
        &endpoints(&server),
        &code,
        &pkce,
        "http://127.0.0.1:1/",
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, TokenError::SignIn(_)), "{err:?}");
    let text = err.to_string();
    assert!(
        text.contains("HTTP 400, error 1102, sub_error 20023"),
        "{text}"
    );
    assert!(text.contains("--redirect-uri"), "{text}");
    assert!(!text.contains(&code), "{text}");
    assert!(!text.contains(pkce.expose_verifier()), "{text}");
    assert!(!text.contains(&client_secret()), "{text}");
}

// ---------- токены и заголовки ----------

#[tokio::test]
async fn requests_carry_oauth2_token_and_team_id_only() {
    let server = MockServer::start().await;
    let tokens = mount_tokens(&server, Duration::ZERO, false).await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(header("oauth2Token", access_token(1).as_str()))
        .and(header("teamId", team().as_str()))
        .and(no_api_client_headers)
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(2)
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server, session(&server, Some(team())))).unwrap();
    let op = spec::find("get_app_info").unwrap();
    for _ in 0..2 {
        client.call(op, &json!({"appId": "100"})).await.unwrap();
    }
    // Токен получен один раз и взят из памяти для второго запроса.
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 1);
    let sent = tokens.forms.lock().unwrap()[0].clone();
    assert_eq!(sent["grant_type"], "refresh_token");
    assert_eq!(sent["refresh_token"], refresh_token(0));
    assert_eq!(sent["client_id"], client_id());
    assert_eq!(sent["client_secret"], client_secret());

    // Аргументы и `api_request` не задают служебные заголовки этого режима.
    for name in ["teamId", "oauth2Token", "Authorization", "client_id"] {
        let request =
            ApiRequest::new(HttpMethod::Get, "/api/publish/v2/app-info").with_header(name, "other");
        let err = client.execute(request).await.unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(_)),
            "{name}: {err:?}"
        );
    }
}

#[tokio::test]
async fn user_token_is_refreshed_ahead_of_expiry_and_rotated() {
    let server = MockServer::start().await;
    let tokens = mount_tokens(&server, Duration::ZERO, true).await;
    let saved = Arc::new(Mutex::new(Vec::new()));
    let sink_saved = saved.clone();
    let mut session = session(&server, Some(team()));
    session.sink = Some(Arc::new(move |credentials: &OAuthCredentials| {
        sink_saved
            .lock()
            .unwrap()
            .push(credentials.expose_refresh_token().to_owned());
        Ok(())
    }));
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_secs(START)));
    let source = UserTokens::new(&session).unwrap().with_clock(clock.clone());

    assert_eq!(source.token().await.unwrap().expose(), access_token(1));
    clock.advance(Duration::from_secs(3600) - REFRESH_MARGIN - Duration::from_secs(1));
    assert_eq!(source.token().await.unwrap().expose(), access_token(1));
    clock.advance(Duration::from_secs(2));
    assert_eq!(source.token().await.unwrap().expose(), access_token(2));
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 2);

    // Сервер выдал новый refresh token: следующее обновление идёт с ним, он сохранён.
    let forms = tokens.forms.lock().unwrap();
    assert_eq!(forms[0]["refresh_token"], refresh_token(0));
    assert_eq!(forms[1]["refresh_token"], refresh_token(1));
    assert_eq!(*saved.lock().unwrap(), [refresh_token(1), refresh_token(2)]);
    assert_eq!(
        source.credentials().expose_refresh_token(),
        refresh_token(2)
    );

    // Все токены и секреты известны для вырезания; `Debug` их не раскрывает.
    let known: Vec<String> = source
        .known_tokens()
        .iter()
        .map(|token| token.expose().to_owned())
        .collect();
    for value in [
        access_token(1),
        access_token(2),
        refresh_token(0),
        refresh_token(1),
        refresh_token(2),
        client_secret(),
    ] {
        assert!(known.contains(&value), "{value}");
    }
    assert_clean(&format!("{source:?}"));
}

#[tokio::test]
async fn parallel_requests_wait_for_one_user_token_refresh() {
    let server = MockServer::start().await;
    let tokens = mount_tokens(&server, Duration::from_millis(150), false).await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(header("oauth2Token", access_token(1).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(6)
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server, session(&server, Some(team())))).unwrap();
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            client
                .execute(ApiRequest::new(HttpMethod::Get, "/api/publish/v2/app-info"))
                .await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn user_token_is_renewed_once_after_401() {
    let server = MockServer::start().await;
    let tokens = mount_tokens(&server, Duration::ZERO, false).await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(header("oauth2Token", access_token(1).as_str()))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(header("oauth2Token", access_token(2).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(1)
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server, session(&server, Some(team())))).unwrap();
    client
        .execute(ApiRequest::new(HttpMethod::Get, "/api/publish/v2/app-info"))
        .await
        .unwrap();
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn expired_sign_in_asks_to_sign_in_again_without_secrets() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": 1203,
            "sub_error": 31204,
            "error_description": format!("token {} expired", refresh_token(0)),
        })))
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server, session(&server, Some(team())))).unwrap();
    let err = client.authenticate().await.unwrap_err();
    assert!(
        matches!(err, ApiError::Token(TokenError::SignIn(_))),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("sub_error 31204"), "{text}");
    assert!(text.contains("auth login --oauth"), "{text}");
    assert_clean(&text);
}

#[tokio::test]
async fn api_errors_and_responses_are_scrubbed_of_user_secrets() {
    let server = MockServer::start().await;
    mount_tokens(&server, Duration::ZERO, false).await;
    let echoed = format!(
        "bad oauth2Token {} for {} and {}",
        access_token(1),
        refresh_token(0),
        client_secret()
    );
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .respond_with(ResponseTemplate::new(403).set_body_string(echoed.clone()))
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server, session(&server, Some(team())))).unwrap();
    let err = client
        .execute(ApiRequest::new(HttpMethod::Get, "/api/publish/v2/app-info"))
        .await
        .unwrap_err();
    assert_clean(&err.to_string());
    let scrubbed = client.scrub(&echoed);
    assert_clean(&scrubbed);
    // Client ID OAuth-клиента — идентификатор приложения: он остаётся.
    assert_eq!(client.scrub(&client_id()), client_id());
    assert_clean(&format!("{client:?}"));
}

// ---------- режимы авторизации ----------

#[tokio::test]
async fn api_client_only_operations_are_refused_before_sending() {
    let server = MockServer::start().await;
    let tokens = mount_tokens(&server, Duration::ZERO, false).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(0)
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server, session(&server, Some(team())))).unwrap();
    let op = spec::find("list_reviews").unwrap();
    let args = json!({
        "appId": "100",
        "beginTime": 1,
        "endTime": 2,
        "countries": "RU",
    });
    let err = client.call(op, &args).await.unwrap_err();
    let ApiError::UnsupportedAuthMode { operation, modes } = &err else {
        panic!("{err:?}");
    };
    assert_eq!(operation, "list_reviews");
    assert_eq!(modes, "API client");
    let text = err.to_string();
    assert!(text.contains("signed-in user"), "{text}");
    assert!(text.contains("auth login --profile"), "{text}");

    // `api_request` к пути той же операции: режимы переносит `select_project`.
    let built = ApiRequest::for_operation(op, &args).unwrap();
    let mut request = ApiRequest::new(built.method, built.path);
    assert!(request.auth_modes.is_empty());
    request.select_project(op).unwrap();
    assert!(matches!(
        client.execute(request).await,
        Err(ApiError::UnsupportedAuthMode { .. })
    ));
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 0);
}

// ---------- команды ----------

#[tokio::test]
async fn team_list_needs_only_the_user_token() {
    let server = MockServer::start().await;
    mount_tokens(&server, Duration::ZERO, false).await;
    Mock::given(method("GET"))
        .and(path("/api/ups/user-permission-service/v1/user-team-list"))
        .and(header("oauth2Token", access_token(1).as_str()))
        .and(|request: &Request| !request.headers.contains_key("teamid"))
        .and(no_api_client_headers)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "teams": [
                {"id": team(), "name": "Main", "siteId": 8, "userType": 2},
                {"id": "200", "name": "Other", "siteId": 7, "userType": 1},
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server, session(&server, None))).unwrap();
    let teams = user_oauth::list_teams(&client).await.unwrap();
    assert_eq!(teams.len(), 2);
    assert_eq!(teams[0].id, team());
    assert_eq!(teams[0].site, Some(Region::Ru));
    assert_eq!(teams[1].site, Some(Region::De));
}

#[test]
fn both_api_client_and_user_sign_in_are_refused() {
    let mut config =
        ClientConfig::with_oauth(Region::Cn, OAuthSession::new(sign_in(false), Some(team())));
    config.credentials = Some(
        appgallery_connect_api::ApiClientCredentials::from_parts(client_id(), client_secret())
            .unwrap(),
    );
    assert!(matches!(
        ApiClient::new(config),
        Err(ApiError::InvalidConfig(_))
    ));
    let bad_team = ClientConfig::with_oauth(
        Region::Cn,
        OAuthSession::new(sign_in(false), Some("bad team".into())),
    );
    assert!(ApiClient::new(bad_team).is_err());
}

// ---------- отзыв ----------

#[tokio::test]
async fn revoke_sends_the_refresh_token_and_reports_failures_without_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .and(|request: &Request| form(request).get("token") == Some(&refresh_token(0)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    user_oauth::revoke(&sign_in(true), &endpoints(&server))
        .await
        .unwrap();

    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": 1101,
            "sub_error": 20001,
            "error_description": format!("bad token {}", refresh_token(0)),
        })))
        .mount(&failing)
        .await;
    let text = user_oauth::revoke(&sign_in(true), &endpoints(&failing))
        .await
        .unwrap_err()
        .to_string();
    assert!(text.contains("revoking the sign-in"), "{text}");
    assert!(text.contains("Control account access"), "{text}");
    assert_clean(&text);
}

// ---------- клиенты проектов при входе пользователя ----------

/// Remote Configuration подписывает API-клиент проекта и при входе пользователя: справочник
/// не описывает её для OAuth-клиента, но запрос идёт не от имени пользователя. Без клиента
/// проекта — `MissingProjectCredentials` с командой `auth project add`, без запроса.
#[tokio::test]
async fn project_operations_use_the_project_client_with_a_user_sign_in() {
    let server = MockServer::start().await;
    let user_tokens = mount_tokens(&server, Duration::ZERO, false).await;
    let project_id = ["9900", "0001"].concat();
    let project_client = ["5500", "1234", "77"].concat();
    let project_secret = ["AB12", "CD34", "EF56", "7890"].concat().repeat(4);
    let project_token = ["project", "-token-", "for-test"].concat();
    Mock::given(method("POST"))
        .and(path("/api/oauth2/v1/token"))
        .and(wiremock::matchers::body_partial_json(json!({
            "client_id": project_client,
            "client_secret": project_secret,
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": project_token, "expires_in": 172_800})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let bearer = format!("Bearer {project_token}");
    Mock::given(method("GET"))
        .and(path("/api/remote-config/v1/config"))
        .and(header("client_id", project_client.as_str()))
        .and(header("authorization", bearer.as_str()))
        .and(header("productId", project_id.as_str()))
        .and(|request: &Request| {
            !request.headers.contains_key("oauth2token") && !request.headers.contains_key("teamid")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "configItems": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut with_project = config(&server, session(&server, Some(team())));
    with_project.profile = Some("guest".into());
    let without_project = with_project.clone();
    with_project.projects = vec![
        appgallery_connect_api::ProjectCredentials::new(
            &project_id,
            appgallery_connect_api::ApiClientCredentials::from_parts(
                project_client.clone(),
                project_secret.clone(),
            )
            .unwrap(),
        )
        .unwrap(),
    ];
    let op = spec::find("get_remote_config").unwrap();
    let args = json!({"productId": project_id, "appId": "100"});
    let client = ApiClient::new(with_project).unwrap();
    client.call(op, &args).await.unwrap();
    // Вход пользователя для этой операции не нужен.
    assert_eq!(user_tokens.calls.load(Ordering::SeqCst), 0);

    let client = ApiClient::new(without_project).unwrap();
    let err = client.call(op, &args).await.unwrap_err();
    assert!(
        matches!(err, ApiError::MissingProjectCredentials { .. }),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("auth project add --profile guest"), "{text}");
    assert!(text.contains("user sign-in"), "{text}");
}
