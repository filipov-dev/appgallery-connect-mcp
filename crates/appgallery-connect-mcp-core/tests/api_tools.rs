//! `api_search` и `api_request` в MCP-сессии с сервером ядра в памяти: список инструментов
//! при разных уровнях доступа, поиск без обращения к API, вызовы `api_request` против
//! `wiremock` (конверт ошибки при HTTP 200, вырезание секретов) и отказы до запроса. Сеть,
//! настоящие учётные данные и системный keychain не используются.

use std::sync::Arc;
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy,
};
use appgallery_connect_mcp_core::{AppGalleryServer, ServerOptions, Services};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Выдуманный идентификатор API-клиента, собранный из частей.
fn client_id() -> String {
    ["2718", "2818", "2845", "9045"].concat()
}

/// Выдуманный секрет API-клиента, собранный из частей.
fn client_secret() -> String {
    ["FE98", "DC76", "BA54", "3210"].concat().repeat(4)
}

/// Токен, который тестовый клиент отправляет в `Authorization`.
fn token() -> String {
    ["api", "-tools-", "token-", "9876543210"].concat()
}

fn server(base_url: &str, max_access: AccessLevel, toolsets: Option<&[&str]>) -> AppGalleryServer {
    let credentials = ApiClientCredentials::from_parts(client_id(), client_secret()).unwrap();
    let mut config = ClientConfig::new(Region::De, Some(credentials));
    config.base_url = Some(base_url.to_owned());
    config.max_response_bytes = 64 * 1024;
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(10),
    };
    let api = ApiClient::with_token_source(config, Arc::new(StaticToken::new(token()))).unwrap();
    AppGalleryServer::new(
        Services::new(api),
        ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| (*s).to_owned()).collect()),
            ..ServerOptions::default()
        },
    )
    .unwrap()
}

async fn connect(server: AppGalleryServer) -> RunningService<RoleClient, ()> {
    let (server_io, client_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    ().serve(client_io).await.unwrap()
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> CallToolResult {
    let Value::Object(arguments) = arguments else {
        panic!("не объект: {arguments}");
    };
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
        .await
        .unwrap()
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block: &ContentBlock| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn json_of(result: &CallToolResult) -> Value {
    let text = text(result);
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("не JSON ({err}): {text}"))
}

#[tokio::test]
async fn search_and_request_are_listed_at_every_level() {
    for (access, read_only, destructive) in [
        (AccessLevel::Read, Some(true), None),
        (AccessLevel::Write, Some(false), Some(false)),
        (AccessLevel::Destructive, Some(false), Some(true)),
    ] {
        for toolsets in [None, Some(&["provisioning"][..])] {
            let client = connect(server("http://127.0.0.1:9", access, toolsets)).await;
            let tools = client.list_all_tools().await.unwrap();
            let search = tools.iter().find(|t| t.name == "api_search").unwrap();
            let hints = search.annotations.as_ref().unwrap();
            assert_eq!(hints.read_only_hint, Some(true));
            let request = tools.iter().find(|t| t.name == "api_request").unwrap();
            let hints = request.annotations.as_ref().unwrap();
            assert_eq!(
                (hints.read_only_hint, hints.destructive_hint),
                (read_only, destructive),
                "{access:?}"
            );
            let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
            assert!(instructions.contains("`api_search`"), "{instructions}");
            client.cancel().await.unwrap();
        }
    }
}

#[tokio::test]
async fn search_works_without_the_api() {
    let api = MockServer::start().await;
    let client = connect(server(&api.uri(), AccessLevel::Read, None)).await;
    let result = call(&client, "api_search", json!({"query": "сертификаты"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    let found = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool"] == "list_certificates")
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(found["toolset"], "provisioning");
    assert!(found["status"].as_str().unwrap().contains("--toolsets"));

    let result = call(&client, "api_search", json!({"tool_name": "submit_app"})).await;
    let body = json_of(&result);
    assert!(
        body["status"]
            .as_str()
            .unwrap()
            .contains("--allow-destructive")
    );
    assert!(body["input_schema"]["properties"].is_object());

    let result = call(&client, "api_search", json!({})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(api.received_requests().await.unwrap().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn request_calls_the_api_with_query_headers_and_body() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/reviews/v1/manage/dev/reviews"))
        .and(query_param("appId", "100000001"))
        .and(query_param("pageSize", "5"))
        .and(header("client_id", client_id().as_str()))
        .and(header(
            "authorization",
            format!("Bearer {}", token()).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": 0, "rtnDesc": "success"},
            "reviews": [{"reviewId": "r-1", "rating": 5}],
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/api/pms/product-price-service/v1/manage/product/list",
        ))
        .and(header("appId", "100000001"))
        .and(body_json(json!({"pageNum": 1})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": {"errorCode": 0, "errorMsg": null},
            "products": [],
        })))
        .expect(1)
        .mount(&api)
        .await;
    // Сервер только на чтение, набор отзывов не включён: `api_request` наборами не ограничен.
    let client = connect(server(&api.uri(), AccessLevel::Read, Some(&["apps"]))).await;
    let result = call(
        &client,
        "api_request",
        json!({
            "path": "/api/reviews/v1/manage/dev/reviews?appId=100000001",
            "query": {"pageSize": 5}
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(json_of(&result)["reviews"][0]["reviewId"], "r-1");

    // POST, который только читает, — на сервере только для чтения.
    let result = call(
        &client,
        "api_request",
        json!({
            "method": "POST",
            "path": "/api/pms/product-price-service/v1/manage/product/list",
            "headers": {"appId": "100000001"},
            "body": {"pageNum": 1}
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn request_reports_result_codes_with_http_200() {
    let api = MockServer::start().await;
    // Известная операция: конверт операции (`ret.rtnCode` у Comments API).
    Mock::given(method("GET"))
        .and(path("/api/reviews/v1/manage/dev/reviews/r-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": 204144660, "rtnDesc": "review not found"},
        })))
        .mount(&api)
        .await;
    // Неизвестный путь: конверт определяется по телу.
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/something-new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": {"errorCode": "1001", "errorMsg": "bad request"},
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), AccessLevel::Read, None)).await;

    let result = call(
        &client,
        "api_request",
        json!({"path": "/api/reviews/v1/manage/dev/reviews/r-1"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("code 204144660: review not found"),
        "{message}"
    );
    assert!(message.contains("`get_review`"), "{message}");
    assert!(
        message.contains("even though the HTTP status is 200"),
        "{message}"
    );

    let result = call(
        &client,
        "api_request",
        json!({"path": "/api/publish/v2/something-new"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("code 1001: bad request"), "{message}");
    assert!(message.contains("error.errorCode"), "{message}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn request_responses_are_redacted() {
    let api = MockServer::start().await;
    let app_secret = ["5EC4", "E7A1", "77B2", "0C9D"].concat().repeat(2);
    Mock::given(method("GET"))
        .and(path(
            "/api/cds/app-distirbution/v1/all/app-extra-info/100000001",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "appExtraInfo": {"appSecret": app_secret, "sha256": ["AA:BB"]},
            "echo": format!("Bearer {}", token()),
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), AccessLevel::Read, None)).await;
    let result = call(
        &client,
        "api_request",
        json!({"path": "/api/cds/app-distirbution/v1/all/app-extra-info/100000001"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let raw = text(&result);
    assert!(
        !raw.contains(&app_secret) && !raw.contains(&token()),
        "{raw}"
    );
    let body = json_of(&result);
    assert_eq!(body["result"]["appExtraInfo"]["appSecret"], "***");
    assert!(
        body["note"].as_str().unwrap().contains("`appSecret`"),
        "{body}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn request_rejections_send_nothing() {
    let api = MockServer::start().await;
    let client = connect(server(&api.uri(), AccessLevel::Write, None)).await;
    let cases = [
        (
            json!({"method": "POST", "path": "/api/publish/v2/app-submit", "query": {"appId": "1"}}),
            "--allow-destructive",
        ),
        (
            json!({"method": "DELETE", "path": "/api/publish/v2/app-language-info"}),
            "--allow-destructive",
        ),
        (
            json!({"path": format!("{}/api/publish/v2/app-info", api.uri())}),
            "not a URL",
        ),
        (
            json!({"path": "https://connect-api-dre.cloud.huawei.com/api/publish/v2/app-info"}),
            "not a URL",
        ),
        (
            json!({"path": "/api/publish/v2/../../oauth2/v1/token"}),
            "`..`",
        ),
        (
            json!({"path": "/api/publish//v2/app-info"}),
            "empty path segment",
        ),
        (
            json!({"path": "/api/publish/v2/app-info", "headers": {"Authorization": "Bearer x"}}),
            "cannot be set",
        ),
        (
            json!({"path": "/api/publish/v2/app-info", "headers": {"client_id": "1"}}),
            "cannot be set",
        ),
        (
            json!({"method": "POST", "path": "/api/oauth2/v1/token", "body": {}}),
            "token endpoint",
        ),
        (
            json!({"path": "/api/publish/v2/upload-url/for-obs", "query": {"appId": "1"}}),
            "package_upload",
        ),
        (
            json!({"method": "POST", "path": "/api/publish/v2/upload/multipart/init"}),
            "cannot be called",
        ),
    ];
    for (arguments, expected) in cases {
        let result = call(&client, "api_request", arguments.clone()).await;
        assert_eq!(result.is_error, Some(true), "{arguments}");
        let message = text(&result);
        assert!(message.contains(expected), "{arguments}: {message}");
    }
    assert!(api.received_requests().await.unwrap().is_empty());
    client.cancel().await.unwrap();
}
