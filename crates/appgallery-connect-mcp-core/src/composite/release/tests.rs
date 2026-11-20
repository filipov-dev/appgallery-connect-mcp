//! Тесты `release_submit`, `release_manage`, `release_status` и `app_overview` против
//! `wiremock`: тела и параметры запросов изменения, отказы до запроса изменения, `dryRun`,
//! пометки при сбое вспомогательных запросов. Учётные данные выдуманы и собраны из частей.

use std::sync::Arc;
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy,
};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request as WireRequest, ResponseTemplate};

use super::*;
use crate::composite::reviews_summary::{DAY_MS, parse_date};
use crate::composite::{
    MemoryVault, ProfileInfo, app_overview, release_manage, release_status, release_submit,
};
use crate::files::FilePolicy;

const APP: &str = "100000001";
const APP_INFO_PATH: &str = "/api/publish/v2/app-info";
const PACKAGES_PATH: &str = "/api/publish/v2/package-list";
const STATUS_PATH: &str = "/api/publish/v2/package/compile/status";
const SUBMIT_PATH: &str = "/api/publish/v2/app-submit";
const PHASED_PATH: &str = "/api/publish/v2/phased-release";
const RELEASE_TIME_PATH: &str = "/api/publish/v2/on-shelf-time";
const RATINGS_PATH: &str = "/api/reviews/v1/manage/dev/ratings";

/// Ссылка на скачивание пакета из списка пакетов: в ответ попадать не должна.
fn package_link() -> String {
    ["https://", "appdl.example/", "pkg-", "link-0042"].concat()
}

fn context(server: &MockServer, region: Region) -> Context {
    let id = ["3141", "5926", "5358"].concat();
    let secret = ["ZX12", "CV34", "BN56"].concat().repeat(4);
    let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
    let mut config = ClientConfig::new(region, Some(credentials));
    config.base_url = Some(server.uri());
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(10),
    };
    let token = ["release", "-test-", "token"].concat();
    let api = ApiClient::with_token_source(config, Arc::new(StaticToken::new(token))).unwrap();
    Context {
        api: Arc::new(api),
        files: Arc::new(FilePolicy::disabled()),
        vault: Arc::new(MemoryVault::default()),
        profile: ProfileInfo {
            name: "work".into(),
            apps: vec![APP.to_owned()],
            packages: Vec::new(),
        },
        access: AccessLevel::Destructive,
        http: false,
    }
}

fn ok(extra: Value) -> ResponseTemplate {
    let mut body = json!({"ret": {"code": 0, "msg": "success"}});
    if let (Some(target), Value::Object(fields)) = (body.as_object_mut(), extra) {
        target.extend(fields);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

fn app_info(state: i64, released: bool) -> Value {
    let mut info = json!({
        "releaseState": state,
        "defaultLang": "ru-RU",
        "parentType": 13,
        "childType": 26,
        "grandChildType": 10041,
        "isFree": 1,
        "publishCountry": "RU,BY",
        "contentRate": "{\"HW\":\"3+\"}",
        "versionNumber": "1.2.0",
        "versionCode": 12,
        "versionId": "900000012",
        "updateTime": "2026-11-10 07:47:39",
        "priceDetail": "[{\"country\":\"RU\",\"price\":\"0\"}]",
    });
    if released {
        info["onShelfVersionNumber"] = json!("1.1.0");
        info["onShelfVersionCode"] = json!(11);
        info["onShelfVersionId"] = json!("900000011");
    }
    json!({
        "appInfo": info,
        "auditInfo": {"auditOpinion": ""},
        "languages": [
            {"lang": "en-US", "appName": "Demo", "appDesc": "long english description"},
            {"lang": "ru-RU", "appName": "Демо", "appDesc": "длинное описание"},
        ],
    })
}

fn phased_info(state: &str, percent: &str) -> Value {
    json!({
        "appInfo": {"releaseState": 0, "versionNumber": "1.2.0", "versionCode": 12},
        "phasedReleaseInfo": {
            "state": state,
            "phasedReleaseStartTime": "2026-01-01T00:00:00+0000",
            "phasedReleaseEndTime": "2099-01-01T00:00:00+0000",
            "phasedReleasePercent": percent,
            "phasedReleaseDescription": "Phased release",
        },
    })
}

fn packages() -> Value {
    json!({
        "retCount": 2,
        "totalCount": 2,
        "packagePath": package_link(),
        "pkgList": [
            {"pkgVersion": "7001", "fileName": "app-11.apk", "versionCode": 11,
             "versionName": "1.1.0", "packageSize": 1000, "uploadTime": 1_700_000_000_000_i64},
            {"pkgVersion": "7002", "fileName": "app-12.aab", "versionCode": 12,
             "versionName": "1.2.0", "packageSize": 2000, "uploadTime": 1_760_000_000_000_i64},
        ],
    })
}

/// Сведения о приложении без `releaseType` (полный выпуск).
async fn mount_app_info(server: &MockServer, body: Value) {
    Mock::given(method("GET"))
        .and(path(APP_INFO_PATH))
        .and(query_param("appId", APP))
        .and(query_param_is_missing("releaseType"))
        .respond_with(ok(body))
        .mount(server)
        .await;
}

/// Сведения о поэтапном выпуске (`releaseType=3`).
async fn mount_phased(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(APP_INFO_PATH))
        .and(query_param("releaseType", "3"))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn mount_packages(server: &MockServer, body: Value) {
    Mock::given(method("GET"))
        .and(path(PACKAGES_PATH))
        .and(query_param("appId", APP))
        .respond_with(ok(body))
        .mount(server)
        .await;
}

async fn mount_status(server: &MockServer, pkg_id: &str, success_status: i64) {
    Mock::given(method("GET"))
        .and(path(STATUS_PATH))
        .and(query_param("pkgIds", pkg_id))
        .respond_with(ok(json!({
            "pkgStateList": [{"pkgId": pkg_id, "successStatus": success_status}],
        })))
        .mount(server)
        .await;
}

/// Всё, что нужно до отправки: версия, пакеты, пакет `7002` собран.
async fn ready_app(server: &MockServer) {
    mount_app_info(server, app_info(7, true)).await;
    mount_packages(server, packages()).await;
    mount_status(server, "7002", 0).await;
}

async fn mount_change(server: &MockServer, http_method: &str, route: &str) {
    Mock::given(method(http_method))
        .and(path(route))
        .respond_with(ok(json!({})))
        .mount(server)
        .await;
}

async fn requests_to(server: &MockServer, route: &str) -> Vec<WireRequest> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == route)
        .collect()
}

fn query_of(request: &WireRequest) -> Map<String, Value> {
    request
        .url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), json!(value.into_owned())))
        .collect()
}

fn body_of(request: &WireRequest) -> Value {
    if request.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&request.body).unwrap()
    }
}

async fn submit(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    (release_submit::tools()[0].run)(ctx, args).await
}

async fn manage(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    (release_manage::tools()[0].run)(ctx, args).await
}

async fn status(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    (release_status::tools()[0].run)(ctx, args).await
}

async fn overview(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    (app_overview::tools()[0].run)(ctx, args).await
}

#[test]
fn times_and_percents() {
    let day = parse_date("2026-12-01").unwrap() * DAY_MS;
    let ten = day + 10 * 3_600_000;
    for text in [
        "2026-12-01T10:00:00Z",
        "2026-12-01T10:00Z",
        "2026-12-01 10:00:00Z",
        "2026-12-01T13:00:00+03:00",
        "2026-12-01T13:00:00+0300",
        "2026-12-01T13:00:00+03",
        "2026-12-01T05:30:00-04:30",
        "2026-12-01T10:00:00.250Z",
    ] {
        assert_eq!(parse_time(text), Some(ten), "{text}");
    }
    for bad in [
        "2026-12-01T10:00:00",
        "2026-12-01",
        "2026-12-01T25:00:00Z",
        "2026-12-01T10:60:00Z",
        "2026-13-01T10:00:00Z",
        "2026-12-01T10:00:00+3",
        "2026-12-01T10:00:00+15:00",
        "01.12.2026 10:00 UTC",
        "2026-12-01T10:00:00.Z",
    ] {
        assert_eq!(parse_time(bad), None, "{bad}");
    }
    assert_eq!(api_time(ten), "2026-12-01T10:00:00+0000");

    let args = json!({"a": 12.5, "b": 0, "c": 100, "d": 12.345, "e": "10", "f": 99.99});
    assert_eq!(percent_arg(&args, "a").unwrap(), Some(12.5));
    assert_eq!(percent_arg(&args, "f").unwrap(), Some(99.99));
    assert_eq!(percent_arg(&args, "missing").unwrap(), None);
    for name in ["b", "c", "d", "e"] {
        assert!(percent_arg(&args, name).is_err(), "{name}");
    }
    assert_eq!(percent_text(12.5), "12.50");
    assert_eq!(percent_text(5.0), "5.00");
}

#[test]
fn current_package_follows_the_version_code() {
    let list = Packages {
        items: vec![
            Package {
                id: "1".into(),
                file_name: None,
                version_code: Some(12),
                version_name: None,
                size: None,
                uploaded: Some(10),
            },
            Package {
                id: "2".into(),
                file_name: None,
                version_code: Some(12),
                version_name: None,
                size: None,
                uploaded: Some(20),
            },
            Package {
                id: "3".into(),
                file_name: None,
                version_code: Some(13),
                version_name: None,
                size: None,
                uploaded: Some(5),
            },
        ],
        total: Some(3),
    };
    assert_eq!(list.current(Some(12)).unwrap().id, "2");
    assert_eq!(list.current(Some(13)).unwrap().id, "3");
    assert_eq!(list.current(Some(99)).unwrap().id, "2");
    assert_eq!(list.current(None).unwrap().id, "2");
    assert!(Packages::default().current(None).is_none());
}

#[tokio::test]
async fn submit_full_release_sends_no_phased_fields() {
    let server = MockServer::start().await;
    ready_app(&server).await;
    mount_change(&server, "POST", SUBMIT_PATH).await;
    let ctx = context(&server, Region::Ru);

    let result = submit(&ctx, json!({"remark": "Fixes the login screen"}))
        .await
        .unwrap();
    assert_eq!(result["submitted"], json!(true));
    assert_eq!(result["releaseType"], json!("full"));
    assert_eq!(result["package"]["pkgId"], json!("7002"));
    assert_eq!(result["package"]["status"], json!("ready"));
    assert!(!result.to_string().contains(&package_link()));

    let sent = requests_to(&server, SUBMIT_PATH).await;
    assert_eq!(sent.len(), 1);
    let query = query_of(&sent[0]);
    assert_eq!(query.get("appId"), Some(&json!(APP)));
    assert_eq!(query.get("remark"), Some(&json!("Fixes the login screen")));
    assert!(!query.contains_key("releaseType"), "{query:?}");
    assert!(!query.contains_key("releaseTime"), "{query:?}");
    assert_eq!(body_of(&sent[0]), Value::Null);
}

#[tokio::test]
async fn submit_scheduled_release_sends_the_time_in_utc() {
    let server = MockServer::start().await;
    ready_app(&server).await;
    mount_change(&server, "POST", SUBMIT_PATH).await;
    let ctx = context(&server, Region::Ru);

    let result = submit(&ctx, json!({"releaseTime": "2099-01-02T03:04:05+03:00"}))
        .await
        .unwrap();
    assert_eq!(
        result["release"],
        json!("at 2099-01-02T00:04:05Z (after approval)")
    );
    let sent = requests_to(&server, SUBMIT_PATH).await;
    assert_eq!(sent.len(), 1);
    let query = query_of(&sent[0]);
    assert_eq!(
        query.get("releaseTime"),
        Some(&json!("2099-01-02T00:04:05+0000"))
    );
    assert!(!query.contains_key("releaseType"));
}

#[tokio::test]
async fn submit_phased_release_puts_the_phase_into_the_body() {
    let server = MockServer::start().await;
    ready_app(&server).await;
    mount_change(&server, "POST", SUBMIT_PATH).await;
    let ctx = context(&server, Region::Ru);

    let result = submit(
        &ctx,
        json!({
            "releaseType": "phased",
            "phasedPercent": 10,
            "phasedStart": "2099-01-01T00:00:00Z",
            "phasedEnd": "2099-01-08T00:00:00Z",
            "phasedDescription": "First 10 percent",
        }),
    )
    .await
    .unwrap();
    assert_eq!(result["releaseType"], json!("phased"));
    let sent = requests_to(&server, SUBMIT_PATH).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(query_of(&sent[0]).get("releaseType"), Some(&json!("3")));
    assert_eq!(
        body_of(&sent[0]),
        json!({
            "phasedReleaseStartTime": "2099-01-01T00:00:00+0000",
            "phasedReleaseEndTime": "2099-01-08T00:00:00+0000",
            "phasedReleasePercent": "10.00",
            "phasedReleaseDescription": "First 10 percent",
        })
    );
}

#[tokio::test]
async fn submit_refuses_a_package_that_is_not_ready() {
    for (success_status, expected) in [
        (1, "still being processed"),
        (2, "could not process package"),
    ] {
        let server = MockServer::start().await;
        mount_app_info(&server, app_info(7, true)).await;
        mount_packages(&server, packages()).await;
        mount_status(&server, "7002", success_status).await;
        mount_change(&server, "POST", SUBMIT_PATH).await;
        let ctx = context(&server, Region::Ru);
        let err = submit(&ctx, json!({})).await.unwrap_err();
        assert!(err.message().contains(expected), "{err}");
        assert!(err.message().contains("nothing was submitted"), "{err}");
        assert!(requests_to(&server, SUBMIT_PATH).await.is_empty());
    }

    // Пакетов нет.
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(7, true)).await;
    mount_packages(
        &server,
        json!({"retCount": 0, "totalCount": 0, "pkgList": []}),
    )
    .await;
    mount_change(&server, "POST", SUBMIT_PATH).await;
    let ctx = context(&server, Region::Ru);
    let err = submit(&ctx, json!({})).await.unwrap_err();
    assert!(err.message().contains("package_upload"), "{err}");
    assert!(requests_to(&server, SUBMIT_PATH).await.is_empty());

    // Неизвестный pkgId и сбой запроса состояния.
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(7, true)).await;
    mount_packages(&server, packages()).await;
    Mock::given(method("GET"))
        .and(path(STATUS_PATH))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    mount_change(&server, "POST", SUBMIT_PATH).await;
    let ctx = context(&server, Region::Ru);
    let err = submit(&ctx, json!({"pkgId": "9999"})).await.unwrap_err();
    assert!(err.message().contains("`9999` is not among"), "{err}");
    let err = submit(&ctx, json!({"pkgId": "7001"})).await.unwrap_err();
    assert!(err.message().contains("could not confirm"), "{err}");
    assert!(requests_to(&server, SUBMIT_PATH).await.is_empty());
}

#[tokio::test]
async fn submit_refuses_versions_in_review_and_phased_without_a_released_version() {
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(4, true)).await;
    let ctx = context(&server, Region::Ru);
    let err = submit(&ctx, json!({})).await.unwrap_err();
    assert!(err.message().contains("already in review"), "{err}");

    let server = MockServer::start().await;
    mount_app_info(&server, app_info(7, false)).await;
    let ctx = context(&server, Region::Ru);
    let err = submit(
        &ctx,
        json!({
            "releaseType": "phased",
            "phasedPercent": 5,
            "phasedStart": "2099-01-01T00:00:00Z",
            "phasedEnd": "2099-01-02T00:00:00Z",
        }),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("already released"), "{err}");
    assert!(requests_to(&server, SUBMIT_PATH).await.is_empty());
}

#[tokio::test]
async fn submit_checks_arguments_before_any_request() {
    let server = MockServer::start().await;
    let ctx = context(&server, Region::Ru);
    let phased = |extra: Value| {
        let mut args = json!({
            "releaseType": "phased",
            "phasedPercent": 10,
            "phasedStart": "2099-01-01T00:00:00Z",
            "phasedEnd": "2099-01-08T00:00:00Z",
        });
        if let (Some(target), Value::Object(fields)) = (args.as_object_mut(), extra) {
            target.extend(fields);
        }
        args
    };
    for (args, expected) in [
        (
            json!({"releaseTime": "2020-01-01T00:00:00Z"}),
            "not in the future",
        ),
        (json!({"releaseTime": "2099-01-01 10:00"}), "UTC offset"),
        (json!({"releaseTime": "2099-01-01T10:00:00"}), "UTC offset"),
        (
            json!({"phasedPercent": 10}),
            "only for `releaseType: \"phased\"`",
        ),
        (json!({"releaseType": "phased"}), "needs `phasedPercent`"),
        (phased(json!({"phasedPercent": 0})), "greater than 0"),
        (phased(json!({"phasedPercent": 100})), "less than 100"),
        (phased(json!({"phasedPercent": 12.345})), "two"),
        (
            phased(json!({"phasedEnd": "2098-12-31T00:00:00Z"})),
            "later than",
        ),
        (
            phased(json!({"phasedStart": "2001-01-01T00:00:00Z"})),
            "not in the future",
        ),
        (
            phased(json!({"releaseTime": "2099-02-01T00:00:00Z"})),
            "only for a full",
        ),
        (json!({"remark": "short"}), "10-300"),
        (json!({"appId": "com.example.app"}), "numeric app ID"),
    ] {
        let err = submit(&ctx, args.clone()).await.unwrap_err();
        assert!(err.message().contains(expected), "{args}: {err}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn submit_dry_run_sends_no_submission() {
    let server = MockServer::start().await;
    ready_app(&server).await;
    mount_change(&server, "POST", SUBMIT_PATH).await;
    let ctx = context(&server, Region::Ru);

    let result = submit(
        &ctx,
        json!({
            "dryRun": true,
            "releaseType": "phased",
            "phasedPercent": 25.5,
            "phasedStart": "2099-01-01T00:00:00Z",
            "phasedEnd": "2099-01-08T00:00:00Z",
        }),
    )
    .await
    .unwrap();
    assert_eq!(result["dryRun"], json!(true));
    assert!(result.get("submitted").is_none());
    assert_eq!(result["request"]["tool"], json!("submit_app"));
    assert_eq!(result["request"]["query"]["releaseType"], json!(3));
    assert_eq!(
        result["request"]["body"]["phasedReleasePercent"],
        json!("25.50")
    );
    assert!(requests_to(&server, SUBMIT_PATH).await.is_empty());
}

#[tokio::test]
async fn manage_phased_actions_send_the_expected_bodies() {
    type Case = (Value, &'static str, Value, Option<&'static str>);
    let cases: Vec<Case> = vec![
        (
            json!({"action": "update_phased", "phasedPercent": 50, "phasedEnd": "2099-02-01T00:00:00Z"}),
            "RELEASE",
            json!({"phasedReleaseEndTime": "2099-02-01T00:00:00+0000", "phasedReleasePercent": "50.00"}),
            None,
        ),
        (
            json!({"action": "update_phased", "phasedStart": "2099-01-01T00:00:00Z"}),
            "DRAFT",
            json!({"phasedReleaseStartTime": "2099-01-01T00:00:00+0000"}),
            None,
        ),
        (
            json!({"action": "suspend"}),
            "RELEASE",
            json!({"state": "SUSPEND"}),
            None,
        ),
        (
            json!({"action": "resume"}),
            "SUSPEND",
            json!({"state": "RELEASE"}),
            None,
        ),
        (
            json!({"action": "complete"}),
            "RELEASE",
            json!({}),
            Some("1"),
        ),
    ];
    for (args, state, body, release_type) in cases {
        let server = MockServer::start().await;
        mount_phased(&server, ok(phased_info(state, "20.00"))).await;
        mount_change(&server, "PUT", PHASED_PATH).await;
        let ctx = context(&server, Region::Ru);
        let result = manage(&ctx, args.clone()).await.unwrap();
        assert_eq!(result["changed"], json!(true), "{args}");
        assert_eq!(result["before"]["phasedRelease"]["state"], json!(state));
        let sent = requests_to(&server, PHASED_PATH).await;
        assert_eq!(sent.len(), 1, "{args}");
        assert_eq!(body_of(&sent[0]), body, "{args}");
        let query = query_of(&sent[0]);
        assert_eq!(query.get("appId"), Some(&json!(APP)));
        assert_eq!(
            query.get("releaseType").and_then(Value::as_str),
            release_type,
            "{args}"
        );
    }
}

#[tokio::test]
async fn manage_refuses_transitions_the_api_does_not_allow() {
    let cases = [
        (
            json!({"action": "update_phased", "phasedPercent": 10}),
            "RELEASE",
            "can only grow",
        ),
        (
            json!({"action": "update_phased", "phasedPercent": 20}),
            "RELEASE",
            "can only grow",
        ),
        (
            json!({"action": "update_phased", "phasedStart": "2099-01-01T00:00:00Z"}),
            "RELEASE",
            "cannot change after it has started",
        ),
        (json!({"action": "resume"}), "RELEASE", "only a suspended"),
        (json!({"action": "suspend"}), "SUSPEND", "only a running"),
        (
            json!({"action": "suspend"}),
            "CANCEL",
            "no active phased release",
        ),
        (
            json!({"action": "complete"}),
            "CANCEL",
            "no active phased release",
        ),
    ];
    for (args, state, expected) in cases {
        let server = MockServer::start().await;
        mount_phased(&server, ok(phased_info(state, "20.00"))).await;
        mount_change(&server, "PUT", PHASED_PATH).await;
        let ctx = context(&server, Region::Ru);
        let err = manage(&ctx, args.clone()).await.unwrap_err();
        assert!(err.message().contains(expected), "{args}: {err}");
        assert!(
            err.message().contains("nothing was changed"),
            "{args}: {err}"
        );
        assert!(requests_to(&server, PHASED_PATH).await.is_empty(), "{args}");
    }

    // Нет сведений о поэтапном выпуске и сбой их чтения.
    let server = MockServer::start().await;
    mount_phased(&server, ok(json!({"appInfo": {"releaseState": 0}}))).await;
    let ctx = context(&server, Region::Ru);
    let err = manage(&ctx, json!({"action": "suspend"}))
        .await
        .unwrap_err();
    assert!(err.message().contains("no active phased release"), "{err}");

    let server = MockServer::start().await;
    mount_phased(
        &server,
        ResponseTemplate::new(200)
            .set_body_json(json!({"ret": {"code": 204144647, "msg": "no phased release"}})),
    )
    .await;
    let ctx = context(&server, Region::Ru);
    let err = manage(&ctx, json!({"action": "suspend"}))
        .await
        .unwrap_err();
    assert!(err.message().contains("nothing was changed"), "{err}");
    assert!(err.message().contains("204144647"), "{err}");
}

#[tokio::test]
async fn manage_checks_arguments_before_any_request() {
    let server = MockServer::start().await;
    let ctx = context(&server, Region::Ru);
    for (args, expected) in [
        (json!({"action": "cancel"}), "must be one of"),
        (json!({"action": "update_phased"}), "at least one"),
        (
            json!({"action": "update_phased", "phasedPercent": 100}),
            "less than 100",
        ),
        (
            json!({"action": "update_phased", "phasedEnd": "2001-01-01T00:00:00Z"}),
            "not in the future",
        ),
        (
            json!({"action": "suspend", "phasedPercent": 30}),
            "takes no",
        ),
        (json!({"action": "set_release_time"}), "needs `releaseTime`"),
        (
            json!({"action": "set_release_time", "releaseTime": "2099-01-01"}),
            "UTC offset",
        ),
        (
            json!({"action": "update_phased", "releaseTime": "2099-01-01T00:00:00Z"}),
            "set_release_time",
        ),
    ] {
        let err = manage(&ctx, args.clone()).await.unwrap_err();
        assert!(err.message().contains(expected), "{args}: {err}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn manage_release_time_actions() {
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(3, true)).await;
    mount_change(&server, "PUT", RELEASE_TIME_PATH).await;
    let ctx = context(&server, Region::Ru);

    let result = manage(
        &ctx,
        json!({"action": "set_release_time", "releaseTime": "2099-03-01T12:00:00+02:00"}),
    )
    .await
    .unwrap();
    assert_eq!(result["changed"], json!(true));
    let result = manage(&ctx, json!({"action": "release_now"}))
        .await
        .unwrap();
    assert_eq!(result["action"], json!("release_now"));

    let sent = requests_to(&server, RELEASE_TIME_PATH).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(
        body_of(&sent[0]),
        json!({"changeType": 3, "releaseTime": "2099-03-01T10:00:00+0000", "releaseType": 1})
    );
    let now_body = body_of(&sent[1]);
    assert_eq!(now_body["changeType"], json!(2));
    assert_eq!(now_body["releaseType"], json!(1));
    assert!(
        now_body["releaseTime"]
            .as_str()
            .is_some_and(|time| time.ends_with("+0000")),
        "{now_body}"
    );

    // Версия не ждёт выпуска — отказ без запроса изменения.
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(7, true)).await;
    mount_change(&server, "PUT", RELEASE_TIME_PATH).await;
    let ctx = context(&server, Region::Ru);
    let err = manage(&ctx, json!({"action": "release_now"}))
        .await
        .unwrap_err();
    assert!(err.message().contains("state 7 (draft)"), "{err}");
    assert!(requests_to(&server, RELEASE_TIME_PATH).await.is_empty());
}

#[tokio::test]
async fn manage_dry_run_changes_nothing() {
    let server = MockServer::start().await;
    mount_phased(&server, ok(phased_info("RELEASE", "20.00"))).await;
    mount_app_info(&server, app_info(3, true)).await;
    mount_change(&server, "PUT", PHASED_PATH).await;
    mount_change(&server, "PUT", RELEASE_TIME_PATH).await;
    let ctx = context(&server, Region::Ru);

    let result = manage(
        &ctx,
        json!({"action": "update_phased", "phasedPercent": 40, "dryRun": true}),
    )
    .await
    .unwrap();
    assert_eq!(result["dryRun"], json!(true));
    assert_eq!(result["request"]["tool"], json!("update_phased_release"));
    assert_eq!(
        result["request"]["body"],
        json!({"phasedReleasePercent": "40.00"})
    );
    let result = manage(&ctx, json!({"action": "release_now", "dryRun": true}))
        .await
        .unwrap();
    assert_eq!(result["request"]["tool"], json!("update_release_time"));
    assert!(requests_to(&server, PHASED_PATH).await.is_empty());
    assert!(requests_to(&server, RELEASE_TIME_PATH).await.is_empty());
}

#[tokio::test]
async fn status_combines_version_phased_release_and_packages() {
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(0, true)).await;
    mount_phased(&server, ok(phased_info("SUSPEND", "30.00"))).await;
    mount_packages(&server, packages()).await;
    mount_status(&server, "7002", 0).await;
    let ctx = context(&server, Region::Ru);

    let result = status(&ctx, json!({})).await.unwrap();
    assert_eq!(
        result["version"]["state"],
        json!({"code": 0, "name": "released"})
    );
    assert_eq!(result["released"]["versionNumber"], json!("1.1.0"));
    assert_eq!(result["phasedRelease"]["state"], json!("SUSPEND"));
    assert_eq!(result["phasedRelease"]["percent"], json!(30.0));
    assert_eq!(result["packages"]["total"], json!(2));
    assert_eq!(result["packages"]["newest"][0]["pkgId"], json!("7002"));
    assert_eq!(result["currentPackage"]["status"], json!("ready"));
    assert_eq!(result["readyToSubmit"], json!(true));
    assert!(result.get("notes").is_none(), "{result}");
    let text = result.to_string();
    assert!(!text.contains(&package_link()), "{text}");
    assert!(!text.contains("long english description"), "{text}");
}

#[tokio::test]
async fn status_notes_failed_auxiliary_requests() {
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(3, true)).await;
    mount_phased(
        &server,
        ResponseTemplate::new(200)
            .set_body_json(json!({"ret": {"code": 204144647, "msg": "no phased release"}})),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(PACKAGES_PATH))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let ctx = context(&server, Region::Ru);

    let result = status(&ctx, json!({})).await.unwrap();
    assert_eq!(result["waitsForScheduledRelease"], json!(true));
    assert_eq!(result["phasedRelease"], Value::Null);
    let notes = result["notes"].to_string();
    assert!(
        notes.contains("phased release state could not be read"),
        "{notes}"
    );
    assert!(notes.contains("packages could not be read"), "{notes}");
    assert!(notes.contains("scheduled release"), "{notes}");

    // Основное чтение не удалось — ошибка.
    let server = MockServer::start().await;
    let ctx = context(&server, Region::Ru);
    let err = status(&ctx, json!({})).await.unwrap_err();
    assert!(err.message().contains("get_app_info"), "{err}");
}

#[tokio::test]
async fn overview_is_compact_and_has_the_rating() {
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(4, true)).await;
    Mock::given(method("GET"))
        .and(path(RATINGS_PATH))
        .and(query_param("countries", "RU"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": 0, "rtnDesc": "success"},
            "data": {"score": [{"country": "RU", "averageScore": 4.5, "totalRatings": 10}]},
        })))
        .mount(&server)
        .await;
    let ctx = context(&server, Region::Ru);

    let result = overview(&ctx, json!({})).await.unwrap();
    assert_eq!(result["name"], json!("Демо"));
    assert_eq!(result["defaultLanguage"], json!("ru-RU"));
    assert_eq!(result["languages"], json!(["en-US", "ru-RU"]));
    assert_eq!(result["free"], json!(true));
    assert_eq!(result["releaseCountryCount"], json!(2));
    assert_eq!(result["contentRating"], json!({"HW": "3+"}));
    assert_eq!(result["version"]["state"]["name"], json!("in_review"));
    assert_eq!(result["released"]["versionCode"], json!(11));
    assert_eq!(result["rating"]["average"], json!(4.5));
    assert_eq!(result["rating"]["total"], json!(10));
    assert!(result.get("notes").is_none(), "{result}");
    let text = result.to_string();
    assert!(!text.contains("description"), "{text}");
    assert!(!text.contains("priceDetail"), "{text}");
}

#[tokio::test]
async fn overview_notes_a_missing_rating() {
    // Сбой статистики оценок — пометка, а не ошибка.
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(0, true)).await;
    Mock::given(method("GET"))
        .and(path(RATINGS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": 204144647, "rtnDesc": "no permission"},
        })))
        .mount(&server)
        .await;
    let ctx = context(&server, Region::Ru);
    let result = overview(&ctx, json!({"countries": ["by"]})).await.unwrap();
    assert_eq!(result["rating"], Value::Null);
    let notes = result["notes"].to_string();
    assert!(notes.contains("rating is not available"), "{notes}");
    assert!(notes.contains("204144647"), "{notes}");
    let sent = requests_to(&server, RATINGS_PATH).await;
    assert_eq!(query_of(&sent[0]).get("countries"), Some(&json!("BY")));

    // Не Россия и несколько стран выпуска: страны не угадываются, запроса нет.
    let server = MockServer::start().await;
    mount_app_info(&server, app_info(0, true)).await;
    let ctx = context(&server, Region::De);
    let result = overview(&ctx, json!({})).await.unwrap();
    assert_eq!(result["rating"], Value::Null);
    assert!(result["notes"].to_string().contains("needs `countries`"));
    assert!(requests_to(&server, RATINGS_PATH).await.is_empty());
}
