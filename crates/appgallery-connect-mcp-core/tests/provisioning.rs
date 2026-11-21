//! Набор `provisioning` (подпись HarmonyOS, Provisioning API) через сервер: набор выключен
//! по умолчанию, уровни и аннотации инструментов, запросы операций, ссылки на файлы
//! сертификатов и профилей скрываются без `save_to` и скачиваются с ним, удаление требует
//! `--allow-destructive`, запрос сертификата идёт только через `certificate_request` с CSR
//! из файла. HTTP — `wiremock`, файлы — временный каталог, ссылки ведут на loopback
//! (`TransferHosts::HuaweiAndLoopback`); похожие на секреты строки собраны из частей.

use std::sync::Arc;
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy, TransferHosts,
};
use appgallery_connect_mcp_core::{AppGalleryServer, ServerOptions, Services};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP_ID: &str = "100000001";

const CERT_CREATE: &str = "/api/publish/v3/cert";
const CERT_LIST: &str = "/api/publish/v3/cert/list";
const CERT_DELETE: &str = "/api/publish/v2/cert/delete";
const DEVICE_ADD: &str = "/api/publish/v2/device";
const DEVICE_LIST: &str = "/api/publish/v2/device/list";
const DEVICE_DELETE: &str = "/api/publish/v2/device/delete";
const PROFILE: &str = "/api/publish/v3/provision";
const PROFILE_LIST: &str = "/api/publish/v3/provision/list";
const PROFILE_DELETE: &str = "/api/publish/v2/provision";

/// Инструменты набора по уровням (без составного `certificate_request`).
const READ_TOOLS: &[&str] = &["list_certificates", "list_devices", "list_profiles"];
const WRITE_TOOLS: &[&str] = &[
    "add_devices",
    "certificate_request",
    "create_profile",
    "update_profile_devices",
];
const DESTRUCTIVE_TOOLS: &[&str] = &["delete_certificates", "delete_devices", "delete_profile"];

fn token() -> String {
    ["provisioning", "-test-", "token-", "2468013579"].concat()
}

/// Подпись временной ссылки на файл.
fn signature() -> String {
    ["Prov", "Link", "Sig", "nature", "8642"].concat()
}

struct Fixture {
    api: MockServer,
    dir: tempfile::TempDir,
    server: AppGalleryServer,
}

fn api_client(base_url: &str) -> ApiClient {
    let id = ["3141", "5926", "5358", "9793"].concat();
    let secret = ["Pr12", "Ov34", "Is56", "Ng78"].concat().repeat(4);
    let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
    let mut config = ClientConfig::new(Region::Cn, Some(credentials));
    config.base_url = Some(base_url.to_owned());
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    config.transfer.hosts = TransferHosts::HuaweiAndLoopback;
    ApiClient::with_token_source(config, Arc::new(StaticToken::new(token()))).unwrap()
}

async fn fixture_with(access: AccessLevel, toolsets: Option<&[&str]>) -> Fixture {
    let api = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let options = ServerOptions {
        max_access: access,
        toolsets: toolsets.map(|sets| sets.iter().map(|s| (*s).to_owned()).collect()),
        profile: Some("work".into()),
        allowed_paths: vec![dir.path().to_path_buf()],
        ..ServerOptions::default()
    };
    let server = AppGalleryServer::new(Services::new(api_client(&api.uri())), options).unwrap();
    Fixture { api, dir, server }
}

async fn fixture(access: AccessLevel) -> Fixture {
    fixture_with(access, Some(&["provisioning"])).await
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
            "{}/files/{name}?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature={}",
            self.api.uri(),
            signature()
        )
    }

    async fn mock_file(&self, name: &str, body: &[u8]) {
        Mock::given(method("GET"))
            .and(path(format!("/files/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .expect(1)
            .mount(&self.api)
            .await;
    }

    async fn requests(&self) -> usize {
        self.api.received_requests().await.unwrap().len()
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

fn ok(result: &CallToolResult) -> Value {
    assert_eq!(result.is_error, Some(false), "{}", text(result));
    json_of(result)
}

fn success() -> Value {
    json!({"ret": {"code": 0, "msg": "success"}})
}

fn with_success(extra: Value) -> Value {
    let mut body = success();
    if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
    body
}

fn profile_info(link: &str) -> Value {
    json!({
        "id": "prov-1",
        "provisionName": "debug-profile",
        "provisionType": 1,
        "certName": "debug-cert",
        "deviceList": [{"id": "dev-1", "deviceName": "test phone", "udid": "UDID-1", "deviceType": 4}],
        "provisionDownloadUrl": link,
        "appId": APP_ID,
    })
}

fn cert_info(id: &str, link: &str) -> Value {
    json!({"id": id, "certName": "debug-cert", "certType": 1, "certDownloadUrl": link})
}

fn provisioning_tools(f: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = f
        .server
        .registry()
        .iter()
        .filter(|tool| {
            let name = tool.tool.name.as_ref();
            [READ_TOOLS, WRITE_TOOLS, DESTRUCTIVE_TOOLS]
                .concat()
                .contains(&name)
                || name.contains("certificate")
                || name.contains("profile")
                || name.contains("device")
        })
        .map(|tool| tool.tool.name.to_string())
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
    // Наборы по умолчанию: provisioning выключен даже на уровне удаления.
    let f = fixture_with(AccessLevel::Destructive, None).await;
    assert!(
        provisioning_tools(&f).is_empty(),
        "{:?}",
        provisioning_tools(&f)
    );

    let f = fixture(AccessLevel::Read).await;
    assert_eq!(provisioning_tools(&f), sorted(&[READ_TOOLS]));
    let f = fixture(AccessLevel::Write).await;
    assert_eq!(provisioning_tools(&f), sorted(&[READ_TOOLS, WRITE_TOOLS]));
    let f = fixture(AccessLevel::Destructive).await;
    assert_eq!(
        provisioning_tools(&f),
        sorted(&[READ_TOOLS, WRITE_TOOLS, DESTRUCTIVE_TOOLS])
    );
    // CSR строкой в теле не принимается: инструмента операции нет.
    assert!(f.server.registry().get("create_certificate").is_none());

    let registry = f.server.registry();
    for (names, read_only, destructive) in [
        (READ_TOOLS, true, false),
        (WRITE_TOOLS, false, false),
        (DESTRUCTIVE_TOOLS, false, true),
    ] {
        for name in names {
            let tool = registry.get(name).unwrap_or_else(|| panic!("{name}"));
            let hints = tool.tool.annotations.as_ref().unwrap();
            assert_eq!(hints.read_only_hint, Some(read_only), "{name}");
            assert_eq!(
                hints.destructive_hint.unwrap_or(false),
                destructive,
                "{name}"
            );
        }
    }
    // Причина уровня удаления видна агенту в описании.
    for name in ["delete_certificates", "delete_devices"] {
        let description = registry
            .get(name)
            .unwrap()
            .tool
            .description
            .as_deref()
            .unwrap();
        assert!(
            description.contains("Destructive: deletes"),
            "{description}"
        );
        assert!(description.contains("signed"), "{description}");
    }
    let description = registry
        .get("delete_profile")
        .unwrap()
        .tool
        .description
        .as_deref()
        .unwrap();
    assert!(description.contains("cannot be undone"), "{description}");
    // `save_to` — у операций со ссылкой на файл сертификата или профиля.
    for name in [
        "list_certificates",
        "list_profiles",
        "create_profile",
        "update_profile_devices",
        "certificate_request",
    ] {
        let schema = &registry.get(name).unwrap().tool.input_schema;
        assert!(schema["properties"].get("save_to").is_some(), "{name}");
    }
    for name in [
        "list_devices",
        "add_devices",
        "delete_certificates",
        "delete_profile",
    ] {
        let schema = &registry.get(name).unwrap().tool.input_schema;
        assert!(schema["properties"].get("save_to").is_none(), "{name}");
    }
}

#[tokio::test]
async fn operations_send_the_documented_requests() {
    let f = fixture(AccessLevel::Destructive).await;
    let api = &f.api;
    Mock::given(method("POST"))
        .and(path(CERT_LIST))
        .and(body_json(json!({"certType": 2})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({"certList": []}))),
        )
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("POST"))
        .and(path(CERT_DELETE))
        .and(body_json(json!({"certIds": ["cert-1"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(success()))
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("POST"))
        .and(path(DEVICE_ADD))
        .and(body_json(json!({"deviceList": [
            {"deviceName": "test phone", "udid": "UDID-1", "deviceType": 4}
        ]})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({
                "successedCount": 1, "failedCount": 0, "failedAddDeviceDetail": []
            }))),
        )
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("GET"))
        .and(path(DEVICE_LIST))
        .and(query_param("deviceName", "test"))
        .and(query_param("fromRecCount", "1"))
        .and(query_param("maxReqCount", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(with_success(json!({
            "totalCount": 1,
            "deviceList": [{"id": "dev-1", "deviceName": "test phone", "udid": "UDID-1", "deviceType": 4}],
        }))))
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("POST"))
        .and(path(DEVICE_DELETE))
        .and(body_json(json!({"deviceIdList": ["dev-1"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(success()))
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("GET"))
        .and(path(PROFILE_LIST))
        .and(header("appId", APP_ID))
        .and(query_param("maxReqCount", "10"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({
                "totalCount": 0, "provisionList": []
            }))),
        )
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("POST"))
        .and(path(PROFILE))
        .and(body_json(json!({
            "provisionName": "debug-profile",
            "provisionType": 1,
            "certId": "cert-1",
            "appId": APP_ID,
            "deviceIdList": ["dev-1"],
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({
                "provisionInfo": {"id": "prov-1", "provisionName": "debug-profile"}
            }))),
        )
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("PUT"))
        .and(path(PROFILE))
        .and(body_json(
            json!({"provisionId": "prov-1", "deviceIdList": ["dev-1", "dev-2"]}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({
                "provisionInfo": {"id": "prov-1"}
            }))),
        )
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("DELETE"))
        .and(path(PROFILE_DELETE))
        .and(query_param("id", "prov-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success()))
        .expect(1)
        .mount(api)
        .await;

    ok(&f
        .call("list_certificates", json!({"body": {"certType": 2}}))
        .await);
    ok(&f
        .call(
            "delete_certificates",
            json!({"body": {"certIds": ["cert-1"]}}),
        )
        .await);
    let added = ok(&f
        .call(
            "add_devices",
            json!({"body": {"deviceList": [
                {"deviceName": "test phone", "udid": "UDID-1", "deviceType": 4}
            ]}}),
        )
        .await);
    assert_eq!(added["successedCount"], 1);
    let devices = ok(&f
        .call(
            "list_devices",
            json!({"deviceName": "test", "fromRecCount": 1, "maxReqCount": 100}),
        )
        .await);
    assert_eq!(devices["deviceList"][0]["id"], "dev-1");
    ok(&f
        .call(
            "delete_devices",
            json!({"body": {"deviceIdList": ["dev-1"]}}),
        )
        .await);
    ok(&f
        .call("list_profiles", json!({"appId": APP_ID, "maxReqCount": 10}))
        .await);
    ok(&f
        .call(
            "create_profile",
            json!({"body": {
                "provisionName": "debug-profile",
                "provisionType": 1,
                "certId": "cert-1",
                "appId": APP_ID,
                "deviceIdList": ["dev-1"],
            }}),
        )
        .await);
    ok(&f
        .call(
            "update_profile_devices",
            json!({"body": {"provisionId": "prov-1", "deviceIdList": ["dev-1", "dev-2"]}}),
        )
        .await);
    ok(&f.call("delete_profile", json!({"id": "prov-1"})).await);

    // Ошибка с HTTP 200 и кодом Provisioning API — ошибка инструмента.
    Mock::given(method("POST"))
        .and(path(DEVICE_DELETE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 204144769, "msg": "Some devices are in use."}
        })))
        .mount(api)
        .await;
    let result = f
        .call(
            "delete_devices",
            json!({"body": {"deviceIdList": ["dev-2"]}}),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("204144769"), "{}", text(&result));
}

#[tokio::test]
async fn download_links_are_hidden_without_save_to() {
    let f = fixture(AccessLevel::Write).await;
    Mock::given(method("POST"))
        .and(path(CERT_LIST))
        .respond_with(ResponseTemplate::new(200).set_body_json(with_success(json!({
            "certList": [cert_info("cert-1", &f.link("a.cer")), cert_info("cert-2", &f.link("b.cer"))],
            "certInfo": [cert_info("cert-1", &f.link("a.cer"))],
        }))))
        .mount(&f.api)
        .await;
    Mock::given(method("GET"))
        .and(path(PROFILE_LIST))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({
                "totalCount": 1,
                "provisionList": [profile_info(&f.link("p.p7b"))],
            }))),
        )
        .mount(&f.api)
        .await;
    Mock::given(method("POST"))
        .and(path(PROFILE))
        .respond_with(ResponseTemplate::new(200).set_body_json(with_success(
            json!({"provisionInfo": profile_info(&f.link("p.p7b"))}),
        )))
        .mount(&f.api)
        .await;

    let body = ok(&f.call("list_certificates", json!({"body": {}})).await);
    for list in ["certList", "certInfo"] {
        for cert in body["result"][list].as_array().unwrap() {
            assert_eq!(cert["certDownloadUrl"], "***", "{body}");
            assert!(cert["id"].as_str().is_some(), "{body}");
        }
    }
    let note = body["note"].as_str().unwrap();
    assert!(note.contains("save_to"), "{note}");
    assert!(note.contains("certIds"), "{note}");

    let body = ok(&f.call("list_profiles", json!({"appId": APP_ID})).await);
    let profile = &body["result"]["provisionList"][0];
    assert_eq!(profile["provisionDownloadUrl"], "***");
    assert_eq!(profile["deviceList"][0]["udid"], "UDID-1");
    assert!(
        body["note"].as_str().unwrap().contains("provisionId"),
        "{body}"
    );

    let body = ok(&f
        .call(
            "create_profile",
            json!({"body": {"provisionName": "p", "provisionType": 2, "certId": "c", "appId": APP_ID}}),
        )
        .await);
    assert_eq!(
        body["result"]["provisionInfo"]["provisionDownloadUrl"],
        "***"
    );
    assert!(body["note"].as_str().unwrap().contains("save_to"), "{body}");

    // `api_request` по пути операции скрывает её ссылки так же.
    let body = ok(&f
        .call(
            "api_request",
            json!({"method": "POST", "path": CERT_LIST, "body": {}}),
        )
        .await);
    assert_eq!(body["result"]["certList"][1]["certDownloadUrl"], "***");

    for name in ["a.cer", "b.cer", "p.p7b"] {
        assert!(!f.dir.path().join(name).exists());
    }
    let requests = f.api.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| !r.url.path().starts_with("/files/"))
    );
    for result in [
        f.call("list_certificates", json!({"body": {}})).await,
        f.call("list_profiles", json!({"appId": APP_ID})).await,
    ] {
        assert!(!text(&result).contains(&signature()), "{}", text(&result));
    }
}

#[tokio::test]
async fn save_to_downloads_certificate_and_profile_files() {
    let f = fixture(AccessLevel::Write).await;
    Mock::given(method("POST"))
        .and(path(CERT_LIST))
        .and(body_json(json!({"certIds": ["cert-1"]})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({
                "certList": [cert_info("cert-1", &f.link("cert-1.cer"))],
            }))),
        )
        .mount(&f.api)
        .await;
    Mock::given(method("POST"))
        .and(path(CERT_LIST))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(with_success(json!({
            "certList": [cert_info("cert-1", &f.link("x.cer")), cert_info("cert-2", &f.link("y.cer"))],
        }))))
        .mount(&f.api)
        .await;
    Mock::given(method("GET"))
        .and(path(PROFILE_LIST))
        .and(header("provisionId", "prov-1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({
                "totalCount": 1,
                "provisionList": [profile_info(&f.link("prov-1.p7b"))],
            }))),
        )
        .mount(&f.api)
        .await;
    Mock::given(method("PUT"))
        .and(path(PROFILE))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({
                "provisionInfo": profile_info(&f.link("updated.p7b")),
            }))),
        )
        .mount(&f.api)
        .await;
    f.mock_file("cert-1.cer", b"CER-1").await;
    f.mock_file("prov-1.p7b", b"P7B-1").await;
    f.mock_file("updated.p7b", b"P7B-UPDATED").await;

    let target = f.file("cert-1.cer");
    let body = ok(&f
        .call(
            "list_certificates",
            json!({"body": {"certIds": ["cert-1"]}, "save_to": target}),
        )
        .await);
    assert_eq!(std::fs::read(&target).unwrap(), b"CER-1");
    assert_eq!(body["saved"]["bytes"], 5);
    assert_eq!(body["saved"]["field"], "certList[].certDownloadUrl");
    assert_eq!(body["result"]["certList"][0]["certDownloadUrl"], "***");

    let target = f.file("prov-1.p7b");
    let body = ok(&f
        .call(
            "list_profiles",
            json!({"appId": APP_ID, "provisionId": "prov-1", "save_to": target}),
        )
        .await);
    assert_eq!(std::fs::read(&target).unwrap(), b"P7B-1");
    assert_eq!(
        body["saved"]["field"],
        "provisionList[].provisionDownloadUrl"
    );

    let target = f.file("updated.p7b");
    let body = ok(&f
        .call(
            "update_profile_devices",
            json!({"body": {"provisionId": "prov-1", "deviceIdList": ["dev-1"]}, "save_to": target}),
        )
        .await);
    assert_eq!(std::fs::read(&target).unwrap(), b"P7B-UPDATED");
    assert_eq!(
        body["result"]["provisionInfo"]["provisionDownloadUrl"],
        "***"
    );
    assert!(!text_of(&body).contains(&signature()));

    // Два сертификата и один `save_to`: чтение — ошибка, файла нет.
    let target = f.file("two.cer");
    let result = f
        .call("list_certificates", json!({"body": {}, "save_to": target}))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("narrow the request"),
        "{}",
        text(&result)
    );
    assert!(!text(&result).contains(&signature()), "{}", text(&result));
    assert!(!std::path::Path::new(&target).exists());

    // Существующий файл не перезаписывается, путь проверяется до запроса.
    let before = f.requests().await;
    let result = f
        .call(
            "list_certificates",
            json!({"body": {"certIds": ["cert-1"]}, "save_to": f.file("cert-1.cer")}),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("already exists"),
        "{}",
        text(&result)
    );
    assert_eq!(f.requests().await, before);
}

fn text_of(value: &Value) -> String {
    value.to_string()
}

#[tokio::test]
async fn deleting_needs_allow_destructive() {
    let f = fixture(AccessLevel::Write).await;
    for (name, args) in [
        (
            "delete_certificates",
            json!({"body": {"certIds": ["cert-1"]}}),
        ),
        (
            "delete_devices",
            json!({"body": {"deviceIdList": ["dev-1"]}}),
        ),
        ("delete_profile", json!({"id": "prov-1"})),
    ] {
        let result = f.call(name, args).await;
        assert_eq!(result.is_error, Some(true), "{name}");
        assert!(
            text(&result).contains("destructive access"),
            "{name}: {}",
            text(&result)
        );
    }
    // `api_request` берёт уровень у операции пути, а не у метода.
    for (method_name, path_text) in [
        ("POST", CERT_DELETE.to_owned()),
        ("POST", DEVICE_DELETE.to_owned()),
        ("DELETE", format!("{PROFILE_DELETE}?id=prov-1")),
    ] {
        let result = f
            .call(
                "api_request",
                json!({"method": method_name, "path": path_text, "body": {}}),
            )
            .await;
        assert_eq!(result.is_error, Some(true), "{path_text}");
        assert!(
            text(&result).contains("destructive access"),
            "{path_text}: {}",
            text(&result)
        );
    }
    // Только чтение: и запись не регистрируется.
    let read = fixture(AccessLevel::Read).await;
    for name in ["certificate_request", "add_devices", "create_profile"] {
        let result = read.call(name, json!({})).await;
        assert_eq!(result.is_error, Some(true), "{name}");
        assert!(
            text(&result).contains("write access"),
            "{name}: {}",
            text(&result)
        );
    }
    assert_eq!(f.requests().await, 0);
    assert_eq!(read.requests().await, 0);
}

fn csr_pem() -> String {
    format!(
        "-----BEGIN NEW CERTIFICATE REQUEST-----\n{}\n{}\n-----END NEW CERTIFICATE REQUEST-----\n",
        ["MIIBPTCB5AIBADBC", "MQswCQYDVQQGEwJS"].concat(),
        ["VTEPMA0GA1UEChMG", "RXhhbXBsZTEiMCAG"].concat(),
    )
}

#[tokio::test]
async fn certificate_request_reads_the_csr_file_and_saves_the_certificate() {
    let f = fixture(AccessLevel::Write).await;
    std::fs::write(f.dir.path().join("debug.csr"), csr_pem()).unwrap();
    Mock::given(method("POST"))
        .and(path(CERT_CREATE))
        .and(body_json(json!({
            "csr": csr_pem().trim_end(),
            "certName": "debug-cert",
            "certType": 1,
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_success(json!({
                "certInfo": {
                    "id": "cert-9",
                    "certName": "debug-cert",
                    "certType": 1,
                    "createTime": 1_718_363_106_749_i64,
                    "expireTime": 1_844_593_505_000_i64,
                    "certDownloadUrl": f.link("cert-9.cer"),
                },
            }))),
        )
        .expect(1)
        .mount(&f.api)
        .await;
    f.mock_file("cert-9.cer", b"CER-9").await;

    let target = f.file("cert-9.cer");
    let result = f
        .call(
            "certificate_request",
            json!({
                "csr_file": f.file("debug.csr"),
                "certName": "debug-cert",
                "certType": 1,
                "save_to": target,
            }),
        )
        .await;
    let body = ok(&result);
    assert_eq!(std::fs::read(&target).unwrap(), b"CER-9");
    assert_eq!(body["certificate"]["id"], "cert-9");
    assert_eq!(body["saved"]["bytes"], 5);
    let text = text(&result);
    assert!(!text.contains("certDownloadUrl"), "{text}");
    assert!(!text.contains(&signature()), "{text}");
    assert!(!text.contains("MIIBPTCB"), "{text}");

    // CSR текстом аргументом не принимается.
    let result = f
        .call(
            "certificate_request",
            json!({"csr": csr_pem(), "certName": "x", "certType": 1}),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn certificate_operation_is_closed_to_api_request() {
    let f = fixture(AccessLevel::Destructive).await;
    let result = f
        .call(
            "api_request",
            json!({
                "method": "POST",
                "path": CERT_CREATE,
                "body": {"csr": csr_pem(), "certName": "x", "certType": 1},
            }),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("certificate_request"),
        "{}",
        text(&result)
    );
    assert_eq!(f.requests().await, 0);

    // Поиск находит составной инструмент и исключённую операцию с заменой.
    let body = ok(&f
        .call(
            "api_search",
            json!({"query": "request certificate", "limit": 50}),
        )
        .await);
    let results = body["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["tool"] == "certificate_request"),
        "{body}"
    );
    let excluded = results
        .iter()
        .find(|r| r["operation_id"] == "provisioning.cert.create")
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(excluded["callable"], false);
    assert!(
        excluded["alternative"]
            .as_str()
            .unwrap()
            .contains("certificate_request")
    );
}
