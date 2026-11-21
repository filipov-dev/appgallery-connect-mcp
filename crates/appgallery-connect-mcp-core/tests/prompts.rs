//! Промпты через MCP-клиент `rmcp`: capability, `prompts/list` при разных уровнях доступа,
//! `prompts/get` с аргументами и без, проверка аргументов, текст по уровню доступа и режиму
//! наборов. Запросов к API нет.

mod harness;

use appgallery_connect_api::AccessLevel;
use appgallery_connect_mcp_core::{ServerOptions, prompts};
use harness::{args, call, connect, server};
use rmcp::RoleClient;
use rmcp::model::GetPromptRequestParams;
use rmcp::service::RunningService;
use serde_json::{Value, json};

fn options(max_access: AccessLevel) -> ServerOptions {
    ServerOptions {
        max_access,
        ..ServerOptions::default()
    }
}

async fn names(client: &RunningService<RoleClient, ()>) -> Vec<String> {
    client
        .list_all_prompts()
        .await
        .unwrap()
        .iter()
        .map(|p| p.name.to_string())
        .collect()
}

async fn prompt_text(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> String {
    let result = client
        .get_prompt(GetPromptRequestParams::new(name).with_arguments(args(arguments)))
        .await
        .unwrap();
    assert!(result.description.is_some());
    assert_eq!(result.messages.len(), 1);
    let message = serde_json::to_value(&result.messages[0]).unwrap();
    assert_eq!(message["role"], "user");
    message["content"]["text"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn prompts_are_listed_by_access_level() {
    let all: Vec<String> = prompts::all().iter().map(|p| p.name.to_string()).collect();
    for (access, expected) in [
        (
            AccessLevel::Read,
            all.iter()
                .filter(|n| *n != "listing_update")
                .cloned()
                .collect::<Vec<_>>(),
        ),
        (AccessLevel::Write, all.clone()),
        (AccessLevel::Destructive, all.clone()),
    ] {
        let client = connect(server("http://127.0.0.1:9", options(access))).await;
        let info = client.peer_info().unwrap();
        assert!(info.capabilities.prompts.is_some(), "{access:?}");
        assert_eq!(names(&client).await, expected, "{access:?}");
        client.cancel().await.unwrap();
    }

    let client = connect(server("http://127.0.0.1:9", options(AccessLevel::Read))).await;
    let listed = client.list_all_prompts().await.unwrap();
    let triage = listed.iter().find(|p| p.name == "triage_reviews").unwrap();
    assert_eq!(triage.title.as_deref(), Some("Triage user reviews"));
    let arguments = triage.arguments.as_ref().unwrap();
    assert_eq!(arguments[0].name, "appId");
    assert_eq!(arguments[0].required, Some(false));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn prompt_text_follows_the_access_level() {
    let client = connect(server("http://127.0.0.1:9", options(AccessLevel::Read))).await;
    // Без аргументов: у промпта нет обязательных.
    let result = client
        .get_prompt(GetPromptRequestParams::new("prepare_release"))
        .await
        .unwrap();
    let text = serde_json::to_value(&result.messages[0]).unwrap()["content"]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        text.contains("this server runs with read-only access"),
        "{text}"
    );
    assert!(
        text.contains("`release_submit` (destructive; not available on this server"),
        "{text}"
    );
    assert!(
        text.contains("`package_upload` (write; not available"),
        "{text}"
    );
    assert!(text.contains("the app the user names"), "{text}");
    // Наборы по умолчанию: apps, releases, reviews — listing не включён.
    assert!(
        text.contains("Not enabled on this server: listing"),
        "{text}"
    );

    let text = prompt_text(&client, "triage_reviews", json!({"appId": "100000001"})).await;
    assert!(text.contains("app 100000001"), "{text}");
    assert!(text.contains("cannot be called here"), "{text}");
    client.cancel().await.unwrap();

    let client = connect(server(
        "http://127.0.0.1:9",
        ServerOptions {
            apps: vec!["100000002".into()],
            ..options(AccessLevel::Destructive)
        },
    ))
    .await;
    let text = prompt_text(
        &client,
        "prepare_release",
        json!({"release_type": "phased"}),
    )
    .await;
    assert!(
        text.contains("the profile's default app 100000002"),
        "{text}"
    );
    assert!(
        text.contains("`release_submit` (destructive) with releaseType phased"),
        "{text}"
    );
    assert!(text.contains("explicit confirmation"), "{text}");
    assert!(!text.contains("not available"), "{text}");
    let text = prompt_text(&client, "triage_reviews", json!({"countries": "RU"})).await;
    assert!(
        text.contains("`reply_review` (destructive) with requestId"),
        "{text}"
    );
    assert!(text.contains("countries [RU]"), "{text}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn prompt_text_follows_dynamic_toolsets() {
    let client = connect(server(
        "http://127.0.0.1:9",
        ServerOptions {
            dynamic_toolsets: true,
            ..options(AccessLevel::Read)
        },
    ))
    .await;
    let text = prompt_text(&client, "products_overview", json!({})).await;
    assert!(text.contains("Not enabled yet: products, apps."), "{text}");
    assert!(
        text.contains("`enable_toolset` with toolset \"products,apps\""),
        "{text}"
    );

    // После включения наборов в сессии промпт больше не просит их включить.
    let result = call(
        &client,
        "enable_toolset",
        json!({"toolset": "products,apps"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true));
    let text = prompt_text(&client, "products_overview", json!({})).await;
    assert!(!text.contains("Not enabled"), "{text}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn prompt_arguments_are_checked() {
    let client = connect(server("http://127.0.0.1:9", options(AccessLevel::Write))).await;
    let text = prompt_text(&client, "listing_update", json!({"dir": "metadata"})).await;
    assert!(text.contains("from the directory metadata"), "{text}");

    for (name, arguments) in [
        ("listing_update", json!({})),
        ("listing_update", json!({"dir": "  "})),
        ("triage_reviews", json!({"appId": "1", "other": "x"})),
        ("no_such_prompt", json!({})),
    ] {
        let err = client
            .get_prompt(GetPromptRequestParams::new(name).with_arguments(args(arguments)))
            .await;
        assert!(err.is_err(), "{name}");
    }
    client.cancel().await.unwrap();

    // Промпт выше уровня доступа не зарегистрирован, и получить его нельзя.
    let client = connect(server("http://127.0.0.1:9", options(AccessLevel::Read))).await;
    let err = client
        .get_prompt(
            GetPromptRequestParams::new("listing_update")
                .with_arguments(args(json!({"dir": "metadata"}))),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("write access"), "{err}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn no_prompts_capability_without_prompts() {
    let client = connect(server(
        "http://127.0.0.1:9",
        ServerOptions {
            prompts: Vec::new(),
            ..ServerOptions::default()
        },
    ))
    .await;
    let info = client.peer_info().unwrap();
    assert!(info.capabilities.prompts.is_none());
    client.cancel().await.unwrap();

    // Только промпты выше уровня доступа — тоже без capability.
    let client = connect(server(
        "http://127.0.0.1:9",
        ServerOptions {
            prompts: prompts::all()
                .into_iter()
                .filter(|p| p.name == "listing_update")
                .collect(),
            ..ServerOptions::default()
        },
    ))
    .await;
    let info = client.peer_info().unwrap();
    assert!(info.capabilities.prompts.is_none());
    client.cancel().await.unwrap();
}
