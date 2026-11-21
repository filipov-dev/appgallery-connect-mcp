//! Составные инструменты выпуска и `app_overview` в MCP-сессии: регистрация по уровню
//! доступа и набору, подсказка флага для инструментов уровня удаления, ошибка в конверте при
//! HTTP 200 как `isError: true`. AppGallery Connect API подменён `wiremock`.

mod harness;

use appgallery_connect_api::AccessLevel;
use appgallery_connect_mcp_core::ServerOptions;
use harness::{call, client_secret, connect, json_of, server, text, token, tool_names};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP: &str = "100000001";

fn options(max_access: AccessLevel) -> ServerOptions {
    ServerOptions {
        max_access,
        profile: Some("work".into()),
        ..ServerOptions::default()
    }
}

#[tokio::test]
async fn destructive_release_tools_need_allow_destructive() {
    let client = connect(server("http://127.0.0.1:9", options(AccessLevel::Write))).await;
    let names = tool_names(&client).await;
    for name in ["release_status", "app_overview", "package_upload"] {
        assert!(names.iter().any(|n| n == name), "{name}: {names:?}");
    }
    for name in ["release_submit", "release_manage"] {
        assert!(names.iter().all(|n| n != name), "{name}: {names:?}");
        let result = call(&client, name, json!({"appId": APP, "action": "suspend"})).await;
        assert_eq!(result.is_error, Some(true), "{name}");
        let message = text(&result);
        assert!(message.contains("destructive access"), "{name}: {message}");
    }
    client.cancel().await.unwrap();

    let client = connect(server(
        "http://127.0.0.1:9",
        options(AccessLevel::Destructive),
    ))
    .await;
    let tools = client.list_all_tools().await.unwrap();
    for name in ["release_submit", "release_manage"] {
        let tool = tools.iter().find(|tool| tool.name == name).unwrap();
        let annotations = tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(false), "{name}");
        assert_eq!(annotations.destructive_hint, Some(true), "{name}");
        let description = tool.description.as_deref().unwrap();
        assert!(
            description.contains("visible to AppGallery users"),
            "{description}"
        );
        assert!(
            description.contains("only when the user has explicitly asked"),
            "{description}"
        );
    }
    for name in ["release_status", "app_overview"] {
        let tool = tools.iter().find(|tool| tool.name == name).unwrap();
        assert_eq!(
            tool.annotations.as_ref().unwrap().read_only_hint,
            Some(true),
            "{name}"
        );
    }
    client.cancel().await.unwrap();

    // Чтение только: из инструментов выпуска остаются release_status и app_overview.
    let client = connect(server("http://127.0.0.1:9", options(AccessLevel::Read))).await;
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "release_status"));
    assert!(names.iter().any(|n| n == "app_overview"));
    assert!(names.iter().all(|n| n != "package_upload"));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn envelope_errors_with_http_200_are_tool_errors() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "appInfo": {"releaseState": 7, "versionNumber": "1.2.0", "versionCode": 12,
                        "onShelfVersionNumber": "1.1.0"},
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/package-list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "totalCount": 1,
            "pkgList": [{"pkgVersion": "7002", "fileName": "app.apk", "versionCode": 12}],
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/package/compile/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "pkgStateList": [{"pkgId": "7002", "successStatus": 0}],
        })))
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/publish/v2/app-submit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 204144660, "msg": "app information is incomplete"},
        })))
        .expect(1)
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options(AccessLevel::Destructive))).await;

    let result = call(&client, "release_submit", json!({"appId": APP})).await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    let message = text(&result);
    assert!(message.contains("204144660"), "{message}");
    assert!(
        message.contains("app information is incomplete"),
        "{message}"
    );
    assert!(!message.contains(&token()), "{message}");
    assert!(!message.contains(&client_secret()), "{message}");

    // Неизвестный аргумент отклоняется до запроса.
    let result = call(
        &client,
        "release_manage",
        json!({"action": "suspend", "share": 5}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("unknown argument"),
        "{}",
        text(&result)
    );

    // Чтение проходит: release_status собирает состояние.
    let result = call(&client, "release_status", json!({"appId": APP})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value = json_of(&result);
    assert_eq!(value["currentPackage"]["pkgId"], json!("7002"));
    client.cancel().await.unwrap();
}
