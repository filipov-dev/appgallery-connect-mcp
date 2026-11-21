//! Фильтр `jq` и пользовательские инструменты в MCP-сессии: проверка фильтра до запроса,
//! ошибка фильтра после выполненного запроса, несколько выходов и пустой выход, вырезание
//! секретов до фильтра, лимит размера после фильтра, таймаут; `tools.toml` — фиксированные
//! аргументы, уровень доступа и набор операции. AppGallery Connect API подменён `wiremock`.

mod harness;

use std::time::Duration;

use appgallery_connect_api::AccessLevel;
use appgallery_connect_api::spec::find;
use appgallery_connect_mcp_core::{JqRunner, ServerOptions};
use harness::{
    MAX_RESPONSE_BYTES, call, client_secret, connect, custom, json_of, server, text, tool_names,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP_INFO: &str = "/api/publish/v2/app-info";

fn options(max_access: AccessLevel) -> ServerOptions {
    ServerOptions {
        max_access,
        toolsets: Some(vec!["all".into()]),
        ..ServerOptions::default()
    }
}

fn app_info() -> Value {
    json!({
        "ret": {"code": 0, "msg": "success"},
        "appInfo": {"defaultLang": "en-US", "releaseState": 0},
        "languages": [
            {"lang": "en-US", "appName": "Demo"},
            {"lang": "ru-RU", "appName": "Демо"}
        ]
    })
}

async fn app_info_api(expect: u64) -> MockServer {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(APP_INFO))
        .and(query_param("appId", "100000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(app_info()))
        .expect(expect)
        .mount(&api)
        .await;
    api
}

#[tokio::test]
async fn filter_selects_from_the_response() {
    let api = app_info_api(3).await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read))).await;

    // Несколько выходов — массив.
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": "100000001", "jq": ".languages[] | .appName"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(json_of(&result), json!(["Demo", "Демо"]));

    // Ровно один выход — как есть; фильтр видит и конверт результата.
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": "100000001", "jq": "{code: .ret.code, lang: .appInfo.defaultLang}"}),
    )
    .await;
    assert_eq!(json_of(&result), json!({"code": 0, "lang": "en-US"}));

    // Ни одного выхода — пустой массив.
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": "100000001", "jq": ".languages[] | select(.lang == \"de-DE\")"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), "[]");

    // Схема инструмента объявляет `jq`.
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "get_app_info").unwrap();
    assert_eq!(tool.input_schema["properties"]["jq"]["type"], "string");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn broken_filter_is_rejected_before_the_request() {
    let api = app_info_api(0).await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read))).await;
    for (filter, needle) in [
        (json!(".languages |"), "syntax error"),
        (json!("env"), "undefined filter `env/0`"),
        (json!("$ENV"), "undefined variable `$ENV`"),
        (json!("input"), "undefined filter `input/0`"),
        (json!("include \"x\"; ."), "module"),
        (json!(""), "empty"),
        (json!(1), "must be a string"),
    ] {
        let result = call(
            &client,
            "get_app_info",
            json!({"appId": "100000001", "jq": filter}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{filter}");
        let message = text(&result);
        assert!(message.contains("`jq`"), "{filter}: {message}");
        assert!(message.contains(needle), "{filter}: {message}");
    }
    client.cancel().await.unwrap();
    // `expect(0)`: ни одного запроса к API.
    api.verify().await;
}

#[tokio::test]
async fn runtime_error_after_a_change_says_not_to_repeat() {
    let api = MockServer::start().await;
    let op = find("update_app_language_info").unwrap();
    Mock::given(method(op.method.as_str()))
        .and(path(op.path))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"}
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path(APP_INFO))
        .respond_with(ResponseTemplate::new(200).set_body_json(app_info()))
        .expect(1)
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options(AccessLevel::Write))).await;
    let result = call(
        &client,
        "update_app_language_info",
        json!({
            "appId": "100000001",
            "body": {"lang": "en-US", "appName": "Demo"},
            "jq": ".ret.code | .[0]"
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("succeeded (HTTP 200)"), "{message}");
    assert!(message.contains("the `jq` filter failed"), "{message}");
    assert!(message.contains("Do not repeat the request"), "{message}");

    // Ошибка фильтра у чтения — без предупреждения об изменении.
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": "100000001", "jq": ".languages | .lang"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("the `jq` filter failed"), "{message}");
    assert!(!message.contains("Do not repeat"), "{message}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn secrets_are_redacted_before_the_filter() {
    let api = MockServer::start().await;
    let app_secret = ["9A8B", "7C6D", "5E4F", "3A2B"].concat().repeat(2);
    Mock::given(method("GET"))
        .and(path(APP_INFO))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "appInfo": {
                "appName": "Demo",
                "appSecret": app_secret,
                "echo": format!("secret {}", client_secret()),
            }
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read))).await;
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": "100000001", "jq": "[.. | strings]"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let raw = text(&result);
    assert!(!raw.contains(&app_secret), "{raw}");
    assert!(!raw.contains(&client_secret()), "{raw}");
    // Пометка о вырезанном остаётся вне результата фильтра.
    let body = json_of(&result);
    assert!(body["result"].as_array().unwrap().contains(&json!("Demo")));
    assert!(
        body["note"].as_str().unwrap().contains("`appSecret`"),
        "{body}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn size_limit_applies_to_the_filtered_result() {
    let api = MockServer::start().await;
    let languages: Vec<Value> = (0..400)
        .map(|i| json!({"lang": format!("l{i}"), "appName": "x".repeat(100)}))
        .collect();
    Mock::given(method("GET"))
        .and(path(APP_INFO))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "languages": languages
        })))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read))).await;

    // Без фильтра ответ больше лимита.
    let result = call(&client, "get_app_info", json!({"appId": "100000001"})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains(&MAX_RESPONSE_BYTES.to_string()),
        "{message}"
    );

    // С фильтром из большого ответа возвращается малая часть.
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": "100000001", "jq": ".languages | length"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), "400");

    // Результат фильтра больше лимита — ошибка.
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": "100000001", "jq": ".languages"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("filtered result is larger"),
        "{}",
        text(&result)
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn slow_filter_times_out() {
    let api = app_info_api(1).await;
    let client = connect(server(
        &api.uri(),
        ServerOptions {
            jq: JqRunner::in_process().with_timeout(Duration::from_millis(200)),
            ..options(AccessLevel::Read)
        },
    ))
    .await;
    let result = call(
        &client,
        "get_app_info",
        json!({"appId": "100000001", "jq": "last(range(1e15))"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("did not finish"),
        "{}",
        text(&result)
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn custom_tools_fix_arguments_and_filter() {
    let api = app_info_api(2).await;
    let main_app = custom(json!({
        "name": "main_app_languages",
        "description": "Languages of the main app",
        "operation": "get_app_info",
        "args": {"appId": "100000001"},
        "jq": "[.languages[] | .lang]"
    }));
    let client = connect(server(
        &api.uri(),
        ServerOptions {
            custom_tools: vec![main_app],
            ..options(AccessLevel::Read)
        },
    ))
    .await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "main_app_languages")
        .unwrap();
    assert!(tool.input_schema["properties"].get("appId").is_none());
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );

    let result = call(&client, "main_app_languages", json!({})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(json_of(&result), json!(["en-US", "ru-RU"]));

    // Фильтр вызова применяется к результату фильтра файла.
    let result = call(&client, "main_app_languages", json!({"jq": ".[0]"})).await;
    assert_eq!(text(&result), r#""en-US""#);

    // Фиксированный аргумент агент не подменяет; запроса нет.
    let result = call(&client, "main_app_languages", json!({"appId": "100000002"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("fixed"), "{}", text(&result));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn custom_tools_inherit_access_level_and_toolset() {
    let api = MockServer::start().await;
    let tools = vec![
        custom(json!({
            "name": "rename_main_app",
            "description": "Rename the main app",
            "operation": "update_app_language_info",
            "args": {"appId": "100000001"}
        })),
        custom(json!({
            "name": "main_app_reviews",
            "description": "Reviews of the main app",
            "operation": "comments.review.list",
            "args": {"appId": "100000001", "countries": "CN"}
        })),
    ];
    let read = server(
        &api.uri(),
        ServerOptions {
            custom_tools: tools.clone(),
            ..options(AccessLevel::Read)
        },
    );
    let client = connect(read).await;
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "main_app_reviews"), "{names:?}");
    // Уровень выше разрешённого: инструмент не регистрируется, вызов объясняет флаг.
    assert!(!names.iter().any(|n| n == "rename_main_app"), "{names:?}");
    let result = call(&client, "rename_main_app", json!({"body": {}})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("custom tool `rename_main_app`"),
        "{message}"
    );
    assert!(message.contains("write access"), "{message}");
    client.cancel().await.unwrap();

    // На сервере записи — уровень записи и набор `listing` операции.
    let write = server(
        &api.uri(),
        ServerOptions {
            custom_tools: tools.clone(),
            toolsets: Some(vec!["listing".into()]),
            ..options(AccessLevel::Write)
        },
    );
    let registry = write.registry();
    let tool = registry.get("rename_main_app").unwrap();
    assert_eq!(tool.access, AccessLevel::Write);
    let annotations = tool.tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    // Набор `reviews` выключен — вызов называет набор.
    assert!(registry.get("main_app_reviews").is_none());
    let reason = registry.unavailable_reason("main_app_reviews").unwrap();
    assert!(reason.contains("toolset `reviews`"), "{reason}");
}

/// `api_request` принимает `jq` так же, как инструменты операций: фильтр проверяется до
/// запроса и применяется к ответу.
#[tokio::test]
async fn api_request_takes_jq() {
    let api = app_info_api(1).await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read))).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "api_request").unwrap();
    assert_eq!(tool.input_schema["properties"]["jq"]["type"], "string");

    let result = call(
        &client,
        "api_request",
        json!({
            "path": APP_INFO,
            "query": {"appId": "100000001"},
            "jq": "[.languages[].lang]"
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(json_of(&result), json!(["en-US", "ru-RU"]));

    // Сломанный фильтр — ошибка до запроса (`expect(1)`: второго запроса нет).
    let result = call(
        &client,
        "api_request",
        json!({
            "path": APP_INFO,
            "query": {"appId": "100000001"},
            "jq": ".languages |"
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("`jq`") && message.contains("syntax error"),
        "{message}"
    );
    // Неизвестный аргумент перечисляет и `jq`.
    let result = call(
        &client,
        "api_request",
        json!({"path": APP_INFO, "save_to": "x.json"}),
    )
    .await;
    assert!(text(&result).contains("`jq`"), "{}", text(&result));
    client.cancel().await.unwrap();
    api.verify().await;
}

/// Составной `reviews_summary` получает `jq`: фильтр применяется к его сводке.
#[tokio::test]
async fn reviews_summary_takes_jq() {
    let api = MockServer::start().await;
    let review = |id: &str, rating: u64, time: i64| {
        json!({
            "reviewId": id,
            "rating": rating,
            "dateline": time,
            "countryCode": "RU",
            "content": format!("text of {id}"),
        })
    };
    Mock::given(method("GET"))
        .and(path("/api/reviews/v1/manage/dev/reviews"))
        .and(query_param("appId", "100000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": 0, "rtnDesc": "success"},
            "data": {"reviewList": [review("r2", 2, 20), review("r1", 5, 10)], "hasNext": 0}
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/reviews/v1/manage/dev/ratings"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&api)
        .await;
    let client = connect(server(&api.uri(), options(AccessLevel::Read))).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "reviews_summary").unwrap();
    assert_eq!(tool.input_schema["properties"]["jq"]["type"], "string");

    // Сломанный фильтр — ошибка до первого запроса.
    let result = call(
        &client,
        "reviews_summary",
        json!({"appId": "100000001", "countries": ["RU"], "jq": "{"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("`jq`"), "{}", text(&result));

    let result = call(
        &client,
        "reviews_summary",
        json!({
            "appId": "100000001",
            "countries": ["RU"],
            "jq": "{count: .reviewCount, ids: ([.reviews[].reviewId] | sort)}"
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(json_of(&result), json!({"count": 2, "ids": ["r1", "r2"]}));
    client.cancel().await.unwrap();
    api.verify().await;
}
