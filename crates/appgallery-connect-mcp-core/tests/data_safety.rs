//! Безопасная работа с данными и файлами: секреты не выходят в ответах, ошибках и логах,
//! `save_to` сохраняет файл из ответа (скачивание по ссылке — только с разрешённых хостов),
//! пути вне разрешённых каталогов и перезапись отклоняются, большой ответ — ошибка с
//! подсказкой. Сеть, настоящие учётные данные и системный keychain не используются: HTTP —
//! `wiremock`, файлы — временные каталоги, ссылки на файлы ведут на loopback (тестовый
//! режим `TransferHosts::HuaweiAndLoopback`). Похожие на секреты строки собраны из частей,
//! чтобы сканер секретов экспорта их не принял за настоящие.

use std::path::{MAIN_SEPARATOR, Path};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy, TransferHosts,
};
use appgallery_connect_mcp_core::{AppGalleryServer, CustomTool, ServerOptions, Services};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client_id() -> String {
    ["2718", "2818", "2845", "9045"].concat()
}

fn client_secret() -> String {
    ["Qw12", "Er34", "Ty56", "Ui78"].concat().repeat(4)
}

fn token() -> String {
    ["data", "-safety-", "token-", "9876543210"].concat()
}

/// Секрет приложения из ответа `get_app_fingerprints`.
fn app_secret() -> String {
    ["5e", "cr", "e7", "Ap", "pV", "a1", "ue"]
        .concat()
        .repeat(3)
}

/// Ключ API внутри файла конфигурации.
fn config_api_key() -> String {
    ["Cg", "Fy", "Ke", "y0", "12", "34", "56"]
        .concat()
        .repeat(3)
}

/// Подпись временной ссылки.
fn signature() -> String {
    ["Sig", "nat", "ure", "Val", "ue", "4242"].concat()
}

/// Код загрузки.
fn upload_code() -> String {
    ["Up", "lo", "ad", "Co", "de", "7777"].concat()
}

/// Приватный ключ PEM.
fn private_key() -> String {
    format!(
        "{}\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n{}",
        concat!("-----", "BEGIN PRIVATE KEY-----"),
        concat!("-----", "END PRIVATE KEY-----")
    )
}

/// Все значения, которые не должны попасть ни в ответ, ни в ошибку, ни в лог.
fn secrets() -> Vec<String> {
    vec![
        client_secret(),
        token(),
        app_secret(),
        config_api_key(),
        signature(),
        upload_code(),
        "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC".to_owned(),
    ]
}

fn assert_no_secrets(what: &str, text: &str) {
    for secret in secrets() {
        assert!(!text.contains(&secret), "{what} содержит секрет: {text}");
    }
}

const APP_ID: &str = "100000001";

struct Fixture {
    api: MockServer,
    dir: tempfile::TempDir,
    server: AppGalleryServer,
}

fn api_client(base_url: &str, max_response_bytes: usize) -> ApiClient {
    let credentials = ApiClientCredentials::from_parts(client_id(), client_secret()).unwrap();
    let mut config = ClientConfig::new(Region::Ru, Some(credentials));
    config.base_url = Some(base_url.to_owned());
    config.max_response_bytes = max_response_bytes;
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    // Ссылки на файлы в тестах ведут на `wiremock`: loopback разрешён явно.
    config.transfer.hosts = TransferHosts::HuaweiAndLoopback;
    ApiClient::with_token_source(config, Arc::new(StaticToken::new(token()))).unwrap()
}

async fn fixture_with_limit(max_response_bytes: usize) -> Fixture {
    fixture_with(max_response_bytes, Vec::new()).await
}

async fn fixture_with(max_response_bytes: usize, custom_tools: Vec<CustomTool>) -> Fixture {
    let api = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let options = ServerOptions {
        max_access: AccessLevel::Destructive,
        toolsets: Some(vec!["all".into()]),
        profile: Some("work".into()),
        allowed_paths: vec![dir.path().to_path_buf()],
        custom_tools,
        ..ServerOptions::default()
    };
    let server = AppGalleryServer::new(
        Services::new(api_client(&api.uri(), max_response_bytes)),
        options,
    )
    .unwrap();
    Fixture { api, dir, server }
}

async fn fixture() -> Fixture {
    fixture_with_limit(64 * 1024).await
}

impl Fixture {
    async fn call(&self, name: &str, args: Value) -> CallToolResult {
        let Value::Object(args) = args else {
            panic!("аргументы не объект");
        };
        self.server.call(name, args).await.unwrap()
    }

    fn file(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    /// Ссылка на файл на loopback-адресе `wiremock` с подписью в query.
    fn link(&self, name: &str) -> String {
        format!(
            "{}/files/{name}?X-Amz-Signature={}&token={}",
            self.api.uri(),
            signature(),
            signature()
        )
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

fn json_of(result: &CallToolResult) -> Value {
    let text = text(result);
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("не JSON ({err}): {text}"))
}

fn report_args(save_to: Option<&str>) -> Value {
    let mut args = json!({
        "appId": APP_ID,
        "language": "en-US",
        "startTime": "20260101",
        "endTime": "20260131",
        "currency": "USD",
    });
    if let Some(save_to) = save_to {
        args["save_to"] = json!(save_to);
    }
    args
}

async fn mock_report(f: &Fixture, link: &str) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/report/distribution-operation-quality/v1/IAPExport/{APP_ID}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "fileURL": link,
        })))
        .mount(&f.api)
        .await;
}

const CSV: &str = "date,country,amount\n2026-01-01,RU,10\n";

async fn mock_file(f: &Fixture, name: &str, body: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/files/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/csv"))
        .mount(&f.api)
        .await;
}

#[tokio::test]
async fn report_link_is_hidden_and_save_to_downloads_the_file() {
    let f = fixture().await;
    mock_report(&f, &f.link("report.csv")).await;
    mock_file(&f, "report.csv", CSV).await;

    // Без `save_to`: ссылка скрыта, пометка объясняет, как получить файл.
    let result = f.call("get_iap_report_url", report_args(None)).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["result"]["fileURL"], "***");
    let note = body["note"].as_str().unwrap();
    assert!(
        note.contains("`fileURL` (a temporary link to the report file)"),
        "{note}"
    );
    assert!(
        note.contains("call `get_iap_report_url` with `save_to`"),
        "{note}"
    );
    assert_no_secrets("ответ", &text(&result));
    assert!(
        !text(&result).contains("/files/report.csv"),
        "{}",
        text(&result)
    );

    // С `save_to`: файл скачан с адреса из ответа, агент получает путь и размер.
    let target = f.file("report.csv");
    let result = f
        .call("get_iap_report_url", report_args(Some(&target)))
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["result"]["fileURL"], "***");
    assert_eq!(body["saved"]["bytes"], CSV.len());
    assert_eq!(body["saved"]["field"], "fileURL");
    assert_eq!(body["saved"]["content_type"], "text/csv");
    let saved = Path::new(body["saved"]["path"].as_str().unwrap());
    assert_eq!(saved.file_name().unwrap(), "report.csv");
    assert!(
        body["note"]
            .as_str()
            .unwrap()
            .contains("saved the file instead")
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), CSV);
    assert_no_secrets("ответ", &text(&result));
    assert!(
        !text(&result).contains("/files/report.csv"),
        "{}",
        text(&result)
    );

    // На файловый хост не уходят ни токен, ни идентификатор клиента.
    let requests = f.api.received_requests().await.unwrap();
    let download = requests
        .iter()
        .find(|request| request.url.path() == "/files/report.csv")
        .expect("файл скачан");
    assert!(download.headers.get("authorization").is_none());
    assert!(download.headers.get("client_id").is_none());

    // Повтор с тем же путём: файл не перезаписывается, запрос к API не отправляется.
    let calls = requests.len();
    let result = f
        .call("get_iap_report_url", report_args(Some(&target)))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("already exists"),
        "{}",
        text(&result)
    );
    assert_eq!(f.api.received_requests().await.unwrap().len(), calls);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), CSV);
}

#[tokio::test]
async fn save_to_outside_the_allowed_directories_is_rejected_before_the_request() {
    let f = fixture().await;
    mock_report(&f, &f.link("report.csv")).await;
    let outside = format!("{MAIN_SEPARATOR}agc-connect-outside-report.csv");
    for (target, expected) in [
        (outside.as_str(), "outside the allowed directories"),
        ("", "non-empty path"),
    ] {
        let result = f
            .call("get_iap_report_url", report_args(Some(target)))
            .await;
        assert_eq!(result.is_error, Some(true), "{target}");
        let message = text(&result);
        assert!(message.contains(expected), "{message}");
        if expected.starts_with("outside") {
            assert!(message.contains("--allow-path"), "{message}");
        }
    }
    let missing_dir = f.file("missing/report.csv");
    let result = f
        .call("get_iap_report_url", report_args(Some(&missing_dir)))
        .await;
    assert!(
        text(&result).contains("does not exist"),
        "{}",
        text(&result)
    );
    assert!(f.api.received_requests().await.unwrap().is_empty());
    assert!(!Path::new(&outside).exists());
}

#[cfg(unix)]
#[tokio::test]
async fn save_to_does_not_follow_symlinks_out_of_the_allowed_directories() {
    let f = fixture().await;
    mock_report(&f, &f.link("report.csv")).await;
    let outside = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    // Каталог вне корней сервера — домашний каталог тестового процесса недоступен, поэтому
    // корни сужаем: сервер с единственным разрешённым каталогом проверяет сам `FilePolicy`.
    let link = f.dir.path().join("out");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let policy =
        appgallery_connect_mcp_core::FilePolicy::new(&[f.dir.path().to_path_buf()]).unwrap();
    let err = policy
        .check_write(&link.join("report.csv").display().to_string())
        .unwrap_err();
    assert!(err.0.contains("outside the allowed directories"), "{err}");
    assert!(!outside.path().join("report.csv").exists());
}

#[tokio::test]
async fn links_to_other_hosts_are_not_downloaded() {
    let f = fixture().await;
    let link = format!(
        "https://files.example.com/report.csv?X-Amz-Signature={}",
        signature()
    );
    mock_report(&f, &link).await;
    let target = f.file("report.csv");
    let result = f
        .call("get_iap_report_url", report_args(Some(&target)))
        .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("was not saved"), "{message}");
    assert!(message.contains("not a Huawei file host"), "{message}");
    assert_no_secrets("ошибка", &message);
    assert!(!message.contains("report.csv?"), "{message}");
    assert!(!Path::new(&target).exists());
}

#[tokio::test]
async fn failed_save_after_a_change_is_not_an_error() {
    // Профиль создан, но ссылка ведёт на чужой хост: изменение выполнено, повторять
    // запрос нельзя — ответ успешный, ссылка скрыта, пометка объясняет.
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path("/api/publish/v3/provision"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "provisionInfo": {
                "id": "prov-1",
                "provisionName": "debug",
                "provisionDownloadUrl": format!("https://evil.example.com/p.p7b?sign={}", signature()),
            },
        })))
        .expect(1)
        .mount(&f.api)
        .await;
    let target = f.file("debug.p7b");
    let result = f
        .call(
            "create_profile",
            json!({
                "body": {"provisionName": "debug", "provisionType": 1, "certId": "cert-1", "appId": APP_ID},
                "save_to": target,
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["result"]["provisionInfo"]["id"], "prov-1");
    assert_eq!(
        body["result"]["provisionInfo"]["provisionDownloadUrl"],
        "***"
    );
    let note = body["note"].as_str().unwrap();
    assert!(
        note.contains("changed data, but the file was not saved"),
        "{note}"
    );
    assert!(note.contains("Do not repeat the request"), "{note}");
    assert_no_secrets("ответ", &text(&result));
    assert!(!Path::new(&target).exists());
}

#[tokio::test]
async fn config_file_content_is_hidden_and_saved_on_request() {
    let f = fixture().await;
    let content = json!({
        "client": {"app_id": APP_ID, "api_key": config_api_key(), "client_secret": app_secret()},
    })
    .to_string();
    Mock::given(method("GET"))
        .and(path("/api/cpms/project-management-service/v1/config-file"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "fileName": "agconnect-services.json",
            "content": content,
        })))
        .mount(&f.api)
        .await;

    let result = f
        .call("get_app_config_file", json!({"appId": APP_ID}))
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["result"]["content"], "***");
    assert_eq!(body["result"]["fileName"], "agconnect-services.json");
    assert!(
        body["note"]
            .as_str()
            .unwrap()
            .contains("call `get_app_config_file` with `save_to`")
    );
    assert_no_secrets("ответ", &text(&result));

    let target = f.file("agconnect-services.json");
    let result = f
        .call(
            "get_app_config_file",
            json!({"appId": APP_ID, "save_to": target}),
        )
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["saved"]["bytes"], content.len());
    assert_eq!(body["saved"]["field"], "content");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), content);
    assert_no_secrets("ответ", &text(&result));
}

#[tokio::test]
async fn app_secret_is_never_returned() {
    let f = fixture().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/cds/app-distirbution/v1/all/app-extra-info/{APP_ID}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "secret": app_secret(),
            "certFingerprints": ["AB:CD:EF"],
        })))
        .mount(&f.api)
        .await;
    let args = json!({"appId": APP_ID, "uid": "42"});
    let result = f.call("get_app_fingerprints", args.clone()).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["result"]["secret"], "***");
    assert_eq!(body["result"]["certFingerprints"][0], "AB:CD:EF");
    let note = body["note"].as_str().unwrap();
    assert!(note.contains("`secret` (the app secret"), "{note}");
    assert!(note.contains("never returns it"), "{note}");
    assert_no_secrets("ответ", &text(&result));

    // Секрет приложения в файл не сохраняется: `save_to` у инструмента нет.
    let mut with_save = args;
    with_save["save_to"] = json!(f.file("secret.txt"));
    let result = f.call("get_app_fingerprints", with_save).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("unknown argument(s) `save_to`"),
        "{}",
        text(&result)
    );
}

/// Ответ и ошибки, в которых Huawei (или посредник) отразил учётные данные.
fn leaky_text() -> String {
    format!(
        "client_secret={} access_token={} Authorization: Bearer {} authCode={} {} \
         https://store.example.com/u?X-Amz-Signature={} appSecret: {}",
        client_secret(),
        token(),
        token(),
        upload_code(),
        private_key().replace('\n', "\\n"),
        signature(),
        app_secret()
    )
}

#[tokio::test]
async fn secrets_do_not_leak_in_responses_errors_and_logs() {
    let logs = captured_logs();

    let f = fixture().await;
    // HTTP 400 с отражёнными секретами в теле.
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ret": {"code": 204144647, "msg": leaky_text()},
        })))
        .mount(&f.api)
        .await;
    // HTTP 200 с ненулевым кодом результата.
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/appid-list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 204144660, "msg": leaky_text()},
        })))
        .mount(&f.api)
        .await;
    // Успешный ответ с секретами в разных местах.
    Mock::given(method("GET"))
        .and(path_regex("^/api/publish/v2/package/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "pkgList": [{"fileName": "app.apk", "note": leaky_text(), "uploadAuthCode": upload_code()}],
            "accessToken": token(),
            "privateKey": private_key(),
        })))
        .mount(&f.api)
        .await;
    mock_report(&f, &f.link("report.csv")).await;
    mock_file(&f, "report.csv", CSV).await;

    let error = f
        .call("get_app_info", json!({"appId": APP_ID, "lang": "en-US"}))
        .await;
    assert_eq!(error.is_error, Some(true));
    assert!(text(&error).contains("HTTP 400"), "{}", text(&error));
    assert_no_secrets("ошибка HTTP", &text(&error));

    let error = f
        .call("list_app_ids", json!({"packageName": "com.example.app"}))
        .await;
    assert_eq!(error.is_error, Some(true));
    assert!(text(&error).contains("204144660"), "{}", text(&error));
    assert_no_secrets("ошибка в конверте", &text(&error));

    let ops = appgallery_connect_api::spec::operations();
    let package_tool = ops
        .iter()
        .find(|op| op.path.starts_with("/api/publish/v2/package/") && op.method.as_str() == "GET")
        .map(|op| op.tool_name);
    if let Some(tool) = package_tool {
        let op = appgallery_connect_api::spec::find(tool).unwrap();
        let mut args = serde_json::Map::new();
        for param in op.params.iter().filter(|p| p.required) {
            args.insert(param.name.to_owned(), json!("1"));
        }
        let result = f.call(tool, Value::Object(args)).await;
        assert_no_secrets("ответ", &text(&result));
    }

    let target = f.file("report.csv");
    let result = f
        .call("get_iap_report_url", report_args(Some(&target)))
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_no_secrets("ответ", &text(&result));

    // Ссылка на чужой хост — ошибка без ссылки.
    let other = fixture().await;
    mock_report(
        &other,
        &format!("https://evil.example.com/r.csv?token={}", signature()),
    )
    .await;
    let result = other
        .call(
            "get_iap_report_url",
            report_args(Some(&other.file("r.csv"))),
        )
        .await;
    assert_no_secrets("ошибка скачивания", &text(&result));

    let logged = logs.text();
    assert!(
        logged.contains("AppGallery Connect API request"),
        "логи не перехвачены: {logged}"
    );
    assert_no_secrets("лог", &logged);
    assert!(!logged.contains("/files/report.csv"), "{logged}");
}

#[tokio::test]
async fn large_responses_are_errors_with_a_hint() {
    let f = fixture_with_limit(2048).await;
    let reviews: Vec<Value> = (0..100)
        .map(|i| json!({"reviewId": format!("r{i}"), "content": "x".repeat(40)}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/reviews/v1/manage/dev/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": 0, "rtnDesc": "success"},
            "data": {"reviewList": reviews},
        })))
        .mount(&f.api)
        .await;
    let result = f
        .call(
            "list_reviews",
            json!({"appId": APP_ID, "beginTime": 1, "endTime": 2, "countries": "RU"}),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("larger than 2048 bytes"), "{message}");
    assert!(message.contains("Arguments of `list_reviews`"), "{message}");
    assert!(message.contains("`limit`"), "{message}");
}

#[tokio::test]
async fn jq_cannot_be_combined_with_save_to() {
    let f = fixture().await;
    mock_report(&f, &f.link("report.csv")).await;
    let target = f.file("report.csv");
    let mut args = report_args(Some(&target));
    args["jq"] = json!(".fileURL");
    let result = f.call("get_iap_report_url", args).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("jq cannot be combined with save_to"),
        "{}",
        text(&result)
    );
    assert!(f.api.received_requests().await.unwrap().is_empty());
    assert!(!Path::new(&target).exists());
}

#[tokio::test]
async fn jq_sees_operation_secrets_already_hidden() {
    let f = fixture().await;
    mock_report(&f, &f.link("report.csv")).await;
    let mut args = report_args(None);
    args["jq"] = json!("{link: .fileURL}");
    let result = f.call("get_iap_report_url", args).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["result"], json!({"link": "***"}));
    // Пометка о скрытом остаётся снаружи фильтра.
    assert!(
        body["note"]
            .as_str()
            .unwrap()
            .contains("call `get_iap_report_url` with `save_to`")
    );
    assert_no_secrets("ответ", &text(&result));
    assert!(!text(&result).contains("/files/"), "{}", text(&result));
}

#[tokio::test]
async fn api_request_hides_secret_fields_of_the_matched_operation() {
    // `content` файла конфигурации — не имя учётных данных: его скрывает только правило
    // операции, найденной по пути запроса.
    let f = fixture().await;
    let content = json!({"client": {"api_key": config_api_key()}}).to_string();
    Mock::given(method("GET"))
        .and(path("/api/cpms/project-management-service/v1/config-file"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "fileName": "agconnect-services.json",
            "content": content,
        })))
        .mount(&f.api)
        .await;
    let args = json!({
        "method": "GET",
        "path": "/api/cpms/project-management-service/v1/config-file",
        "query": {"appId": APP_ID},
    });
    let result = f.call("api_request", args.clone()).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["result"]["content"], "***");
    assert_eq!(body["result"]["fileName"], "agconnect-services.json");
    assert!(
        body["note"]
            .as_str()
            .unwrap()
            .contains("call `get_app_config_file` with `save_to`")
    );
    assert_no_secrets("ответ", &text(&result));

    // С фильтром поле тоже уже скрыто.
    let mut filtered = args.clone();
    filtered["jq"] = json!(".content");
    let result = f.call("api_request", filtered).await;
    assert_eq!(json_of(&result)["result"], "***");
    assert_no_secrets("ответ", &text(&result));

    // `save_to` у `api_request` нет.
    let mut with_save = args;
    with_save["save_to"] = json!(f.file("agconnect-services.json"));
    let result = f.call("api_request", with_save).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("save_to"), "{}", text(&result));
    assert!(!Path::new(&f.file("agconnect-services.json")).exists());
}

#[tokio::test]
async fn size_limit_applies_after_the_filter() {
    let f = fixture_with_limit(2048).await;
    let reviews: Vec<Value> = (0..100)
        .map(|i| json!({"reviewId": format!("r{i}"), "content": "x".repeat(40)}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/reviews/v1/manage/dev/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": 0, "rtnDesc": "success"},
            "data": {"reviewList": reviews},
        })))
        .mount(&f.api)
        .await;
    let args = |jq: &str| {
        json!({
            "appId": APP_ID, "beginTime": 1, "endTime": 2, "countries": "RU", "jq": jq,
        })
    };
    // Сырой ответ больше предела, отфильтрованный — меньше: ответ возвращается.
    let result = f
        .call(
            "list_reviews",
            args("[.data.reviewList[] | .reviewId] | length"),
        )
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(json_of(&result), json!(100));
    // Отфильтрованный результат больше предела — ошибка.
    let result = f.call("list_reviews", args(".data.reviewList")).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("larger than 2048 bytes"),
        "{}",
        text(&result)
    );
}

#[tokio::test]
async fn custom_tools_over_saving_operations_take_save_to() {
    let report = |name: &str, jq: Option<&str>| {
        let mut definition = json!({
            "name": name,
            "description": "IAP report of the main app",
            "operation": "get_iap_report_url",
            "args": {"appId": APP_ID, "currency": "USD", "language": "en-US"},
        });
        if let Some(jq) = jq {
            definition["jq"] = json!(jq);
        }
        CustomTool::compile(serde_json::from_value(definition).unwrap(), "tools.toml").unwrap()
    };
    let f = fixture_with(
        64 * 1024,
        vec![
            report("main_iap_report", None),
            report("main_iap_report_link", Some(".fileURL")),
        ],
    )
    .await;
    mock_report(&f, &f.link("report.csv")).await;
    mock_file(&f, "report.csv", CSV).await;

    let registry = f.server.registry();
    let schema =
        |name: &str| Value::Object((*registry.get(name).unwrap().tool.input_schema).clone());
    assert_eq!(
        schema("main_iap_report")["properties"]["save_to"]["type"],
        "string"
    );
    assert!(
        schema("main_iap_report_link")["properties"]
            .get("save_to")
            .is_none()
    );

    let target = f.file("main.csv");
    let period = json!({"startTime": "20260101", "endTime": "20260131", "save_to": target});
    let result = f.call("main_iap_report", period.clone()).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(json_of(&result)["saved"]["bytes"], CSV.len());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), CSV);
    assert_no_secrets("ответ", &text(&result));

    // Свой `jq` определения и `save_to` не сочетаются.
    let mut other = period;
    other["save_to"] = json!(f.file("other.csv"));
    let result = f.call("main_iap_report_link", other).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("jq cannot be combined with save_to"),
        "{}",
        text(&result)
    );
    assert!(!Path::new(&f.file("other.csv")).exists());
}

#[test]
fn instructions_describe_local_files_and_hidden_values() {
    let dir = tempfile::tempdir().unwrap();
    let options = ServerOptions {
        toolsets: Some(vec!["reports".into(), "projects".into()]),
        allowed_paths: vec![dir.path().to_path_buf()],
        ..ServerOptions::default()
    };
    let server = AppGalleryServer::new(
        Services::new(api_client("http://127.0.0.1:9", 1024)),
        options,
    )
    .unwrap();
    let text = server.instructions();
    assert!(text.contains("- Local files: `save_to`"), "{text}");
    assert!(text.contains("appgallery-connect-mcp ui"), "{text}");
    let root = dir.path().canonicalize().unwrap();
    let root = root.display().to_string();
    let shown = root.strip_prefix(r"\\?\").unwrap_or(&root);
    assert!(text.contains(shown), "{text}");
    assert!(text.contains("`get_app_config_file`"), "{text}");
    assert!(text.contains("never returned"), "{text}");
    let registry = server.registry();
    let tool = registry.get("get_iap_report_url").unwrap();
    let schema = Value::Object((*tool.tool.input_schema).clone());
    assert_eq!(schema["properties"]["save_to"]["type"], "string");
    assert!(
        tool.tool
            .description
            .as_deref()
            .unwrap()
            .contains("Pass `save_to`")
    );

    // Без инструментов с файлами раздела о файлах нет; несуществующий каталог — ошибка.
    let server = AppGalleryServer::new(
        Services::new(api_client("http://127.0.0.1:9", 1024)),
        ServerOptions {
            composite_tools: Vec::new(),
            ..ServerOptions::default()
        },
    )
    .unwrap();
    assert!(!server.instructions().contains("- Local files:"));
    // Составной инструмент с `save_to` (`config_download` в наборе `apps` по умолчанию)
    // тоже пишет файлы.
    let server = AppGalleryServer::new(
        Services::new(api_client("http://127.0.0.1:9", 1024)),
        ServerOptions::default(),
    )
    .unwrap();
    assert!(server.registry().get("config_download").is_some());
    assert!(server.instructions().contains("- Local files: `save_to`"));
    // В динамическом режиме наборы включаются позже: раздел есть сразу.
    let server = AppGalleryServer::new(
        Services::new(api_client("http://127.0.0.1:9", 1024)),
        ServerOptions {
            dynamic_toolsets: true,
            ..ServerOptions::default()
        },
    )
    .unwrap();
    assert!(server.registry().get("get_iap_report_url").is_none());
    assert!(server.instructions().contains("- Local files: `save_to`"));
    let err = AppGalleryServer::new(
        Services::new(api_client("http://127.0.0.1:9", 1024)),
        ServerOptions {
            allowed_paths: vec![dir.path().join("missing")],
            ..ServerOptions::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("is not accessible"), "{err}");
}

/// Логи всех тестов этого файла. Подписчик глобальный, а не `set_default`: с
/// подписчиком на поток кэш интереса `tracing` гоняется с другими потоками тестов, и
/// события иногда теряются. Заодно проверяются логи всех сценариев файла.
fn captured_logs() -> LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "appgallery_connect_api=trace,appgallery_connect_mcp_core=trace",
            ))
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        logs
    })
    .clone()
}

/// Буфер для логов теста.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
