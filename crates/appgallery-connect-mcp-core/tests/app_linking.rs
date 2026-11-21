//! Набор `app-linking` (серверный REST API App Linking) через сервер: набор выключен по
//! умолчанию, оба инструмента — уровня записи, запросы подписывает API-клиент проекта из
//! заголовка `productId` (свой токен), без клиента проекта запрос не отправляется, код
//! результата при HTTP 200 — ошибка, `api_request` к тем же путям идёт тем же клиентом, а в
//! HTTP-режиме подсказка называет оператора сервера. HTTP — `wiremock`, учётные данные и
//! токены выдуманы и собраны из частей.

use std::time::Duration;

use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, ProjectCredentials, Region,
    RetryPolicy,
};
use appgallery_connect_mcp_core::{AppGalleryServer, ServerOptions, Services};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN_PATH: &str = "/api/oauth2/v1/token";
const LINKS: &str = "/api/applinking/v1/links";
const UNIFIED_LINKS: &str = "/api/applinking/v1/unified-links";

/// Проект с API-клиентом проекта в профиле и проект без него.
const PROJECT: &str = "100000002";
const OTHER_PROJECT: &str = "100000003";

fn team_client_id() -> String {
    ["1618", "0339", "8874", "9894"].concat()
}

fn team_secret() -> String {
    ["Te12", "Am34", "Cl56", "Nt78"].concat().repeat(4)
}

fn project_client_id() -> String {
    ["5772", "1566", "4901", "5329"].concat()
}

fn project_secret() -> String {
    ["Pj12", "Ap34", "Pl56", "Nk78"].concat().repeat(4)
}

fn project_token() -> String {
    ["project", "-test-", "token-", "3455891442"].concat()
}

fn api_client(base_url: &str) -> ApiClient {
    let team = ApiClientCredentials::from_parts(team_client_id(), team_secret()).unwrap();
    let mut config = ClientConfig::new(Region::Cn, Some(team));
    config.base_url = Some(base_url.to_owned());
    config.profile = Some("work".into());
    config.projects = vec![
        ProjectCredentials::new(
            PROJECT,
            ApiClientCredentials::from_parts(project_client_id(), project_secret()).unwrap(),
        )
        .unwrap(),
    ];
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    ApiClient::new(config).unwrap()
}

struct Fixture {
    api: MockServer,
    server: AppGalleryServer,
}

async fn fixture_with(access: AccessLevel, toolsets: Option<&[&str]>, http: bool) -> Fixture {
    let api = MockServer::start().await;
    let options = ServerOptions {
        max_access: access,
        toolsets: toolsets.map(|sets| sets.iter().map(|s| (*s).to_owned()).collect()),
        profile: Some("work".into()),
        http,
        ..ServerOptions::default()
    };
    let server = AppGalleryServer::new(Services::new(api_client(&api.uri())), options).unwrap();
    Fixture { api, server }
}

async fn fixture(access: AccessLevel) -> Fixture {
    fixture_with(access, Some(&["app-linking"]), false).await
}

impl Fixture {
    async fn call(&self, name: &str, args: Value) -> CallToolResult {
        let Value::Object(args) = args else {
            panic!("аргументы не объект");
        };
        self.server.call(name, args).await.unwrap()
    }

    /// Токен выдаётся только клиенту проекта; клиенту команды App Linking не нужен.
    async fn mount_project_token(&self) {
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .and(body_partial_json(json!({
                "grant_type": "client_credentials",
                "client_id": project_client_id(),
                "client_secret": project_secret(),
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": project_token(), "expires_in": 172_800})),
            )
            .expect(1)
            .mount(&self.api)
            .await;
    }

    /// Запрос App Linking с заголовками клиента проекта и заданным телом.
    async fn mount_link(&self, route: &str, body: Value, response: Value) {
        let bearer = format!("Bearer {}", project_token());
        Mock::given(method("POST"))
            .and(path(route))
            .and(header("productId", PROJECT))
            .and(header("client_id", project_client_id().as_str()))
            .and(header("authorization", bearer.as_str()))
            .and(body_json(body))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&self.api)
            .await;
    }

    async fn requests(&self) -> Vec<wiremock::Request> {
        self.api.received_requests().await.unwrap()
    }
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn ok(result: &CallToolResult) -> Value {
    assert_eq!(result.is_error, Some(false), "{}", text(result));
    serde_json::from_str(&text(result)).unwrap()
}

fn failed(result: &CallToolResult) -> String {
    assert_eq!(result.is_error, Some(true), "{}", text(result));
    text(result)
}

fn short_link_body() -> Value {
    json!({
        "linkInfo": {
            "uriPrefix": "https://example.drcn.agconnect.link",
            "deepLink": "https://example.com/detail?id=123",
            "android": {"packageName": "com.example.app", "openType": 1},
            "campaign": {"channel": "newsletter", "medium": "email", "name": "spring"}
        },
        "suffixLength": "SHORT",
        "expireTime": 3600
    })
}

fn unified_link_body() -> Value {
    json!({
        "uriPrefix": "https://example.drcn.agconnect.link",
        "name": "Spring newsletter",
        "defaultRedirectionUrl": "https://example.com/app",
        "iosRedirectionUrl": "https://apps.apple.com/app/id000000001",
        "agRedirectionUrl": "hiapplink://com.huawei.appmarket?appId=C100000001"
    })
}

fn success(extra: Value) -> Value {
    let mut body = json!({"ret": {"code": 0, "msg": "success"}});
    if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
    body
}

const TOOLS: [&str; 2] = ["create_short_link", "create_unified_link"];

#[tokio::test]
async fn toolset_is_off_by_default_and_needs_write_access() {
    // Наборы по умолчанию: app-linking выключен даже на уровне удаления.
    let f = fixture_with(AccessLevel::Destructive, None, false).await;
    for name in TOOLS {
        assert!(f.server.registry().get(name).is_none(), "{name}");
        let message = failed(&f.call(name, json!({"productId": PROJECT})).await);
        assert!(message.contains("app-linking"), "{message}");
    }

    // Набор включён, но уровень чтения: инструменты записи не регистрируются.
    let f = fixture(AccessLevel::Read).await;
    for name in TOOLS {
        assert!(f.server.registry().get(name).is_none(), "{name}");
        let message = failed(&f.call(name, json!({"productId": PROJECT})).await);
        assert!(message.contains("write access"), "{message}");
    }

    let f = fixture(AccessLevel::Write).await;
    let registry = f.server.registry();
    for name in TOOLS {
        let tool = registry.get(name).unwrap_or_else(|| panic!("{name}"));
        let hints = tool.tool.annotations.as_ref().unwrap();
        assert_eq!(hints.read_only_hint, Some(false), "{name}");
        assert_eq!(hints.destructive_hint, Some(false), "{name}");
        let description = tool.tool.description.as_deref().unwrap();
        // Агент знает до вызова: ссылки публичные, в консоли не видны, через API не удалить.
        assert!(
            description.contains("not shown in AppGallery Connect"),
            "{description}"
        );
        assert!(
            description.contains("cannot list, change or delete"),
            "{description}"
        );
        assert!(
            description.contains("Project-level API client"),
            "{description}"
        );
        assert!(description.contains("auth project add"), "{description}");
        let schema = serde_json::to_value(&*tool.tool.input_schema).unwrap();
        assert_eq!(
            schema["required"],
            json!(["productId", "body"]),
            "{name}: {schema}"
        );
    }
    assert!(f.requests().await.is_empty());
}

#[tokio::test]
async fn short_link_is_created_with_the_project_client() {
    let f = fixture(AccessLevel::Write).await;
    f.mount_project_token().await;
    f.mount_link(
        LINKS,
        short_link_body(),
        success(json!({
            "shortUrl": "https://example.drcn.agconnect.link/ADKQ",
            "testUrl": "https://example.drcn.agconnect.link/ADKQ?test=1"
        })),
    )
    .await;
    let body = ok(&f
        .call(
            "create_short_link",
            json!({"productId": PROJECT, "body": short_link_body()}),
        )
        .await);
    // Ссылки не секрет: агент получает их как есть.
    assert_eq!(body["shortUrl"], "https://example.drcn.agconnect.link/ADKQ");
    assert_eq!(
        body["testUrl"],
        "https://example.drcn.agconnect.link/ADKQ?test=1"
    );
    assert!(body.get("note").is_none(), "{body}");
    let all = serde_json::to_string(&body).unwrap();
    assert!(!all.contains(&project_token()), "{all}");
    // Клиент команды в App Linking не участвует: токен получал только клиент проекта.
    let requests = f.requests().await;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let raw = String::from_utf8_lossy(&request.body);
        assert!(!raw.contains(&team_client_id()), "{raw}");
        assert_eq!(
            request
                .headers
                .get("client_id")
                .map(|v| v.to_str().unwrap().to_owned()),
            (request.url.path() != TOKEN_PATH).then(project_client_id),
        );
    }
}

#[tokio::test]
async fn unified_link_is_created_with_the_project_client() {
    let f = fixture(AccessLevel::Write).await;
    f.mount_project_token().await;
    f.mount_link(
        UNIFIED_LINKS,
        unified_link_body(),
        success(json!({"shortUrl": "https://example.drcn.agconnect.link/ABCD"})),
    )
    .await;
    let body = ok(&f
        .call(
            "create_unified_link",
            json!({"productId": PROJECT, "body": unified_link_body()}),
        )
        .await);
    assert_eq!(body["shortUrl"], "https://example.drcn.agconnect.link/ABCD");
}

#[tokio::test]
async fn arguments_are_checked_before_the_request() {
    let f = fixture(AccessLevel::Write).await;
    // Без проекта, без тела, с неверным типом поля тела, с недопустимым ID проекта.
    // Обязательность полей внутри `body` сервер не проверяет: решает AppGallery Connect.
    for (name, args) in [
        ("create_short_link", json!({"body": short_link_body()})),
        ("create_short_link", json!({"productId": PROJECT})),
        (
            "create_short_link",
            json!({"productId": PROJECT, "body": {"longUrl": "https://example.com", "expireTime": "soon"}}),
        ),
        (
            "create_short_link",
            json!({"productId": "../other", "body": short_link_body()}),
        ),
    ] {
        let message = failed(&f.call(name, args.clone()).await);
        assert!(!message.is_empty(), "{name} {args}");
        assert!(f.requests().await.is_empty(), "{name} {args}: {message}");
    }
    assert!(f.requests().await.is_empty());
}

#[tokio::test]
async fn project_without_a_client_is_not_sent() {
    let f = fixture(AccessLevel::Write).await;
    let message = failed(
        &f.call(
            "create_short_link",
            json!({"productId": OTHER_PROJECT, "body": short_link_body()}),
        )
        .await,
    );
    assert!(
        message.contains(&format!(
            "auth project add --profile work --project-id {OTHER_PROJECT}"
        )),
        "{message}"
    );
    assert!(message.contains("was not sent"), "{message}");
    assert!(message.contains("terminal"), "{message}");
    // Клиентом команды запрос не подписывается: в API не ушло ничего, даже за токеном.
    assert!(f.requests().await.is_empty());

    // В HTTP-режиме клиента проекта добавляет оператор сервера.
    let f = fixture_with(AccessLevel::Write, Some(&["app-linking"]), true).await;
    let message = failed(
        &f.call(
            "create_unified_link",
            json!({"productId": OTHER_PROJECT, "body": unified_link_body()}),
        )
        .await,
    );
    assert!(message.contains("server operator"), "{message}");
    assert!(!message.contains("their terminal"), "{message}");
    assert!(f.requests().await.is_empty());
}

#[tokio::test]
async fn result_code_with_http_200_is_an_error() {
    let f = fixture(AccessLevel::Write).await;
    f.mount_project_token().await;
    f.mount_link(
        LINKS,
        short_link_body(),
        json!({"ret": {"code": 204865576, "msg": "The URL prefix of the cross-platform link is not found."}}),
    )
    .await;
    let message = failed(
        &f.call(
            "create_short_link",
            json!({"productId": PROJECT, "body": short_link_body()}),
        )
        .await,
    );
    assert!(message.contains("204865576"), "{message}");
    assert!(message.contains("URL prefix"), "{message}");
}

#[tokio::test]
async fn api_request_uses_the_project_client_too() {
    let f = fixture(AccessLevel::Write).await;
    f.mount_project_token().await;
    f.mount_link(
        UNIFIED_LINKS,
        unified_link_body(),
        success(json!({"shortUrl": "https://example.drcn.agconnect.link/ABCD"})),
    )
    .await;
    let body = ok(&f
        .call(
            "api_request",
            json!({
                "method": "POST",
                "path": UNIFIED_LINKS,
                "headers": {"productId": PROJECT},
                "body": unified_link_body(),
            }),
        )
        .await);
    assert_eq!(body["shortUrl"], "https://example.drcn.agconnect.link/ABCD");

    // Без заголовка проекта путь операции проекта не вызывается клиентом команды.
    let message = failed(
        &f.call(
            "api_request",
            json!({"method": "POST", "path": LINKS, "body": short_link_body()}),
        )
        .await,
    );
    assert!(message.contains("create_short_link"), "{message}");
    assert!(message.contains("productId"), "{message}");

    // На уровне чтения запись через api_request отклоняется до запроса.
    let read = fixture(AccessLevel::Read).await;
    let message = failed(
        &read
            .call(
                "api_request",
                json!({"method": "POST", "path": LINKS, "headers": {"productId": PROJECT}, "body": short_link_body()}),
            )
            .await,
    );
    assert!(message.contains("write access"), "{message}");
    assert!(read.requests().await.is_empty());
}

#[tokio::test]
async fn api_search_finds_the_links_in_english_and_russian() {
    let f = fixture_with(AccessLevel::Read, None, false).await;
    for query in [
        "short link",
        "unified download link",
        "короткая ссылка",
        "единая ссылка",
    ] {
        let body = ok(&f.call("api_search", json!({"query": query})).await);
        let results = body["results"].as_array().unwrap();
        assert!(
            results
                .iter()
                .any(|r| r["tool"] == "create_short_link" || r["tool"] == "create_unified_link"),
            "{query}: {body}"
        );
        let found = results
            .iter()
            .find(|r| r["tool"] == "create_short_link" || r["tool"] == "create_unified_link")
            .unwrap();
        assert_eq!(found["toolset"], "app-linking", "{found}");
    }
}
