//! Тесты `products_export` и `products_apply` против `wiremock`: выдуманное приложение с
//! группой подписок, тремя товарами и двумя акциями (одна закончилась).

use appgallery_connect_api::AccessLevel;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::composite::listing::testing::context;

const BASE: &str = "/api/pms/product-price-service/v1/manage/product";

fn at(suffix: &str) -> String {
    format!("{BASE}{suffix}")
}

/// Ответ PMS API с нулевым кодом и полями `extra`.
fn ok(extra: Value) -> ResponseTemplate {
    let mut body = json!({"error": {"errorCode": 0, "errorMsg": null}});
    if let (Value::Object(body), Value::Object(extra)) = (&mut body, extra) {
        body.extend(extra);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

fn coins() -> Value {
    json!({
        "id": "p-1",
        "productNo": "coins_100",
        "productName": "100 coins",
        "productDesc": "A pack of coins",
        "purchaseType": "consumable",
        "status": "active",
        "currency": "CNY",
        "country": "CN",
        "price": "600",
        "defaultLocale": "en_US",
        "languages": [
            {"locale": "ru_RU", "productName": "100 монет", "productDesc": "Набор монет"},
            {"locale": "en_US", "productName": "100 coins", "productDesc": "A pack of coins"},
        ],
        "prices": [
            {"price": 9900, "currencyCode": "RUB", "country": "RU", "countryEnName": "Russia"},
            {"price": 600, "currencyCode": "CNY", "country": "CN"},
        ],
    })
}

fn premium() -> Value {
    json!({
        "id": "p-2",
        "productNo": "premium_month",
        "productName": "Premium",
        "productDesc": "Monthly subscription",
        "purchaseType": "auto_subscription",
        "status": "active",
        "currency": "CNY",
        "country": "CN",
        "price": "1200",
        "defaultLocale": "en_US",
        "groupId": "g1",
        "groupName": "Premium",
        "periodUnit": "M",
        "numberOfUnits": 1,
        "languages": [
            {"locale": "en_US", "productName": "Premium", "productDesc": "Monthly subscription"},
        ],
        "prices": [{"price": 1200, "currencyCode": "CNY", "country": "CN"}],
    })
}

fn old_item() -> Value {
    json!({
        "id": "p-3",
        "productNo": "old_item",
        "productName": "Old item",
        "productDesc": "",
        "purchaseType": "non_consumable",
        "status": "inactive",
        "currency": "CNY",
        "country": "CN",
        "price": "100",
        "defaultLocale": "en_US",
        "languages": [],
        "prices": [],
    })
}

fn promo() -> Value {
    json!({
        "promotionId": "promo1",
        "productId": "p-2",
        "startDate": 1_767_225_600_000_i64,
        "endDate": 1_769_904_000_000_i64,
        "status": 0,
        "promotionStatus": "active",
        "strategy": "introductory",
        "promotionTitle": "First month",
        "defaultLocale": "en_US",
        "promotionDesc": "Half price",
        "periodUnit": "M",
        "numberOfUnits": 1,
        "defaultPrice": "600",
        "country": "CN",
        "currency": "CNY",
        "prices": [
            {"price": 600, "currencyCode": "CNY", "country": "CN"},
            {"price": 4900, "currencyCode": "RUB", "country": "RU"},
        ],
        "languages": [{"locale": "ru_RU", "title": "Первый месяц", "desc": "Половина цены"}],
    })
}

/// Монтирует чтения: группы, товары, акции.
async fn mount_reads(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(at("/group/list")))
        .respond_with(ok(json!({
            "totalNumber": 1,
            "simpleGroups": [{"groupId": "g1", "groupName": "Premium", "totalProductNumber": 1}],
        })))
        .mount(server)
        .await;
    let summaries: Vec<Value> = [coins(), premium(), old_item()]
        .iter()
        .map(|p| {
            json!({
                "id": p["id"], "productNo": p["productNo"], "status": p["status"],
                "purchaseType": p["purchaseType"], "price": p["price"],
            })
        })
        .collect();
    Mock::given(method("POST"))
        .and(path(at("/list")))
        .respond_with(ok(json!({"totalNumber": 3, "products": summaries})))
        .mount(server)
        .await;
    for product in [coins(), premium(), old_item()] {
        Mock::given(method("GET"))
            .and(path(BASE))
            .and(query_param(
                "productNo",
                product["productNo"].as_str().unwrap(),
            ))
            .respond_with(ok(json!({"product": product})))
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(at("/promotion/list")))
        .respond_with(ok(json!({
            "totalNumber": 2,
            "productPromotions": [
                {"promotionId": "promo1", "productId": "p-2", "status": 0, "promotionStatus": "active"},
                {"promotionId": "promo-old", "productId": "p-2", "status": 2, "promotionStatus": "active"},
            ],
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(at("/promotion")))
        .and(query_param("promotionId", "promo1"))
        .respond_with(ok(json!({"promotion": promo()})))
        .mount(server)
        .await;
}

/// Монтирует записи: все успешны.
async fn mount_writes(server: &MockServer) {
    for (verb, suffix) in [
        ("PUT", "/group"),
        ("PUT", "/batchImportProducts"),
        ("PUT", "/batchActive"),
        ("PUT", "/batchDeactive"),
        ("POST", "/promotion"),
        ("PUT", "/promotion"),
        ("PUT", "/promotion/batchActive"),
        ("PUT", "/promotion/batchDeactive"),
    ] {
        Mock::given(method(verb))
            .and(path(at(suffix)))
            .respond_with(ok(json!({"failedNumber": 0, "successNumber": 1})))
            .mount(server)
            .await;
    }
}

/// Изменяющие запросы: `METHOD path` (чтения — `GET` и списки `POST .../list`).
async fn writes(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() != "GET" && !r.url.path().ends_with("/list"))
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

fn bodies(requests: &[Request], verb: &str, suffix: &str) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == at(suffix))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

async fn export(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    (crate::composite::products_export::tools()[0].run)(ctx, args).await
}

async fn apply(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    (crate::composite::products_apply::tools()[0].run)(ctx, args).await
}

async fn exported(server: &MockServer, dir: &std::path::Path) -> Value {
    let ctx = context(server, dir, AccessLevel::Read);
    export(&ctx, json!({"save_to": "products.json"}))
        .await
        .unwrap();
    serde_json::from_str(&std::fs::read_to_string(dir.join("products.json")).unwrap()).unwrap()
}

fn save(dir: &std::path::Path, name: &str, file: &Value) {
    std::fs::write(dir.join(name), serde_json::to_string_pretty(file).unwrap()).unwrap();
}

fn product<'a>(file: &'a mut Value, no: &str) -> &'a mut Value {
    file["products"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["productNo"] == no)
        .unwrap()
}

#[tokio::test]
async fn export_writes_the_file_and_apply_finds_no_changes() {
    let server = MockServer::start().await;
    mount_reads(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&server, dir.path(), AccessLevel::Read);
    let result = export(&ctx, json!({"save_to": "products.json"}))
        .await
        .unwrap();
    assert_eq!(result["products"], 3, "{result}");
    assert_eq!(result["subscriptions"], 1);
    assert_eq!(result["promotions"], 1);
    assert_eq!(result["subscriptionGroups"], 1);
    assert_eq!(result["priceCountries"], 2);
    // В ответе нет названий и цен.
    assert!(!result.to_string().contains("100 coins"), "{result}");

    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("products.json")).unwrap())
            .unwrap();
    assert_eq!(file["format"], FORMAT);
    assert_eq!(file["version"], VERSION);
    assert_eq!(file["appId"], "100000001");
    let nos: Vec<&str> = file["products"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["productNo"].as_str().unwrap())
        .collect();
    assert_eq!(nos, ["coins_100", "old_item", "premium_month"]);
    assert_eq!(
        file["products"][0],
        json!({
            "productNo": "coins_100",
            "purchaseType": "consumable",
            "status": "active",
            "defaultLocale": "en_US",
            "productName": "100 coins",
            "productDesc": "A pack of coins",
            "languages": [
                {"locale": "en_US", "productName": "100 coins", "productDesc": "A pack of coins"},
                {"locale": "ru_RU", "productName": "100 монет", "productDesc": "Набор монет"},
            ],
            "defaultPrice": {"country": "CN", "currency": "CNY", "price": "600"},
            "prices": [
                {"country": "CN", "currency": "CNY", "price": "600"},
                {"country": "RU", "currency": "RUB", "price": "9900"},
            ],
        })
    );
    let premium = &file["products"][2];
    assert_eq!(premium["groupId"], "g1");
    assert_eq!(premium["subPeriod"], 1);
    assert_eq!(premium["subPeriodUnit"], "M");
    // Закончившаяся акция не выгружается.
    assert_eq!(file["promotions"].as_array().unwrap().len(), 1);
    assert_eq!(file["promotions"][0]["promotionId"], "promo1");
    assert_eq!(file["promotions"][0]["productNo"], "premium_month");
    assert_eq!(file["promotions"][0]["state"], "ongoing");
    assert_eq!(
        file["promotions"][0]["prices"],
        json!([{"country": "CN", "price": "600"}, {"country": "RU", "price": "4900"}])
    );

    // Повторное применение без правок ничего не меняет — на любом уровне.
    for access in [AccessLevel::Write, AccessLevel::Destructive] {
        let ctx = context(&server, dir.path(), access);
        for dry_run in [true, false] {
            let result = apply(&ctx, json!({"file": "products.json", "dry_run": dry_run}))
                .await
                .unwrap();
            assert_eq!(result["changes"], json!([]), "{result}");
            assert_eq!(
                result["unchanged"],
                json!({"subscriptionGroups": 1, "products": 3, "promotions": 1})
            );
            assert!(result.get("not_in_file").is_none(), "{result}");
        }
    }
    assert!(writes(&server).await.is_empty());

    // Файл уже есть: не перезаписывается, запросов нет.
    let fresh = MockServer::start().await;
    let ctx = context(&fresh, dir.path(), AccessLevel::Read);
    let err = export(&ctx, json!({"save_to": "products.json"}))
        .await
        .unwrap_err();
    assert!(err.message().contains("exists"), "{err}");
    assert!(fresh.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn plans_and_applies_the_differences() {
    let server = MockServer::start().await;
    mount_reads(&server).await;
    mount_writes(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let mut file = exported(&server, dir.path()).await;
    file["subscriptionGroups"][0]["groupName"] = json!("Premium plus");
    {
        let coins = product(&mut file, "coins_100");
        coins["languages"][1]["productName"] = json!("Сто монет");
        coins["prices"] = json!([{"country": "RU", "currency": "RUB", "price": 8900}]);
    }
    product(&mut file, "old_item")["status"] = json!("active");
    file["promotions"][0]["promotionTitle"] = json!("First month off");
    save(dir.path(), "edited.json", &file);
    let ctx = context(&server, dir.path(), AccessLevel::Destructive);

    // По умолчанию — только план.
    let plan = apply(&ctx, json!({"file": "edited.json"})).await.unwrap();
    assert_eq!(plan["dry_run"], true);
    assert_eq!(
        plan["changes"],
        json!([
            {"action": "update_group", "groupId": "g1", "level": "write", "changes": [
                {"field": "groupName", "old": "Premium", "new": "Premium plus"}
            ]},
            {"action": "update_product", "productNo": "coins_100", "level": "destructive", "changes": [
                {"field": "languages", "locale": "ru_RU",
                 "old": {"productName": "100 монет", "productDesc": "Набор монет"},
                 "new": {"productName": "Сто монет", "productDesc": "Набор монет"}},
                {"field": "prices", "country": "RU", "old": "9900", "new": "8900"},
            ]},
            {"action": "activate_product", "productNo": "old_item", "level": "destructive", "changes": [
                {"field": "status", "old": "inactive", "new": "active"}
            ]},
            {"action": "update_promotion", "promotionId": "promo1", "level": "destructive", "changes": [
                {"field": "promotionTitle", "old": "First month", "new": "First month off"}
            ]},
        ]),
        "{plan}"
    );
    // Цена CN есть в AppGallery Connect, но не в записи файла: остаётся как есть.
    assert!(
        plan["notes"]
            .to_string()
            .contains("product coins_100: price for CN"),
        "{plan}"
    );
    assert_eq!(plan["unchanged"]["products"], 1);
    assert!(writes(&server).await.is_empty());

    let result = apply(&ctx, json!({"file": "edited.json", "dry_run": false}))
        .await
        .unwrap();
    assert_eq!(
        result["applied"],
        json!([
            "updated subscription group g1",
            "updated product coins_100",
            "activated product old_item",
            "updated promotion promo1",
        ]),
        "{result}"
    );
    assert_eq!(
        writes(&server).await,
        [
            format!("PUT {}", at("/group")),
            format!("PUT {}", at("/batchImportProducts")),
            format!("PUT {}", at("/batchActive")),
            format!("PUT {}", at("/promotion")),
        ]
    );
    let requests = server.received_requests().await.unwrap();
    let group = &bodies(&requests, "PUT", "/group")[0];
    assert_eq!(
        group["resource"],
        json!({"groupId": "g1", "groupName": "Premium plus"})
    );
    assert!(group["requestId"].as_str().unwrap().starts_with("mcp-"));
    let import = &bodies(&requests, "PUT", "/batchImportProducts")[0];
    assert_eq!(
        import["products"],
        json!([{
            "productNo": "coins_100",
            "status": "active",
            "productName": "100 coins",
            "productDesc": "A pack of coins",
            "purchaseType": "consumable",
            "defaultLocale": "en_US",
            "defaultPriceInfo": {"country": "CN", "currency": "CNY", "price": "600"},
            "languages": [
                {"locale": "en_US", "productName": "100 coins", "productDesc": "A pack of coins"},
                {"locale": "ru_RU", "productName": "Сто монет", "productDesc": "Набор монет"},
            ],
            // Только изменённые цены.
            "prices": [{"country": "RU", "price": "8900"}],
        }])
    );
    assert_eq!(
        bodies(&requests, "PUT", "/batchActive")[0]["products"],
        json!(["old_item"])
    );
    let promotion = &bodies(&requests, "PUT", "/promotion")[0]["resource"];
    assert_eq!(promotion["promotionId"], "promo1");
    assert_eq!(promotion["promotionTitle"], "First month off");
    assert_eq!(promotion["status"], "active");
    // Цены акции уходят целиком: без них акция не действует.
    assert_eq!(
        promotion["prices"],
        json!([{"country": "CN", "price": "600"}, {"country": "RU", "price": "4900"}])
    );
    assert_eq!(promotion["defaultPrice"], "600");
}

#[tokio::test]
async fn refuses_destructive_changes_on_a_write_server() {
    let server = MockServer::start().await;
    mount_reads(&server).await;
    mount_writes(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let mut file = exported(&server, dir.path()).await;
    {
        let coins = product(&mut file, "coins_100");
        coins["productDesc"] = json!("A bag of coins");
        coins["defaultPrice"]["price"] = json!("700");
    }
    save(dir.path(), "edited.json", &file);
    let ctx = context(&server, dir.path(), AccessLevel::Write);

    // План строится и на уровне записи, с пометкой.
    let plan = apply(&ctx, json!({"file": "edited.json"})).await.unwrap();
    assert_eq!(plan["changes"][0]["level"], "destructive", "{plan}");
    assert!(
        plan["notes"].to_string().contains("destructive access"),
        "{plan}"
    );
    // Применение — отказ до записи, с перечнем правок.
    let err = apply(&ctx, json!({"file": "edited.json", "dry_run": false}))
        .await
        .unwrap_err();
    let message = err.message();
    assert!(message.contains("destructive access"), "{message}");
    assert!(
        message.contains("update product coins_100 (prices)"),
        "{message}"
    );
    assert!(writes(&server).await.is_empty());

    // Только тексты — уровень записи: применяется.
    product(&mut file, "coins_100")["defaultPrice"]["price"] = json!("0600");
    save(dir.path(), "texts.json", &file);
    let result = apply(&ctx, json!({"file": "texts.json", "dry_run": false}))
        .await
        .unwrap();
    assert_eq!(result["changes"][0]["level"], "write", "{result}");
    assert_eq!(result["applied"], json!(["updated product coins_100"]));
    let requests = server.received_requests().await.unwrap();
    let item = &bodies(&requests, "PUT", "/batchImportProducts")[0]["products"][0];
    assert_eq!(item["productDesc"], "A bag of coins");
    assert!(item.get("prices").is_none(), "{item}");
    assert_eq!(item["defaultPriceInfo"]["price"], "600");
}

#[tokio::test]
async fn checks_the_file_before_any_request() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&server, dir.path(), AccessLevel::Destructive);
    let file = json!({
        "format": FORMAT,
        "version": 1,
        "products": [{
            "productNo": "_bad",
            "purchaseType": "consumable",
            "status": "active",
            "defaultLocale": "en_US",
            "productName": "x".repeat(56),
            "productDesc": "a|b",
            "defaultPrice": {"country": "China", "currency": "cny", "price": "6.00"},
            "prices": [
                {"country": "RU", "price": "0"},
                {"country": "RU", "currency": "RUB", "price": 100},
            ],
        }, {
            "productNo": "sub",
            "purchaseType": "auto_subscription",
            "status": "inactive",
            "defaultLocale": "en_US",
            "productName": "Sub",
        }],
        "promotions": [{
            "productNo": "sub",
            "strategy": "introductory",
            "promotionStatus": "inactive",
            "promotionTitle": "Intro",
            "defaultLocale": "en_US",
            "startDate": 10,
            "endDate": 5,
            "prices": [{"country": "RU"}],
        }],
    });
    save(dir.path(), "bad.json", &file);
    let err = apply(&ctx, json!({"file": "bad.json"})).await.unwrap_err();
    let message = err.message();
    for expected in [
        "product _bad: productNo must start",
        "productName: 56 characters",
        "productDesc: the vertical bar",
        "defaultPrice.country `China`",
        "defaultPrice.currency `cny`",
        "defaultPrice.price `6.00`",
        "the price for RU `0` is not a positive amount",
        "the price for RU is listed twice",
        "product sub: a subscription needs groupId",
        "subPeriodUnit",
        "a new promotion is always created active",
        "endDate must be later than startDate",
        "an introductory price needs defaultPrice",
        "the introductory price for RU has no price",
    ] {
        assert!(message.contains(expected), "{expected}\n{message}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());

    // Неизвестное поле и чужой формат — тоже до запросов.
    save(
        dir.path(),
        "typo.json",
        &json!({"format": FORMAT, "version": 1, "products": [{"productNo": "a", "purchaseType": "consumable", "status": "active", "defaultLocale": "en_US", "productName": "A", "price": "1"}]}),
    );
    let err = apply(&ctx, json!({"file": "typo.json"})).await.unwrap_err();
    assert!(err.message().contains("unknown field `price`"), "{err}");
    save(
        dir.path(),
        "other.json",
        &json!({"format": "other", "version": 1}),
    );
    let err = apply(&ctx, json!({"file": "other.json"}))
        .await
        .unwrap_err();
    assert!(err.message().contains("appgallery-products"), "{err}");
    // Файл другого приложения.
    save(
        dir.path(),
        "app.json",
        &json!({"format": FORMAT, "version": 1, "appId": "100000002"}),
    );
    let err = apply(&ctx, json!({"file": "app.json", "appId": "100000001"}))
        .await
        .unwrap_err();
    assert!(err.message().contains("belongs to app 100000002"), "{err}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn creates_products_and_promotions_and_checks_them_against_the_app() {
    let server = MockServer::start().await;
    mount_reads(&server).await;
    mount_writes(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let mut file = exported(&server, dir.path()).await;
    file["products"].as_array_mut().unwrap().push(json!({
        "productNo": "gems_10",
        "purchaseType": "consumable",
        "status": "inactive",
        "defaultLocale": "en_US",
        "productName": "10 gems",
        "defaultPrice": {"country": "CN", "currency": "CNY", "price": 100},
        "prices": [{"country": "RU", "price": 1900}],
    }));
    let new_promotion = json!({
        "productNo": "premium_month",
        "strategy": "trial",
        "promotionTitle": "Free week",
        "defaultLocale": "en_US",
        "startDate": 1_767_225_600_000_i64,
        "periodUnit": "W",
        "numberOfUnits": 1,
        "prices": [{"country": "RU"}],
    });
    file["promotions"]
        .as_array_mut()
        .unwrap()
        .push(new_promotion.clone());
    save(dir.path(), "new.json", &file);
    let ctx = context(&server, dir.path(), AccessLevel::Destructive);
    let result = apply(&ctx, json!({"file": "new.json", "dry_run": false}))
        .await
        .unwrap();
    assert_eq!(
        result["applied"],
        json!([
            "created product gems_10",
            "created new promotion \"Free week\" of premium_month",
        ]),
        "{result}"
    );
    let requests = server.received_requests().await.unwrap();
    let item = &bodies(&requests, "PUT", "/batchImportProducts")[0]["products"][0];
    assert_eq!(item["status"], "inactive");
    assert_eq!(item["prices"], json!([{"country": "RU", "price": "1900"}]));
    assert_eq!(
        item["defaultPriceInfo"],
        json!({"country": "CN", "currency": "CNY", "price": "100"})
    );
    let created = &bodies(&requests, "POST", "/promotion")[0]["resource"];
    assert_eq!(created["status"], "active");
    assert_eq!(created["strategy"], "trial");
    assert_eq!(created["prices"], json!([{"country": "RU"}]));

    // Проверки по товару и неизменяемые поля — после чтения и до записи.
    let mut bad = file.clone();
    bad["promotions"][1]["numberOfUnits"] = json!(5);
    bad["promotions"][1]["periodUnit"] = json!("D");
    let mut intro = new_promotion.clone();
    intro["strategy"] = json!("introductory");
    intro["periodUnit"] = json!("M");
    intro["numberOfUnits"] = json!(13);
    intro["defaultPrice"] = json!({"country": "CN", "currency": "CNY", "price": "300"});
    intro["prices"] = json!([{"country": "RU", "price": "4900"}]);
    let mut on_consumable = intro.clone();
    on_consumable["productNo"] = json!("coins_100");
    bad["promotions"]
        .as_array_mut()
        .unwrap()
        .extend([intro, on_consumable]);
    product(&mut bad, "premium_month")["subPeriod"] = json!(3);
    product(&mut bad, "coins_100")["prices"][1]["currency"] = json!("USD");
    save(dir.path(), "bad.json", &bad);
    let before = writes(&server).await.len();
    let err = apply(&ctx, json!({"file": "bad.json", "dry_run": false}))
        .await
        .unwrap_err();
    let message = err.message();
    for expected in [
        "a free trial lasts 3 days",
        "must be a multiple of the subscription period",
        "promotions are only for auto_subscription products, coins_100 is consumable",
        "product premium_month: subPeriod cannot be changed",
        "the currency for RU is RUB in AppGallery Connect",
    ] {
        assert!(message.contains(expected), "{expected}\n{message}");
    }
    assert_eq!(writes(&server).await.len(), before);
}

#[tokio::test]
async fn failure_lists_what_was_applied() {
    let server = MockServer::start().await;
    mount_reads(&server).await;
    Mock::given(method("PUT"))
        .and(path(at("/batchImportProducts")))
        .respond_with(ok(json!({"failedNumber": 0, "successNumber": 1})))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(at("/batchActive")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"error": {"errorCode": 204144645, "errorMsg": "product state error"}}),
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut file = exported(&server, dir.path()).await;
    product(&mut file, "coins_100")["productName"] = json!("Coins");
    product(&mut file, "old_item")["status"] = json!("active");
    save(dir.path(), "edited.json", &file);
    let ctx = context(&server, dir.path(), AccessLevel::Destructive);
    let err = apply(&ctx, json!({"file": "edited.json", "dry_run": false}))
        .await
        .unwrap_err();
    let message = err.message();
    assert!(message.contains("pms.product.batchActivate"), "{message}");
    assert!(message.contains("product state error"), "{message}");
    assert!(
        message.contains("Already applied: updated product coins_100."),
        "{message}"
    );
    // Дальше сбоя запросы не идут.
    assert!(
        !writes(&server)
            .await
            .iter()
            .any(|w| w.contains("promotion"))
    );
}

#[tokio::test]
async fn batch_rejections_are_failures() {
    let server = MockServer::start().await;
    mount_reads(&server).await;
    Mock::given(method("PUT"))
        .and(path(at("/batchImportProducts")))
        .respond_with(ok(json!({
            "failedNumber": 1,
            "successNumber": 1,
            "resultInfo": [{"productNo": "old_item", "errorCode": 204144650, "errorReason": "invalid name"}],
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut file = exported(&server, dir.path()).await;
    product(&mut file, "coins_100")["productName"] = json!("Coins");
    product(&mut file, "old_item")["productName"] = json!("Old");
    save(dir.path(), "edited.json", &file);
    let ctx = context(&server, dir.path(), AccessLevel::Write);
    let err = apply(&ctx, json!({"file": "edited.json", "dry_run": false}))
        .await
        .unwrap_err();
    let message = err.message();
    assert!(message.contains("old_item: invalid name"), "{message}");
    assert!(
        message.contains("Already applied: updated product coins_100."),
        "{message}"
    );
}

#[tokio::test]
async fn export_reports_a_failure_in_the_middle() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(at("/group/list")))
        .respond_with(ok(json!({"totalNumber": 0, "simpleGroups": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(at("/list")))
        .respond_with(ok(json!({"totalNumber": 2, "products": [
            {"id": "p-1", "productNo": "coins_100", "status": "active"},
            {"id": "p-3", "productNo": "old_item", "status": "inactive"},
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(BASE))
        .and(query_param("productNo", "coins_100"))
        .respond_with(ok(json!({"product": coins()})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(BASE))
        .and(query_param("productNo", "old_item"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&server, dir.path(), AccessLevel::Read);
    let err = export(&ctx, json!({"save_to": "products.json"}))
        .await
        .unwrap_err();
    assert!(
        err.message()
            .contains("reading product old_item failed after 1 of 2 products"),
        "{err}"
    );
    // Неполный файл не пишется.
    assert!(!dir.path().join("products.json").exists());
}

#[test]
fn registered_by_access_level() {
    let registry = |max_access| {
        crate::registry::ToolRegistry::new(&crate::ServerOptions {
            max_access,
            toolsets: Some(vec!["products".into()]),
            ..crate::ServerOptions::default()
        })
        .unwrap()
    };
    let read = registry(AccessLevel::Read);
    assert!(read.get("products_export").is_some());
    assert!(read.get("products_apply").is_none());
    let write = registry(AccessLevel::Write);
    let tool = &write.get("products_apply").unwrap().tool;
    let annotations = tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    let export = &write.get("products_export").unwrap().tool;
    assert_eq!(
        export.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    assert!(
        crate::registry::ToolRegistry::new(&crate::ServerOptions {
            max_access: AccessLevel::Write,
            toolsets: Some(vec!["listing".into()]),
            ..crate::ServerOptions::default()
        })
        .unwrap()
        .get("products_apply")
        .is_none()
    );
}

#[test]
fn amounts_and_codes() {
    assert_eq!(Amount("0600".into()).normalized(), "600");
    assert_eq!(Amount("000".into()).normalized(), "0");
    assert!(Amount("199".into()).is_positive());
    assert!(!Amount("0".into()).is_positive());
    assert!(!Amount("1.99".into()).is_positive());
    assert!(!Amount("-5".into()).is_positive());
    assert!(is_country("RU") && !is_country("ru") && !is_country("RUS"));
    assert!(is_currency("RUB") && !is_currency("RU"));
    assert!(is_product_no("coins.100_a") && !is_product_no(".coins") && !is_product_no("a-b"));
}
