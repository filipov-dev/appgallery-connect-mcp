//! Тесты загрузки и скачивания файлов по адресам из ответов API на `wiremock`, без
//! реальной сети. Проверка хоста в тестах расширяется loopback-адресами явным параметром
//! конфигурации.

use std::sync::Arc;
use std::time::Duration;

use appgallery_connect_api::executor::{ApiClient, ApiError, ClientConfig};
use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::transfer::{
    TransferClient, TransferConfig, TransferHosts, UploadRequest,
};
use appgallery_connect_api::{ApiClientCredentials, HttpMethod, Region};
use serde_json::json;
use wiremock::matchers::{body_bytes, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client_id() -> String {
    ["1088", "9300", "8395", "7248"].concat()
}

fn client_secret() -> String {
    ["B15B", "497B", "44E0", "80EB", "E2C4", "DE4E"]
        .concat()
        .repeat(2)
}

fn token() -> String {
    ["access", "-token-", "value-", "0123456789"].concat()
}

/// Подпись хранилища из ответа API (выдумана).
fn signature() -> String {
    [
        "AWS4-HMAC-SHA256 ",
        "Credential=AK",
        "EXAMPLE/x, ",
        "Signature=0f1e2d3c4b5a",
    ]
    .concat()
}

fn transfer_config() -> TransferConfig {
    TransferConfig {
        hosts: TransferHosts::HuaweiAndLoopback,
        timeout: Duration::from_secs(10),
        max_download_bytes: 1024,
    }
}

fn api_client(server: &MockServer) -> ApiClient {
    let credentials = ApiClientCredentials::from_parts(client_id(), client_secret()).unwrap();
    let mut config = ClientConfig::new(Region::Cn, Some(credentials));
    config.base_url = Some(server.uri());
    config.transfer = transfer_config();
    ApiClient::with_token_source(config, Arc::new(StaticToken::new(token()))).unwrap()
}

/// Канонический временный каталог (на macOS `/var` — ссылка на `/private/var`).
fn temp_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

fn host_of(server: &MockServer) -> String {
    server.uri().trim_start_matches("http://").to_owned()
}

#[tokio::test]
async fn uploads_a_file_with_the_headers_from_the_api() {
    let server = MockServer::start().await;
    let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    Mock::given(method("PUT"))
        .and(path("/bucket/object-1.apk"))
        .and(header("x-amz-content-sha256", "UNSIGNED-PAYLOAD"))
        .and(header("x-amz-date", "20260101T000000Z"))
        .and(header("content-type", "application/octet-stream"))
        .and(header("user-agent", "agc-upload"))
        .and(header("content-length", "200000"))
        .and(body_bytes(content.clone()))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"etag-1\""))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, dir) = temp_dir();
    let file = dir.join("app.apk");
    std::fs::write(&file, &content).unwrap();

    let client = api_client(&server);
    let response = client
        .upload_file(
            UploadRequest::new(
                HttpMethod::Put,
                format!("{}/bucket/object-1.apk", server.uri()),
                &file,
            )
            .with_header("Authorization", signature())
            .with_header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
            .with_header("x-amz-date", "20260101T000000Z")
            .with_header("Host", host_of(&server))
            .with_header("user-agent", "agc-upload")
            .with_header("Content-Type", "application/octet-stream")
            .with_header("Content-Length", "200000"),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.bytes, 200_000);
    assert_eq!(response.etag.as_deref(), Some("\"etag-1\""));
    assert_eq!(response.body, None);

    // Учётные данные Connect API на адрес загрузки не отправляются.
    let requests = server.received_requests().await.unwrap();
    // Подпись хранилища из ответа API передана как есть.
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        signature()
    );
    assert!(requests[0].headers.get("client_id").is_none());
    let sent = format!("{:?}", requests[0].headers);
    assert!(!sent.contains(&token()), "{sent}");
    assert!(!sent.contains(&client_secret()), "{sent}");
}

#[tokio::test]
async fn uploads_a_part_of_a_file_and_an_empty_file() {
    let server = MockServer::start().await;
    let content: Vec<u8> = (0..1000u32).map(|i| (i % 7) as u8).collect();
    Mock::given(method("POST"))
        .and(path("/part/2"))
        .and(header("content-length", "300"))
        .and(header("content-type", "application/octet-stream"))
        .and(body_bytes(content[500..800].to_vec()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "part-2")
                .set_body_json(json!({"result": {"resultCode": 0}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/empty"))
        .and(header("content-length", "0"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, dir) = temp_dir();
    let file = dir.join("big.bin");
    std::fs::write(&file, &content).unwrap();
    let empty = dir.join("empty.bin");
    std::fs::write(&empty, b"").unwrap();
    let transfer = TransferClient::new(transfer_config()).unwrap();

    let response = transfer
        .upload_file(
            UploadRequest::new(HttpMethod::Post, format!("{}/part/2", server.uri()), &file)
                .with_range(500, 300),
        )
        .await
        .unwrap();
    assert_eq!(response.bytes, 300);
    assert_eq!(response.etag.as_deref(), Some("part-2"));
    assert_eq!(response.body, Some(json!({"result": {"resultCode": 0}})));

    let response = transfer
        .upload_file(UploadRequest::new(
            HttpMethod::Put,
            format!("{}/empty", server.uri()),
            &empty,
        ))
        .await
        .unwrap();
    assert_eq!(response.bytes, 0);

    // Часть за пределами файла.
    let err = transfer
        .upload_file(
            UploadRequest::new(HttpMethod::Put, format!("{}/part/3", server.uri()), &file)
                .with_range(900, 200),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("outside the file"), "{err}");
}

#[tokio::test]
async fn upload_rejects_bad_requests_before_sending() {
    let server = MockServer::start().await;
    let (_dir, dir) = temp_dir();
    let file = dir.join("app.apk");
    std::fs::write(&file, b"content").unwrap();
    let client = api_client(&server);
    let url = format!("{}/bucket/object", server.uri());
    let upload = || UploadRequest::new(HttpMethod::Put, url.clone(), &file);

    let cases: Vec<(UploadRequest, &str)> = vec![
        (
            UploadRequest::new(HttpMethod::Get, url.clone(), &file),
            "PUT or POST",
        ),
        (
            upload().with_header("Cookie", "a=b"),
            "`cookie` cannot be set",
        ),
        (
            upload().with_header("client_id", "1"),
            "`client_id` cannot be set",
        ),
        (
            upload().with_header("Proxy-Authorization", "x"),
            "`proxy-authorization` cannot be set",
        ),
        (
            upload().with_header("Transfer-Encoding", "chunked"),
            "cannot be set",
        ),
        (
            upload().with_header("Host", "other.example.com"),
            "does not match the upload URL",
        ),
        (
            upload().with_header("Content-Length", "999"),
            "does not match the size",
        ),
        (
            upload().with_header("x-amz-date", "a\r\nb: c"),
            "invalid value",
        ),
        (upload().with_header("bad name", "x"), "invalid name"),
        // Токен и секрет Connect API не должны уйти на внешний адрес.
        (
            upload().with_header("Authorization", format!("Bearer {}", token())),
            "must not contain the Connect API access token",
        ),
        (
            upload().with_header("x-secret", client_secret()),
            "must not contain the Connect API access token",
        ),
        (
            UploadRequest::new(HttpMethod::Put, url.clone(), "relative.apk"),
            "must be absolute",
        ),
        (
            UploadRequest::new(HttpMethod::Put, url.clone(), dir.join("missing.apk")),
            "could not open the file",
        ),
        (
            UploadRequest::new(HttpMethod::Put, url.clone(), &dir),
            "file",
        ),
    ];
    for (request, expected) in cases {
        let err = client.upload_file(request).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains(expected), "{expected}: {text}");
        assert!(!text.contains(&token()), "{text}");
        assert!(!text.contains(&client_secret()), "{text}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn transfers_go_only_to_allowed_hosts() {
    let server = MockServer::start().await;
    let (_dir, dir) = temp_dir();
    let file = dir.join("app.apk");
    std::fs::write(&file, b"content").unwrap();
    // Рабочая конфигурация: loopback не разрешён, как и любой другой хост вне списка.
    let strict = TransferClient::new(TransferConfig::default()).unwrap();
    let object = "object-id-123456";
    for url in [
        format!("{}/bucket/{object}?X-Amz-Signature=abcdef", server.uri()),
        format!("https://evil.example/{object}?X-Amz-Signature=abcdef"),
        format!("https://files.huawei.com.evil.example/{object}"),
        format!("http://files.huawei.com/{object}"),
        format!("https://user:pw@files.huawei.com/{object}"),
        format!("https://files.huawei.com:8443/{object}"),
        format!("https://10.0.0.1/{object}"),
        format!("file:///etc/{object}"),
    ] {
        let err = strict
            .upload_file(UploadRequest::new(HttpMethod::Put, url.clone(), &file))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::HostNotAllowed(_)), "{url}: {err:?}");
        let text = format!("{err} {err:?}");
        assert!(!text.contains(object) && !text.contains("abcdef"), "{text}");

        let err = strict
            .download_to_file(&url, &dir.join("report.csv"))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::HostNotAllowed(_)), "{url}: {err:?}");
    }
    assert!(!dir.join("report.csv").exists());
    assert!(server.received_requests().await.unwrap().is_empty());

    // Тестовая конфигурация разрешает только loopback, а не любой http-адрес.
    let relaxed = TransferClient::new(transfer_config()).unwrap();
    let err = relaxed
        .download_to_file("http://evil.example/report.csv", &dir.join("report.csv"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::HostNotAllowed(_)), "{err:?}");
}

#[tokio::test]
async fn upload_errors_do_not_leak_the_signature_or_the_url() {
    let server = MockServer::start().await;
    let xml = format!(
        "<?xml version=\"1.0\"?><Error><Code>SignatureDoesNotMatch</Code>\
         <Message>The request signature does not match</Message>\
         <StringToSign>{}</StringToSign><AWSAccessKeyId>AKEXAMPLE</AWSAccessKeyId></Error>",
        signature()
    );
    Mock::given(method("PUT"))
        .and(path("/bucket/object-secret"))
        .respond_with(ResponseTemplate::new(403).set_body_string(xml))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/bucket/plain"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("Authorization: OBS key:signature-value"),
        )
        .mount(&server)
        .await;
    let (_dir, dir) = temp_dir();
    let file = dir.join("app.apk");
    std::fs::write(&file, b"content").unwrap();
    let transfer = TransferClient::new(transfer_config()).unwrap();

    let err = transfer
        .upload_file(
            UploadRequest::new(
                HttpMethod::Put,
                format!(
                    "{}/bucket/object-secret?X-Amz-Signature=abcdef",
                    server.uri()
                ),
                &file,
            )
            .with_header("Authorization", signature()),
        )
        .await
        .unwrap_err();
    let http = err.http().unwrap();
    assert_eq!(http.status, 403);
    assert_eq!(http.code.as_deref(), Some("SignatureDoesNotMatch"));
    assert_eq!(
        http.message.as_deref(),
        Some("The request signature does not match")
    );
    let text = format!("{err} {err:?}");
    for secret in [
        "0f1e2d3c4b5a",
        "AKEXAMPLE",
        "object-secret",
        "abcdef",
        "127.0.0.1",
    ] {
        assert!(!text.contains(secret), "{secret}: {text}");
    }

    let err = transfer
        .upload_file(UploadRequest::new(
            HttpMethod::Put,
            format!("{}/bucket/plain", server.uri()),
            &file,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(500));
    assert!(!err.to_string().contains("signature-value"), "{err}");
}

#[tokio::test]
async fn downloads_a_report_to_a_new_file() {
    let server = MockServer::start().await;
    let csv = "Time,Downloads\n20260101,10\n";
    Mock::given(method("GET"))
        .and(path("/reports/report-1.csv"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(csv, "text/csv"))
        .mount(&server)
        .await;
    let (_dir, dir) = temp_dir();
    let destination = dir.join("report.csv");
    let client = api_client(&server);
    let url = format!("{}/reports/report-1.csv?token=signed-value", server.uri());

    let downloaded = client.download_to_file(&url, &destination).await.unwrap();
    assert_eq!(downloaded.bytes, csv.len() as u64);
    assert_eq!(downloaded.path, destination);
    assert_eq!(downloaded.content_type.as_deref(), Some("text/csv"));
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), csv);

    // Учётные данные Connect API по ссылке не отправляются.
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("authorization").is_none());
    assert!(requests[0].headers.get("client_id").is_none());

    // Существующий файл не перезаписывается.
    let err = client
        .download_to_file(&url, &destination)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), csv);
    // Относительный путь не принимается.
    let err = client
        .download_to_file(&url, std::path::Path::new("report.csv"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must be absolute"), "{err}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn download_enforces_the_size_limit_and_removes_partial_files() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/reports/big.csv"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 5000]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/reports/missing.csv"))
        .respond_with(
            ResponseTemplate::new(404).set_body_string("<Error><Code>NoSuchKey</Code></Error>"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/reports/moved.csv"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "https://evil.example/steal.csv?sig=abc"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/reports/local-redirect.csv"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/reports/small.csv", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/reports/small.csv"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let (_dir, dir) = temp_dir();
    let transfer = TransferClient::new(transfer_config()).unwrap();
    let url = |name: &str| format!("{}/reports/{name}", server.uri());

    // Лимит из настроек (1024 байта).
    let destination = dir.join("big.csv");
    let err = transfer
        .download_to_file(&url("big.csv"), &destination)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ApiError::DownloadTooLarge { limit: 1024 }),
        "{err:?}"
    );
    assert!(!destination.exists());
    // С большим лимитом тот же файл скачивается.
    let downloaded = transfer
        .download_to_file_with_limit(&url("big.csv"), &destination, 10_000)
        .await
        .unwrap();
    assert_eq!(downloaded.bytes, 5000);

    let destination = dir.join("missing.csv");
    let err = transfer
        .download_to_file(&url("missing.csv"), &destination)
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(404));
    assert_eq!(err.http().unwrap().code.as_deref(), Some("NoSuchKey"));
    assert!(!destination.exists());

    // Редирект на хост вне списка не выполняется.
    let destination = dir.join("moved.csv");
    let err = transfer
        .download_to_file(&url("moved.csv"), &destination)
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(302));
    assert!(!err.to_string().contains("evil.example"), "{err}");
    assert!(!destination.exists());

    // Редирект на разрешённый адрес выполняется.
    let destination = dir.join("small.csv");
    transfer
        .download_to_file(&url("local-redirect.csv"), &destination)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), "ok");
}
