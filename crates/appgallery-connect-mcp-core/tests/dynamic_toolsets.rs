//! Динамические наборы инструментов в MCP-сессии: начальный список, `enable_toolset` и
//! `disable_toolset` с уведомлением `notifications/tools/list_changed` (в сессии с
//! `initialize` и через подписку протокола 2026-07-28), неизменный уровень доступа,
//! пользовательские инструменты в наборе своей операции. API подменён `wiremock`.

mod harness;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use appgallery_connect_api::AccessLevel;
use appgallery_connect_mcp_core::ServerOptions;
use harness::{args, call, custom, server, spawn, text, tool_names};
use rmcp::model::{
    CallToolRequestParams, JsonObject, ProtocolVersion, ServerNotification, SubscriptionFilter,
};
use rmcp::service::NotificationContext;
use rmcp::{ClientHandler, ClientServiceExt, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::sync::Notify;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Инструменты динамического режима до включения наборов: поиск, запрос и управление
/// наборами (по имени).
const TOOLSET_TOOLS: [&str; 5] = [
    "api_request",
    "api_search",
    "disable_toolset",
    "enable_toolset",
    "list_toolsets",
];

/// Клиент, который считает уведомления о смене списка инструментов.
#[derive(Clone, Default)]
struct CountingClient {
    changes: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl ClientHandler for CountingClient {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changes.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

fn options(max_access: AccessLevel, dynamic: bool) -> ServerOptions {
    ServerOptions {
        max_access,
        dynamic_toolsets: dynamic,
        ..ServerOptions::default()
    }
}

/// Главный сценарий: агент включает набор, клиент получает уведомление, агент вызывает
/// инструмент набора (с фильтром `jq`) и выключает набор.
#[tokio::test]
async fn enable_toolset_adds_tools_and_notifies() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(query_param("appId", "100000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "languages": [{"lang": "en-US"}, {"lang": "ru-RU"}]
        })))
        .expect(1)
        .mount(&api)
        .await;

    let handler = CountingClient::default();
    let client = handler
        .clone()
        .serve(spawn(server(&api.uri(), options(AccessLevel::Read, true))))
        .await
        .unwrap();

    let info = client.peer_info().unwrap();
    assert_eq!(
        info.capabilities.tools.as_ref().unwrap().list_changed,
        Some(true)
    );
    let instructions = info.instructions.as_deref().unwrap();
    assert!(instructions.contains("`enable_toolset`"), "{instructions}");
    assert!(
        instructions.contains("enabled at start: none"),
        "{instructions}"
    );
    assert_eq!(tool_names(&client).await, TOOLSET_TOOLS);

    // До включения набора инструмент недоступен, ошибка подсказывает `enable_toolset`.
    let result = call(&client, "get_app_info", json!({"appId": "100000001"})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("`enable_toolset`") && message.contains("toolset: \"apps\""),
        "{message}"
    );

    let listed: Value =
        serde_json::from_str(&text(&call(&client, "list_toolsets", json!({})).await)).unwrap();
    let apps = listed["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["toolset"] == "apps")
        .unwrap();
    assert_eq!(apps["enabled"], false);

    let result = call(&client, "enable_toolset", json!({"toolset": "apps"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 1);

    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "get_app_info"), "{names:?}");
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": "100000001", "jq": "[.languages[].lang]"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"["en-US","ru-RU"]"#);

    // Повторное включение не меняет список и не шлёт уведомление.
    let result = call(&client, "enable_toolset", json!({"toolset": "apps"})).await;
    assert!(
        text(&result).contains("already_enabled"),
        "{}",
        text(&result)
    );
    let result = call(&client, "disable_toolset", json!({"toolset": "apps"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 2);
    assert_eq!(tool_names(&client).await, TOOLSET_TOOLS);

    client.cancel().await.unwrap();
}

/// Включённый набор даёт только инструменты разрешённого уровня: `enable_toolset` не
/// повышает доступ.
#[tokio::test]
async fn enabled_toolsets_keep_the_access_level() {
    let api = MockServer::start().await;
    let server = server(&api.uri(), options(AccessLevel::Write, true));
    let client = ().serve(spawn(server.clone())).await.unwrap();
    let result = call(
        &client,
        "enable_toolset",
        json!({"toolset": "listing,reviews"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(server.registry().max_access(), AccessLevel::Write);
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "update_app_info"), "{names:?}");
    assert!(names.iter().any(|n| n == "list_reviews"), "{names:?}");
    // Удаление локализации и публичный ответ на отзыв требуют `Destructive`.
    assert!(
        !names.iter().any(|n| n == "delete_app_language_info"),
        "{names:?}"
    );
    assert!(!names.iter().any(|n| n == "reply_review"), "{names:?}");
    let result = call(
        &client,
        "reply_review",
        json!({"appId": "100000001", "body": {}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("destructive access"),
        "{}",
        text(&result)
    );
    // Аннотации включённых инструментов — по их уровню.
    let tools = client.list_all_tools().await.unwrap();
    for tool in &tools {
        let annotations = tool.annotations.as_ref().unwrap();
        assert_ne!(annotations.destructive_hint, Some(true), "{}", tool.name);
    }
    client.cancel().await.unwrap();

    // На уровне чтения из набора добавляются только инструменты чтения.
    let read_only = ()
        .serve(spawn(harness::server(
            &api.uri(),
            options(AccessLevel::Read, true),
        )))
        .await
        .unwrap();
    let result = call(&read_only, "enable_toolset", json!({"toolset": "listing"})).await;
    let out: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(out["added_tools"], json!(["listing_export"]), "{out}");
    let mut expected: Vec<&str> = TOOLSET_TOOLS.to_vec();
    expected.push("listing_export");
    expected.sort_unstable();
    let mut names = tool_names(&read_only).await;
    names.sort_unstable();
    assert_eq!(names, expected);
    read_only.cancel().await.unwrap();
}

#[tokio::test]
async fn toolsets_from_options_are_enabled_at_start() {
    let api = MockServer::start().await;
    let server = server(
        &api.uri(),
        ServerOptions {
            toolsets: Some(vec!["reviews".into()]),
            ..options(AccessLevel::Read, true)
        },
    );
    assert!(server.registry().get("list_reviews").is_some());
    assert!(server.registry().get("get_app_info").is_none());
    assert!(
        server.instructions().contains("enabled at start: reviews"),
        "{}",
        server.instructions()
    );
}

#[tokio::test]
async fn subscription_receives_list_changed() {
    let api = MockServer::start().await;
    let client = ()
        .serve_with_lifecycle(
            spawn(server(&api.uri(), options(AccessLevel::Read, true))),
            rmcp::ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    let mut subscription = client
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await
        .unwrap();
    assert_eq!(
        subscription.acknowledged(),
        &SubscriptionFilter::builder().tools_list_changed().build()
    );

    let result = call(&client, "enable_toolset", json!({"toolset": "reviews"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let notification = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap()
        .unwrap()
        .expect("tools/list_changed");
    assert!(matches!(
        notification,
        ServerNotification::ToolListChangedNotification(_)
    ));
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "list_reviews"), "{names:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn static_mode_is_unchanged() {
    let api = MockServer::start().await;
    let client =
        ().serve(spawn(server(&api.uri(), options(AccessLevel::Read, false))))
            .await
            .unwrap();
    let info = client.peer_info().unwrap();
    assert_ne!(
        info.capabilities.tools.as_ref().unwrap().list_changed,
        Some(true)
    );
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "get_app_info"));
    assert!(!names.iter().any(|n| n == "enable_toolset"));
    let result = client
        .call_tool(CallToolRequestParams::new("enable_toolset").with_arguments(JsonObject::new()))
        .await;
    assert!(result.is_err(), "неизвестный инструмент — ошибка протокола");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn clones_share_the_session() {
    let api = MockServer::start().await;
    let first = server(&api.uri(), options(AccessLevel::Read, true));
    first
        .call("enable_toolset", args(json!({"toolset": "apps"})))
        .await
        .unwrap();
    assert!(first.registry().get("get_app_info").is_some());
    assert!(first.clone().registry().get("get_app_info").is_some());
}

/// Пользовательский инструмент входит в набор своей операции: включается и выключается
/// вместе с ним.
#[tokio::test]
async fn custom_tools_follow_their_toolset() {
    let api = MockServer::start().await;
    let server = server(
        &api.uri(),
        ServerOptions {
            custom_tools: vec![custom(json!({
                "name": "main_app_info",
                "description": "The main app",
                "operation": "get_app_info",
                "args": {"appId": "100000001"}
            }))],
            ..options(AccessLevel::Read, true)
        },
    );
    assert!(server.registry().get("main_app_info").is_none());
    let reason = server
        .registry()
        .unavailable_reason("main_app_info")
        .unwrap();
    assert!(reason.contains("toolset: \"apps\""), "{reason}");

    let result = server
        .call("enable_toolset", args(json!({"toolset": "apps"})))
        .await
        .unwrap();
    assert!(text(&result).contains("main_app_info"), "{}", text(&result));
    assert!(server.registry().get("main_app_info").is_some());
    let listed = server
        .call("list_toolsets", JsonObject::new())
        .await
        .unwrap();
    let listed: Value = serde_json::from_str(&text(&listed)).unwrap();
    let apps = listed["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["toolset"] == "apps")
        .unwrap()
        .clone();
    // Операции и составные инструменты набора `apps` и один пользовательский.
    let expected = appgallery_connect_api::spec::operations()
        .iter()
        .filter(|op| op.toolset == "apps")
        .count()
        + appgallery_connect_mcp_core::composite::all()
            .iter()
            .filter(|tool| tool.toolset == "apps")
            .count()
        + 1;
    assert_eq!(apps["tools"], expected, "{apps}");

    server
        .call("disable_toolset", args(json!({"toolset": "apps"})))
        .await
        .unwrap();
    assert!(server.registry().get("main_app_info").is_none());
}

/// `api_search` в динамическом режиме: инструмент невключённого набора — с подсказкой
/// `enable_toolset` и вызовом через `api_request`; после включения — `available`.
#[tokio::test]
async fn api_search_status_points_to_enable_toolset() {
    let api = MockServer::start().await;
    let server = server(&api.uri(), options(AccessLevel::Read, true));
    let status = |out: &str| -> String {
        let value: Value = serde_json::from_str(out).unwrap();
        value["status"].as_str().unwrap().to_owned()
    };
    for (tool, toolset) in [("get_app_info", "apps"), ("reviews_summary", "reviews")] {
        let result = server
            .call("api_search", args(json!({"tool_name": tool})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        let found = status(&text(&result));
        assert!(
            found.contains("`enable_toolset`")
                && found.contains(&format!("toolset: \"{toolset}\"")),
            "{tool}: {found}"
        );
    }
    let result = server
        .call("api_search", args(json!({"tool_name": "get_app_info"})))
        .await
        .unwrap();
    assert!(
        status(&text(&result)).contains("`api_request`"),
        "{}",
        text(&result)
    );

    server
        .call("enable_toolset", args(json!({"toolset": "apps"})))
        .await
        .unwrap();
    let result = server
        .call("api_search", args(json!({"tool_name": "get_app_info"})))
        .await
        .unwrap();
    assert_eq!(status(&text(&result)), "available");
}
