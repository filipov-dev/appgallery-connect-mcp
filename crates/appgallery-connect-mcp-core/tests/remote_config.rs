//! Набор `remote-config` (Remote Configuration) через сервер: набор выключен по умолчанию,
//! уровни инструментов, запросы подписывает API-клиент проекта из заголовка `productId` (без
//! него — понятная ошибка без запроса, в HTTP-режиме — с подсказкой про оператора сервера),
//! чтение, история, откат, запись с маской `***` отклоняется, составной `remote_config_set`
//! (разница, `dry_run`, запись с прочитанной версией, конфликт версии). HTTP — `wiremock`,
//! похожие на секреты строки собраны из частей.

use std::sync::Arc;
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, ProjectCredentials, Region,
    RetryPolicy,
};
use appgallery_connect_mcp_core::{AppGalleryServer, ServerOptions, Services};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP_ID: &str = "100000001";
const PROJECT: &str = "736430079245000001";
const CONFIG: &str = "/api/remote-config/v1/config";
const VERSIONS: &str = "/api/remote-config/v1/config/versions";
const ROLLBACK: &str = "/api/remote-config/v1/config/rollback";
const TOKEN: &str = "/api/oauth2/v1/token";

const READ_TOOLS: &[&str] = &["get_remote_config", "list_remote_config_versions"];
const DESTRUCTIVE_TOOLS: &[&str] = &[
    "remote_config_set",
    "rollback_remote_config",
    "update_remote_config",
];

fn team_token() -> String {
    ["team", "-test-", "token-", "1357924680"].concat()
}

fn project_client_id() -> String {
    ["6810", "6420", "1357", "0001"].concat()
}

fn project_client_secret() -> String {
    ["RC12", "Pr34", "Oj56", "Ec78"].concat().repeat(4)
}

fn project_token() -> String {
    ["project", "-test-", "token-", "8642097531"].concat()
}

/// Значение параметра, похожее на JWT: сервер скрывает его в ответах, но записывать
/// обратно должен настоящее.
fn jwt_value() -> String {
    [
        "eyJhbGciOiJIUzI1NiJ9",
        ".",
        "eyJzdWIiOiJyZW1vdGUtY29uZmlnIn0",
        ".",
        "c2lnbmF0dXJlLW9mLXRoZS10ZXN0",
    ]
    .concat()
}

fn api_client(base_url: &str, with_project: bool) -> ApiClient {
    let id = ["2718", "3141", "5926", "5358"].concat();
    let secret = ["Tm12", "Ea34", "Mc56", "Li78"].concat().repeat(4);
    let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
    let mut config = ClientConfig::new(Region::Ru, Some(credentials));
    config.base_url = Some(base_url.to_owned());
    config.profile = Some("work".into());
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    if with_project {
        let project =
            ApiClientCredentials::from_parts(project_client_id(), project_client_secret()).unwrap();
        config.projects = vec![ProjectCredentials::new(PROJECT, project).unwrap()];
    }
    ApiClient::with_token_source(config, Arc::new(StaticToken::new(team_token()))).unwrap()
}

struct Fixture {
    api: MockServer,
    server: AppGalleryServer,
}

async fn fixture_with(
    access: AccessLevel,
    toolsets: Option<&[&str]>,
    with_project: bool,
    http: bool,
) -> Fixture {
    let api = MockServer::start().await;
    let options = ServerOptions {
        max_access: access,
        toolsets: toolsets.map(|sets| sets.iter().map(|s| (*s).to_owned()).collect()),
        profile: Some("work".into()),
        apps: vec![APP_ID.to_owned()],
        http,
        ..ServerOptions::default()
    };
    let client = api_client(&api.uri(), with_project);
    let server = AppGalleryServer::new(Services::new(client), options).unwrap();
    Fixture { api, server }
}

async fn fixture() -> Fixture {
    fixture_with(
        AccessLevel::Destructive,
        Some(&["remote-config"]),
        true,
        false,
    )
    .await
}

impl Fixture {
    async fn call(&self, name: &str, args: Value) -> CallToolResult {
        let Value::Object(args) = args else {
            panic!("аргументы не объект");
        };
        self.server.call(name, args).await.unwrap()
    }

    async fn requests(&self) -> Vec<wiremock::Request> {
        self.api.received_requests().await.unwrap()
    }

    /// Получение токена клиентом проекта (один раз за тест).
    async fn mount_project_token(&self) {
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .and(body_partial_json(json!({
                "grant_type": "client_credentials",
                "client_id": project_client_id(),
                "client_secret": project_client_secret(),
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": project_token(), "expires_in": 172_800})),
            )
            .expect(1)
            .mount(&self.api)
            .await;
    }

    /// Запрос, подписанный клиентом проекта, с заголовками проекта и приложения.
    fn signed(&self, http_method: &str, route: &str) -> wiremock::MockBuilder {
        Mock::given(method(http_method))
            .and(path(route.to_owned()))
            .and(header("client_id", project_client_id().as_str()))
            .and(header(
                "authorization",
                format!("Bearer {}", project_token()).as_str(),
            ))
            .and(header("productId", PROJECT))
            .and(header("appId", APP_ID))
    }

    async fn mount_config(&self) {
        self.signed("GET", CONFIG)
            .respond_with(ResponseTemplate::new(200).set_body_json(current_config()))
            .mount(&self.api)
            .await;
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
    let text = text(result);
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("не JSON ({err}): {text}"))
}

fn failure(result: &CallToolResult) -> String {
    assert_eq!(result.is_error, Some(true), "{}", text(result));
    text(result)
}

fn items() -> Value {
    json!([
        {
            "name": "welcome_text",
            "desc": "",
            "defaultValue": {"value": "Hello", "useSdkValue": false},
            "filterValues": [{"name": "russian", "value": {"value": "Привет", "useSdkValue": false}}]
        },
        {"name": "partner_token", "defaultValue": {"value": jwt_value(), "useSdkValue": false}}
    ])
}

fn filters() -> Value {
    json!([{"name": "russian", "desc": "", "language": {"values": ["ru"]}, "audiences": []}])
}

fn current_config() -> Value {
    json!({
        "ret": {"code": 0, "msg": "OK"},
        "versionInfo": {"version": 7, "updateTime": 1_700_000_000_000_i64},
        "configItems": items(),
        "filters": filters(),
    })
}

fn remote_tools(f: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = f
        .server
        .registry()
        .iter()
        .map(|tool| tool.tool.name.to_string())
        .filter(|name| name.contains("remote_config"))
        .collect();
    names.sort();
    names
}

fn sorted(lists: &[&[&str]]) -> Vec<String> {
    let mut names: Vec<String> = lists.concat().iter().map(|s| (*s).to_owned()).collect();
    names.sort();
    names
}

#[tokio::test]
async fn toolset_is_off_by_default_and_tools_follow_access_levels() {
    let f = fixture_with(AccessLevel::Destructive, None, true, false).await;
    assert!(remote_tools(&f).is_empty(), "{:?}", remote_tools(&f));
    let set = Some(&["remote-config"][..]);
    for (access, expected) in [
        (AccessLevel::Read, sorted(&[READ_TOOLS])),
        (AccessLevel::Write, sorted(&[READ_TOOLS])),
        (
            AccessLevel::Destructive,
            sorted(&[READ_TOOLS, DESTRUCTIVE_TOOLS]),
        ),
    ] {
        let f = fixture_with(access, set, true, false).await;
        assert_eq!(remote_tools(&f), expected, "{access:?}");
    }
    let f = fixture().await;
    let registry = f.server.registry();
    for name in DESTRUCTIVE_TOOLS {
        let tool = registry.get(name).unwrap_or_else(|| panic!("{name}"));
        let hints = tool.tool.annotations.as_ref().unwrap();
        assert_eq!(hints.destructive_hint, Some(true), "{name}");
        let description = tool.tool.description.as_deref().unwrap();
        assert!(description.contains("at once"), "{name}: {description}");
    }
    let description = registry
        .get("get_remote_config")
        .unwrap()
        .tool
        .description
        .as_deref()
        .unwrap();
    assert!(
        description.contains("Project-level API client"),
        "{description}"
    );
    // Недоступный на уровне чтения инструмент объясняет, какой флаг нужен.
    let f = fixture_with(AccessLevel::Read, set, true, false).await;
    let text = failure(&f.call("update_remote_config", json!({})).await);
    assert!(text.contains("destructive access"), "{text}");
}

#[tokio::test]
async fn reads_are_signed_with_the_project_client() {
    let f = fixture().await;
    f.mount_project_token().await;
    f.mount_config().await;
    f.signed("GET", VERSIONS)
        .and(query_param("fromPage", "2"))
        .and(query_param("pageSize", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "OK"},
            "versionInfos": [{"version": 7, "updateTime": 1_700_000_000_000_i64}],
            "total": 21,
        })))
        .expect(1)
        .mount(&f.api)
        .await;

    let body = ok(&f
        .call(
            "get_remote_config",
            json!({"productId": PROJECT, "appId": APP_ID}),
        )
        .await);
    // Значение, похожее на токен, скрыто, и пометка говорит об этом.
    let result = &body["result"];
    assert_eq!(result["versionInfo"]["version"], 7);
    assert_eq!(result["configItems"][0]["defaultValue"]["value"], "Hello");
    assert_eq!(result["configItems"][1]["defaultValue"]["value"], "***");
    assert!(body["note"].as_str().unwrap().contains("hidden"), "{body}");
    assert!(!body.to_string().contains(&jwt_value()));

    let body = ok(&f
        .call(
            "list_remote_config_versions",
            json!({"productId": PROJECT, "appId": APP_ID, "fromPage": 2, "pageSize": 50}),
        )
        .await);
    assert_eq!(body["total"], 21);

    // api_request к пути операции выбирает тот же клиент проекта.
    let body = ok(&f
        .call(
            "api_request",
            json!({"method": "GET", "path": CONFIG, "headers": {"productId": PROJECT, "appId": APP_ID}}),
        )
        .await);
    assert_eq!(body["result"]["versionInfo"]["version"], 7);
    // Токен клиента команды не уходил ни в один запрос.
    for request in f.requests().await {
        let auth = request
            .headers
            .get("authorization")
            .map(|value| value.to_str().unwrap().to_owned());
        assert_ne!(auth, Some(format!("Bearer {}", team_token())));
    }
}

#[tokio::test]
async fn missing_project_client_is_an_error_without_a_request() {
    let f = fixture_with(
        AccessLevel::Destructive,
        Some(&["remote-config"]),
        false,
        false,
    )
    .await;
    for (name, args) in [
        (
            "get_remote_config",
            json!({"productId": PROJECT, "appId": APP_ID}),
        ),
        (
            "remote_config_set",
            json!({"productId": PROJECT, "parameters": [{"name": "a", "value": "1"}]}),
        ),
    ] {
        let text = failure(&f.call(name, args).await);
        assert!(
            text.contains(&format!(
                "auth project add --profile work --project-id {PROJECT}"
            )),
            "{name}: {text}"
        );
        assert!(text.contains("not sent"), "{name}: {text}");
    }
    // Без productId у составного и без проектов в профиле — подсказка, где взять ID.
    let text = failure(
        &f.call(
            "remote_config_set",
            json!({"parameters": [{"name": "a", "value": "1"}]}),
        )
        .await,
    );
    assert!(text.contains("`productId` is required"), "{text}");
    assert!(f.requests().await.is_empty());

    // HTTP-режим: клиента проекта добавляет оператор сервера.
    let f = fixture_with(AccessLevel::Read, Some(&["remote-config"]), false, true).await;
    let text = failure(
        &f.call(
            "get_remote_config",
            json!({"productId": PROJECT, "appId": APP_ID}),
        )
        .await,
    );
    assert!(text.contains("server operator"), "{text}");
    assert!(!text.contains("--profile work"), "{text}");
    assert!(f.requests().await.is_empty());
}

#[tokio::test]
async fn masked_values_are_never_written() {
    let f = fixture().await;
    let body = json!({
        "version": 7,
        "configItems": [{"name": "partner_token", "defaultValue": {"value": "***"}}],
    });
    let text = failure(
        &f.call(
            "update_remote_config",
            json!({"productId": PROJECT, "appId": APP_ID, "body": body}),
        )
        .await,
    );
    assert!(
        text.contains("`body.configItems[0].defaultValue.value` contains `***`"),
        "{text}"
    );
    assert!(text.contains("remote_config_set"), "{text}");
    let text = failure(
        &f.call(
            "api_request",
            json!({
                "method": "PUT",
                "path": CONFIG,
                "headers": {"productId": PROJECT, "appId": APP_ID},
                "body": body,
            }),
        )
        .await,
    );
    assert!(text.contains("contains `***`"), "{text}");
    let text = failure(
        &f.call(
            "remote_config_set",
            json!({"productId": PROJECT, "parameters": [{"name": "partner_token", "value": "***"}]}),
        )
        .await,
    );
    assert!(text.contains("contains `***`"), "{text}");
    assert!(f.requests().await.is_empty());
}

#[tokio::test]
async fn update_and_rollback_operations() {
    let f = fixture().await;
    f.mount_project_token().await;
    let body = json!({"version": 7, "configItems": items(), "filters": filters()});
    f.signed("PUT", CONFIG)
        .and(body_json(body.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ret": {"code": 0, "msg": "OK"}, "version": 8})),
        )
        .expect(1)
        .mount(&f.api)
        .await;
    f.signed("POST", ROLLBACK)
        .and(body_json(json!({"version": 5})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ret": {"code": 0, "msg": "OK"}, "version": 9})),
        )
        .expect(1)
        .mount(&f.api)
        .await;
    let result = ok(&f
        .call(
            "update_remote_config",
            json!({"productId": PROJECT, "appId": APP_ID, "body": body}),
        )
        .await);
    assert_eq!(result["version"], 8);
    let result = ok(&f
        .call(
            "rollback_remote_config",
            json!({"productId": PROJECT, "appId": APP_ID, "body": {"version": 5}}),
        )
        .await);
    assert_eq!(result["version"], 9);
}

#[tokio::test]
async fn remote_config_set_dry_run_does_not_write() {
    let f = fixture().await;
    f.mount_project_token().await;
    f.mount_config().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&f.api)
        .await;
    // productId — единственный проект профиля, appId — приложение по умолчанию.
    let result = ok(&f
        .call(
            "remote_config_set",
            json!({"parameters": [{"name": "welcome_text", "value": "Hi"}]}),
        )
        .await);
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["applied"], false);
    assert_eq!(result["version"], 7);
    assert_eq!(result["productId"], PROJECT);
    let change = &result["changes"]["parameters"][0];
    assert_eq!(change["name"], "welcome_text");
    assert_eq!(change["action"], "update");
    assert_eq!(change["before"]["defaultValue"]["value"], "Hello");
    assert_eq!(change["after"]["defaultValue"]["value"], "Hi");
    assert_eq!(result["request"]["configItems"], 2);
    assert!(
        result["notes"][0]
            .as_str()
            .unwrap()
            .contains("baseVersion 7"),
        "{result}"
    );
}

#[tokio::test]
async fn remote_config_set_writes_with_the_read_version() {
    let f = fixture().await;
    f.mount_project_token().await;
    f.mount_config().await;
    // Неизменённый параметр с похожим на токен значением уходит настоящим, а не `***`.
    let mut expected_items = items();
    expected_items[0]["defaultValue"]["value"] = json!("Hi");
    let expected = json!({"version": 7, "configItems": expected_items, "filters": filters()});
    f.signed("PUT", CONFIG)
        .and(body_json(expected))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ret": {"code": 0, "msg": "OK"}, "version": 8})),
        )
        .expect(1)
        .mount(&f.api)
        .await;
    let result = ok(&f
        .call(
            "remote_config_set",
            json!({
                "productId": PROJECT,
                "appId": APP_ID,
                "parameters": [{"name": "welcome_text", "value": "Hi"}],
                "baseVersion": 7,
                "dry_run": false,
            }),
        )
        .await);
    assert_eq!(result["applied"], true);
    assert_eq!(result["newVersion"], 8);
    assert!(!result.to_string().contains(&jwt_value()));
}

#[tokio::test]
async fn remote_config_set_reports_version_conflicts() {
    let f = fixture().await;
    f.mount_project_token().await;
    f.mount_config().await;
    f.signed("PUT", CONFIG)
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"ret": {"code": 204_083_201, "msg": "The version has been modified."}}),
        ))
        .expect(1)
        .mount(&f.api)
        .await;
    let args = json!({
        "productId": PROJECT,
        "parameters": [{"name": "welcome_text", "value": "Hi"}],
        "dry_run": false,
    });
    let text = failure(&f.call("remote_config_set", args.clone()).await);
    assert!(text.contains("changed by someone else"), "{text}");
    assert!(text.contains("Nothing was written"), "{text}");

    // Версия ушла вперёд после просмотра разницы: запись не отправляется.
    let mut stale = args;
    stale["baseVersion"] = json!(6);
    let text = failure(&f.call("remote_config_set", stale).await);
    assert!(text.contains("not at baseVersion 6"), "{text}");
    let puts = f
        .requests()
        .await
        .iter()
        .filter(|request| request.method.as_str() == "PUT")
        .count();
    assert_eq!(puts, 1);
}
