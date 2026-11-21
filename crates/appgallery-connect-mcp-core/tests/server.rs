//! MCP-сессия с сервером ядра в памяти (`rmcp` поверх `tokio::io::duplex`): список
//! инструментов при разных уровнях доступа и наборах, аннотации, вызовы операций против
//! `wiremock`, ошибки AppGallery Connect как `isError: true`, вырезание секретов. Сеть,
//! настоящие учётные данные и системный keychain не используются.

use std::sync::Arc;
use std::time::Duration;

use appgallery_connect_api::spec::{find, operations, toolsets};
use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy,
};
use appgallery_connect_mcp_core::{AppGalleryServer, ServerOptions, Services, default_toolsets};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Выдуманный идентификатор API-клиента, собранный из частей.
fn client_id() -> String {
    ["3141", "5926", "5358", "9793"].concat()
}

/// Выдуманный секрет API-клиента, собранный из частей.
fn client_secret() -> String {
    ["AB12", "CD34", "EF56", "7890"].concat().repeat(4)
}

/// Токен, который тестовый клиент отправляет в `Authorization`.
fn token() -> String {
    ["core", "-server-", "token-", "0123456789"].concat()
}

fn api_client(base_url: &str, region: Region) -> ApiClient {
    let credentials = ApiClientCredentials::from_parts(client_id(), client_secret()).unwrap();
    let mut config = ClientConfig::new(region, Some(credentials));
    config.base_url = Some(base_url.to_owned());
    config.max_response_bytes = 64 * 1024;
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(10),
    };
    ApiClient::with_token_source(config, Arc::new(StaticToken::new(token()))).unwrap()
}

/// Настройки без составных инструментов: тесты инструментов операций сверяют список с
/// таблицей операций. Составные проверяются отдельно ([`with_composites`]).
fn options(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ServerOptions {
    ServerOptions {
        composite_tools: Vec::new(),
        ..with_composites(max_access, toolsets)
    }
}

/// Настройки со всеми составными инструментами сервера.
fn with_composites(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ServerOptions {
    ServerOptions {
        max_access,
        toolsets: toolsets.map(|t| t.iter().map(|s| (*s).to_owned()).collect()),
        profile: Some("work".into()),
        ..ServerOptions::default()
    }
}

fn server(base_url: &str, options: ServerOptions) -> AppGalleryServer {
    AppGalleryServer::new(Services::new(api_client(base_url, Region::Ru)), options).unwrap()
}

async fn connect(server: AppGalleryServer) -> RunningService<RoleClient, ()> {
    let (server_io, client_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    ().serve(client_io).await.unwrap()
}

fn args(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("не объект: {other}"),
    }
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(arguments)))
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

/// Служебные инструменты ядра, которые регистрируются всегда.
const BUILT_IN: &[&str] = &["api_search", "api_request"];

/// Инструменты уровня и наборов, по имени: операции и служебные.
fn expected(access: AccessLevel, toolsets: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = operations()
        .iter()
        .filter(|op| access.allows(op.access) && toolsets.contains(&op.toolset))
        .map(|op| op.tool_name.to_owned())
        .chain(BUILT_IN.iter().map(|name| (*name).to_owned()))
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn tool_list_follows_access_and_toolsets() {
    let all: Vec<&str> = toolsets().iter().map(|t| t.slug).collect();
    type Case<'a> = (AccessLevel, Option<&'a [&'a str]>, Vec<&'a str>);
    let cases: Vec<Case> = vec![
        (AccessLevel::Read, None, default_toolsets()),
        (AccessLevel::Write, None, default_toolsets()),
        (AccessLevel::Destructive, None, default_toolsets()),
        (AccessLevel::Read, Some(&["all"]), all.clone()),
        (AccessLevel::Destructive, Some(&["all"]), all.clone()),
        (
            AccessLevel::Write,
            Some(&["products", "provisioning"]),
            vec!["products", "provisioning"],
        ),
    ];
    for (access, chosen, slugs) in cases {
        let client = connect(server("http://127.0.0.1:9", options(access, chosen))).await;
        let tools = client.list_all_tools().await.unwrap();
        let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(names, expected(access, &slugs), "{access:?} {chosen:?}");
        assert!(!names.is_empty(), "{access:?} {chosen:?}");
        for tool in tools.iter().filter(|t| !BUILT_IN.contains(&&*t.name)) {
            let op = find(&tool.name).unwrap();
            let annotations = tool.annotations.as_ref().unwrap();
            assert_eq!(annotations.open_world_hint, Some(true), "{}", tool.name);
            assert_eq!(
                annotations.read_only_hint,
                Some(op.access == AccessLevel::Read),
                "{}",
                tool.name
            );
            if op.access != AccessLevel::Read {
                assert_eq!(
                    annotations.destructive_hint,
                    Some(op.access == AccessLevel::Destructive),
                    "{}",
                    tool.name
                );
            }
            assert!(access.allows(op.access), "{}", tool.name);
        }
        let info = client.peer_info().unwrap();
        assert_eq!(
            info.server_info.as_ref().map(|s| s.name.as_str()),
            Some("agc-work")
        );
        let instructions = info.instructions.as_deref().unwrap();
        assert!(
            instructions.contains("the profile `work`"),
            "{instructions}"
        );
        assert!(instructions.contains("connect-api-drru"), "{instructions}");
        client.cancel().await.unwrap();
    }
}

#[tokio::test]
async fn without_flags_no_tool_changes_data() {
    // Даже со всеми наборами без флагов доступа нет ни одного инструмента записи или
    // удаления.
    let client = connect(server(
        "http://127.0.0.1:9",
        options(AccessLevel::Read, Some(&["all"])),
    ))
    .await;
    let tools = client.list_all_tools().await.unwrap();
    assert!(!tools.is_empty());
    for tool in &tools {
        let access = match find(&tool.name) {
            Some(op) => op.access,
            None if BUILT_IN.contains(&&*tool.name) => AccessLevel::Read,
            None => panic!("{}", tool.name),
        };
        assert_eq!(access, AccessLevel::Read, "{}", tool.name);
        let annotations = tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(true), "{}", tool.name);
        assert_ne!(annotations.destructive_hint, Some(true), "{}", tool.name);
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn unregistered_tools_explain_the_flag_and_unknown_tools_fail() {
    let api = MockServer::start().await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read, None))).await;

    let result = call(
        &client,
        "update_app_info",
        json!({"appId": "1", "body": {}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("write access"), "{}", text(&result));

    let result = call(&client, "submit_app", json!({"appId": "1"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("destructive access"),
        "{}",
        text(&result)
    );

    let result = call(&client, "list_certificates", json!({})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("toolset `provisioning`"), "{message}");
    assert!(message.contains("--toolsets"), "{message}");

    // Совсем неизвестное имя — ошибка протокола.
    let err = client
        .call_tool(CallToolRequestParams::new("no_such_tool"))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("unknown tool `no_such_tool`"),
        "{err}"
    );
    // Ничего не ушло в API.
    assert!(api.received_requests().await.unwrap().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn read_tool_calls_the_api() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(query_param("appId", "100000001"))
        .and(query_param("lang", "en-US"))
        .and(header("client_id", client_id().as_str()))
        .and(header(
            "authorization",
            format!("Bearer {}", token()).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "appInfo": {"releaseState": 0, "defaultLang": "en-US"},
        })))
        .expect(1)
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read, None))).await;
    // `appId` числом: исполнитель передаст его строкой.
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": 100000001, "lang": "en-US", "releaseType": null}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["appInfo"]["defaultLang"], "en-US");
    assert!(body.get("note").is_none(), "{body}");
    let raw = text(&result);
    assert!(
        !raw.contains(&token()) && !raw.contains(&client_secret()),
        "{raw}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn invalid_arguments_are_rejected_before_the_request() {
    let api = MockServer::start().await;
    let client = connect(server(&api.uri(), options(AccessLevel::Destructive, None))).await;
    let cases = [
        (
            "get_app_info",
            json!({}),
            "missing required argument(s): `appId`",
        ),
        (
            "get_app_info",
            json!({"appId": "1", "language": "en"}),
            "unknown argument(s) `language`",
        ),
        (
            "get_app_info",
            json!({"appId": "1", "releaseType": "full"}),
            "`releaseType` must be an integer",
        ),
        (
            "get_app_info",
            json!({"appId": [1]}),
            "must be a single value",
        ),
        (
            "reply_review",
            json!({"requestId": "1", "body": [1]}),
            "must be a JSON object",
        ),
    ];
    for (tool, arguments, expected) in cases {
        let result = call(&client, tool, arguments.clone()).await;
        assert_eq!(result.is_error, Some(true), "{tool} {arguments}");
        let message = text(&result);
        assert!(message.contains(expected), "{tool} {arguments}: {message}");
    }
    assert!(api.received_requests().await.unwrap().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn api_errors_are_tool_errors() {
    let api = MockServer::start().await;
    // HTTP 403 с конвертом результата.
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(query_param("appId", "100000001"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "ret": {"code": 204144647, "msg": "no permission"},
        })))
        .mount(&api)
        .await;
    // HTTP 200 с ненулевым кодом результата.
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(query_param("appId", "100000002"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 204144660, "msg": "Invalid appId."},
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read, None))).await;

    let result = call(&client, "get_app_info", json!({"appId": "100000001"})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP 403"), "{message}");
    assert!(
        message.contains("code 204144647: no permission"),
        "{message}"
    );
    assert!(message.contains("Users and permissions"), "{message}");

    let result = call(&client, "get_app_info", json!({"appId": "100000002"})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("code 204144660: Invalid appId."),
        "{message}"
    );
    assert!(
        message.contains("even though the HTTP status is 200"),
        "{message}"
    );
    client.cancel().await.unwrap();

    // Сеть недоступна — тоже управляемая ошибка, процесс жив.
    let client = connect(server(
        "http://127.0.0.1:9",
        options(AccessLevel::Read, None),
    ))
    .await;
    let result = call(&client, "get_app_info", json!({"appId": "1"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("could not reach AppGallery Connect"),
        "{}",
        text(&result)
    );
    let tools = client.list_all_tools().await.unwrap();
    assert!(!tools.is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn secrets_are_redacted_from_responses() {
    let api = MockServer::start().await;
    let app_secret = ["5EC4", "E7A1", "77B2", "0C9D"].concat().repeat(2);
    let jwt = ["eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiJ4In0", "c2lnbmF0dXJl"].join(".");
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "appInfo": {
                "appName": "Demo",
                "appSecret": app_secret,
                "clientSecret": client_secret(),
                "comment": format!("token {jwt} and Bearer {}", token()),
            },
            "echo": format!("client {}", client_id()),
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read, None))).await;
    let result = call(&client, "get_app_info", json!({"appId": "100000001"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let raw = text(&result);
    for secret in [&app_secret, &client_secret(), &jwt, &token(), &client_id()] {
        assert!(!raw.contains(secret.as_str()), "{raw}");
    }
    let body = json_of(&result);
    assert_eq!(body["result"]["appInfo"]["appName"], "Demo");
    assert_eq!(body["result"]["appInfo"]["appSecret"], "***");
    assert_eq!(body["result"]["appInfo"]["clientSecret"], "***");
    let note = body["note"].as_str().unwrap();
    assert!(note.contains("`appSecret`, `clientSecret`"), "{note}");
    assert!(note.contains("2 text value(s)"), "{note}");

    // Ошибки тоже проходят вырезание.
    api.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ret": {"code": 1, "msg": format!("bad client_secret={}", client_secret())},
        })))
        .mount(&api)
        .await;
    let result = call(&client, "get_app_info", json!({"appId": "100000001"})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP 400"), "{message}");
    assert!(!message.contains(&client_secret()), "{message}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn write_tools_send_the_body() {
    let api = MockServer::start().await;
    let op = find("update_app_language_info").unwrap();
    Mock::given(method(op.method.as_str()))
        .and(path(op.path))
        .and(query_param("appId", "100000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
        })))
        .expect(1)
        .mount(&api)
        .await;
    let client = connect(server(
        &api.uri(),
        options(AccessLevel::Write, Some(&["listing"])),
    ))
    .await;
    let result = call(
        &client,
        "update_app_language_info",
        json!({"appId": "100000001", "body": {"lang": "en-US", "appName": "Demo"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let requests = api.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent, json!({"lang": "en-US", "appName": "Demo"}));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn reviews_summary_is_registered_with_the_reviews_toolset() {
    // Уровень чтения и набор `reviews`: инструмент виден, только на чтение.
    let client = connect(server(
        "http://127.0.0.1:9",
        with_composites(AccessLevel::Read, Some(&["reviews"])),
    ))
    .await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|tool| tool.name == "reviews_summary")
        .unwrap();
    let annotations = tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(true));
    let description = tool.description.as_deref().unwrap();
    assert!(description.contains("`reply_review`"), "{description}");
    assert!(description.ends_with("Access: read-only."), "{description}");
    let info = client.peer_info().unwrap();
    let instructions = info.instructions.as_deref().unwrap();
    assert!(
        instructions.contains("- Composite tools:"),
        "{instructions}"
    );
    client.cancel().await.unwrap();

    // Без набора `reviews` инструмента нет, а вызов объясняет, какой набор нужен.
    let client = connect(server(
        "http://127.0.0.1:9",
        with_composites(AccessLevel::Destructive, Some(&["apps"])),
    ))
    .await;
    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.iter().all(|tool| tool.name != "reviews_summary"));
    let result = call(&client, "reviews_summary", json!({"countries": ["RU"]})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("toolset `reviews`"),
        "{}",
        text(&result)
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn reviews_summary_reports_envelope_errors_as_tool_errors() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/reviews/v1/manage/dev/reviews"))
        .and(query_param("appId", "100000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": 204144647, "rtnDesc": "no permission"},
        })))
        .expect(1)
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), with_composites(AccessLevel::Read, None))).await;

    // Неизвестный аргумент отклоняется до запроса.
    let result = call(
        &client,
        "reviews_summary",
        json!({"appId": "100000001", "countries": ["RU"], "author": "x"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("unknown argument"),
        "{}",
        text(&result)
    );

    let result = call(
        &client,
        "reviews_summary",
        json!({"appId": "100000001", "countries": ["RU"]}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("code 204144647: no permission"),
        "{message}"
    );
    assert!(message.contains("page 1"), "{message}");
    assert!(!message.contains(&token()), "{message}");
    client.cancel().await.unwrap();
}
