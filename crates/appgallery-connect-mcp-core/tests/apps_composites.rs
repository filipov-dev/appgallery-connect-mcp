//! Составные инструменты набора `apps` через сервер: `search_apps` и `config_download`
//! регистрируются как инструменты чтения, проходят общую постобработку ответа, а
//! содержимое файла конфигурации и секреты из него не попадают ни в ответ, ни в лог. HTTP —
//! `wiremock`, файлы — временный каталог; похожие на секреты строки собраны из частей.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy,
};
use appgallery_connect_mcp_core::{AppGalleryServer, ServerOptions, Services};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP_ID: &str = "100000001";
const CONFIG: &str = "/api/cpms/project-management-service/v1/config-file";

fn token() -> String {
    ["apps", "-composite-", "token-", "1234567890"].concat()
}

/// Ключ API внутри файла конфигурации.
fn config_api_key() -> String {
    ["Cg", "Fy", "Ke", "y9", "87", "65", "43"]
        .concat()
        .repeat(3)
}

/// Секрет клиента внутри файла конфигурации.
fn config_client_secret() -> String {
    ["Cl", "ie", "nt", "Se", "cr", "et", "77"]
        .concat()
        .repeat(3)
}

fn config_content() -> String {
    json!({
        "agcgw": {"url": "connect-drru.example.test"},
        "client": {
            "app_id": APP_ID,
            "api_key": config_api_key(),
            "client_secret": config_client_secret(),
            "package_name": "com.example.app",
        },
        "configuration_version": "3.0",
    })
    .to_string()
}

struct Fixture {
    api: MockServer,
    dir: tempfile::TempDir,
    server: AppGalleryServer,
}

async fn fixture(packages: &[&str]) -> Fixture {
    let api = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let id = ["2718", "2818", "2845", "1111"].concat();
    let secret = ["Zx12", "Cv34", "Bn56", "Mm78"].concat().repeat(4);
    let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
    let mut config = ClientConfig::new(Region::Ru, Some(credentials));
    config.base_url = Some(api.uri());
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    let client = ApiClient::with_token_source(config, Arc::new(StaticToken::new(token()))).unwrap();
    let options = ServerOptions {
        toolsets: Some(vec!["apps".into()]),
        profile: Some("work".into()),
        apps: vec![APP_ID.into()],
        packages: packages.iter().map(|p| (*p).to_owned()).collect(),
        allowed_paths: vec![dir.path().to_path_buf()],
        ..ServerOptions::default()
    };
    let server = AppGalleryServer::new(Services::new(client), options).unwrap();
    Fixture { api, dir, server }
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

fn assert_no_secrets(what: &str, text: &str) {
    for secret in [
        config_api_key(),
        config_client_secret(),
        token(),
        "connect-drru.example.test".to_owned(),
    ] {
        assert!(!text.contains(&secret), "{what} содержит секрет: {text}");
    }
}

#[tokio::test]
async fn tools_are_registered_as_read_only_in_apps() {
    let f = fixture(&[]).await;
    let registry = f.server.registry();
    for name in ["search_apps", "config_download"] {
        let tool = registry.get(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(tool.access, AccessLevel::Read, "{name}");
        let annotations = tool.tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(true), "{name}");
        assert!(
            tool.tool
                .description
                .as_deref()
                .unwrap()
                .ends_with("Access: read-only."),
            "{name}"
        );
    }
    let schema =
        Value::Object((*registry.get("config_download").unwrap().tool.input_schema).clone());
    assert_eq!(schema["required"], json!(["save_to"]));
    let instructions = f.server.instructions();
    assert!(
        instructions.contains("call `search_apps` first"),
        "{instructions}"
    );
    assert!(
        instructions.contains("- Local files: `save_to`"),
        "{instructions}"
    );
}

#[tokio::test]
async fn config_download_saves_the_file_without_showing_it() {
    let logs = captured_logs();
    let f = fixture(&[]).await;
    Mock::given(method("GET"))
        .and(path(CONFIG))
        .and(query_param("appId", APP_ID))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "fileName": "agconnect-services.json",
            "content": config_content(),
        })))
        .mount(&f.api)
        .await;

    // Без `save_to` — отказ по схеме, до запроса.
    let result = f.call("config_download", json!({"appId": APP_ID})).await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert!(text(&result).contains("save_to"), "{}", text(&result));
    assert!(f.api.received_requests().await.unwrap().is_empty());

    // `appId` — единственное приложение профиля.
    let target = f.file("agconnect-services.json");
    let result = f.call("config_download", json!({"save_to": target})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["appId"], APP_ID);
    assert_eq!(body["saved"]["bytes"], config_content().len());
    assert_eq!(
        body["topLevelKeys"],
        json!(["agcgw", "client", "configuration_version"])
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), config_content());
    assert_no_secrets("ответ", &text(&result));

    // Перезапись запрещена: файл остаётся прежним, запроса нет.
    let before = f.api.received_requests().await.unwrap().len();
    let result = f.call("config_download", json!({"save_to": target})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("already exists"),
        "{}",
        text(&result)
    );
    assert_eq!(f.api.received_requests().await.unwrap().len(), before);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), config_content());

    let logged = logs.text();
    assert!(
        logged.contains("AppGallery Connect API request"),
        "логи не перехвачены: {logged}"
    );
    assert_no_secrets("лог", &logged);
}

#[tokio::test]
async fn search_apps_falls_back_through_the_server() {
    let f = fixture(&["com.example.app"]).await;
    Mock::given(method("GET"))
        .and(path("/api/project-service/v1/projects"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "ret": {"code": 403, "msg": "no permission"},
        })))
        .mount(&f.api)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/appid-list"))
        .and(query_param("packageName", "com.example.app"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "appids": [{"key": "Example", "value": APP_ID}],
        })))
        .mount(&f.api)
        .await;
    let result = f
        .call("search_apps", json!({"jq": "[.apps[] | .appId]"}))
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(json_of(&result), json!([APP_ID]));

    let result = f.call("search_apps", json!({})).await;
    let body = json_of(&result);
    assert_eq!(body["source"], "publishing");
    assert_eq!(body["projectsAvailable"], false);
    assert_eq!(body["apps"][0]["default"], true);
    assert_eq!(body["apps"][0]["packageName"], "com.example.app");
}

/// Логи этого файла (глобальный подписчик, как в `data_safety.rs`).
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
