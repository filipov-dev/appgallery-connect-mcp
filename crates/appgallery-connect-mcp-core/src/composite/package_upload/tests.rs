//! Тесты `package_upload`: API и файловое хранилище — один `wiremock` на loopback
//! (`TransferHosts::HuaweiAndLoopback`), файлы — во временном каталоге, учётные данные и
//! подписи выдуманы и собраны из частей.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy, TransferHosts,
};
use sha2::{Digest, Sha256};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request as WireRequest, ResponseTemplate};

use super::*;
use crate::composite::{MemoryVault, ProfileInfo};
use crate::files::FilePolicy;

const APP: &str = "100000001";
const UPLOAD_URL_PATH: &str = "/api/publish/v2/upload-url/for-obs";
const INIT_PATH: &str = "/api/publish/v2/upload/multipart/init";
const PARTS_PATH: &str = "/api/publish/v2/upload/multipart/parts";
const COMPOSE_PATH: &str = "/api/publish/v2/upload/multipart/compose";
const FILE_INFO_PATH: &str = "/api/publish/v2/app-file-info";
const STATUS_PATH: &str = "/api/publish/v2/package/compile/status";

/// Подпись хранилища в заголовке `Authorization` загрузки.
fn signature() -> String {
    [
        "AWS4-HMAC-SHA256 Credential=",
        "TESTKEY0042",
        "/20261115/ru/s3/aws4_request, Signature=",
        "c0ffee0123456789",
    ]
    .concat()
}

/// Подпись в адресе загрузки.
fn url_signature() -> String {
    ["5ig", "n4ture", "-in-url-", "0123456789"].concat()
}

/// Идентификатор объекта в хранилище.
fn object_id() -> String {
    ["CN/2026", "1115/obj-", "secret-4711"].concat()
}

/// Идентификатор загрузки частями.
fn upload_id() -> String {
    ["nsp-upload-", "id-0042"].concat()
}

/// Всё, чего не должно быть в ответе, ошибке и логах.
fn secrets() -> Vec<String> {
    vec![
        signature(),
        url_signature(),
        object_id(),
        upload_id(),
        "/storage/".to_owned(),
    ]
}

fn assert_clean(text: &str) {
    for secret in secrets() {
        assert!(!text.contains(&secret), "`{secret}` leaked: {text}");
    }
}

struct Fixture {
    server: MockServer,
    dir: tempfile::TempDir,
    ctx: Context,
}

async fn fixture(apps: &[&str]) -> Fixture {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&server, dir.path(), apps);
    Fixture { server, dir, ctx }
}

fn context(server: &MockServer, root: &Path, apps: &[&str]) -> Context {
    let id = ["3141", "5926", "5358"].concat();
    let secret = ["ZX98", "CV76", "BN54"].concat().repeat(4);
    let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
    let mut config = ClientConfig::new(Region::Ru, Some(credentials));
    config.base_url = Some(server.uri());
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    config.transfer.hosts = TransferHosts::HuaweiAndLoopback;
    let token = ["upload", "-test-", "token"].concat();
    let api = ApiClient::with_token_source(config, Arc::new(StaticToken::new(token))).unwrap();
    Context {
        api: Arc::new(api),
        files: Arc::new(FilePolicy::new(&[root.to_path_buf()]).unwrap()),
        vault: Arc::new(MemoryVault::default()),
        profile: ProfileInfo {
            name: "work".into(),
            apps: apps.iter().map(|app| (*app).to_owned()).collect(),
            packages: Vec::new(),
        },
        access: AccessLevel::Write,
        http: false,
    }
}

fn tuning(threshold: u64) -> Tuning {
    Tuning {
        upload: UploadTuning {
            multipart_threshold: threshold,
            part_bytes: 8,
            retries: 2,
            retry_delay: Duration::from_millis(1),
        },
        poll_interval: Duration::from_millis(10),
    }
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().into_owned()
}

fn ok(extra: Value) -> ResponseTemplate {
    let mut body = json!({"ret": {"code": 0, "msg": "success"}});
    if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
        body.extend(extra);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

/// `urlInfo`/`FilePartUploadInfo` с адресом на хранилище `wiremock`.
fn target(uri: &str, storage_path: &str) -> Value {
    let host = uri.trim_start_matches("http://");
    json!({
        "objectId": object_id(),
        "url": format!("{uri}{storage_path}?uploadId={}&X-Amz-Signature={}", upload_id(),
            url_signature()),
        "method": "PUT",
        "partObjectId": "",
        "headers": {
            "Authorization": signature(),
            "x-amz-date": "20261115T000000Z",
            "x-amz-content-sha256": "UNSIGNED-PAYLOAD",
            "Host": host,
            "Content-Type": "application/octet-stream",
            "user-agent": "agc-test-agent",
        }
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn mount_upload_url(server: &MockServer, storage_path: &str) {
    Mock::given(method("GET"))
        .and(path(UPLOAD_URL_PATH))
        .respond_with(ok(json!({"urlInfo": target(&server.uri(), storage_path)})))
        .mount(server)
        .await;
}

async fn mount_attach(server: &MockServer, file_name: &str, pkg_id: &str) {
    Mock::given(method("PUT"))
        .and(path(FILE_INFO_PATH))
        .and(query_param("appId", APP))
        .and(body_json(json!({
            "fileType": 5,
            "files": [{"fileName": file_name, "fileDestUrl": object_id()}],
        })))
        .respond_with(ok(json!({"pkgVersion": [pkg_id]})))
        .expect(1)
        .mount(server)
        .await;
}

/// Части: адрес части `N` — `/storage/partN`; хранилище отвечает `ETag` `"etag-N"`.
async fn mount_multipart(server: &MockServer, parts: usize) {
    Mock::given(method("POST"))
        .and(path(INIT_PATH))
        .and(query_param("appId", APP))
        .and(query_param("fileType", "3"))
        .and(query_param("chineseMainlandFlag", "0"))
        .respond_with(ok(json!({
            "objectId": object_id(),
            "nspUploadId": upload_id(),
            "nspPartMinSize": 1,
            "nspPartMaxSize": 8,
        })))
        .expect(1)
        .mount(server)
        .await;
    let uri = server.uri();
    Mock::given(method("POST"))
        .and(path(PARTS_PATH))
        .and(query_param("objectId", object_id()))
        .and(query_param("nspUploadId", upload_id()))
        .respond_with(move |request: &WireRequest| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let key = body.as_object().unwrap().keys().next().unwrap().clone();
            let number = key.trim_start_matches("additionalProp");
            ok(json!({"uploadInfoMap": {
                key.clone(): target(&uri, &format!("/storage/part{number}")),
            }}))
        })
        .mount(server)
        .await;
    for number in 1..=parts {
        Mock::given(method("PUT"))
            .and(path(format!("/storage/part{number}")))
            .respond_with(
                ResponseTemplate::new(200).insert_header("ETag", format!("\"etag-{number}\"")),
            )
            .mount(server)
            .await;
    }
}

async fn requests_to(server: &MockServer, request_path: &str) -> Vec<WireRequest> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == request_path)
        .collect()
}

#[tokio::test]
async fn an_apk_is_uploaded_in_one_request_and_attached() {
    let f = fixture(&[APP]).await;
    let content = b"apk-content-".repeat(10);
    let file = write(f.dir.path(), "app.apk", &content);
    Mock::given(method("GET"))
        .and(path(UPLOAD_URL_PATH))
        .and(query_param("appId", APP))
        .and(query_param("fileName", "app.apk"))
        .and(query_param("contentLength", content.len().to_string()))
        .and(query_param("sha256", sha256_hex(&content)))
        .and(query_param("chineseMainlandFlag", "0"))
        .respond_with(ok(
            json!({"urlInfo": target(&f.server.uri(), "/storage/single")}),
        ))
        .expect(1)
        .mount(&f.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/storage/single"))
        .and(header("content-type", "application/octet-stream"))
        .and(header("user-agent", "agc-test-agent"))
        .and(header("x-amz-date", "20261115T000000Z"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&f.server)
        .await;
    mount_attach(&f.server, "app.apk", "pkg-77").await;
    Mock::given(method("GET"))
        .and(path(STATUS_PATH))
        .respond_with(ok(json!({})))
        .expect(0)
        .mount(&f.server)
        .await;

    let out = package_upload(&f.ctx, json!({"file": file}), &tuning(1024))
        .await
        .unwrap();
    assert_eq!(out["appId"], APP);
    assert_eq!(out["fileName"], "app.apk");
    assert_eq!(out["bytes"], content.len());
    assert_eq!(out["packageType"], "apk");
    assert_eq!(out["sha256"], sha256_hex(&content));
    assert_eq!(
        out["upload"],
        json!({"chunked": false, "parts": 1, "retries": 0})
    );
    assert_eq!(out["attached"], true);
    assert_eq!(out["pkgId"], "pkg-77");
    assert_eq!(out["packageStatus"], "not_checked");
    assert_eq!(out["readyForReview"], false);
    assert!(out["next"].as_str().unwrap().contains("pkg-77"), "{out}");
    assert_clean(&out.to_string());

    let stored = requests_to(&f.server, "/storage/single").await;
    assert_eq!(stored[0].body, content);
    // Подпись хранилища уходит как есть (в ней запятые: matcher `header` её бы разрезал).
    assert_eq!(stored[0].headers["authorization"], signature().as_str());
    assert_eq!(
        stored[0].headers["content-length"],
        content.len().to_string().as_str()
    );
    // Токен Connect API на адрес хранилища не уходит.
    assert!(!stored[0].headers.contains_key("client_id"));
}

#[tokio::test]
async fn a_large_package_is_uploaded_in_parts_and_merged() {
    let f = fixture(&[APP]).await;
    let content = b"abcdefghijklmnopqrst";
    let file = write(f.dir.path(), "big.apk", content);
    mount_multipart(&f.server, 3).await;
    Mock::given(method("POST"))
        .and(path(COMPOSE_PATH))
        .and(query_param("objectId", object_id()))
        .and(query_param("nspUploadId", upload_id()))
        .and(body_json(json!({
            "additionalProp1": {"partObjectId": "", "etag": "etag-1"},
            "additionalProp2": {"partObjectId": "", "etag": "etag-2"},
            "additionalProp3": {"partObjectId": "", "etag": "etag-3"},
        })))
        .respond_with(ok(json!({})))
        .expect(1)
        .mount(&f.server)
        .await;
    mount_attach(&f.server, "big.apk", "pkg-1").await;

    let out = package_upload(&f.ctx, json!({"file": file}), &tuning(10))
        .await
        .unwrap();
    assert_eq!(
        out["upload"],
        json!({"chunked": true, "parts": 3, "retries": 0})
    );
    assert!(out.get("sha256").is_none(), "{out}");
    assert_eq!(out["pkgId"], "pkg-1");
    assert_clean(&out.to_string());

    for (number, expected) in [(1, &b"abcdefgh"[..]), (2, b"ijklmnop"), (3, b"qrst")] {
        let stored = requests_to(&f.server, &format!("/storage/part{number}")).await;
        assert_eq!(stored.len(), 1, "part {number}");
        assert_eq!(stored[0].body, expected, "part {number}");
        assert_eq!(stored[0].headers["authorization"], signature().as_str());
    }
    let lengths: Vec<Value> = requests_to(&f.server, PARTS_PATH)
        .await
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(
        lengths,
        vec![
            json!({"additionalProp1": {"length": 8}}),
            json!({"additionalProp2": {"length": 8}}),
            json!({"additionalProp3": {"length": 4}}),
        ]
    );
}

#[tokio::test]
async fn a_failed_part_is_retried_with_a_new_url() {
    let f = fixture(&[APP]).await;
    let file = write(f.dir.path(), "big.aab", b"abcdefghijklmnopqrst");
    // Первая попытка части 2 — сбой хранилища; смонтирована раньше и поэтому первой.
    Mock::given(method("PUT"))
        .and(path("/storage/part2"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&f.server)
        .await;
    mount_multipart(&f.server, 3).await;
    Mock::given(method("POST"))
        .and(path(COMPOSE_PATH))
        .respond_with(ok(json!({})))
        .expect(1)
        .mount(&f.server)
        .await;
    mount_attach(&f.server, "big.aab", "pkg-2").await;

    let out = package_upload(
        &f.ctx,
        json!({"file": file, "appId": APP, "waitSeconds": 0}),
        &tuning(10),
    )
    .await
    .unwrap();
    assert_eq!(
        out["upload"],
        json!({"chunked": true, "parts": 3, "retries": 1})
    );
    assert_eq!(out["packageType"], "aab");
    assert_eq!(requests_to(&f.server, PARTS_PATH).await.len(), 4);
    let part2 = requests_to(&f.server, "/storage/part2").await;
    assert_eq!(part2.len(), 2);
    assert_eq!(part2[1].body, b"ijklmnop");
}

#[tokio::test]
async fn exhausted_retries_stop_before_merging_and_attaching() {
    let f = fixture(&[APP]).await;
    let file = write(f.dir.path(), "big.apk", b"abcdefghijklmnopqrst");
    Mock::given(method("PUT"))
        .and(path("/storage/part2"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.server)
        .await;
    mount_multipart(&f.server, 3).await;
    for request_path in [COMPOSE_PATH, FILE_INFO_PATH] {
        Mock::given(path(request_path))
            .respond_with(ok(json!({})))
            .expect(0)
            .mount(&f.server)
            .await;
    }

    let err = package_upload(&f.ctx, json!({"file": file}), &tuning(10))
        .await
        .unwrap_err();
    let text = err.message();
    assert!(text.contains("part 2 of 3"), "{text}");
    assert!(text.contains("after 2 retries"), "{text}");
    assert!(text.contains("HTTP 503"), "{text}");
    assert!(text.contains("nothing was attached"), "{text}");
    assert_clean(text);
    // Первая попытка и два повтора.
    assert_eq!(requests_to(&f.server, "/storage/part2").await.len(), 3);
    assert!(requests_to(&f.server, "/storage/part3").await.is_empty());
}

#[tokio::test]
async fn an_unfinished_aab_build_times_out() {
    let f = fixture(&[APP]).await;
    let file = write(f.dir.path(), "app.aab", b"aab-content");
    mount_upload_url(&f.server, "/storage/single").await;
    Mock::given(method("PUT"))
        .and(path("/storage/single"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&f.server)
        .await;
    mount_attach(&f.server, "app.aab", "pkg-9").await;
    Mock::given(method("GET"))
        .and(path(STATUS_PATH))
        .and(query_param("appId", APP))
        .and(query_param("pkgIds", "pkg-9"))
        .respond_with(ok(json!({"pkgStateList": [
            {"pkgId": "pkg-9", "aabCompileStatus": 1, "successStatus": 1},
        ]})))
        .mount(&f.server)
        .await;

    let out = package_upload(
        &f.ctx,
        json!({"file": file, "waitSeconds": 1}),
        &tuning(1024),
    )
    .await
    .unwrap();
    assert_eq!(out["packageStatus"], "processing");
    assert_eq!(out["readyForReview"], false);
    assert_eq!(out["timedOut"], true);
    assert!(out["statusChecks"].as_u64().unwrap() >= 2, "{out}");
    assert!(
        out["notes"].to_string().contains("still being processed"),
        "{out}"
    );
    assert!(
        out["next"].as_str().unwrap().contains("waitSeconds"),
        "{out}"
    );
}

#[tokio::test]
async fn a_compiled_aab_is_ready_for_review() {
    let f = fixture(&[APP]).await;
    let file = write(f.dir.path(), "app.aab", b"aab-content");
    mount_upload_url(&f.server, "/storage/single").await;
    Mock::given(method("PUT"))
        .and(path("/storage/single"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&f.server)
        .await;
    mount_attach(&f.server, "app.aab", "pkg-5").await;
    Mock::given(method("GET"))
        .and(path(STATUS_PATH))
        .respond_with(ok(json!({"pkgStateList": [
            {"pkgId": "pkg-5", "aabCompileStatus": 1, "successStatus": 1},
        ]})))
        .up_to_n_times(1)
        .mount(&f.server)
        .await;
    Mock::given(method("GET"))
        .and(path(STATUS_PATH))
        .respond_with(ok(json!({"pkgStateList": [
            {"pkgId": "pkg-5", "aabCompileStatus": 2, "successStatus": 0},
        ]})))
        .mount(&f.server)
        .await;

    // Без `waitSeconds`: для AAB ожидание включено по умолчанию.
    let out = package_upload(&f.ctx, json!({"file": file}), &tuning(1024))
        .await
        .unwrap();
    assert_eq!(out["packageStatus"], "ready");
    assert_eq!(out["readyForReview"], true);
    assert_eq!(out["statusChecks"], 2);
    assert_eq!(out["timedOut"], false);
    assert!(
        out["next"].as_str().unwrap().contains("submit_app"),
        "{out}"
    );
}

#[tokio::test]
async fn a_failed_aab_build_is_reported_with_the_reason() {
    let f = fixture(&[APP]).await;
    let file = write(f.dir.path(), "app.aab", b"aab-content");
    mount_upload_url(&f.server, "/storage/single").await;
    Mock::given(method("PUT"))
        .and(path("/storage/single"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&f.server)
        .await;
    mount_attach(&f.server, "app.aab", "pkg-6").await;
    Mock::given(method("GET"))
        .and(path(STATUS_PATH))
        .respond_with(ok(json!({"pkgStateList": {
            "pkgId": "pkg-6", "aabCompileStatus": 3, "failReason": 19, "successStatus": 2,
        }})))
        .mount(&f.server)
        .await;

    let out = package_upload(&f.ctx, json!({"file": file}), &tuning(1024))
        .await
        .unwrap();
    assert_eq!(out["packageStatus"], "failed");
    assert_eq!(out["readyForReview"], false);
    assert_eq!(out["failReason"]["code"], 19);
    assert!(
        out["failReason"]["description"]
            .as_str()
            .unwrap()
            .contains("150 MB")
    );
}

#[tokio::test]
async fn files_and_arguments_are_checked_before_any_request() {
    let f = fixture(&[APP, "100000002"]).await;
    let apk = write(f.dir.path(), "app.apk", b"apk");
    let zip = write(f.dir.path(), "app.zip", b"zip");
    let empty = write(f.dir.path(), "empty.apk", b"");
    let exe = write(f.dir.path(), "setup.exe", b"exe");
    let big = f.dir.path().join("big.aab");
    // Разреженный файл: размер больше предела AAB без записи 150 MB на диск.
    std::fs::File::create(&big)
        .unwrap()
        .set_len(150 * MIB + 1)
        .unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = write(outside_dir.path(), "app.apk", b"apk");

    let cases = [
        (json!({"file": zip, "appId": APP}), "not an app package"),
        (json!({"file": empty, "appId": APP}), "is empty"),
        (
            json!({"file": big.to_string_lossy(), "appId": APP}),
            "AAB packages up to 157286400 bytes (150 MB)",
        ),
        (
            json!({"file": outside, "appId": APP}),
            "outside the allowed directories",
        ),
        (
            json!({"file": f.dir.path().join("missing.apk").to_string_lossy(), "appId": APP}),
            "does not exist",
        ),
        (json!({"file": exe, "appId": APP}), "need `versionName`"),
        (
            json!({"file": apk, "appId": APP, "versionName": "1.0"}),
            "only for EXE",
        ),
        (
            json!({"file": apk, "appId": APP, "releaseType": 2}),
            "`releaseType` must be",
        ),
        (
            json!({"file": apk, "appId": APP, "waitSeconds": 100_000}),
            "`waitSeconds` must be",
        ),
        (
            json!({"file": apk, "appId": "com.example.app"}),
            "numeric app ID",
        ),
        (json!({"file": apk}), "several default apps"),
        (json!({"appId": APP}), "`file` is required"),
    ];
    for (args, expected) in cases {
        let err = package_upload(&f.ctx, args.clone(), &tuning(1024))
            .await
            .unwrap_err();
        assert!(err.message().contains(expected), "{args}: {err}");
    }
    assert!(f.server.received_requests().await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_allowed_directories_is_rejected() {
    let f = fixture(&[APP]).await;
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = write(outside_dir.path(), "real.apk", b"apk");
    let link = f.dir.path().join("link.apk");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let err = package_upload(
        &f.ctx,
        json!({"file": link.to_string_lossy()}),
        &tuning(1024),
    )
    .await
    .unwrap_err();
    assert!(
        err.message().contains("outside the allowed directories"),
        "{err}"
    );
    assert!(f.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn dry_run_sends_no_requests() {
    let f = fixture(&[APP]).await;
    let file = write(f.dir.path(), "big.aab", b"abcdefghijklmnopqrst");
    let out = package_upload(
        &f.ctx,
        json!({"file": file, "dryRun": true, "releaseType": 3}),
        &tuning(10),
    )
    .await
    .unwrap();
    assert_eq!(out["dryRun"], true);
    assert_eq!(out["appId"], APP);
    assert_eq!(out["fileName"], "big.aab");
    assert_eq!(out["bytes"], 20);
    assert_eq!(out["packageType"], "aab");
    assert_eq!(
        out["upload"],
        json!({"chunked": true, "parts": 3, "partBytes": 8})
    );
    assert_eq!(out["waitSeconds"], DEFAULT_AAB_WAIT_SECS);
    let steps = out["steps"].to_string();
    assert!(steps.contains("phased release"), "{steps}");
    assert!(steps.contains("get_package_compile_status"), "{steps}");
    assert!(
        out["note"]
            .as_str()
            .unwrap()
            .contains("Nothing was uploaded")
    );
    assert!(f.server.received_requests().await.unwrap().is_empty());

    // Обычный APK — одним запросом и без ожидания.
    let apk = write(f.dir.path(), "app.apk", b"apk");
    let out = package_upload(&f.ctx, json!({"file": apk, "dryRun": true}), &tuning(10))
        .await
        .unwrap();
    assert_eq!(out["upload"]["chunked"], false);
    assert_eq!(out["waitSeconds"], 0);
    assert!(f.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn upload_urls_and_headers_stay_out_of_errors_and_logs() {
    let logs = captured_logs();
    let f = fixture(&[APP]).await;
    let file = write(f.dir.path(), "app.apk", b"apk-content");

    // Хранилище отказывает и отражает в тексте подпись и объект.
    mount_upload_url(&f.server, "/storage/single").await;
    let xml = format!(
        "<Error><Code>AccessDenied</Code><Message>denied for {} by {}</Message>\
         <StringToSign>{}</StringToSign></Error>",
        object_id(),
        signature(),
        url_signature()
    );
    Mock::given(method("PUT"))
        .and(path("/storage/single"))
        .respond_with(ResponseTemplate::new(403).set_body_string(xml))
        .up_to_n_times(3)
        .mount(&f.server)
        .await;
    let err = package_upload(&f.ctx, json!({"file": file.clone()}), &tuning(1024))
        .await
        .unwrap_err();
    let text = err.message();
    assert!(text.contains("HTTP 403"), "{text}");
    assert!(text.contains("AccessDenied"), "{text}");
    assert!(text.contains("after 2 retries"), "{text}");
    assert_clean(text);
    assert!(!text.contains("StringToSign"), "{text}");

    // Привязка отказывает и отражает идентификатор объекта.
    Mock::given(method("PUT"))
        .and(path("/storage/single"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&f.server)
        .await;
    Mock::given(method("PUT"))
        .and(path(FILE_INFO_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 204144647, "msg": format!("invalid fileDestUrl {}", object_id())},
        })))
        .mount(&f.server)
        .await;
    let err = package_upload(&f.ctx, json!({"file": file}), &tuning(1024))
        .await
        .unwrap_err();
    let text = err.message();
    assert!(text.contains("attaching it to the app"), "{text}");
    assert!(text.contains("code 204144647"), "{text}");
    assert_clean(text);

    // Логи: загрузка в них видна, адреса, подписи и идентификаторы — нет.
    let logged = logs.text();
    assert!(logged.contains("uploading a file"), "{logged}");
    assert_clean(&logged);
}

#[test]
fn package_states_are_read_carefully() {
    let state = |entry: Value| package_state(&json!({"pkgStateList": [entry]}), "p");
    assert_eq!(
        state(json!({"pkgId": "p", "successStatus": 0})),
        Some((BuildStatus::Ready, None))
    );
    assert_eq!(
        state(json!({"pkgId": "p", "successStatus": 0, "aabCompileStatus": 1})),
        Some((BuildStatus::Processing, None))
    );
    assert_eq!(
        state(json!({"pkgId": "p", "successStatus": "2", "failReason": 12})),
        Some((BuildStatus::Failed, Some(12)))
    );
    assert_eq!(
        state(json!({"pkgId": "p", "aabCompileStatus": 2})),
        Some((BuildStatus::Ready, None))
    );
    assert_eq!(
        state(json!({"pkgId": "p"})),
        Some((BuildStatus::Unknown, None))
    );
    // Чужой пакет в списке из нескольких не берётся.
    let body = json!({"pkgStateList": [
        {"pkgId": "a", "successStatus": 0}, {"pkgId": "b", "successStatus": 0},
    ]});
    assert_eq!(package_state(&body, "p"), None);
    assert_eq!(package_state(&json!({}), "p"), None);
}

#[test]
fn package_types_and_limits() {
    assert_eq!(PackageType::from_file_name("a.APK"), Some(PackageType::Apk));
    assert_eq!(
        PackageType::from_file_name("a.b.aab"),
        Some(PackageType::Aab)
    );
    assert_eq!(PackageType::from_file_name("apk"), None);
    assert_eq!(PackageType::from_file_name("a.zip"), None);
    assert_eq!(PackageType::Aab.max_bytes(), 150 * MIB);
    assert_eq!(PackageType::Apk.max_bytes(), 4 * GIB);
    assert_eq!(human_size(4 * GIB), "4 GB");
    assert_eq!(human_size(30 * MIB), "30 MB");
    assert_eq!(PackageType::Rpk.multipart(), None);
    assert_eq!(
        PackageType::App.multipart(),
        Some(MultipartFileType::HarmonyApp)
    );
}

#[test]
fn tool_is_registered_as_write_in_releases() {
    let tools = tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, NAME);
    assert_eq!(tools[0].toolset, "releases");
    assert_eq!(tools[0].access, AccessLevel::Write);
    assert!(DESCRIPTION.contains("`submit_app`"));
    let schema = input_schema();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["file"]));
    let tool = crate::registry::composite_tool(&tools[0]).unwrap();
    let annotations = tool.annotations.unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert!(operation(FILE_INFO_UPDATE).is_ok() && operation(COMPILE_STATUS).is_ok());
}

/// Логи тестов этого модуля. Подписчик глобальный (как в `tests/data_safety.rs`): с
/// подписчиком на поток события иногда теряются из-за кэша интереса `tracing`.
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
