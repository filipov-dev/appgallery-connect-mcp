//! Сквозные тесты HTTP-режима: настоящий HTTP-сервер на loopback, AppGallery Connect API и
//! выпуск токенов подменены `wiremock`, запросы MCP отправляются вручную через `reqwest`.
//! Учётные данные API-клиента выдуманы в тесте и читаются из файла секрета с правами 0600,
//! как секрет Docker.

use std::path::{Path, PathBuf};

use appgallery_connect_api::MemoryStore;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::access_tokens::hash_hex;
use super::*;
use crate::testing::{access_token as huawei_token, client_id, client_secret, token_server};

const PROTOCOL_VERSION: &str = "2025-06-18";
const TAG: &str = "http";

/// Токены доступа клиентов: собираются из частей, длина — не меньше минимальной.
fn access_token(name: &str) -> String {
    format!("{name}-{}", "0123456789abcdef".repeat(3))
}

fn tokens_file(dir: &Path) -> PathBuf {
    let text = format!(
        r#"
        [[tokens]]
        name = "reader"
        sha256 = "{reader}"

        [[tokens]]
        name = "writer"
        sha256 = "{writer}"
        access = "write"
        toolsets = ["apps", "listing"]
        "#,
        reader = hash_hex(&access_token("reader")),
        writer = hash_hex(&access_token("writer")),
    );
    let path = dir.join("tokens.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// Файл секрета: только секрет API-клиента, как секрет Docker (идентификатор — флагом).
fn secret_file(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("agc-client-secret");
    std::fs::write(&path, format!("{}\n", client_secret(TAG))).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

#[derive(Debug, clap::Parser)]
struct Wrapper {
    #[command(flatten)]
    serve: ServeArgs,
}

fn try_parse(args: &[&str]) -> Result<ServeArgs, clap::Error> {
    use clap::Parser;
    let mut argv = vec!["serve"];
    argv.extend_from_slice(args);
    Wrapper::try_parse_from(argv).map(|w| w.serve)
}

fn load(args: &[&str]) -> anyhow::Result<HttpSetup> {
    HttpSetup::load(&try_parse(args).unwrap(), &MemoryStore::new(), None, None)
}

/// Средства теста: временный каталог, файлы секрета и токенов.
struct Fixture {
    _dir: tempfile::TempDir,
    tokens: String,
    secret: String,
    client_id: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let tokens = tokens_file(dir.path()).to_str().unwrap().to_owned();
    let secret = secret_file(dir.path()).to_str().unwrap().to_owned();
    Fixture {
        _dir: dir,
        tokens,
        secret,
        client_id: client_id(TAG),
    }
}

struct TestServer {
    base: String,
    http: reqwest::Client,
    cancel: CancellationToken,
    _fixture: Fixture,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Сервер из флагов `serve --http ...` с AppGallery Connect API и выпуском токенов на `api`.
async fn start(api: &MockServer, extra: &[&str]) -> TestServer {
    let fixture = fixture();
    let mut args = vec![
        "--http",
        "--access-tokens",
        fixture.tokens.as_str(),
        "--secret-file",
        fixture.secret.as_str(),
        "--client-id",
        fixture.client_id.as_str(),
        "--region",
        "cn",
        "--allow-write",
    ];
    args.extend_from_slice(extra);
    let setup = load(&args).unwrap();
    serve(api, setup, fixture).await
}

/// Запускает HTTP-сервер из готовых настроек; API — на `api`.
async fn serve(api: &MockServer, mut setup: HttpSetup, fixture: Fixture) -> TestServer {
    setup.config.base_url = Some(api.uri());
    setup.config.retry.max_retries = 0;
    // Бинарника с `jq-worker` у модульного теста нет: фильтр — в потоке.
    setup.options.jq = appgallery_connect_mcp_core::JqRunner::in_process();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut options = setup.http.clone();
    options.allowed_hosts = allowed_hosts(addr, &[]);
    let clients = setup.clients().unwrap();
    let cancel = CancellationToken::new();
    let app = router(clients, &options, cancel.clone());
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
            .await;
    });
    TestServer {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        cancel,
        _fixture: fixture,
    }
}

impl TestServer {
    fn post(&self, body: &Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{MCP_PATH}", self.base))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .body(body.to_string())
    }

    fn authed(&self, name: &str, body: &Value) -> reqwest::RequestBuilder {
        self.post(body)
            .header("authorization", format!("Bearer {}", access_token(name)))
    }

    async fn send(request: reqwest::RequestBuilder) -> (u16, String) {
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn rpc(&self, name: &str, body: &Value) -> Value {
        let (status, body) = Self::send(self.authed(name, body)).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn tools(&self, name: &str) -> Vec<Value> {
        let message = self.rpc(name, &list_tools()).await;
        message["result"]["tools"].as_array().unwrap().clone()
    }
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    })
}

fn list_tools() -> Value {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
}

fn call(name: &str, args: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {"name": name, "arguments": args}
    })
}

fn names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect()
}

fn assert_no_secrets(text: &str) {
    assert!(
        !text.contains(&client_secret(TAG)),
        "client secret in: {text}"
    );
    assert!(!text.contains(&client_id(TAG)), "client ID in: {text}");
    assert!(!text.contains(&huawei_token()), "Huawei token in: {text}");
    for name in ["reader", "writer"] {
        assert!(
            !text.contains(&access_token(name)),
            "access token in: {text}"
        );
    }
}

/// Выпуск токена для учётных данных из файла секрета и `get_app_info`.
async fn mount_api() -> MockServer {
    let api = token_server(&crate::testing::credentials(TAG)).await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(query_param("appId", "100000001"))
        .and(header("client_id", client_id(TAG).as_str()))
        .and(header(
            "authorization",
            format!("Bearer {}", huawei_token()).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "appInfo": {"defaultLang": "en-US", "releaseState": 0}
        })))
        .expect(1)
        .mount(&api)
        .await;
    api
}

#[tokio::test]
async fn health_needs_no_token() {
    let api = MockServer::start().await;
    let server = start(&api, &[]).await;
    let response = server
        .http
        .get(format!("{}{HEALTH_PATH}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "appgallery-connect-mcp");
    assert_eq!(body["mcp_endpoint"], MCP_PATH);
}

#[tokio::test]
async fn missing_malformed_or_foreign_token_is_401() {
    let api = MockServer::start().await;
    let server = start(&api, &[]).await;

    let response = server.post(&initialize()).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(
        response.headers()["www-authenticate"],
        "Bearer realm=\"appgallery-connect-mcp\""
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("Authorization: Bearer <access token>"),
        "{body}"
    );

    let foreign = access_token("stranger");
    let short = "short-token";
    let cases = [
        (format!("Bearer {foreign}"), "invalid_token"),
        (format!("bearer {foreign}"), "invalid_token"),
        (format!("Bearer {short}"), "invalid_request"),
        (
            format!("Basic {}", access_token("reader")),
            "invalid_request",
        ),
        (access_token("reader"), "invalid_request"),
        ("Bearer".to_owned(), "invalid_request"),
        // Хеш из файла токенов вместо самого токена — тоже чужой токен.
        (
            format!("Bearer {}", hash_hex(&access_token("reader"))),
            "invalid_token",
        ),
    ];
    for (value, error) in cases {
        let response = server
            .post(&initialize())
            .header("authorization", value.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401, "{value}");
        let challenge = response.headers()["www-authenticate"].to_str().unwrap();
        assert!(challenge.contains(&format!("error=\"{error}\"")), "{value}");
        let body = response.text().await.unwrap();
        assert!(!body.contains(&foreign) && !body.contains(short), "{body}");
        assert_no_secrets(&body);
    }

    // Два заголовка Authorization — неоднозначно, отклоняется.
    let response = server
        .post(&initialize())
        .header(
            "authorization",
            format!("Bearer {}", access_token("reader")),
        )
        .header(
            "authorization",
            format!("Bearer {}", access_token("writer")),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);

    // До AppGallery Connect API и выпуска токенов ничего не дошло.
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn foreign_host_and_origin_are_403_even_without_token() {
    let api = MockServer::start().await;
    let server = start(&api, &["--allowed-origin", "http://localhost:6274"]).await;
    let (status, body) =
        TestServer::send(server.post(&initialize()).header("host", "evil.example")).await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("--allowed-host"), "{body}");
    let (status, body) = TestServer::send(
        server
            .authed("reader", &initialize())
            .header("origin", "http://evil.example"),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("--allowed-origin"), "{body}");
    let (status, _) = TestServer::send(
        server
            .authed("reader", &initialize())
            .header("origin", "http://localhost:6274"),
    )
    .await;
    assert_eq!(status, 200);
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn reader_calls_the_api_with_the_server_credentials() {
    let api = mount_api().await;
    let server = start(&api, &[]).await;

    let message = server.rpc("reader", &initialize()).await;
    let text = message.to_string();
    let instructions = message["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("runs remotely over HTTP"),
        "{instructions}"
    );
    assert!(instructions.contains("server operator"), "{instructions}");
    assert!(!instructions.contains("auth login"), "{instructions}");
    assert!(
        instructions.contains("has no file directory"),
        "{instructions}"
    );
    assert_no_secrets(&text);
    // Запуск и `initialize` обходятся без сети: токен ещё не запрошен.
    assert!(api.received_requests().await.unwrap_or_default().is_empty());

    let tools = server.tools("reader").await;
    let tool_names = names(&tools);
    assert!(
        tool_names.contains(&"get_app_info".to_owned()),
        "{tool_names:?}"
    );
    assert!(tool_names.contains(&"release_status".to_owned()));
    // Только чтение, без инструментов, которым нужен файл.
    assert!(
        tools
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == json!(true))
    );
    assert!(!tool_names.contains(&"update_app_info".to_owned()));
    assert!(!tool_names.contains(&"listing_export".to_owned()));
    // Динамических наборов в HTTP-режиме нет.
    assert!(!tool_names.contains(&"enable_toolset".to_owned()));

    let result = server
        .rpc(
            "reader",
            &call("get_app_info", json!({"appId": "100000001"})),
        )
        .await;
    let text = result.to_string();
    assert_eq!(result["result"]["isError"], json!(false), "{text}");
    assert!(text.contains("en-US"), "{text}");
    assert_no_secrets(&text);

    // Заголовок Authorization клиента к Huawei не уходит.
    for request in api.received_requests().await.unwrap() {
        let auth = request
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        assert!(!auth.contains(&access_token("reader")), "{auth}");
    }
}

#[tokio::test]
async fn read_token_neither_sees_nor_calls_write_tools() {
    let api = MockServer::start().await;
    let server = start(&api, &[]).await;
    let reader = names(&server.tools("reader").await);
    let writer_tools = server.tools("writer").await;
    let writer = names(&writer_tools);
    assert!(
        writer_tools
            .iter()
            .any(|t| t["annotations"]["readOnlyHint"] == json!(false)),
        "{writer:?}"
    );
    // Наборы токена: apps и listing, без reviews и releases по умолчанию.
    assert!(writer.contains(&"update_app_info".to_owned()), "{writer:?}");
    assert!(reader.contains(&"list_reviews".to_owned()), "{reader:?}");
    assert!(!writer.contains(&"list_reviews".to_owned()), "{writer:?}");
    assert!(
        !reader.contains(&"update_app_info".to_owned()),
        "{reader:?}"
    );

    // Вызов инструмента записи токеном чтения — ошибка инструмента, запрос в API не уходит.
    let message = server
        .rpc(
            "reader",
            &call(
                "update_app_info",
                json!({"appId": "100000001", "body": {"defaultLang": "en-US"}}),
            ),
        )
        .await;
    assert_eq!(message["result"]["isError"], json!(true), "{message}");
    let text = message.to_string();
    assert!(text.contains("server operator"), "{text}");
    assert!(!text.contains("--allow-write"), "{text}");
    // `api_request` токена чтения тоже не пишет.
    let message = server
        .rpc(
            "reader",
            &call(
                "api_request",
                json!({"method": "PUT", "path": "/api/publish/v2/app-info", "body": {}}),
            ),
        )
        .await;
    assert_eq!(message["result"]["isError"], json!(true), "{message}");
    // Удаление и выпуск (destructive) недоступны никому: сервер запущен с --allow-write.
    let message = server
        .rpc("writer", &call("release_submit", json!({})))
        .await
        .to_string();
    assert!(message.contains("destructive"), "{message}");
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn large_request_body_is_413() {
    let api = MockServer::start().await;
    let server = start(&api, &["--max-request-bytes", "1024"]).await;
    let big = call("get_app_info", json!({"jq": "x".repeat(4096)}));
    let (status, _) = TestServer::send(server.authed("reader", &big)).await;
    assert_eq!(status, 413);
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn allowed_path_enables_file_tools_on_the_server() {
    let api = MockServer::start().await;
    let files = tempfile::tempdir().unwrap();
    let dir = files.path().to_str().unwrap();
    let server = start(&api, &["--allow-path", dir]).await;
    let tools = names(&server.tools("writer").await);
    assert!(tools.contains(&"listing_export".to_owned()), "{tools:?}");
    let message = server.rpc("writer", &initialize()).await;
    let instructions = message["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("paths on the server"),
        "{instructions}"
    );
    assert!(!instructions.contains("--allow-path"), "{instructions}");
}

#[test]
fn flags_and_files_are_checked_at_start() {
    let fixture = fixture();
    let (tokens, secret, id) = (
        fixture.tokens.as_str(),
        fixture.secret.as_str(),
        fixture.client_id.as_str(),
    );

    // Флаги HTTP-режима без --http и флаги stdio с --http — ошибки разбора.
    for args in [
        &["--listen", "127.0.0.1:1"][..],
        &["--access-tokens", tokens],
        &["--secret-file", secret],
        &["--region", "ru"],
        &["--http", "--client-id", id],
        &["--http", "--status"],
        &["--http", "--dynamic-toolsets"],
        &["--http", "--region", "mars"],
    ] {
        assert!(try_parse(args).is_err(), "{args:?}");
    }

    let err = load(&["--http"]).err().unwrap().to_string();
    assert!(err.contains("--access-tokens"), "{err}");
    // Токен с записью при сервере только на чтение — ошибка, а не понижение.
    let err = format!(
        "{:#}",
        load(&[
            "--http",
            "--access-tokens",
            tokens,
            "--secret-file",
            secret,
            "--client-id",
            id
        ])
        .err()
        .unwrap()
    );
    assert!(err.contains("token `writer` has access `write`"), "{err}");

    // Токен открытым текстом вместо SHA-256 — ошибка запуска, токен в ней не повторяется.
    let plain = fixture._dir.path().join("plain.toml");
    std::fs::write(
        &plain,
        format!(
            "[[tokens]]\nname = \"plain\"\nsha256 = \"{}\"\n",
            access_token("plain")
        ),
    )
    .unwrap();
    let err = format!(
        "{:#}",
        load(&["--http", "--access-tokens", plain.to_str().unwrap()])
            .err()
            .unwrap()
    );
    assert!(err.contains("64 hex characters"), "{err}");
    assert!(!err.contains(&access_token("plain")), "{err}");

    // Учётные данные из хранилища: без каталога профилей — подсказка про --secret-file.
    let err = format!(
        "{:#}",
        load(&["--http", "--access-tokens", tokens, "--allow-write"])
            .err()
            .unwrap()
    );
    assert!(err.contains("--secret-file"), "{err}");

    // Секрет без идентификатора клиента — подсказка про --client-id.
    let err = format!(
        "{:#}",
        load(&[
            "--http",
            "--access-tokens",
            tokens,
            "--secret-file",
            secret,
            "--allow-write"
        ])
        .err()
        .unwrap()
    );
    assert!(err.contains("--client-id"), "{err}");

    // Секрет, открытый другим пользователям, — ошибка запуска.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(secret, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = format!(
            "{:#}",
            load(&[
                "--http",
                "--access-tokens",
                tokens,
                "--secret-file",
                secret,
                "--client-id",
                id,
                "--allow-write"
            ])
            .err()
            .unwrap()
        );
        assert!(err.contains("accessible by other users"), "{err}");
        assert!(!err.contains(&client_secret(TAG)), "{err}");
        std::fs::set_permissions(secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let setup = load(&[
        "--http",
        "--access-tokens",
        tokens,
        "--secret-file",
        secret,
        "--client-id",
        id,
        "--allow-write",
        "--toolsets",
        "apps",
        "--region",
        "ru",
    ])
    .unwrap();
    assert_eq!(setup.listen.to_string(), DEFAULT_LISTEN);
    assert_eq!(setup.http.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
    assert_eq!(setup.options.max_access, AccessLevel::Write);
    assert_eq!(setup.options.toolsets, Some(vec!["apps".to_owned()]));
    assert!(setup.options.http);
    assert!(!setup.options.local_files());
    assert_eq!(setup.config.region, appgallery_connect_api::Region::Ru);
    assert!(setup.config.credentials.is_some());
    // В логах запуска — только имена токенов, в настройках клиента — ни секрета, ни
    // идентификатора.
    let debug = format!("{:?} {:?}", setup.tokens, setup.config);
    assert!(debug.contains("reader") && !debug.contains(&hash_hex(&access_token("reader"))));
    assert!(!debug.contains(&client_secret(TAG)), "{debug}");
    assert!(!debug.contains(id), "{debug}");
    let clients = setup.clients().unwrap();
    assert_eq!(clients.servers.len(), 2);
    assert_eq!(
        clients.servers[0].options().toolsets,
        Some(vec!["apps".to_owned()])
    );
    assert_eq!(
        clients.servers[1].options().toolsets,
        Some(vec!["apps".to_owned(), "listing".to_owned()])
    );
}

#[test]
fn credentials_and_settings_come_from_the_profile() {
    use crate::profiles::ProfileSettings;

    let fixture = fixture();
    let dir = tempfile::tempdir().unwrap();
    let repo = ProfileRepo::in_dir(dir.path().join("config"));
    repo.upsert(
        "ci",
        ProfileSettings {
            region: appgallery_connect_api::Region::Ru,
            apps: vec!["100000001".into()],
            // Потолок прав сервера с учётными данными из хранилища — права профиля.
            allow_write: true,
            ..ProfileSettings::default()
        },
    )
    .unwrap();
    let store = MemoryStore::new();
    store.set("ci", &crate::testing::credentials(TAG)).unwrap();
    let args = |extra: &[&str]| {
        let mut args = vec![
            "--http",
            "--profile",
            "ci",
            "--allow-write",
            "--access-tokens",
            fixture.tokens.as_str(),
        ];
        args.extend_from_slice(extra);
        try_parse(&args).unwrap()
    };

    // Учётные данные из хранилища профиля, регион и приложения — из его настроек.
    let setup = HttpSetup::load(&args(&[]), &store, Some(&repo), None).unwrap();
    assert_eq!(setup.config.region, appgallery_connect_api::Region::Ru);
    assert_eq!(
        setup.config.credentials,
        Some(crate::testing::credentials(TAG))
    );
    assert_eq!(setup.options.apps, ["100000001"]);
    // Нет учётных данных в хранилище — подсказка про вход.
    let err = format!(
        "{:#}",
        HttpSetup::load(&args(&[]), &MemoryStore::new(), Some(&repo), None)
            .err()
            .unwrap()
    );
    assert!(err.contains("auth login --profile ci"), "{err}");

    // Флаг сверх прав профиля — ошибка запуска: HTTP-режим не расширяет права профиля.
    let err = format!(
        "{:#}",
        HttpSetup::load(&args(&["--allow-destructive"]), &store, Some(&repo), None)
            .err()
            .unwrap()
    );
    assert!(err.contains("profile allows only `write`"), "{err}");
    // На macOS без одобрения — только чтение, токен `write` не пройдёт.
    let approvals = mcp_kit::approval::testing::MemoryStore::default();
    let err = format!(
        "{:#}",
        HttpSetup::load(&args(&[]), &store, Some(&repo), Some(&approvals))
            .err()
            .unwrap()
    );
    assert!(err.contains("server runs with `read-only` access"), "{err}");

    // Файл секрета побеждает хранилище, --region — профиль; JSON несёт и идентификатор.
    use std::os::unix::fs::PermissionsExt;
    let json = dir.path().join("credentials.json");
    std::fs::write(&json, crate::testing::credentials("json").to_stored_json()).unwrap();
    std::fs::set_permissions(&json, std::fs::Permissions::from_mode(0o400)).unwrap();
    let setup = HttpSetup::load(
        &args(&["--secret-file", json.to_str().unwrap(), "--region", "de"]),
        &store,
        Some(&repo),
        None,
    )
    .unwrap();
    assert_eq!(setup.config.region, appgallery_connect_api::Region::De);
    assert_eq!(
        setup.config.credentials,
        Some(crate::testing::credentials("json"))
    );

    // Файл секрета без --region: регион сохранённого профиля.
    let setup = HttpSetup::load(
        &args(&["--secret-file", json.to_str().unwrap()]),
        &store,
        Some(&repo),
        None,
    )
    .unwrap();
    assert_eq!(setup.config.region, appgallery_connect_api::Region::Ru);
}

#[test]
fn secret_file_without_a_profile_needs_the_region() {
    let fixture = fixture();
    let base = [
        "--http",
        "--access-tokens",
        fixture.tokens.as_str(),
        "--secret-file",
        fixture.secret.as_str(),
        "--client-id",
        fixture.client_id.as_str(),
        "--allow-write",
    ];
    // Ни каталога профилей (контейнер), ни профиля в нём: регион не подставляется молча.
    let dir = tempfile::tempdir().unwrap();
    let empty = ProfileRepo::in_dir(dir.path().join("config"));
    for repo in [None, Some(&empty)] {
        let err = format!(
            "{:#}",
            HttpSetup::load(&try_parse(&base).unwrap(), &MemoryStore::new(), repo, None)
                .err()
                .unwrap()
        );
        assert!(err.contains("needs --region cn|de|sg|ru"), "{err}");
        assert!(err.contains("profile `default`"), "{err}");
        assert!(!err.contains(&client_secret(TAG)), "{err}");
    }
    let mut with_region = base.to_vec();
    with_region.extend(["--region", "sg"]);
    let setup = HttpSetup::load(
        &try_parse(&with_region).unwrap(),
        &MemoryStore::new(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(setup.config.region, appgallery_connect_api::Region::Sg);
}

/// Профиль со входом пользователя на хосте сервера: запросы идут от имени пользователя в
/// команде профиля, инструкции говорят об операторе, `--region` не может увести с сайта
/// команды.
#[tokio::test]
async fn user_sign_in_profile_serves_over_http() {
    use crate::profiles::{ProfileKind, ProfileSettings};
    use crate::testing::{mount_user_sign_in, refresh_token, sign_in, user_access_token};
    use appgallery_connect_api::{OAuthEndpoints, Region};

    let fixture = fixture();
    let dir = tempfile::tempdir().unwrap();
    let repo = ProfileRepo::in_dir(dir.path().join("config"));
    repo.upsert(
        "guest",
        ProfileSettings {
            kind: ProfileKind::Oauth,
            team_id: Some("5100".into()),
            region: Region::Ru,
            allow_write: true,
            ..ProfileSettings::default()
        },
    )
    .unwrap();
    let store = MemoryStore::new();
    store.set_oauth("guest", &sign_in(TAG)).unwrap();
    let args = |extra: &[&str]| {
        let mut args = vec![
            "--http",
            "--profile",
            "guest",
            "--allow-write",
            "--access-tokens",
            fixture.tokens.as_str(),
        ];
        args.extend_from_slice(extra);
        try_parse(&args).unwrap()
    };
    let err = format!(
        "{:#}",
        HttpSetup::load(&args(&["--region", "cn"]), &store, Some(&repo), None)
            .err()
            .unwrap()
    );
    assert!(err.contains("site ru"), "{err}");
    // Нет входа в хранилище хоста — подсказка оператору про `auth login --oauth`.
    let err = format!(
        "{:#}",
        HttpSetup::load(&args(&[]), &MemoryStore::new(), Some(&repo), None)
            .err()
            .unwrap()
    );
    assert!(err.contains("auth login --oauth --profile guest"), "{err}");

    let mut setup = HttpSetup::load(&args(&["--region", "ru"]), &store, Some(&repo), None).unwrap();
    assert!(setup.config.credentials.is_none());
    let api = MockServer::start().await;
    mount_user_sign_in(&api, TAG, json!([])).await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(header("oauth2Token", user_access_token().as_str()))
        .and(header("teamId", "5100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "appInfo": {"defaultLang": "en-US"}
        })))
        .expect(1)
        .mount(&api)
        .await;
    let session = setup.config.oauth.as_mut().unwrap();
    assert_eq!(session.team_id.as_deref(), Some("5100"));
    session.endpoints = OAuthEndpoints::huawei()
        .with_token_url(&format!("{}/oauth2/v3/token", api.uri()))
        .unwrap();
    let server = serve(&api, setup, fixture).await;

    let message = server.rpc("reader", &initialize()).await;
    let instructions = message["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("team `5100`"), "{instructions}");
    assert!(instructions.contains("server operator"), "{instructions}");
    assert!(!instructions.contains("auth login"), "{instructions}");

    let result = server
        .rpc(
            "reader",
            &call("get_app_info", json!({"appId": "100000001"})),
        )
        .await;
    let text = result.to_string();
    assert_eq!(result["result"]["isError"], json!(false), "{text}");
    assert!(text.contains("en-US"), "{text}");
    assert!(!text.contains(&user_access_token()), "{text}");
    assert!(!text.contains(&refresh_token(TAG)), "{text}");
    // Comments API от имени пользователя не вызывается; подсказка — про оператора.
    let result = server
        .rpc(
            "reader",
            &call(
                "list_reviews",
                json!({
                    "appId": "100000001",
                    "countries": "RU",
                    "beginTime": 1_700_000_000_000_i64,
                    "endTime": 1_700_100_000_000_i64
                }),
            ),
        )
        .await;
    let text = result.to_string();
    assert_eq!(result["result"]["isError"], json!(true), "{text}");
    assert!(text.contains("server operator"), "{text}");
}
