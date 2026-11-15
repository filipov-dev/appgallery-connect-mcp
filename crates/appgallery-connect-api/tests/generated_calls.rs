//! Исполнитель со сгенерированными операциями: путь, query (и массивы повтором имени),
//! параметры-заголовки, тело и конверт результата каждой группы API. Сеть — `wiremock`,
//! учётные данные выдуманы и собраны из частей.

use std::sync::Arc;
use std::time::Duration;

use appgallery_connect_api::executor::{ApiClient, ApiError, ClientConfig, RetryPolicy};
use appgallery_connect_api::spec::{OperationDef, find};
use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{ApiClientCredentials, Region};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn client_id() -> String {
    ["2077", "4410", "9821", "3306"].concat()
}

fn token() -> String {
    ["generated", "-calls-", "token-", "0123456789"].concat()
}

fn client(server: &MockServer) -> ApiClient {
    let secret = ["C0FF", "EE12", "3456", "789A", "BCDE", "F012"]
        .concat()
        .repeat(2);
    let credentials = ApiClientCredentials::from_parts(client_id(), secret).unwrap();
    let mut config = ClientConfig::new(Region::Ru, Some(credentials));
    config.base_url = Some(server.uri());
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(10),
    };
    ApiClient::with_token_source(config, Arc::new(StaticToken::new(token()))).unwrap()
}

fn op(name: &str) -> &'static OperationDef {
    find(name).unwrap_or_else(|| panic!("нет операции {name}"))
}

fn bearer() -> String {
    format!("Bearer {}", token())
}

/// Пары query последнего запроса в порядке отправки.
async fn last_query(server: &MockServer) -> Vec<(String, String)> {
    let requests: Vec<Request> = server.received_requests().await.unwrap_or_default();
    let request = requests.last().expect("запрос отправлен");
    request
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn json_of(response: &appgallery_connect_api::ApiResponse) -> &Value {
    response.body.json().expect("JSON-ответ")
}

#[tokio::test]
async fn publishing_get_app_info_with_ret_code_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .and(query_param("appId", "100000001"))
        .and(query_param("lang", "en-US"))
        .and(header("client_id", client_id().as_str()))
        .and(header("authorization", bearer().as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "appInfo": {"releaseState": 0},
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    let response = client
        .call(
            op("get_app_info"),
            &json!({"appId": "100000001", "lang": "en-US"}),
        )
        .await
        .unwrap();
    assert_eq!(json_of(&response)["appInfo"]["releaseState"], 0);
    assert!(response.result.is_some());

    // HTTP 200 с ненулевым `ret.code` — ошибка с кодом Huawei.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v2/app-info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 204144660, "msg": "Invalid appId."},
        })))
        .mount(&server)
        .await;
    let error = client
        .call(op("get_app_info"), &json!({"appId": "100000001"}))
        .await
        .unwrap_err();
    assert_eq!(error.result_code(), Some("204144660"));
    let ApiError::Result(result) = &error else {
        panic!("ожидалась ошибка результата: {error:?}");
    };
    assert_eq!(result.operation.as_deref(), Some("get_app_info"));
    assert_eq!(result.message.as_deref(), Some("Invalid appId."));
}

#[tokio::test]
async fn reports_path_param_and_repeated_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/api/report/distribution-operation-quality/v1/appDownloadExport/100000001",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "fileURL": "https://example.invalid/report.csv",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    client
        .call(
            op("get_download_report_url"),
            &json!({
                "appId": "100000001",
                "language": "en-US",
                "startTime": "20260101",
                "endTime": "20260131",
                "filterCondition": ["countryId", "appVersion"],
                "filterConditionValue": ["RU", "1.0.0"],
            }),
        )
        .await
        .unwrap();
    let query = last_query(&server).await;
    let values = |name: &str| -> Vec<&str> {
        query
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    };
    assert_eq!(values("filterCondition"), ["countryId", "appVersion"]);
    assert_eq!(values("filterConditionValue"), ["RU", "1.0.0"]);
    assert_eq!(values("startTime"), ["20260101"]);
    assert!(values("appId").is_empty(), "appId — параметр пути");
}

#[tokio::test]
async fn comments_reply_with_header_body_and_rtn_code_envelope() {
    let server = MockServer::start().await;
    let body = json!({
        "appId": "100000001",
        "reviewId": "review-1",
        "lang": "en_US",
        "countryCode": "RU",
        "devReplyContent": "Thank you!",
    });
    Mock::given(method("POST"))
        .and(path("/api/reviews/v1/manage/dev/reviews"))
        .and(header("requestId", "req-0001"))
        .and(header("content-type", "application/json"))
        .and(body_json(body.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": 0, "rtnDesc": "success"},
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    client
        .call(
            op("reply_review"),
            &json!({"requestId": "req-0001", "body": body}),
        )
        .await
        .unwrap();

    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/reviews/v1/manage/dev/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"rtnCode": "1000", "rtnDesc": "Review not found."},
        })))
        .mount(&server)
        .await;
    let error = client
        .call(
            op("reply_review"),
            &json!({"requestId": "req-0002", "body": {"reviewId": "x"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.result_code(), Some("1000"));

    // Обязательный заголовок не передан — запрос не отправляется.
    let error = client
        .call(op("reply_review"), &json!({"body": {}}))
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::InvalidRequest(_)), "{error:?}");
}

#[tokio::test]
async fn pms_header_app_id_body_and_error_envelope() {
    let server = MockServer::start().await;
    let body = json!({
        "requestId": "req-0003",
        "resource": {"productNo": "coins_100", "appId": "100000001"},
    });
    Mock::given(method("PUT"))
        .and(path("/api/pms/product-price-service/v1/manage/product"))
        .and(header("appId", "100000001"))
        .and(body_json(body.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": {"errorCode": 0, "errorMsg": null},
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    client
        .call(
            op("update_product"),
            &json!({"appId": "100000001", "body": body}),
        )
        .await
        .unwrap();
    assert!(
        last_query(&server).await.is_empty(),
        "appId — заголовок, не query"
    );

    server.reset().await;
    Mock::given(method("POST"))
        .and(path(
            "/api/pms/product-price-service/v1/manage/product/list",
        ))
        .and(header("appId", "100000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": {"errorCode": 1, "errorMsg": "Parameter error."},
        })))
        .mount(&server)
        .await;
    let error = client
        .call(
            op("search_products"),
            &json!({"appId": "100000001", "body": {"requestId": "r", "country": "RU"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.result_code(), Some("1"));
}

#[tokio::test]
async fn project_integer_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/cds/app-distirbution/v1/all/app-brief-info/list"))
        .and(header("userID", "123456789"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "list": [],
        })))
        .expect(1)
        .mount(&server)
        .await;
    client(&server)
        .call(op("list_app_brief_info"), &json!({"userID": 123_456_789}))
        .await
        .unwrap();
}

#[tokio::test]
async fn provisioning_headers_on_get_and_unknown_arguments() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/publish/v3/provision/list"))
        .and(header("appId", "100000001"))
        .and(header("provisionId", "prov-1"))
        .and(query_param("maxReqCount", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ret": {"code": 0, "msg": "success"},
            "provisionList": [],
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    client
        .call(
            op("list_profiles"),
            &json!({"appId": "100000001", "provisionId": "prov-1", "maxReqCount": 10}),
        )
        .await
        .unwrap();

    let error = client
        .call(
            op("list_profiles"),
            &json!({"appId": "100000001", "Authorization": "Bearer x"}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ApiError::InvalidRequest(text) if text.contains("unknown argument")),
        "{error:?}"
    );
}
