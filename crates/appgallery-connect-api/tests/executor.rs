//! Тесты исполнителя запросов и токенов API-клиента на `wiremock`, без реальной сети и
//! системного хранилища. Учётные данные выдуманы и собраны из частей.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use appgallery_connect_api::envelope::{RESULT_ERROR_CODE, RESULT_RET_CODE, RESULT_RET_RTN_CODE};
use appgallery_connect_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, ResponseBody, ResultCheck, RetryPolicy,
};
use appgallery_connect_api::token::{
    ApiClientTokens, ManualClock, REFRESH_MARGIN, StaticToken, TokenError, TokenSource,
};
use appgallery_connect_api::{
    AccessLevel, ApiClientCredentials, BodyKind, HttpMethod, OperationDef, ParamDef, ParamLocation,
    ParamType, Region, ResponseKind, ResultDef,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN_PATH: &str = "/api/oauth2/v1/token";
/// Начало отсчёта управляемых часов.
const START: u64 = 1_800_000_000;

fn client_id() -> String {
    ["1088", "9300", "8395", "7248"].concat()
}

fn client_secret() -> String {
    ["B15B", "497B", "44E0", "80EB", "E2C4", "DE4E"]
        .concat()
        .repeat(2)
}

fn credentials() -> ApiClientCredentials {
    ApiClientCredentials::from_parts(client_id(), client_secret()).unwrap()
}

fn token(n: usize) -> String {
    ["access", "-token-", "value-", &n.to_string(), "-0123456789"].concat()
}

fn retry() -> RetryPolicy {
    RetryPolicy {
        max_retries: 2,
        default_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(500),
    }
}

fn config(server: &MockServer) -> ClientConfig {
    let mut config = ClientConfig::new(Region::Ru, Some(credentials()));
    config.base_url = Some(server.uri());
    config.retry = retry();
    config
}

/// Клиент с заранее известным токеном: метод получения токена не вызывается.
fn static_client(server: &MockServer) -> ApiClient {
    ApiClient::with_token_source(config(server), Arc::new(StaticToken::new(token(0)))).unwrap()
}

/// Метод получения токена: каждый вызов выдаёт следующий токен.
struct TokenEndpoint {
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

impl Respond for TokenEndpoint {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        ResponseTemplate::new(200)
            .set_delay(self.delay)
            .set_body_json(json!({"access_token": token(n), "expires_in": 172_800}))
    }
}

async fn mount_tokens(server: &MockServer, delay: Duration) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(TokenEndpoint {
            calls: calls.clone(),
            delay,
        })
        .mount(server)
        .await;
    calls
}

fn ok_body() -> Value {
    json!({"ret": {"code": 0, "msg": "success"}, "appInfo": {"releaseState": 0}})
}

fn bearer(n: usize) -> String {
    format!("Bearer {}", token(n))
}

// ---------- токены ----------

#[tokio::test]
async fn obtains_token_once_and_sends_auth_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
            "grant_type": "client_credentials",
            "client_id": client_id(),
            "client_secret": client_secret(),
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": token(1), "expires_in": 172_800})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(query_param("appId", "100000001"))
        .and(header("client_id", client_id().as_str()))
        .and(header("authorization", bearer(1).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(2)
        .mount(&server)
        .await;

    let client = ApiClient::new(config(&server)).unwrap();
    client.authenticate().await.unwrap();
    for _ in 0..2 {
        let response = client
            .execute(
                ApiRequest::new(HttpMethod::Get, "/api/publish/v2/app-info")
                    .with_query("appId", "100000001"),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, ResponseBody::Json(ok_body()));
        assert!(response.result.unwrap().is_success());
        assert!(response.headers["content-type"].starts_with("application/json"));
    }
    // Запросы уходят с User-Agent сервера.
    let requests = server.received_requests().await.unwrap();
    let agent = requests[1]
        .headers
        .get("user-agent")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(agent.starts_with("appgallery-connect-mcp/"), "{agent}");
    // Debug клиента не раскрывает учётные данные.
    let debug = format!("{client:?}");
    assert!(!debug.contains(&client_secret()), "{debug}");
    assert!(!debug.contains(&client_id()), "{debug}");
}

#[tokio::test]
async fn refreshes_token_ahead_of_expiry() {
    let server = MockServer::start().await;
    let calls = mount_tokens(&server, Duration::ZERO).await;
    let clock = Arc::new(ManualClock::new(UNIX_EPOCH + Duration::from_secs(START)));
    let tokens = ApiClientTokens::with_base_url(Region::Cn, Some(&server.uri()), credentials())
        .unwrap()
        .with_clock(clock.clone());

    assert_eq!(tokens.token().await.unwrap().expose(), token(1));
    // До запаса времени токен берётся из памяти.
    clock.advance(Duration::from_secs(172_800) - REFRESH_MARGIN - Duration::from_secs(1));
    assert_eq!(tokens.token().await.unwrap().expose(), token(1));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Внутри запаса — новый токен, хотя прежний ещё действует.
    clock.advance(Duration::from_secs(2));
    assert_eq!(tokens.token().await.unwrap().expose(), token(2));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // Часы отвели назад раньше времени получения — тоже новый.
    clock.set(UNIX_EPOCH + Duration::from_secs(START - 10));
    assert_eq!(tokens.token().await.unwrap().expose(), token(3));
    // Прежние токены помнятся для вырезания из текстов.
    let known: Vec<String> = tokens
        .known_tokens()
        .iter()
        .map(|t| t.expose().to_owned())
        .collect();
    assert_eq!(known, [token(1), token(2), token(3)]);
    let debug = format!("{tokens:?}");
    assert!(
        !debug.contains(&client_secret()) && !debug.contains(&token(3)),
        "{debug}"
    );
}

#[tokio::test]
async fn parallel_requests_wait_for_one_refresh() {
    let server = MockServer::start().await;
    let calls = mount_tokens(&server, Duration::from_millis(150)).await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(header("authorization", bearer(1).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(8)
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    let mut tasks = Vec::new();
    for _ in 0..8 {
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
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn login_error_with_http_200_is_a_rejection() {
    for (ret, code) in [
        (
            json!({"code": 205524993, "msg": "client auth failed"}),
            "205524993",
        ),
        (
            json!({"code": "205524993", "msg": "client auth failed"}),
            "205524993",
        ),
        // По справочнику `ret` — строка с JSON внутри.
        (
            json!("{\"code\": 205524994, \"msg\": \"client auth failed\"}"),
            "205524994",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ret": ret})))
            .expect(1)
            .mount(&server)
            .await;
        let client = ApiClient::new(config(&server)).unwrap();
        let err = client
            .execute(ApiRequest::new(HttpMethod::Get, "/api/publish/v2/app-info"))
            .await
            .unwrap_err();
        match &err {
            ApiError::Token(TokenError::Rejected {
                status,
                code: Some(found),
                message,
            }) => {
                assert_eq!(*status, 200);
                assert_eq!(found, code);
                assert_eq!(message, "client auth failed");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(err.result_code(), Some(code));
        let text = err.to_string();
        assert!(text.contains("client auth failed"), "{text}");
        assert!(!text.contains(&client_secret()), "{text}");
    }
}

#[tokio::test]
async fn token_errors_do_not_leak_credentials() {
    let server = MockServer::start().await;
    // Сервер отражает тело запроса в ошибке.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_string(format!(
            "bad request: client_id={} secret {} rejected",
            client_id(),
            client_secret()
        )))
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    let err = client.authenticate().await.unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(matches!(
        err,
        ApiError::Token(TokenError::Rejected {
            status: 400,
            code: None,
            ..
        })
    ));
    assert!(!text.contains(&client_secret()), "{text}");
    assert!(!text.contains(&client_id()), "{text}");
    assert!(text.contains("rejected"), "{text}");

    // HTTP 200 без токена и без кода результата.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"expires_in": 5})))
        .mount(&server)
        .await;
    let err = ApiClient::new(config(&server))
        .unwrap()
        .authenticate()
        .await
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Token(TokenError::InvalidResponse(_))),
        "{err:?}"
    );
}

#[tokio::test]
async fn retries_once_with_a_new_token_after_401() {
    let server = MockServer::start().await;
    let calls = mount_tokens(&server, Duration::ZERO).await;
    Mock::given(method("POST"))
        .and(path("/api/publish/v2/app-submit"))
        .and(header("authorization", bearer(1).as_str()))
        .respond_with(ResponseTemplate::new(401).set_body_string("token expired"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/publish/v2/app-submit"))
        .and(header("authorization", bearer(2).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(1)
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    // POST: повтор после 401 безопасен для любого метода.
    client
        .execute(ApiRequest::new(
            HttpMethod::Post,
            "/api/publish/v2/app-submit",
        ))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // Второй 401 подряд — ошибка, а не бесконечные повторы.
    let server = MockServer::start().await;
    let calls = mount_tokens(&server, Duration::ZERO).await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .respond_with(ResponseTemplate::new(401).set_body_string("no access"))
        .expect(2)
        .mount(&server)
        .await;
    let client = ApiClient::new(config(&server)).unwrap();
    let err = client
        .execute(ApiRequest::new(HttpMethod::Get, "/api/publish/v2/app-info"))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(401));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn without_credentials_nothing_is_sent() {
    let server = MockServer::start().await;
    let mut config = config(&server);
    config.credentials = None;
    let client = ApiClient::new(config).unwrap();
    let err = client
        .execute(ApiRequest::new(HttpMethod::Get, "/api/publish/v2/app-info"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::MissingCredentials), "{err:?}");
    assert!(matches!(
        client.authenticate().await,
        Err(ApiError::MissingCredentials)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn base_url_override_accepts_only_loopback() {
    let mut config = ClientConfig::new(Region::Cn, Some(credentials()));
    config.base_url = Some("https://evil.example".into());
    assert!(matches!(
        ApiClient::new(config),
        Err(ApiError::Token(TokenError::Config(_)) | ApiError::InvalidConfig(_))
    ));
    // Без подмены клиент создаётся для хоста региона.
    ApiClient::new(ClientConfig::new(Region::De, Some(credentials()))).unwrap();
}

// ---------- операции ----------

static UPDATE_LANG: OperationDef = OperationDef {
    tool_name: "update_app_language",
    operation_id: "publishing.appLanguage.update",
    group: "publishing",
    toolset: "listing",
    summary: "Updates a language.",
    description: "Updates a language.",
    method: HttpMethod::Put,
    path: "/api/publish/v2/apps/{appId}/languages/{lang}",
    access: AccessLevel::Write,
    params: &[
        ParamDef {
            name: "appId",
            api_name: "appId",
            location: ParamLocation::Path,
            required: true,
            description: "",
            value_type: ParamType::String,
            repeated: false,
        },
        ParamDef {
            name: "lang",
            api_name: "lang",
            location: ParamLocation::Path,
            required: true,
            description: "",
            value_type: ParamType::String,
            repeated: false,
        },
        ParamDef {
            name: "releaseType",
            api_name: "releaseType",
            location: ParamLocation::Query,
            required: false,
            description: "",
            value_type: ParamType::Integer,
            repeated: false,
        },
        ParamDef {
            name: "filterCondition",
            api_name: "filterCondition",
            location: ParamLocation::Query,
            required: false,
            description: "",
            value_type: ParamType::Array,
            repeated: true,
        },
        ParamDef {
            name: "headerAppId",
            api_name: "appId",
            location: ParamLocation::Header,
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
    result: Some(RESULT_RET_CODE),
    reference: "",
    input_schema: "{}",
    secrets: &[],
};

fn read_operation(result: Option<ResultDef>, method: HttpMethod) -> &'static OperationDef {
    Box::leak(Box::new(OperationDef {
        tool_name: "query_things",
        operation_id: "test.things.query",
        group: "pms",
        toolset: "products",
        summary: "",
        description: "",
        method,
        path: "/api/test/v1/things",
        access: AccessLevel::Read,
        params: &[],
        body: None,
        response: ResponseKind::Json,
        result,
        reference: "",
        input_schema: "{}",
        secrets: &[],
    }))
}

#[tokio::test]
async fn operation_arguments_go_to_path_query_headers_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/publish/v2/apps/100000001/languages/zh-CN%2Fx"))
        .and(query_param("releaseType", "1"))
        .and(header("appid", "100000001"))
        .and(header("content-type", "application/json"))
        .and(header("client_id", client_id().as_str()))
        .and(header("authorization", bearer(0).as_str()))
        .and(body_json(json!({"appName": "Пример"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(1)
        .mount(&server)
        .await;
    let client = static_client(&server);
    let response = client
        .call(
            &UPDATE_LANG,
            &json!({
                "appId": 100000001,
                "lang": "zh-CN/x",
                "releaseType": 1,
                "filterCondition": ["countryId", "appVersion"],
                "headerAppId": "100000001",
                "body": {"appName": "Пример"},
            }),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    // Массив — повтором имени, в исходном порядке.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].url.query(),
        Some("releaseType=1&filterCondition=countryId&filterCondition=appVersion")
    );

    let request = ApiRequest::for_operation(
        &UPDATE_LANG,
        &json!({"appId": "1", "lang": "ru", "headerAppId": 1, "body": "{\"a\": 1}"}),
    )
    .unwrap();
    assert_eq!(request.path, "/api/publish/v2/apps/1/languages/ru");
    assert!(request.idempotent);
    assert_eq!(request.result, ResultCheck::Envelope(RESULT_RET_CODE));
    assert_eq!(request.operation.as_deref(), Some("update_app_language"));
    assert_eq!(request.headers, [("appId".to_owned(), "1".to_owned())]);
    // Операция чтения через POST повторяется, запись через POST — нет.
    let read = ApiRequest::for_operation(read_operation(None, HttpMethod::Post), &json!({}));
    assert!(read.unwrap().idempotent);
}

#[tokio::test]
async fn bad_operation_arguments_are_rejected_before_sending() {
    let server = MockServer::start().await;
    let client = static_client(&server);
    let base = json!({"appId": "1", "lang": "ru", "headerAppId": "1", "body": {}});
    let with = |key: &str, value: Value| {
        let mut args = base.clone();
        args[key] = value;
        args
    };
    let without = |key: &str| {
        let mut args = base.clone();
        args.as_object_mut().unwrap().remove(key);
        args
    };
    let cases = [
        (json!([1]), "JSON object"),
        (with("extra", json!(1)), "unknown argument `extra`"),
        (without("appId"), "missing path parameter `appId`"),
        (with("lang", json!(null)), "missing path parameter `lang`"),
        (with("lang", json!("")), "missing path parameter `lang`"),
        (with("lang", json!("..")), "invalid API path"),
        (with("lang", json!({"a": 1})), "path parameter `lang`"),
        (
            without("headerAppId"),
            "missing required argument `headerAppId`",
        ),
        (without("body"), "missing required argument `body`"),
        (
            with("headerAppId", json!(["a"])),
            "header parameter `headerAppId`",
        ),
        (
            with("headerAppId", json!("1\r\nx: y")),
            "invalid value of header",
        ),
        (
            with("releaseType", json!([1, 2])),
            "does not accept an array",
        ),
        (with("filterCondition", json!([{"a": 1}])), "array items"),
    ];
    for (args, expected) in cases {
        let err = client.call(&UPDATE_LANG, &args).await.unwrap_err();
        assert!(err.to_string().contains(expected), "{expected}: {err}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn service_headers_cannot_be_overridden() {
    let server = MockServer::start().await;
    let client = static_client(&server);
    for name in [
        "Authorization",
        "authorization",
        "client_id",
        "Client_Id",
        "Host",
        "Content-Type",
        "Content-Length",
        "Cookie",
        "Connection",
        "Transfer-Encoding",
        "Proxy-Authorization",
        "oauth2Token",
        "User-Agent",
    ] {
        let err = client
            .execute(
                ApiRequest::new(HttpMethod::Get, "/api/publish/v2/app-info")
                    .with_header(name, "value"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(_)),
            "{name}: {err:?}"
        );
        assert!(err.to_string().contains("cannot be set"), "{name}: {err}");
    }

    // Параметр-заголовок операции с именем служебного заголовка тоже отклоняется.
    static BAD: OperationDef = OperationDef {
        tool_name: "bad_header",
        operation_id: "test.bad",
        group: "pms",
        toolset: "products",
        summary: "",
        description: "",
        method: HttpMethod::Get,
        path: "/api/test/v1/things",
        access: AccessLevel::Read,
        params: &[ParamDef {
            name: "auth",
            api_name: "Authorization",
            location: ParamLocation::Header,
            required: false,
            description: "",
            value_type: ParamType::String,
            repeated: false,
        }],
        body: None,
        response: ResponseKind::Json,
        result: None,
        reference: "",
        input_schema: "{}",
        secrets: &[],
    };
    let err = client
        .call(&BAD, &json!({"auth": "Bearer other"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cannot be set"), "{err}");
    assert!(server.received_requests().await.unwrap().is_empty());

    // Обычный заголовок проходит, авторизация остаётся от исполнителя.
    Mock::given(method("GET"))
        .and(path("/api/pms/v1/products"))
        .and(header("appid", "42"))
        .and(header("authorization", bearer(0).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    client
        .execute(
            ApiRequest::new(HttpMethod::Get, "/api/pms/v1/products").with_header("appId", "42"),
        )
        .await
        .unwrap();
}

// ---------- конверт результата ----------

#[tokio::test]
async fn result_envelopes_turn_nonzero_codes_into_errors() {
    let cases = [
        (
            RESULT_RET_CODE,
            json!({"ret": {"code": 0, "msg": "success"}}),
            None,
        ),
        (
            RESULT_RET_CODE,
            json!({"ret": {"code": "0", "msg": "success"}}),
            None,
        ),
        (
            RESULT_RET_CODE,
            json!({"ret": {"code": 204144660, "msg": "app not found"}}),
            Some(("204144660", "app not found")),
        ),
        (
            RESULT_RET_CODE,
            json!({"ret": {"code": "204144660", "msg": "app not found"}}),
            Some(("204144660", "app not found")),
        ),
        (
            RESULT_RET_RTN_CODE,
            json!({"ret": {"rtnCode": 0, "rtnDesc": "ok"}}),
            None,
        ),
        (RESULT_RET_RTN_CODE, json!({"ret": {"rtnCode": "0"}}), None),
        (
            RESULT_RET_RTN_CODE,
            json!({"ret": {"rtnCode": 3, "rtnDesc": "no review"}}),
            Some(("3", "no review")),
        ),
        (
            RESULT_RET_RTN_CODE,
            json!({"ret": {"rtnCode": "3", "rtnDesc": "no review"}}),
            Some(("3", "no review")),
        ),
        (
            RESULT_ERROR_CODE,
            json!({"error": {"errorCode": 0, "errorMsg": null}}),
            None,
        ),
        (
            RESULT_ERROR_CODE,
            json!({"error": {"errorCode": "0"}}),
            None,
        ),
        (
            RESULT_ERROR_CODE,
            json!({"error": {"errorCode": 13, "errorMsg": "no product"}}),
            Some(("13", "no product")),
        ),
        (
            RESULT_ERROR_CODE,
            json!({"error": {"errorCode": "13", "errorMsg": "no product"}}),
            Some(("13", "no product")),
        ),
    ];
    for (envelope, body, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/test/v1/things"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .expect(2)
            .mount(&server)
            .await;
        let client = static_client(&server);
        // Операция с конвертом из таблицы и запрос без операции (конверт — по телу).
        let by_operation = client
            .call(read_operation(Some(envelope), HttpMethod::Get), &json!({}))
            .await;
        let by_body = client
            .execute(ApiRequest::new(HttpMethod::Get, "/api/test/v1/things"))
            .await;
        for (result, operation) in [(by_operation, Some("query_things")), (by_body, None)] {
            match (result, expected) {
                (Ok(response), None) => {
                    assert_eq!(response.body, ResponseBody::Json(body.clone()));
                    let status = response.result.unwrap();
                    assert!(status.is_success());
                    assert_eq!(status.envelope, envelope);
                }
                (Err(ApiError::Result(error)), Some((code, message))) => {
                    assert_eq!(error.status, 200);
                    assert_eq!(error.code, code);
                    assert_eq!(error.message.as_deref(), Some(message));
                    assert_eq!(error.operation.as_deref(), operation);
                    assert_eq!(error.envelope, envelope);
                    let text = error.to_string();
                    assert!(text.contains(code) && text.contains(message), "{text}");
                }
                (other, _) => panic!("{body}: {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn result_check_can_be_skipped_and_bodies_without_envelope_pass() {
    let server = MockServer::start().await;
    let error_body = json!({"ret": {"code": 5, "msg": "failed"}});
    Mock::given(method("GET"))
        .and(path("/api/test/v1/failed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(error_body.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/test/v1/plain"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [1, 2]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/test/v1/empty"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/test/v1/text"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("hello Bearer {}", token(0))),
        )
        .mount(&server)
        .await;
    let client = static_client(&server);
    let get = |path: &str| ApiRequest::new(HttpMethod::Get, format!("/api/test/v1/{path}"));

    let response = client
        .execute(get("failed").with_result(ResultCheck::Skip))
        .await
        .unwrap();
    assert_eq!(response.body, ResponseBody::Json(error_body));
    assert_eq!(response.result, None);
    assert!(matches!(
        client.execute(get("failed")).await,
        Err(ApiError::Result(_))
    ));

    let response = client.execute(get("plain")).await.unwrap();
    assert_eq!(response.result, None);
    assert_eq!(response.body.json(), Some(&json!({"items": [1, 2]})));
    assert_eq!(
        client.execute(get("empty")).await.unwrap().body,
        ResponseBody::Empty
    );
    // Текстовый ответ — без токена.
    let response = client.execute(get("text")).await.unwrap();
    assert_eq!(response.body, ResponseBody::Text("hello Bearer ***".into()));
}

// ---------- ошибки HTTP ----------

#[tokio::test]
async fn http_errors_are_structured_truncated_and_scrubbed() {
    let server = MockServer::start().await;
    // Сервер отражает заголовки авторизации и секрет в теле ошибки.
    let leaked = format!(
        "{{\"ret\":{{\"code\":204144647,\"msg\":\"no permission for client {} with {}\"}},\
         \"client_secret\":\"{}\",\"authCode\":\"upload-code-1234567890\",\
         \"url\":\"https://files.example.com/o/1?X-Amz-Signature=abcdef0123456789\"}}",
        client_id(),
        bearer(0),
        client_secret()
    );
    Mock::given(method("GET"))
        .and(path("/api/test/v1/forbidden"))
        .respond_with(ResponseTemplate::new(403).set_body_string(leaked))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/test/v1/broken"))
        .respond_with(ResponseTemplate::new(500).set_body_string("x".repeat(100_000)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/test/v1/moved"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "https://evil.example/steal?token=abc"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = static_client(&server);

    let err = client
        .execute(
            ApiRequest::new(HttpMethod::Get, "/api/test/v1/forbidden").with_operation("get_thing"),
        )
        .await
        .unwrap_err();
    let http = err.http().unwrap().clone();
    assert_eq!(http.status, 403);
    assert_eq!(http.code.as_deref(), Some("204144647"));
    assert_eq!(http.operation.as_deref(), Some("get_thing"));
    assert_eq!(err.result_code(), Some("204144647"));
    let text = format!("{err} {err:?}");
    for secret in [
        client_secret(),
        client_id(),
        token(0),
        "upload-code-1234567890".to_owned(),
        "abcdef0123456789".to_owned(),
        "files.example.com".to_owned(),
    ] {
        assert!(!text.contains(&secret), "{secret}: {text}");
    }
    assert!(text.contains("no permission for client"), "{text}");
    assert!(
        text.contains("HTTP 403 Forbidden for `get_thing`"),
        "{text}"
    );

    // Большое тело обрезается; POST после 500 не повторяется.
    let err = client
        .execute(ApiRequest::new(HttpMethod::Post, "/api/test/v1/broken"))
        .await
        .unwrap_err();
    let http = err.http().unwrap();
    assert_eq!(http.status, 500);
    assert!(http.body.chars().count() <= 2001, "{}", http.body.len());
    assert!(http.body.ends_with('…'));

    // Редирект не выполняется, адрес из Location в ошибку не попадает.
    let err = client
        .execute(ApiRequest::new(HttpMethod::Get, "/api/test/v1/moved"))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(302));
    assert!(!err.to_string().contains("evil.example"), "{err}");
}

#[tokio::test]
async fn large_responses_are_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/test/v1/big"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": "x".repeat(5000)})))
        .mount(&server)
        .await;
    let mut config = config(&server);
    config.max_response_bytes = 1000;
    let client =
        ApiClient::with_token_source(config, Arc::new(StaticToken::new(token(0)))).unwrap();
    let request = ApiRequest::new(HttpMethod::Get, "/api/test/v1/big");
    let err = client.execute(request.clone()).await.unwrap_err();
    assert!(
        matches!(err, ApiError::ResponseTooLarge { limit: 1000 }),
        "{err:?}"
    );
    client.execute_with_limit(request, 10_000).await.unwrap();
}

// ---------- повторы ----------

#[tokio::test]
async fn idempotent_requests_are_retried_after_429_and_5xx() {
    for (http_method, status) in [
        (HttpMethod::Get, 503),
        (HttpMethod::Get, 429),
        (HttpMethod::Put, 500),
        (HttpMethod::Delete, 502),
        (HttpMethod::Get, 504),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/api/test/v1/things"))
            .respond_with(ResponseTemplate::new(status).set_body_string("busy"))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(path("/api/test/v1/things"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .expect(1)
            .mount(&server)
            .await;
        let client = static_client(&server);
        let started = Instant::now();
        client
            .execute(ApiRequest::new(http_method, "/api/test/v1/things"))
            .await
            .unwrap_or_else(|err| panic!("{http_method:?} {status}: {err:?}"));
        // Экспоненциальная задержка: 10 мс и 20 мс.
        assert!(started.elapsed() >= Duration::from_millis(30));
    }
}

#[tokio::test]
async fn retries_stop_after_the_limit_and_skip_other_statuses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/test/v1/down"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/test/v1/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .expect(1)
        .mount(&server)
        .await;
    let client = static_client(&server);
    let err = client
        .execute(ApiRequest::new(HttpMethod::Get, "/api/test/v1/down"))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(503));
    let err = client
        .execute(ApiRequest::new(HttpMethod::Get, "/api/test/v1/missing"))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(404));
    assert_eq!(err.http().unwrap().body, "not found");
}

#[tokio::test]
async fn non_idempotent_requests_are_not_retried() {
    for (http_method, status) in [
        (HttpMethod::Post, 503),
        (HttpMethod::Post, 429),
        (HttpMethod::Patch, 500),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/api/test/v1/things"))
            .respond_with(ResponseTemplate::new(status).insert_header("retry-after", "0"))
            .expect(1)
            .mount(&server)
            .await;
        let client = static_client(&server);
        let err = client
            .execute(ApiRequest::new(http_method, "/api/test/v1/things"))
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(status));
        assert_eq!(err.http().unwrap().retry_after, Some(0));
    }
    // Явно помеченный идемпотентным POST (чтение) повторяется.
    let server = MockServer::start().await;
    Mock::given(path("/api/test/v1/things"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/test/v1/things"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(1)
        .mount(&server)
        .await;
    static_client(&server)
        .execute(ApiRequest::new(HttpMethod::Post, "/api/test/v1/things").idempotent(true))
        .await
        .unwrap();
}

#[tokio::test]
async fn retry_after_is_respected() {
    // Retry-After в пределах максимальной паузы: ждём столько, сколько просит сервер.
    let server = MockServer::start().await;
    Mock::given(path("/api/test/v1/things"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/test/v1/things"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = config(&server);
    config.retry.max_delay = Duration::from_secs(2);
    let client =
        ApiClient::with_token_source(config, Arc::new(StaticToken::new(token(0)))).unwrap();
    let started = Instant::now();
    client
        .execute(ApiRequest::new(HttpMethod::Get, "/api/test/v1/things"))
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));

    // Retry-After больше максимальной паузы: запрос не повторяется, срок — в ошибке.
    let server = MockServer::start().await;
    Mock::given(path("/api/test/v1/things"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .expect(1)
        .mount(&server)
        .await;
    let err = static_client(&server)
        .execute(ApiRequest::new(HttpMethod::Get, "/api/test/v1/things"))
        .await
        .unwrap_err();
    assert_eq!(err.http().unwrap().retry_after, Some(120));
    assert!(err.to_string().contains("retry after 120 s"), "{err}");
}

#[tokio::test]
async fn connection_errors_are_retried_and_reported_without_the_url() {
    // Порт закрыт: слушатель освобождён до запроса.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut config = ClientConfig::new(Region::Cn, Some(credentials()));
    config.base_url = Some(format!("http://127.0.0.1:{port}"));
    config.retry = retry();
    let client =
        ApiClient::with_token_source(config, Arc::new(StaticToken::new(token(0)))).unwrap();
    let started = Instant::now();
    let err = client
        .execute(ApiRequest::new(HttpMethod::Post, "/api/test/v1/things").with_query("secret", "v"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
    // Две повторные попытки (10 мс и 20 мс) даже для POST: запрос не был отправлен.
    assert!(started.elapsed() >= Duration::from_millis(30));
    let text = err.to_string();
    assert!(
        !text.contains("127.0.0.1") && !text.contains("secret=v"),
        "{text}"
    );
}

// ---------- сырой запрос ----------

#[tokio::test]
async fn raw_requests_accept_only_api_paths() {
    let server = MockServer::start().await;
    let client = static_client(&server);
    for bad in [
        "",
        "/",
        "publish/v2/app-info",
        "/publish/v2/app-info",
        "/v1/apps",
        "https://connect-api.cloud.huawei.com/api/publish/v2/app-info",
        "https://evil.example/api/x",
        "http://evil.example/api/x",
        "//evil.example/api/x",
        "/api//x",
        "/api/x/../y",
        "/api/../x",
        "/api/x/%2e%2e/y",
        "/api/x/..%2fy",
        "/api/x/%252e%252e/y",
        "/api/x\\y",
        "/api/x/..\\y",
        "/api/x?appId=1",
        "/api/x#frag",
        "/api/x y",
        "/api/x\ny",
        "/api/x\r\nHost: evil.example",
        "/api/x\u{0}",
        "/api/x/%00",
        "/api/x/%zz",
        "/api/oauth2/v1/token",
        "/api/OAUTH2/v1/token",
        "/api/oauth%32/v1/token",
        "/api/oauth2",
    ] {
        for http_method in [HttpMethod::Get, HttpMethod::Post] {
            let err = client
                .execute(ApiRequest::new(http_method, bad))
                .await
                .unwrap_err();
            assert!(matches!(err, ApiError::InvalidPath(_)), "{bad:?}: {err:?}");
        }
    }
    let err = client
        .execute(ApiRequest::new(HttpMethod::Get, "/api/x").with_query("", "v"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());

    // Корректный сырой запрос: query кодируется, тело уходит JSON.
    Mock::given(method("POST"))
        .and(path("/api/publish/v2/app-language-info"))
        .and(query_param("appId", "1&x=2"))
        .and(body_json(json!({"lang": "ru-RU"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(1)
        .mount(&server)
        .await;
    client
        .execute(
            ApiRequest::new(HttpMethod::Post, "/api/publish/v2/app-language-info")
                .with_query_value("appId", &json!("1&x=2"))
                .unwrap()
                .with_json(json!("{\"lang\": \"ru-RU\"}")),
        )
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.query(), Some("appId=1%26x%3D2"));
}

#[test]
fn scrub_removes_known_and_shaped_secrets() {
    let config = ClientConfig::new(Region::Cn, Some(credentials()));
    let client =
        ApiClient::with_token_source(config, Arc::new(StaticToken::new(token(0)))).unwrap();
    let text = format!(
        "id {} secret {} token {} other",
        client_id(),
        client_secret(),
        token(0)
    );
    assert_eq!(client.scrub(&text), "id *** secret *** token *** other");
}
