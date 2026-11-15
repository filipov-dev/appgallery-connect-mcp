//! Составной `report_download` через сервер ядра: параметры каждого вида отчёта уходят в
//! свою операцию Reports API, файл сохраняется по `save_to` без перезаписи, CSV без
//! `save_to` сводится, ссылка на чужой хост отклоняется, период проверяется до запроса,
//! ссылка не видна ни в ответе, ни в ошибке, ни в логе. HTTP — `wiremock`, ссылки на файлы
//! ведут на loopback (тестовый режим `TransferHosts::HuaweiAndLoopback`), учётные данные
//! выдуманные и собраны из частей.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy, TransferHosts,
};
use appgallery_connect_mcp_core::{AppGalleryServer, ServerOptions, Services};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP_ID: &str = "100000001";
const BASE: &str = "/api/report/distribution-operation-quality/v1";

fn token() -> String {
    ["report", "-download-", "token-", "1357924680"].concat()
}

/// Подпись временной ссылки: не должна попасть никуда.
fn signature() -> String {
    ["Rep", "ort", "Sig", "nat", "ure", "8642"].concat()
}

fn api_client(base_url: &str) -> ApiClient {
    let id = ["5772", "1566", "4901"].concat();
    let secret = ["Lk12", "Jh34", "Gf56", "Ds78"].concat().repeat(4);
    let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
    let mut config = ClientConfig::new(Region::Ru, Some(credentials));
    config.base_url = Some(base_url.to_owned());
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    // Ссылки на файлы в тестах ведут на `wiremock`: loopback разрешён явно.
    config.transfer.hosts = TransferHosts::HuaweiAndLoopback;
    ApiClient::with_token_source(config, Arc::new(StaticToken::new(token()))).unwrap()
}

struct Fixture {
    api: MockServer,
    dir: tempfile::TempDir,
    server: AppGalleryServer,
}

async fn fixture_with(toolsets: &[&str]) -> Fixture {
    let api = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let options = ServerOptions {
        max_access: AccessLevel::Read,
        toolsets: Some(toolsets.iter().map(|t| (*t).to_owned()).collect()),
        profile: Some("work".into()),
        apps: vec![APP_ID.to_owned()],
        allowed_paths: vec![dir.path().to_path_buf()],
        ..ServerOptions::default()
    };
    let server = AppGalleryServer::new(Services::new(api_client(&api.uri())), options).unwrap();
    Fixture { api, dir, server }
}

async fn fixture() -> Fixture {
    fixture_with(&["reports"]).await
}

impl Fixture {
    async fn call(&self, args: Value) -> CallToolResult {
        let Value::Object(args) = args else {
            panic!("аргументы не объект");
        };
        self.server.call("report_download", args).await.unwrap()
    }

    fn file(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    /// Ссылка на файл на loopback-адресе `wiremock` с подписью в query.
    fn link(&self, name: &str) -> String {
        format!(
            "{}/files/{name}?X-Amz-Signature={}&Expires=1",
            self.api.uri(),
            signature()
        )
    }

    /// Ответ операции со ссылкой.
    async fn mock_link(&self, endpoint: &str, link: &str) {
        Mock::given(method("GET"))
            .and(path(format!("{BASE}/{endpoint}/{APP_ID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 0, "msg": "success"},
                "fileURL": link,
                "packName": "com.example.app",
            })))
            .mount(&self.api)
            .await;
    }

    async fn mock_file(&self, name: &str, body: &[u8]) {
        Mock::given(method("GET"))
            .and(path(format!("/files/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), "text/csv"))
            .mount(&self.api)
            .await;
    }

    /// Запросы к операциям отчётов (без скачиваний).
    async fn report_requests(&self) -> Vec<wiremock::Request> {
        self.api
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path().starts_with(BASE))
            .collect()
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

fn assert_no_link(what: &str, text: &str) {
    assert!(
        !text.contains(&signature()),
        "{what} содержит подпись: {text}"
    );
    assert!(!text.contains("/files/"), "{what} содержит ссылку: {text}");
    assert!(!text.contains(&token()), "{what} содержит токен: {text}");
}

fn query(request: &wiremock::Request) -> Vec<(String, String)> {
    request
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// Ожидаемые параметры query: имя и значение по порядку.
type Query = Vec<(&'static str, &'static str)>;

const INSTALLS_CSV: &str = "Country/Region,Impressions,New downloads,Update downloads,CTR\n\
RU,1000,40,10,4.5%\n\
BY,500,\"1,200\",5,3%\n";

#[tokio::test]
async fn every_report_sends_its_parameters_to_its_operation() {
    let cases: Vec<(&str, Value, Query)> = vec![
        (
            "appDownloadExport",
            json!({"report": "installs", "language": "ru-RU", "groupBy": "countryId",
                "countries": ["ru", "all"], "businessType": "AppGallery",
                "appVersions": ["1.0.0", "1.1.0"]}),
            vec![
                ("language", "ru-RU"),
                ("startTime", "20260101"),
                ("endTime", "20260131"),
                ("filterCondition", "countryId"),
                ("filterCondition", "businessType"),
                ("filterCondition", "appVersion"),
                ("filterConditionValue", "RU,all"),
                ("filterConditionValue", "AppGallery"),
                ("filterConditionValue", "1.0.0,1.1.0"),
                ("groupBy", "countryId"),
                ("exportType", "CSV"),
            ],
        ),
        (
            "IAPExport",
            json!({"report": "in_app_purchases", "currency": "EUR", "groupBy": "businessType"}),
            vec![
                ("language", "en-US"),
                ("startTime", "20260101"),
                ("endTime", "20260131"),
                ("groupBy", "businessType"),
                ("currency", "EUR"),
                ("exportType", "CSV"),
            ],
        ),
        (
            "orderAnalysisExport",
            json!({"report": "paid_app", "countries": ["CN"], "language": "zh-CN"}),
            vec![
                ("language", "zh-CN"),
                ("startTime", "20260101"),
                ("endTime", "20260131"),
                ("filterCondition", "countryId"),
                ("filterConditionValue", "CN"),
                ("exportType", "CSV"),
            ],
        ),
        (
            "orderDetailExport",
            json!({"report": "paid_app_details", "countries": ["RU"], "dateType": "refund",
                "orderStatus": "refunded", "orderId": "order-1"}),
            vec![
                ("language", "en-US"),
                ("queryType", "1"),
                ("startTime", "20260101"),
                ("endTime", "20260131"),
                ("filterCondition", "countryId"),
                ("filterCondition", "orderID"),
                ("filterCondition", "orderStatus"),
                ("filterConditionValue", "RU"),
                ("filterConditionValue", "order-1"),
                ("filterConditionValue", "1"),
            ],
        ),
        (
            "appDownloadFailExport",
            json!({"report": "install_failures", "groupBy": ["appVersion", "countryId"],
                "downloadType": "new", "deviceName": "P40-lite"}),
            vec![
                ("language", "en-US"),
                ("startTime", "20260101"),
                ("endTime", "20260131"),
                ("filterCondition", "deviceName"),
                ("filterCondition", "downloadType"),
                ("filterConditionValue", "P40-lite"),
                ("filterConditionValue", "1"),
                ("groupBy", "appVersion"),
                ("groupBy", "countryId"),
                ("exportType", "CSV"),
            ],
        ),
    ];
    for (endpoint, mut args, expected) in cases {
        let f = fixture().await;
        f.mock_link(endpoint, &f.link("r.csv")).await;
        f.mock_file("r.csv", b"a,b\n1,2\n").await;
        args["from"] = json!("2026-01-01");
        args["to"] = json!("2026-01-31");
        let result = f.call(args.clone()).await;
        assert_eq!(
            result.is_error,
            Some(false),
            "{endpoint}: {}",
            text(&result)
        );
        let requests = f.report_requests().await;
        assert_eq!(requests.len(), 1, "{endpoint}");
        assert_eq!(
            requests[0].url.path(),
            format!("{BASE}/{endpoint}/{APP_ID}")
        );
        let mut got = query(&requests[0]);
        let mut want = pairs(&expected);
        got.sort();
        want.sort();
        assert_eq!(got, want, "{endpoint}: {args}");
        let body = json_of(&result);
        assert_eq!(body["report"], args["report"]);
        assert_eq!(body["period"]["days"], 31);
        assert_eq!(body["summary"]["rows"], 1);
    }
}

#[tokio::test]
async fn save_to_saves_the_file_without_the_link() {
    let f = fixture().await;
    f.mock_link("appDownloadExport", &f.link("report.csv"))
        .await;
    f.mock_file("report.csv", INSTALLS_CSV.as_bytes()).await;
    let target = f.file("installs.csv");
    let result = f
        .call(
            json!({"report": "installs", "appId": APP_ID, "days": 7, "to": "2026-01-31",
            "save_to": target}),
        )
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_no_link("ответ", &text(&result));
    let body = json_of(&result);
    assert_eq!(body["saved"]["bytes"], INSTALLS_CSV.len());
    assert_eq!(body["saved"]["fileType"], "csv");
    assert_eq!(body["saved"]["content_type"], "text/csv");
    let saved = std::path::PathBuf::from(body["saved"]["path"].as_str().unwrap());
    assert_eq!(saved.file_name().unwrap(), "installs.csv");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), INSTALLS_CSV);
    assert_eq!(body["packageName"], "com.example.app");
    assert_eq!(
        body["period"],
        json!({"from": "2026-01-25", "to": "2026-01-31", "days": 7})
    );
    assert!(body.get("summary").is_none());
    // Скачивание — без заголовков авторизации Connect API.
    let download = f
        .api
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|request| request.url.path() == "/files/report.csv")
        .unwrap();
    assert!(download.headers.get("authorization").is_none());
    assert!(download.headers.get("client_id").is_none());
}

#[tokio::test]
async fn existing_files_are_never_overwritten() {
    let f = fixture().await;
    f.mock_link("appDownloadExport", &f.link("report.csv"))
        .await;
    f.mock_file("report.csv", INSTALLS_CSV.as_bytes()).await;
    let target = f.file("existing.csv");
    std::fs::write(&target, "keep me").unwrap();
    let result = f
        .call(json!({"report": "installs", "save_to": target}))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("already exists"),
        "{}",
        text(&result)
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
    // Путь проверяется до запроса: API не вызывался.
    assert!(f.api.received_requests().await.unwrap().is_empty());

    let outside = format!("{}agc-report-outside.csv", std::path::MAIN_SEPARATOR);
    let result = f
        .call(json!({"report": "installs", "save_to": outside}))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("outside the allowed directories"),
        "{}",
        text(&result)
    );
    assert!(f.api.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn csv_reports_are_summarized() {
    let f = fixture().await;
    f.mock_link("appDownloadExport", &f.link("report.csv"))
        .await;
    // UTF-8 с BOM и строкой итогов в конце.
    let mut csv = vec![0xEF, 0xBB, 0xBF];
    csv.extend_from_slice(INSTALLS_CSV.as_bytes());
    csv.extend_from_slice(b"Total,1500,1240,15,4%\n");
    f.mock_file("report.csv", &csv).await;
    let result = f
        .call(
            json!({"report": "installs", "groupBy": "countryId", "maxRows": 1,
            "from": "2026-01-01", "to": "2026-01-31"}),
        )
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_no_link("ответ", &text(&result));
    let body = json_of(&result);
    let summary = &body["summary"];
    assert_eq!(summary["rows"], 2);
    assert_eq!(
        summary["columns"],
        json!([
            "Country/Region",
            "Impressions",
            "New downloads",
            "Update downloads",
            "CTR"
        ])
    );
    assert_eq!(
        summary["totals"],
        json!({"Impressions": 1500, "New downloads": 1240, "Update downloads": 15})
    );
    assert_eq!(
        summary["firstRows"],
        json!([["RU", "1000", "40", "10", "4.5%"]])
    );
    assert_eq!(summary["truncated"], true);
    assert_eq!(summary["encoding"], "utf-8");
    assert_eq!(body["bytes"], csv.len());
    assert_eq!(body["groupBy"], json!(["countryId"]));
    assert!(body["notes"].to_string().contains("1 total row"), "{body}");
    assert!(body.get("saved").is_none());
}

#[tokio::test]
async fn paid_app_amounts_are_summed_per_currency() {
    let f = fixture().await;
    f.mock_link("orderAnalysisExport", &f.link("paid.csv"))
        .await;
    let csv = "Куплено(UTC);Имя приложения;Цена покупки;Валюта;Страна/регион\n\
               2026-01-02 10:00;App;99,5;RUB;RU\n\
               2026-01-03 11:00;App;1,5;EUR;DE\n\
               2026-01-04 12:00;App;100;RUB;RU\n";
    f.mock_file("paid.csv", csv.as_bytes()).await;
    let result = f
        .call(
            json!({"report": "paid_app", "language": "ru-RU", "from": "2026-01-01",
            "to": "2026-01-31"}),
        )
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let summary = &json_of(&result)["summary"];
    assert_eq!(summary["rows"], 3);
    assert_eq!(summary["delimiter"], ";");
    assert_eq!(
        summary["totalsByCurrency"],
        json!({"Цена покупки": {"RUB": 199.5, "EUR": 1.5}})
    );
}

#[tokio::test]
async fn excel_is_saved_but_not_summarized() {
    let f = fixture().await;
    // Excel без `save_to` — ошибка до запроса.
    let result = f
        .call(json!({"report": "installs", "format": "excel"}))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("pass `save_to`"),
        "{}",
        text(&result)
    );
    assert!(f.api.received_requests().await.unwrap().is_empty());

    // Excel вместо CSV — ошибка с подсказкой; с `save_to` файл сохраняется.
    f.mock_link("appDownloadFailExport", &f.link("failures.xlsx"))
        .await;
    f.mock_file("failures.xlsx", b"PK\x03\x04excel").await;
    let result = f.call(json!({"report": "install_failures"})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("Excel (xlsx)"), "{message}");
    assert!(message.contains("`save_to`"), "{message}");
    assert_no_link("ошибка", &message);

    let target = f.file("failures.xlsx");
    let result = f
        .call(json!({"report": "install_failures", "format": "excel", "save_to": target}))
        .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["saved"]["fileType"], "excel");
    assert_eq!(body["format"], "excel");
    let request = f.report_requests().await.pop().unwrap();
    assert!(query(&request).contains(&("exportType".to_owned(), "EXCEL".to_owned())));
}

#[tokio::test]
async fn links_to_other_hosts_are_refused() {
    let f = fixture().await;
    f.mock_link(
        "IAPExport",
        &format!(
            "https://evil.example.com/files/r.csv?X-Amz-Signature={}",
            signature()
        ),
    )
    .await;
    let target = f.file("iap.csv");
    for args in [
        json!({"report": "in_app_purchases", "save_to": target}),
        json!({"report": "in_app_purchases"}),
    ] {
        let result = f.call(args).await;
        assert_eq!(result.is_error, Some(true));
        let message = text(&result);
        assert!(message.contains("not a Huawei file host"), "{message}");
        assert!(message.contains("only from Huawei hosts"), "{message}");
        assert_no_link("ошибка", &message);
    }
    assert!(!std::path::Path::new(&target).exists());
}

#[tokio::test]
async fn period_limits_are_checked_before_the_request() {
    let f = fixture().await;
    for (args, text_part) in [
        (
            json!({"report": "installs", "from": "2025-01-01", "to": "2025-12-31"}),
            "at most 180 days",
        ),
        (
            json!({"report": "paid_app", "from": "2024-01-01", "to": "2025-12-31"}),
            "at most 365 days",
        ),
        (
            json!({"report": "paid_app_details", "days": 181, "countries": ["RU"]}),
            "180",
        ),
    ] {
        let result = f.call(args).await;
        assert_eq!(result.is_error, Some(true));
        assert!(text(&result).contains(text_part), "{}", text(&result));
    }
    assert!(f.api.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn empty_reports_and_envelope_errors_are_clear() {
    let f = fixture().await;
    Mock::given(method("GET"))
        .and(path(format!("{BASE}/IAPExport/{APP_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 204144660, "msg": "the app does not exist"},
        })))
        .mount(&f.api)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{BASE}/orderAnalysisExport/{APP_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "fileURL": "",
        })))
        .mount(&f.api)
        .await;
    f.mock_link("appDownloadExport", &f.link("header.csv"))
        .await;
    f.mock_file("header.csv", b"Time,Impressions\r\n").await;

    let result = f.call(json!({"report": "in_app_purchases"})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("in-app purchases report failed"),
        "{message}"
    );
    assert!(message.contains("204144660"), "{message}");

    let result = f.call(json!({"report": "paid_app"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = json_of(&result);
    assert_eq!(body["empty"], true);
    assert!(
        body["notes"]
            .to_string()
            .contains("no paid app report file")
    );

    let result = f.call(json!({"report": "installs"})).await;
    let body = json_of(&result);
    assert_eq!(body["empty"], true, "{body}");
    assert_eq!(body["summary"]["rows"], 0);
    assert!(body["notes"].to_string().contains("only the header"));
}

#[tokio::test]
async fn the_link_never_reaches_responses_errors_and_logs() {
    let logs = captured_logs();
    let f = fixture().await;
    f.mock_link("appDownloadExport", &f.link("report.csv"))
        .await;
    f.mock_file("report.csv", INSTALLS_CSV.as_bytes()).await;
    f.mock_link("IAPExport", &f.link("gone.csv")).await;
    Mock::given(method("GET"))
        .and(path("/files/gone.csv"))
        .respond_with(ResponseTemplate::new(403).set_body_string(format!(
            "<Error>AccessDenied for /files/gone.csv?X-Amz-Signature={}</Error>",
            signature()
        )))
        .mount(&f.api)
        .await;

    let summary = f.call(json!({"report": "installs"})).await;
    assert_eq!(summary.is_error, Some(false), "{}", text(&summary));
    assert_no_link("сводка", &text(&summary));
    let saved = f
        .call(json!({"report": "installs", "save_to": f.file("r.csv")}))
        .await;
    assert_eq!(saved.is_error, Some(false), "{}", text(&saved));
    assert_no_link("ответ с save_to", &text(&saved));
    for args in [
        json!({"report": "in_app_purchases"}),
        json!({"report": "in_app_purchases", "save_to": f.file("iap.csv")}),
    ] {
        let error = f.call(args).await;
        assert_eq!(error.is_error, Some(true));
        let message = text(&error);
        assert!(message.contains("403"), "{message}");
        assert_no_link("ошибка скачивания", &message);
    }
    assert!(!std::path::Path::new(&f.file("iap.csv")).exists());

    let logged = logs.text();
    assert!(
        logged.contains("report_download"),
        "логи не перехвачены: {logged}"
    );
    assert_no_link("лог", &logged);
}

#[tokio::test]
async fn the_tool_belongs_to_the_reports_toolset() {
    let f = fixture().await;
    let tool = f
        .server
        .registry()
        .iter()
        .find(|tool| tool.tool.name == "report_download")
        .map(|tool| tool.tool.clone())
        .expect("report_download зарегистрирован");
    let annotations = tool.annotations.unwrap();
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_ne!(annotations.destructive_hint, Some(true));
    assert!(tool.input_schema.contains_key("properties"));

    let f = fixture_with(&["reviews"]).await;
    assert!(
        f.server
            .registry()
            .iter()
            .all(|tool| tool.tool.name != "report_download")
    );
    let result = f.call(json!({"report": "installs"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("reports"), "{}", text(&result));
}

/// Логи всех тестов файла: глобальный подписчик (см. `tests/data_safety.rs`).
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
        self.0.lock().unwrap().extend_from_slice(buf);
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
