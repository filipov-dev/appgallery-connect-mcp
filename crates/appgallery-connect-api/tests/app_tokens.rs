//! Тесты подписи запросов HMS IAP токеном приложения на `wiremock`: получение токена у
//! Huawei OAuth, кэш, обновление после 401, заголовок `Basic APPAT`, корневой конверт
//! `responseCode`, вырезание секрета и токена. Секреты выдуманы и собраны из частей.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use appgallery_connect_api::envelope::RESULT_RESPONSE_CODE;
use appgallery_connect_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, RetryPolicy,
};
use appgallery_connect_api::token::TokenError;
use appgallery_connect_api::{
    AccessLevel, ApiClientCredentials, ApiService, AppCredentials, BodyKind, HttpMethod,
    OperationDef, ParamDef, ParamLocation, ParamType, Region, ResponseKind,
};
use base64::Engine as _;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const OAUTH_PATH: &str = "/oauth2/v3/token";
const CONFIRM_PATH: &str = "/applications/v2/purchases/confirm";
const APP_ID: &str = "100000001";

fn app_secret() -> String {
    ["9f1e", "77ab", "0c3d", "52e8"].concat().repeat(4)
}

fn app_token(n: usize) -> String {
    ["app", "level-", "tok", "en-", &n.to_string(), "-CgB6e3x9"].concat()
}

fn basic(n: usize) -> String {
    let raw = format!("APPAT:{}", app_token(n));
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

fn app() -> AppCredentials {
    AppCredentials::new(APP_ID, None, app_secret()).unwrap()
}

fn team_credentials() -> ApiClientCredentials {
    ApiClientCredentials::from_parts(
        ["1088", "9300", "8395"].concat(),
        ["B15B", "497B", "44E0"].concat().repeat(3),
    )
    .unwrap()
}

fn config(server: &MockServer) -> ClientConfig {
    let mut config = ClientConfig::new(Region::Ru, Some(team_credentials()));
    config.base_url = Some(server.uri());
    config.retry = RetryPolicy {
        max_retries: 1,
        default_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(200),
    };
    config.apps = vec![app()];
    config.profile = Some("work".into());
    config
}

/// Huawei OAuth: каждый вызов выдаёт следующий токен.
struct OAuthEndpoint {
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

impl Respond for OAuthEndpoint {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        ResponseTemplate::new(200)
            .set_delay(self.delay)
            .set_body_json(
                json!({"access_token": app_token(n), "expires_in": 3600, "token_type": "Bearer"}),
            )
    }
}

async fn mount_oauth(server: &MockServer, delay: Duration) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path(OAUTH_PATH))
        .and(header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        ))
        .and(body_string_contains("grant_type=client_credentials"))
        .and(body_string_contains(format!("client_id={APP_ID}").as_str()))
        .and(body_string_contains(
            format!("client_secret={}", app_secret()).as_str(),
        ))
        .respond_with(OAuthEndpoint {
            calls: calls.clone(),
            delay,
        })
        .mount(server)
        .await;
    calls
}

static CONFIRM: OperationDef = OperationDef {
    tool_name: "confirm_product_purchase",
    operation_id: "orders.purchase.confirm",
    group: "orders",
    toolset: "purchases",
    summary: "",
    description: "",
    method: HttpMethod::Post,
    path: CONFIRM_PATH,
    access: AccessLevel::Write,
    params: &[
        ParamDef {
            name: "appId",
            api_name: "appId",
            location: ParamLocation::Credential,
            required: true,
            description: "",
            value_type: ParamType::String,
            repeated: false,
        },
        ParamDef {
            name: "body",
            api_name: "body",
            location: ParamLocation::Body,
            required: true,
            description: "",
            value_type: ParamType::Object,
            repeated: false,
        },
    ],
    body: Some(BodyKind::Json),
    response: ResponseKind::Json,
    result: Some(RESULT_RESPONSE_CODE),
    reference: "",
    input_schema: "{}",
    secrets: &[],
    project_param: None,
    auth_modes: &[],
    service: ApiService::IapOrders,
    json_strings: &[],
    omitted: &[],
};

fn args() -> Value {
    json!({"appId": APP_ID, "body": {"purchaseToken": "pt-000.1", "productId": "gems_10"}})
}

#[tokio::test]
async fn iap_request_uses_app_token_basic_header_and_root_envelope() {
    let server = MockServer::start().await;
    let calls = mount_oauth(&server, Duration::ZERO).await;
    Mock::given(method("POST"))
        .and(path(CONFIRM_PATH))
        .and(header("authorization", basic(1).as_str()))
        .and(body_json(
            json!({"purchaseToken": "pt-000.1", "productId": "gems_10"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"responseCode": "0", "responseMessage": "success"})),
        )
        .expect(2)
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    assert_eq!(client.app_ids(), [APP_ID]);
    for _ in 0..2 {
        let response = client.call(&CONFIRM, &args()).await.unwrap();
        assert!(response.result.unwrap().is_success());
    }
    // Токен кэшируется: второй запрос его не получает.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // В запросе IAP нет `client_id` API-клиента.
    let requests = server.received_requests().await.unwrap();
    let iap: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == CONFIRM_PATH)
        .collect();
    assert_eq!(iap.len(), 2);
    assert!(
        iap.iter()
            .all(|request| !request.headers.contains_key("client_id"))
    );
}

#[tokio::test]
async fn nonzero_response_code_with_http_200_is_an_error() {
    let server = MockServer::start().await;
    mount_oauth(&server, Duration::ZERO).await;
    Mock::given(method("POST"))
        .and(path(CONFIRM_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"responseCode": "6", "responseMessage": "The purchase token is invalid."}),
        ))
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    let err = client.call(&CONFIRM, &args()).await.unwrap_err();
    assert!(matches!(err, ApiError::Result(_)), "{err:?}");
    assert_eq!(err.result_code(), Some("6"));
    let text = err.to_string();
    assert!(text.contains("(HTTP 200, responseCode)"), "{text}");
    assert!(text.contains("purchase token is invalid"), "{text}");
}

#[tokio::test]
async fn http_errors_read_the_root_envelope() {
    let server = MockServer::start().await;
    mount_oauth(&server, Duration::ZERO).await;
    Mock::given(method("POST"))
        .and(path(CONFIRM_PATH))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(
                json!({"responseCode": "5", "responseMessage": "IAP is not enabled"}),
            ),
        )
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    let err = client.call(&CONFIRM, &args()).await.unwrap_err();
    let http = err.http().unwrap();
    assert_eq!(http.status, 403);
    assert_eq!(http.code.as_deref(), Some("5"));
}

#[tokio::test]
async fn rejected_token_is_renewed_once_after_401() {
    let server = MockServer::start().await;
    let calls = mount_oauth(&server, Duration::ZERO).await;
    Mock::given(method("POST"))
        .and(path(CONFIRM_PATH))
        .and(header("authorization", basic(1).as_str()))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(CONFIRM_PATH))
        .and(header("authorization", basic(2).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": "0"})))
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    client.call(&CONFIRM, &args()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn parallel_requests_share_one_token_request() {
    let server = MockServer::start().await;
    let calls = mount_oauth(&server, Duration::from_millis(100)).await;
    Mock::given(method("POST"))
        .and(path(CONFIRM_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": "0"})))
        .mount(&server)
        .await;
    let client = Arc::new(ApiClient::new(config(&server)).unwrap());
    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.call(&CONFIRM, &args()).await })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_app_secret_sends_nothing_and_names_the_command() {
    let server = MockServer::start().await;
    let mut config = config(&server);
    config.apps.clear();
    let client = ApiClient::new(config).unwrap();
    let err = client.call(&CONFIRM, &args()).await.unwrap_err();
    assert!(
        matches!(&err, ApiError::MissingAppCredentials { app_id, .. } if app_id == APP_ID),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("auth app-secret set --profile work --app 100000001"),
        "{text}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());

    // Без `appId` и с неверным `appId` запрос не собирается.
    let err = ApiRequest::for_operation(&CONFIRM, &json!({"body": {}})).unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    let err =
        ApiRequest::for_operation(&CONFIRM, &json!({"appId": "app-1", "body": {}})).unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
}

#[tokio::test]
async fn oauth_rejection_names_the_app_and_hides_the_secret() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(OAUTH_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": 1101,
            "sub_error": 20152,
            "error_description": format!("invalid client secret {}", app_secret()),
        })))
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    let err = client.authenticate_app(APP_ID).await.unwrap_err();
    assert!(
        matches!(&err, ApiError::Token(TokenError::AppRejected { app_id, status: 400, .. }) if app_id == APP_ID),
        "{err:?}"
    );
    assert_eq!(err.result_code(), Some("1101/20152"));
    let text = err.to_string();
    assert!(!text.contains(&app_secret()), "{text}");
    assert!(text.contains("App information"), "{text}");
    assert!(!format!("{err:?}").contains(&app_secret()));
    // Неизвестное приложение — без запроса.
    let err = client.authenticate_app("200000002").await.unwrap_err();
    assert!(matches!(err, ApiError::MissingAppCredentials { .. }));
}

#[tokio::test]
async fn reflected_secret_and_tokens_are_scrubbed() {
    let server = MockServer::start().await;
    mount_oauth(&server, Duration::ZERO).await;
    let reflected = format!(
        "secret={} token={} header={}",
        app_secret(),
        app_token(1),
        basic(1)
    );
    Mock::given(method("POST"))
        .and(path(CONFIRM_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "responseCode": "1",
            "responseMessage": reflected,
        })))
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    let err = client.call(&CONFIRM, &args()).await.unwrap_err();
    let text = client.scrub(&format!("{err} {err:?}"));
    let encoded = basic(1).trim_start_matches("Basic ").to_owned();
    for value in [app_secret(), app_token(1), encoded] {
        assert!(!text.contains(&value), "{text}");
    }
    let text = err.to_string();
    assert!(!text.contains(&app_secret()), "{text}");
    assert!(!text.contains(&app_token(1)), "{text}");
    // Debug клиента и конфигурации не раскрывает секрет.
    assert!(!format!("{client:?}").contains(&app_secret()));
}

#[tokio::test]
async fn arbitrary_requests_stay_on_the_connect_api() {
    let server = MockServer::start().await;
    let client = ApiClient::new(config(&server)).unwrap();
    // `api_request` собирает запрос через `ApiRequest::new`: путь IAP не проходит проверку
    // пути Connect API, и токен приложения к нему не применяется.
    let err = client
        .execute(ApiRequest::new(HttpMethod::Post, CONFIRM_PATH))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::InvalidPath(_)), "{err:?}");
    // Заголовок авторизации нельзя задать и у запроса IAP.
    let mut request = ApiRequest::for_operation(&CONFIRM, &args()).unwrap();
    request
        .headers
        .push(("Authorization".into(), "Basic x".into()));
    let err = client.execute(request).await.unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    // Сервис IAP без приложения не подписывается клиентом команды.
    let mut request = ApiRequest::new(HttpMethod::Post, CONFIRM_PATH);
    request.service = ApiService::IapOrders;
    let err = client.execute(request).await.unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn two_secrets_of_one_app_are_rejected() {
    let server = MockServer::start().await;
    let mut config = config(&server);
    config.apps.push(app());
    assert!(matches!(
        ApiClient::new(config),
        Err(ApiError::InvalidConfig(_))
    ));
}

/// Профиль входа пользователя (OAuth-клиент): операции IAP подписывает токен приложения,
/// а не access token пользователя; проверка режима входа их не касается.
#[tokio::test]
async fn iap_works_with_a_user_sign_in_profile() {
    use appgallery_connect_api::credentials::{ClientId, ClientSecret};
    use appgallery_connect_api::{OAuthClient, OAuthCredentials, OAuthEndpoints, OAuthSession};

    let server = MockServer::start().await;
    mount_oauth(&server, Duration::ZERO).await;
    // Токен пользователя запрашиваться не должен.
    Mock::given(method("POST"))
        .and(path("/user-token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(CONFIRM_PATH))
        .and(header("authorization", basic(1).as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"responseCode": "0", "responseMessage": "success"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let sign_in = OAuthCredentials::new(
        OAuthClient::new(
            ClientId::new(["1088", "0000", "77"].concat()).unwrap(),
            Some(ClientSecret::new(["c5e1", "9a0b"].concat().repeat(8)).unwrap()),
        ),
        &["Rt", "-", "9"].concat().repeat(6),
        vec!["openid".into()],
    )
    .unwrap();
    let session = OAuthSession {
        endpoints: OAuthEndpoints::huawei()
            .with_token_url(&format!("{}/user-token", server.uri()))
            .unwrap(),
        ..OAuthSession::new(sign_in, Some(["7700", "1234", "56"].concat()))
    };
    let mut user = ClientConfig::with_oauth(Region::Ru, session);
    let team = config(&server);
    user.base_url = team.base_url;
    user.retry = team.retry;
    user.apps = team.apps;
    user.profile = team.profile;
    let client = ApiClient::new(user).unwrap();
    let response = client.call(&CONFIRM, &args()).await.unwrap();
    assert!(response.result.unwrap().is_success());
    let requests = server.received_requests().await.unwrap();
    let iap = requests
        .iter()
        .find(|request| request.url.path() == CONFIRM_PATH)
        .unwrap();
    assert!(!iap.headers.contains_key("oauth2token"));
    assert!(!iap.headers.contains_key("teamid"));
}
