//! Набор `purchases` (HMS IAP Order и Subscription services) через сервер: набор выключен
//! по умолчанию, уровни инструментов, запрос каждой операции на хост своего сервиса с
//! заголовком `Basic APPAT`, разбор JSON-строк с данными покупок и удаление подписи,
//! ошибки с HTTP 200 (`responseCode`), отсутствие секрета приложения. HTTP — `wiremock`,
//! секреты и токены выдуманы и собраны из частей.

use std::sync::Arc;
use std::time::Duration;

use appgallery_connect_api::token::StaticToken;
use appgallery_connect_api::{
    AccessLevel, ApiClient, ApiClientCredentials, AppCredentials, ClientConfig, Region, RetryPolicy,
};
use appgallery_connect_mcp_core::{AppGalleryServer, ServerOptions, Services};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP_ID: &str = "100000001";
const OAUTH_PATH: &str = "/oauth2/v3/token";

const READ_TOOLS: &[&str] = &[
    "get_subscription_purchase",
    "list_cancelled_purchases",
    "list_iap_orders",
    "verify_product_purchase",
];
const DESTRUCTIVE_TOOLS: &[&str] = &[
    "cancel_subscription",
    "confirm_product_purchase",
    "defer_subscription",
    "refund_subscription",
    "revoke_subscription",
];

fn app_secret() -> String {
    ["5d2c", "a91f", "e0b7", "3c44"].concat().repeat(4)
}

fn app_token() -> String {
    ["purchases", "-app-", "tok", "en-", "Xq81zW"].concat()
}

fn basic() -> String {
    let token = appgallery_connect_api::token::AccessToken::new(app_token());
    appgallery_connect_api::app::app_authorization(&token).to_string()
}

/// Выдуманный токен покупки в формате примеров справочника.
fn purchase_token() -> String {
    ["00000173", "741056a3", "7eef310d"].concat() + ".5.3089"
}

fn client(server: &MockServer, with_app: bool) -> ApiClient {
    let credentials = ApiClientCredentials::from_parts(
        ["3141", "5926", "5358"].concat(),
        ["AB90", "CD78", "EF56"].concat().repeat(3),
    )
    .unwrap();
    let mut config = ClientConfig::new(Region::Ru, Some(credentials));
    config.base_url = Some(server.uri());
    config.profile = Some("shop".into());
    config.retry = RetryPolicy {
        max_retries: 0,
        default_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(10),
    };
    if with_app {
        config.apps = vec![AppCredentials::new(APP_ID, None, app_secret()).unwrap()];
    }
    let team = Arc::new(StaticToken::new(["team", "-token-", "0042"].concat()));
    ApiClient::with_token_source(config, team).unwrap()
}

fn options(access: AccessLevel) -> ServerOptions {
    ServerOptions {
        max_access: access,
        toolsets: Some(vec!["purchases".to_owned()]),
        profile: Some("shop".to_owned()),
        ..ServerOptions::default()
    }
}

fn server(mock: &MockServer, access: AccessLevel, with_app: bool) -> AppGalleryServer {
    AppGalleryServer::new(Services::new(client(mock, with_app)), options(access)).unwrap()
}

async fn mount_oauth(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(OAUTH_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": app_token(), "expires_in": 3600, "token_type": "Bearer"}),
        ))
        .mount(server)
        .await;
}

async fn call(server: &AppGalleryServer, tool: &str, args: Value) -> CallToolResult {
    let Value::Object(args) = args else {
        panic!("не объект");
    };
    server.call(tool, args).await.unwrap()
}

fn has(server: &AppGalleryServer, tool: &str) -> bool {
    server.registry().get(tool).is_some()
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

/// Ответ операции подписки с данными покупки строкой и подписью.
fn signed_purchase(field: &str, data: &Value) -> Value {
    let signature = ["Zm9v", "YmFy", "c2ln"].concat();
    let mut value = json!({
        "responseCode": "0",
        "dataSignature": signature,
        "signatureAlgorithm": "SHA256WithRSA/PSS",
    });
    value[field] = Value::String(data.to_string());
    value
}

#[tokio::test]
async fn toolset_is_off_by_default_and_levels_hold() {
    let mock = MockServer::start().await;
    let default =
        AppGalleryServer::new(Services::new(client(&mock, true)), ServerOptions::default())
            .unwrap();
    assert!(!has(&default, "verify_product_purchase"));
    assert!(!default.instructions().contains("toolset `purchases`"));

    let read = server(&mock, AccessLevel::Read, true);
    for tool in READ_TOOLS {
        assert!(has(&read, tool), "{tool}");
    }
    for tool in DESTRUCTIVE_TOOLS {
        assert!(!has(&read, tool), "{tool}");
    }
    let instructions = read.instructions();
    assert!(
        instructions.contains("App secrets are stored for app IDs `100000001`"),
        "{instructions}"
    );

    let all = server(&mock, AccessLevel::Destructive, true);
    for tool in READ_TOOLS.iter().chain(DESTRUCTIVE_TOOLS) {
        assert!(has(&all, tool), "{tool}");
    }
}

#[tokio::test]
async fn every_operation_reaches_its_service_and_comes_back_readable() {
    let mock = MockServer::start().await;
    mount_oauth(&mock).await;
    let token = purchase_token();
    let next = ["eyJwYWdl", "U2l6ZSI6", "MX0="].concat();
    let product = json!({
        "productId": "gems_10",
        "purchaseState": 0,
        "purchaseToken": token,
        "developerPayload": "order-77",
        "price": 100,
    });
    let subscription = json!({
        "productId": "pro_month",
        "subscriptionId": "1581789719266.D40972AC.3089",
        "purchaseToken": token,
        "subIsvalid": true,
        "expirationDate": 1_597_677_948_003_u64,
    });
    let cancelled = json!([{
        "orderId": "1581789719266.E359BC66.3089",
        "productId": "gems_10",
        "purchaseToken": token,
        "cancelledReason": 1,
    }]);
    let subscription_body =
        json!({"subscriptionId": "1581789719266.D40972AC.3089", "purchaseToken": token});
    let product_body = json!({"purchaseToken": token, "productId": "gems_10"});
    let cases: Vec<(&str, &str, Value, Value)> = vec![
        (
            "verify_product_purchase",
            "/applications/purchases/tokens/verify",
            product_body.clone(),
            signed_purchase("purchaseTokenData", &product),
        ),
        (
            "confirm_product_purchase",
            "/applications/v2/purchases/confirm",
            product_body,
            json!({"responseCode": "0", "responseMessage": "consume success"}),
        ),
        (
            "list_cancelled_purchases",
            "/applications/v2/purchases/cancelledList",
            json!({"type": 1, "maxRows": 10}),
            json!({"responseCode": "0", "cancelledPurchaseList": cancelled.to_string(), "continuationToken": next}),
        ),
        (
            "list_iap_orders",
            "/applications/v1/merchantQuery",
            json!({"startAt": 1_664_200_000_000_u64, "endAt": 1_664_286_400_000_u64}),
            json!({"responseCode": "0", "orderInfoList": [{"orderNo": "V2022", "tradeType": "PURCHASE"}], "continuationToken": next}),
        ),
        (
            "get_subscription_purchase",
            "/sub/applications/v2/purchases/get",
            subscription_body.clone(),
            signed_purchase("inappPurchaseData", &subscription),
        ),
        (
            "cancel_subscription",
            "/sub/applications/v2/purchases/stop",
            subscription_body.clone(),
            json!({"responseCode": "0"}),
        ),
        (
            "defer_subscription",
            "/sub/applications/v2/purchases/delay",
            json!({
                "subscriptionId": "1581789719266.D40972AC.3089",
                "purchaseToken": token,
                "currentExpirationTime": 1_597_677_948_003_u64,
                "desiredExpirationTime": 1_598_282_748_003_u64,
            }),
            json!({"responseCode": "0", "newExpirationTime": "1598282748003"}),
        ),
        (
            "refund_subscription",
            "/sub/applications/v2/purchases/returnFee",
            subscription_body.clone(),
            json!({"responseCode": "0", "responseMessage": "already refunded, success"}),
        ),
        (
            "revoke_subscription",
            "/sub/applications/v2/purchases/withdrawal",
            subscription_body,
            json!({"responseCode": "0"}),
        ),
    ];
    for (_, route, body, response) in &cases {
        Mock::given(method("POST"))
            .and(path(*route))
            .and(header("authorization", basic().as_str()))
            .and(body_json(body.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(response.clone()))
            .expect(1)
            .mount(&mock)
            .await;
    }
    let server = server(&mock, AccessLevel::Destructive, true);
    let mut results = std::collections::BTreeMap::new();
    for (tool, _, body, _) in &cases {
        let result = call(&server, tool, json!({"appId": APP_ID, "body": body})).await;
        assert_ne!(result.is_error, Some(true), "{tool}: {}", text(&result));
        let raw = text(&result);
        assert!(
            !raw.contains(&app_secret()) && !raw.contains(&app_token()),
            "{tool}: {raw}"
        );
        results.insert(*tool, json_of(&result));
    }

    // Данные покупки разобраны, подпись убрана с пометкой, токен покупки не скрыт.
    let verified = &results["verify_product_purchase"];
    assert_eq!(
        verified["result"]["purchaseTokenData"], product,
        "{verified}"
    );
    assert!(verified["result"].get("dataSignature").is_none());
    assert!(verified["result"].get("signatureAlgorithm").is_none());
    let note = verified["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("Removed `dataSignature`, `signatureAlgorithm`"),
        "{note}"
    );
    let checked = &results["get_subscription_purchase"];
    assert_eq!(
        checked["result"]["inappPurchaseData"], subscription,
        "{checked}"
    );
    assert!(checked["result"].get("dataSignature").is_none());

    // Строка-массив разобрана, `continuationToken` (base64 с `eyJ`) не принят за JWT.
    let list = &results["list_cancelled_purchases"];
    assert_eq!(list["cancelledPurchaseList"], cancelled, "{list}");
    assert_eq!(list["continuationToken"], next.as_str());
    assert!(list.get("note").is_none(), "{list}");
    assert_eq!(
        results["list_iap_orders"]["orderInfoList"][0]["orderNo"],
        "V2022"
    );
    assert_eq!(
        results["list_iap_orders"]["continuationToken"],
        next.as_str()
    );
    assert_eq!(
        results["defer_subscription"]["newExpirationTime"],
        "1598282748003"
    );
    assert_eq!(
        results["confirm_product_purchase"],
        json!({"responseCode": "0", "responseMessage": "consume success"})
    );
}

#[tokio::test]
async fn result_codes_with_http_200_are_tool_errors() {
    let mock = MockServer::start().await;
    mount_oauth(&mock).await;
    Mock::given(method("POST"))
        .and(path("/applications/purchases/tokens/verify"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"responseCode": "12", "responseMessage": "order not exist"})),
        )
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/sub/applications/v2/purchases/returnFee"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"responseCode": "6", "responseMessage": "critical error"})),
        )
        .expect(1)
        .mount(&mock)
        .await;
    let server = server(&mock, AccessLevel::Destructive, true);
    let token = purchase_token();
    let result = call(
        &server,
        "verify_product_purchase",
        json!({"appId": APP_ID, "body": {"purchaseToken": token, "productId": "gems_10"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("12") && message.contains("order not exist"),
        "{message}"
    );
    assert!(message.contains("HTTP 200"), "{message}");

    let result = call(
        &server,
        "refund_subscription",
        json!({"appId": APP_ID, "body": {"subscriptionId": "s1", "purchaseToken": token}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("critical error"),
        "{}",
        text(&result)
    );
}

#[tokio::test]
async fn without_an_app_secret_the_tool_explains_how_to_add_one() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": "0"})))
        .expect(0)
        .mount(&mock)
        .await;
    let server = server(&mock, AccessLevel::Read, false);
    let instructions = server.instructions();
    assert!(
        instructions.contains("No app secret is stored in this profile yet")
            && instructions.contains("auth app-secret set --profile shop"),
        "{instructions}"
    );
    let result = call(
        &server,
        "list_iap_orders",
        json!({"appId": APP_ID, "body": {"startAt": 1, "endAt": 2}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("auth app-secret set --profile shop --app 100000001"),
        "{message}"
    );
}

#[tokio::test]
async fn app_id_and_body_are_checked_before_the_request() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"responseCode": "0"})))
        .expect(0)
        .mount(&mock)
        .await;
    let server = server(&mock, AccessLevel::Read, true);
    for args in [
        json!({"body": {"startAt": 1, "endAt": 2}}),
        json!({"appId": "com.example.app", "body": {"startAt": 1, "endAt": 2}}),
        json!({"appId": APP_ID}),
    ] {
        let result = call(&server, "list_iap_orders", args.clone()).await;
        assert_eq!(result.is_error, Some(true), "{args}");
    }
}
