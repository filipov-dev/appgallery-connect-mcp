//! Общее для тестов фильтра `jq`, пользовательских инструментов и динамических наборов:
//! клиент API против `wiremock` с выдуманными учётными данными и статическим токеном, сервер
//! ядра и сессия `rmcp` поверх `tokio::io::duplex`.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy};
use appgallery_connect_mcp_core::{AppGalleryServer, CustomTool, ServerOptions, Services};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, JsonObject};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

/// Выдуманный идентификатор API-клиента, собранный из частей.
pub fn client_id() -> String {
    ["2718", "2818", "2845", "9045"].concat()
}

/// Выдуманный секрет API-клиента, собранный из частей.
pub fn client_secret() -> String {
    ["CD12", "AB34", "9F56", "7E80"].concat().repeat(4)
}

/// Токен, который тестовый клиент отправляет в `Authorization`.
pub fn token() -> String {
    ["harness", "-token-", "value-", "0123456789"].concat()
}

/// Лимит ответа сервера в тестах.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024;

pub fn api_client(base_url: &str) -> ApiClient {
    let credentials = ApiClientCredentials::from_parts(client_id(), client_secret()).unwrap();
    let mut config = ClientConfig::new(Region::Cn, Some(credentials));
    config.base_url = Some(base_url.to_owned());
    config.max_response_bytes = MAX_RESPONSE_BYTES;
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(10),
    };
    ApiClient::with_token_source(config, Arc::new(StaticToken::new(token()))).unwrap()
}

pub fn server(base_url: &str, options: ServerOptions) -> AppGalleryServer {
    AppGalleryServer::new(Services::new(api_client(base_url)), options).unwrap()
}

/// Запускает сервер в фоне и возвращает конец канала для клиента.
pub fn spawn(server: AppGalleryServer) -> tokio::io::DuplexStream {
    let (server_io, client_io) = tokio::io::duplex(4 * 1024 * 1024);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    client_io
}

pub async fn connect(server: AppGalleryServer) -> RunningService<RoleClient, ()> {
    ().serve(spawn(server)).await.unwrap()
}

pub fn args(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        other => panic!("не объект: {other}"),
    }
}

pub async fn call<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
    name: &str,
    arguments: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(arguments)))
        .await
        .unwrap()
}

pub fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block: &ContentBlock| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn json_of(result: &CallToolResult) -> Value {
    let text = text(result);
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("не JSON ({err}): {text}"))
}

pub async fn tool_names<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
) -> Vec<String> {
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    names
}

/// Пользовательский инструмент из определения в JSON (как из `tools.toml`).
pub fn custom(definition: Value) -> CustomTool {
    CustomTool::compile(serde_json::from_value(definition).unwrap(), "tools.toml").unwrap()
}
