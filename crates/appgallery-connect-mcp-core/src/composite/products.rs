//! Общее для инструментов товаров (`products_export`, `products_apply`): формат файла,
//! текущее состояние в AppGallery Connect (PMS API), разбор и проверка файла.
//!
//! Файл — JSON (`format: "appgallery-products"`, `version: 1`) с тремя списками:
//! `subscriptionGroups` (группы подписок по `groupId`), `products` (товары по `productNo`:
//! названия и описания, языки, цена по умолчанию, цены по странам, состояние, для подписок —
//! группа и период) и `promotions` (акции по `promotionId`; без него — новая акция). Поля
//! названы как в PMS API. Запись, которой нет в файле, не трогается; список языков или цен,
//! которого нет у записи, тоже. Описание формата для пользователя — `docs/tools.md`.
//!
//! Цены — в минимальных единицах валюты («в центах», как в справочнике: 1,99 USD — `199`),
//! строкой или целым числом. Правил пересчёта цен в снимке нет (страница «Price Conversion
//! Rules» справочника в снимок не входит), поэтому цена проверяется только по форме: код
//! страны из двух заглавных букв, валюта из трёх, положительное целое. Правила, которые
//! снимок называет в описаниях полей, проверяются: цена по умолчанию обязательна при ценах
//! по странам и у товара в продаже; длительность бесплатного периода — 3 дня, 1 или 2
//! недели, 1, 2, 3 или 6 месяцев или год; период вводной цены кратен периоду подписки и не
//! длиннее 12 месяцев; у вводной цены обязательны валюта, страна и цена.
//!
//! Текущее состояние читается внутренними чтениями ([`super::release::read`], ответ агенту
//! не отдаётся): группы — `pms.productGroup.list`, товары — `pms.product.search` (страна
//! обязательна, состояния `active` и `inactive`) и `pms.product.get` по каждому (без страны:
//! цены по странам как есть), акции — `pms.promotion.search` и `pms.promotion.get`. Списки
//! читаются страницами по 300 записей (предел API), не больше 20 страниц.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use appgallery_connect_api::AccessLevel;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

use super::Context;
use super::release::read;
use super::reviews_summary::{int, now_ms, uint};
use crate::error::ToolError;

/// Набор инструментов товаров.
pub(crate) const TOOLSET: &str = "products";
/// Значение `format` файла.
pub(crate) const FORMAT: &str = "appgallery-products";
/// Версия формата файла.
pub(crate) const VERSION: u64 = 1;

pub(crate) const GROUP_LIST: &str = "pms.productGroup.list";
pub(crate) const GROUP_UPDATE: &str = "pms.productGroup.update";
pub(crate) const PRODUCT_SEARCH: &str = "pms.product.search";
pub(crate) const PRODUCT_GET: &str = "pms.product.get";
pub(crate) const PRODUCT_IMPORT: &str = "pms.product.batchUpdate";
pub(crate) const PRODUCT_ACTIVATE: &str = "pms.product.batchActivate";
pub(crate) const PRODUCT_DEACTIVATE: &str = "pms.product.batchDeactivate";
pub(crate) const PROMOTION_SEARCH: &str = "pms.promotion.search";
pub(crate) const PROMOTION_GET: &str = "pms.promotion.get";
pub(crate) const PROMOTION_CREATE: &str = "pms.promotion.create";
pub(crate) const PROMOTION_UPDATE: &str = "pms.promotion.update";
pub(crate) const PROMOTION_RELEASE: &str = "pms.promotion.batchActivate";
pub(crate) const PROMOTION_REMOVE: &str = "pms.promotion.batchDeactivate";

/// Размер страницы списков PMS API (предел по справочнику — 300).
const PAGE_SIZE: u64 = 300;
/// Сколько страниц одного списка читается за вызов.
const MAX_PAGES: u64 = 20;
/// Наибольший размер файла товаров.
pub(crate) const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Страна поиска товаров по умолчанию (страна цены по умолчанию в справочнике — `CN`).
pub(crate) const DEFAULT_COUNTRY: &str = "CN";

/// Длины и ограничения полей из схем снимка.
const MAX_PRODUCT_NO: usize = 148;
const MAX_PRODUCT_NAME: usize = 55;
const MAX_PRODUCT_DESC: usize = 100;
const MAX_GROUP_NAME: usize = 30;
const MAX_OFFER_ID: usize = 64;

/// Типы товаров.
pub(crate) const PURCHASE_TYPES: &[&str] = &["consumable", "non_consumable", "auto_subscription"];
pub(crate) const SUBSCRIPTION: &str = "auto_subscription";
/// Длительности бесплатного периода по справочнику: (число, единица).
const TRIAL_PERIODS: &[(u64, &str)] = &[
    (3, "D"),
    (1, "W"),
    (2, "W"),
    (1, "M"),
    (2, "M"),
    (3, "M"),
    (6, "M"),
    (1, "Y"),
];

/// Сумма в минимальных единицах валюты: строка из цифр (в файле — строка или целое).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(crate) struct Amount(pub String);

impl Amount {
    /// Для сравнения: без ведущих нулей у числа.
    pub(crate) fn normalized(&self) -> String {
        let text = self.0.trim();
        if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()) {
            let stripped = text.trim_start_matches('0');
            if stripped.is_empty() {
                "0".to_owned()
            } else {
                stripped.to_owned()
            }
        } else {
            text.to_owned()
        }
    }

    /// Положительное целое.
    fn is_positive(&self) -> bool {
        let text = self.normalized();
        text.chars().all(|c| c.is_ascii_digit()) && !text.is_empty() && text != "0"
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(text) => Ok(Self(text.trim().to_owned())),
            Value::Number(number) if number.is_u64() => Ok(Self(number.to_string())),
            other => Err(serde::de::Error::custom(format!(
                "a price must be an amount in minor units of the currency (cents) as a string \
                 or a non-negative integer, not {other}"
            ))),
        }
    }
}

/// Файл товаров.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProductsFile {
    pub format: String,
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default)]
    pub subscription_groups: Vec<GroupEntry>,
    #[serde(default)]
    pub products: Vec<ProductEntry>,
    #[serde(default)]
    pub promotions: Vec<PromotionEntry>,
}

/// Группа подписок.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GroupEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub group_name: String,
}

/// Товар.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProductEntry {
    pub product_no: String,
    pub purchase_type: String,
    pub status: String,
    pub default_locale: String,
    pub product_name: String,
    #[serde(default)]
    pub product_desc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<ProductLanguage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_price: Option<DefaultPrice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prices: Option<Vec<CountryPrice>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_period: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_period_unit: Option<String>,
}

impl ProductEntry {
    pub(crate) fn is_subscription(&self) -> bool {
        self.purchase_type == SUBSCRIPTION
    }
}

/// Локализация товара.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProductLanguage {
    pub locale: String,
    pub product_name: String,
    #[serde(default)]
    pub product_desc: String,
}

/// Цена по умолчанию (от неё AppGallery Connect пересчитывает цены остальных стран).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DefaultPrice {
    pub country: String,
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Amount>,
}

impl DefaultPrice {
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.country == other.country
            && self.currency == other.currency
            && self.price.as_ref().map(Amount::normalized)
                == other.price.as_ref().map(Amount::normalized)
    }
}

/// Цена в стране. Валюту страны задаёт AppGallery Connect: в файле она для справки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CountryPrice {
    pub country: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    pub price: Amount,
}

/// Акция подписки: бесплатный период (`trial`) или вводная цена (`introductory`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PromotionEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_id: Option<String>,
    pub product_no: String,
    pub strategy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_status: Option<String>,
    /// Ход акции (`ongoing`, `not_started`, `finished`) — только для чтения.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub promotion_title: String,
    #[serde(default)]
    pub promotion_desc: String,
    pub default_locale: String,
    pub start_date: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_date: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number_of_units: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_price: Option<DefaultPrice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prices: Option<Vec<PromotionPrice>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<PromotionLanguage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_type: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_id: Option<String>,
}

impl PromotionEntry {
    /// Состояние продажи акции: `active` (по умолчанию) или `inactive`.
    pub(crate) fn sale_status(&self) -> &str {
        self.promotion_status.as_deref().unwrap_or("active")
    }

    /// Подпись для сообщений: идентификатор или название новой акции.
    pub(crate) fn label(&self) -> String {
        match &self.promotion_id {
            Some(id) => format!("promotion {id}"),
            None => format!(
                "new promotion \"{}\" of {}",
                self.promotion_title, self.product_no
            ),
        }
    }
}

/// Цена акции в стране (у бесплатного периода — только страна).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PromotionPrice {
    pub country: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Amount>,
}

/// Локализация акции.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PromotionLanguage {
    pub locale: String,
    pub title: String,
    #[serde(default)]
    pub desc: String,
}

/// Строка поля ответа без пробелов по краям; нет поля — пустая.
fn text(value: &Value, field: &str) -> String {
    match value.get(field) {
        Some(Value::String(text)) => text.trim().to_owned(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// Непустая строка поля ответа.
fn some_text(value: &Value, field: &str) -> Option<String> {
    Some(text(value, field)).filter(|text| !text.is_empty())
}

/// Сумма из поля ответа (строка или число).
fn amount_of(value: &Value, field: &str) -> Option<Amount> {
    match value.get(field) {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(Amount(text.trim().into())),
        Some(Value::Number(number)) => Some(Amount(
            number
                .as_u64()
                .map_or_else(|| number.to_string(), |n| n.to_string()),
        )),
        _ => None,
    }
}

fn items<'a>(value: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// Товар из ответа `pms.product.get` в виде записи файла.
pub(crate) fn product_from_detail(detail: &Value) -> ProductEntry {
    let purchase_type = text(detail, "purchaseType");
    let subscription = purchase_type == SUBSCRIPTION;
    let mut languages: Vec<ProductLanguage> = items(detail, "languages")
        .filter_map(|lang| {
            Some(ProductLanguage {
                locale: some_text(lang, "locale")?,
                product_name: text(lang, "productName"),
                product_desc: text(lang, "productDesc"),
            })
        })
        .collect();
    languages.sort_by(|a, b| a.locale.cmp(&b.locale));
    let mut prices: Vec<CountryPrice> = items(detail, "prices")
        .filter_map(|price| {
            Some(CountryPrice {
                country: some_text(price, "country")?,
                currency: some_text(price, "currencyCode"),
                price: amount_of(price, "price")?,
            })
        })
        .collect();
    prices.sort_by(|a, b| a.country.cmp(&b.country));
    let default_price = DefaultPrice {
        country: text(detail, "country"),
        currency: text(detail, "currency"),
        price: amount_of(detail, "price"),
    };
    let has_default = !default_price.country.is_empty()
        || !default_price.currency.is_empty()
        || default_price.price.is_some();
    ProductEntry {
        product_no: text(detail, "productNo"),
        purchase_type,
        status: text(detail, "status"),
        default_locale: text(detail, "defaultLocale"),
        product_name: text(detail, "productName"),
        product_desc: text(detail, "productDesc"),
        languages: Some(languages),
        default_price: has_default.then_some(default_price),
        prices: Some(prices),
        group_id: subscription.then(|| some_text(detail, "groupId")).flatten(),
        sub_period: subscription
            .then(|| uint(detail.get("numberOfUnits")))
            .flatten(),
        sub_period_unit: subscription
            .then(|| some_text(detail, "periodUnit"))
            .flatten(),
    }
}

/// Название хода акции по коду `status` (0 идёт, 1 не началась, 2 закончилась).
fn state_name(code: Option<i64>) -> Option<String> {
    match code? {
        0 => Some("ongoing".into()),
        1 => Some("not_started".into()),
        2 => Some("finished".into()),
        _ => None,
    }
}

/// Акция из ответа `pms.promotion.get` в виде записи файла.
pub(crate) fn promotion_from_detail(detail: &Value, product_no: &str) -> PromotionEntry {
    let mut prices: Vec<PromotionPrice> = items(detail, "prices")
        .filter_map(|price| {
            Some(PromotionPrice {
                country: some_text(price, "country")?,
                price: amount_of(price, "price"),
            })
        })
        .collect();
    if prices.is_empty() {
        // У бесплатного периода справочник отдаёт страны в `introductory`.
        prices = items(detail, "introductory")
            .filter_map(|item| {
                Some(PromotionPrice {
                    country: some_text(item, "country")?,
                    price: None,
                })
            })
            .collect();
    }
    prices.sort_by(|a, b| a.country.cmp(&b.country));
    let mut languages: Vec<PromotionLanguage> = items(detail, "languages")
        .filter_map(|lang| {
            Some(PromotionLanguage {
                locale: some_text(lang, "locale")?,
                title: text(lang, "title"),
                desc: text(lang, "desc"),
            })
        })
        .collect();
    languages.sort_by(|a, b| a.locale.cmp(&b.locale));
    let default_price = DefaultPrice {
        country: text(detail, "country"),
        currency: text(detail, "currency"),
        price: amount_of(detail, "defaultPrice"),
    };
    let has_default = !default_price.country.is_empty()
        || !default_price.currency.is_empty()
        || default_price.price.is_some();
    PromotionEntry {
        promotion_id: some_text(detail, "promotionId"),
        product_no: product_no.to_owned(),
        strategy: text(detail, "strategy"),
        promotion_status: Some(some_text(detail, "promotionStatus").unwrap_or("active".into())),
        state: state_name(int(detail.get("status"))),
        promotion_title: text(detail, "promotionTitle"),
        promotion_desc: text(detail, "promotionDesc"),
        default_locale: text(detail, "defaultLocale"),
        start_date: int(detail.get("startDate")).unwrap_or_default(),
        end_date: int(detail.get("endDate")),
        period_unit: some_text(detail, "periodUnit"),
        number_of_units: uint(detail.get("numberOfUnits")),
        default_price: has_default.then_some(default_price),
        prices: Some(prices),
        languages: Some(languages),
        promotion_type: int(detail.get("promotionType")),
        offer_id: some_text(detail, "offerId"),
    }
}

/// Товар из списка `pms.product.search`.
#[derive(Debug, Clone)]
pub(crate) struct ProductSummary {
    /// Идентификатор PMS (`id`): по нему акции ссылаются на товар.
    pub id: String,
    pub status: String,
}

/// Акция из списка `pms.promotion.search`.
#[derive(Debug, Clone)]
pub(crate) struct PromotionSummary {
    /// `productNo` товара акции; `None` — товара нет в списке товаров.
    pub product_no: Option<String>,
    /// Ход акции: 0 идёт, 1 не началась, 2 закончилась.
    pub state: Option<i64>,
}

/// Текущее состояние товаров приложения.
#[derive(Debug, Clone, Default)]
pub(crate) struct Remote {
    /// Группы по `groupId`.
    pub groups: BTreeMap<String, GroupEntry>,
    /// Товары списка по `productNo`.
    pub products: BTreeMap<String, ProductSummary>,
    /// Прочитанные товары по `productNo`.
    pub details: BTreeMap<String, ProductEntry>,
    /// Акции списка (кроме удалённых) по `promotionId`.
    pub promotions: BTreeMap<String, PromotionSummary>,
    /// Прочитанные акции по `promotionId`.
    pub promotion_details: BTreeMap<String, PromotionEntry>,
    /// Пометки для агента (пропущенные записи).
    pub warnings: Vec<String>,
}

/// Что читать подробно: `None` — всё.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Scope<'a> {
    pub products: Option<&'a BTreeSet<String>>,
    pub promotions: Option<&'a BTreeSet<String>>,
    /// Читать и закончившиеся акции (при `promotions: None`).
    pub include_finished: bool,
}

/// Уникальный `requestId` запроса PMS API (до 64 символов).
pub(crate) fn request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let number = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("mcp-{}-{number}", now_ms().unwrap_or_default())
}

/// Аргументы операции PMS API: заголовок `appId` и тело с `requestId`.
pub(crate) fn pms_args(app_id: &str, mut body: Map<String, Value>) -> Value {
    body.insert("requestId".into(), json!(request_id()));
    json!({"appId": app_id, "body": body})
}

/// Все страницы списка PMS API (`pageNum`/`pageSize`).
async fn pages(
    ctx: &Context,
    operation_id: &str,
    app_id: &str,
    filter: &Map<String, Value>,
    list: &str,
    what: &str,
) -> Result<Vec<Value>, ToolError> {
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let mut body = filter.clone();
        body.insert("pageNum".into(), json!(page));
        body.insert("pageSize".into(), json!(PAGE_SIZE));
        let response = read(ctx, operation_id, &pms_args(app_id, body))
            .await
            .map_err(|err| {
                let read = if all.is_empty() {
                    String::new()
                } else {
                    format!(" after {} records", all.len())
                };
                ToolError::new(format!("reading the {what} failed{read}: {err}"))
            })?;
        let batch: Vec<Value> = items(&response, list).cloned().collect();
        let count = batch.len() as u64;
        all.extend(batch);
        let total = uint(response.get("totalNumber"));
        if count < PAGE_SIZE || total.is_some_and(|total| all.len() as u64 >= total) {
            return Ok(all);
        }
    }
    Err(ToolError::new(format!(
        "the app has more than {} {what}; this tool reads at most {MAX_PAGES} pages of \
         {PAGE_SIZE}",
        MAX_PAGES * PAGE_SIZE
    )))
}

/// Читает группы, товары и акции приложения.
pub(crate) async fn fetch_remote(
    ctx: &Context,
    app_id: &str,
    country: &str,
    scope: Scope<'_>,
) -> Result<Remote, ToolError> {
    let mut remote = Remote::default();

    for group in pages(
        ctx,
        GROUP_LIST,
        app_id,
        &Map::new(),
        "simpleGroups",
        "subscription groups",
    )
    .await?
    {
        if let Some(id) = some_text(&group, "groupId") {
            remote.groups.insert(
                id.clone(),
                GroupEntry {
                    group_id: Some(id),
                    group_name: text(&group, "groupName"),
                },
            );
        }
    }

    let mut filter = Map::new();
    filter.insert("country".into(), json!(country));
    filter.insert("status".into(), json!("active,inactive"));
    for product in pages(ctx, PRODUCT_SEARCH, app_id, &filter, "products", "products").await? {
        let Some(product_no) = some_text(&product, "productNo") else {
            continue;
        };
        let status = text(&product, "status");
        if status == "delete" {
            continue;
        }
        remote.products.insert(
            product_no,
            ProductSummary {
                id: text(&product, "id"),
                status,
            },
        );
    }

    let wanted: Vec<String> = remote
        .products
        .keys()
        .filter(|no| scope.products.is_none_or(|set| set.contains(*no)))
        .cloned()
        .collect();
    for (done, product_no) in wanted.iter().enumerate() {
        let response = read(
            ctx,
            PRODUCT_GET,
            &json!({"appId": app_id, "productNo": product_no}),
        )
        .await
        .map_err(|err| {
            ToolError::new(format!(
                "reading product {product_no} failed after {done} of {} products: {err}",
                wanted.len()
            ))
        })?;
        let Some(detail) = response.get("product").filter(|p| p.is_object()) else {
            return Err(ToolError::new(format!(
                "the API returned no details of product {product_no}"
            )));
        };
        let mut entry = product_from_detail(detail);
        if entry.product_no.is_empty() {
            entry.product_no.clone_from(product_no);
        }
        remote.details.insert(product_no.clone(), entry);
    }

    let by_id: BTreeMap<&str, &str> = remote
        .products
        .iter()
        .filter(|(_, summary)| !summary.id.is_empty())
        .map(|(no, summary)| (summary.id.as_str(), no.as_str()))
        .collect();
    for promotion in pages(
        ctx,
        PROMOTION_SEARCH,
        app_id,
        &Map::new(),
        "productPromotions",
        "promotions",
    )
    .await?
    {
        let Some(id) = some_text(&promotion, "promotionId") else {
            continue;
        };
        if text(&promotion, "promotionStatus") == "delete" {
            continue;
        }
        let product_no = by_id
            .get(text(&promotion, "productId").as_str())
            .map(|no| (*no).to_owned());
        remote.promotions.insert(
            id,
            PromotionSummary {
                product_no,
                state: int(promotion.get("status")),
            },
        );
    }

    let mut wanted = Vec::new();
    for (id, summary) in &remote.promotions {
        match scope.promotions {
            Some(set) => {
                if set.contains(id) {
                    wanted.push((id.clone(), summary.product_no.clone()));
                }
            }
            None => {
                if summary.state == Some(2) && !scope.include_finished {
                    continue;
                }
                if summary.product_no.is_none() {
                    remote.warnings.push(format!(
                        "promotion {id} was skipped: its product is not among the active and \
                         inactive products of the app"
                    ));
                    continue;
                }
                wanted.push((id.clone(), summary.product_no.clone()));
            }
        }
    }
    for (done, (id, product_no)) in wanted.iter().enumerate() {
        let response = read(
            ctx,
            PROMOTION_GET,
            &json!({"appId": app_id, "promotionId": id}),
        )
        .await
        .map_err(|err| {
            ToolError::new(format!(
                "reading promotion {id} failed after {done} of {} promotions: {err}",
                wanted.len()
            ))
        })?;
        let Some(detail) = response.get("promotion").filter(|p| p.is_object()) else {
            return Err(ToolError::new(format!(
                "the API returned no details of promotion {id}"
            )));
        };
        let mut entry = promotion_from_detail(detail, product_no.as_deref().unwrap_or_default());
        if entry.promotion_id.is_none() {
            entry.promotion_id = Some(id.clone());
        }
        remote.promotion_details.insert(id.clone(), entry);
    }
    Ok(remote)
}

/// Файл из текущего состояния (для `products_export`): записи по порядку ключей.
pub(crate) fn file_from_remote(app_id: &str, remote: &Remote) -> ProductsFile {
    let mut promotions: Vec<PromotionEntry> = remote.promotion_details.values().cloned().collect();
    promotions.sort_by(|a, b| {
        (a.product_no.as_str(), a.promotion_id.as_deref())
            .cmp(&(b.product_no.as_str(), b.promotion_id.as_deref()))
    });
    ProductsFile {
        format: FORMAT.into(),
        version: VERSION,
        app_id: Some(app_id.to_owned()),
        subscription_groups: remote.groups.values().cloned().collect(),
        products: remote.details.values().cloned().collect(),
        promotions,
    }
}

/// Разбирает файл товаров: JSON, `format` и `version`, строки без пробелов по краям.
pub(crate) fn parse_file(text: &str) -> Result<ProductsFile, ToolError> {
    let text = text.trim_start_matches('\u{feff}');
    let mut file: ProductsFile = serde_json::from_str(text).map_err(|err| {
        ToolError::new(format!(
            "the products file is not valid: {err}. Export one with products_export to see the \
             format"
        ))
    })?;
    if file.format != FORMAT || file.version != VERSION {
        return Err(ToolError::new(format!(
            "the products file must have \"format\": \"{FORMAT}\" and \"version\": {VERSION} \
             (as written by products_export)"
        )));
    }
    trim_file(&mut file);
    Ok(file)
}

fn trim(value: &mut String) {
    let trimmed = value.replace("\r\n", "\n").trim().to_owned();
    *value = trimmed;
}

fn trim_opt(value: &mut Option<String>) {
    if let Some(text) = value {
        trim(text);
        if text.is_empty() {
            *value = None;
        }
    }
}

fn trim_default(price: &mut DefaultPrice) {
    trim(&mut price.country);
    trim(&mut price.currency);
}

/// Строки файла без пробелов по краям (`\r\n` — как `\n`): так же нормализован и ответ API.
fn trim_file(file: &mut ProductsFile) {
    trim_opt(&mut file.app_id);
    for group in &mut file.subscription_groups {
        trim_opt(&mut group.group_id);
        trim(&mut group.group_name);
    }
    for product in &mut file.products {
        trim(&mut product.product_no);
        trim(&mut product.purchase_type);
        trim(&mut product.status);
        trim(&mut product.default_locale);
        trim(&mut product.product_name);
        trim(&mut product.product_desc);
        for lang in product.languages.iter_mut().flatten() {
            trim(&mut lang.locale);
            trim(&mut lang.product_name);
            trim(&mut lang.product_desc);
        }
        if let Some(price) = &mut product.default_price {
            trim_default(price);
        }
        for price in product.prices.iter_mut().flatten() {
            trim(&mut price.country);
            trim_opt(&mut price.currency);
        }
        trim_opt(&mut product.group_id);
        trim_opt(&mut product.sub_period_unit);
    }
    for promotion in &mut file.promotions {
        trim_opt(&mut promotion.promotion_id);
        trim(&mut promotion.product_no);
        trim(&mut promotion.strategy);
        trim_opt(&mut promotion.promotion_status);
        trim(&mut promotion.promotion_title);
        trim(&mut promotion.promotion_desc);
        trim(&mut promotion.default_locale);
        trim_opt(&mut promotion.period_unit);
        if let Some(price) = &mut promotion.default_price {
            trim_default(price);
        }
        for price in promotion.prices.iter_mut().flatten() {
            trim(&mut price.country);
        }
        for lang in promotion.languages.iter_mut().flatten() {
            trim(&mut lang.locale);
            trim(&mut lang.title);
            trim(&mut lang.desc);
        }
        trim_opt(&mut promotion.offer_id);
    }
}

/// Код страны: две заглавные латинские буквы.
pub(crate) fn is_country(code: &str) -> bool {
    code.len() == 2 && code.chars().all(|c| c.is_ascii_uppercase())
}

/// Код валюты: три заглавные латинские буквы.
pub(crate) fn is_currency(code: &str) -> bool {
    code.len() == 3 && code.chars().all(|c| c.is_ascii_uppercase())
}

/// `productNo`: начинается с буквы или цифры, дальше буквы, цифры, `_` и `.`.
fn is_product_no(no: &str) -> bool {
    let mut chars = no.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        && no.len() <= MAX_PRODUCT_NO
}

fn check_text(errors: &mut Vec<String>, label: &str, text: &str, max: usize, required: bool) {
    let count = text.chars().count();
    if required && count == 0 {
        errors.push(format!("{label} is empty"));
    }
    if count > max {
        errors.push(format!(
            "{label}: {count} characters, AppGallery Connect allows at most {max}"
        ));
    }
    if text.contains('|') {
        errors.push(format!("{label}: the vertical bar `|` is not allowed"));
    }
}

fn check_default_price(errors: &mut Vec<String>, label: &str, price: &DefaultPrice) {
    if !is_country(&price.country) {
        errors.push(format!(
            "{label}.defaultPrice.country `{}` is not a two-letter country code such as CN",
            price.country
        ));
    }
    if !is_currency(&price.currency) {
        errors.push(format!(
            "{label}.defaultPrice.currency `{}` is not a three-letter currency code such as CNY",
            price.currency
        ));
    }
    if let Some(amount) = &price.price
        && !amount.is_positive()
    {
        errors.push(format!(
            "{label}.defaultPrice.price `{}` is not a positive amount in minor units (cents)",
            amount.0
        ));
    }
}

/// Проверки файла без обращения к API. Возвращает все ошибки.
pub(crate) fn check_file(file: &ProductsFile) -> Vec<String> {
    let mut errors = Vec::new();

    let mut group_ids = BTreeSet::new();
    for (index, group) in file.subscription_groups.iter().enumerate() {
        let label = match &group.group_id {
            Some(id) => format!("subscription group {id}"),
            None => format!("subscriptionGroups[{index}]"),
        };
        match &group.group_id {
            None => errors.push(format!(
                "{label}: groupId is missing; this tool changes the names of existing groups \
                 only, create new groups in AppGallery Connect"
            )),
            Some(id) => {
                if !group_ids.insert(id.as_str()) {
                    errors.push(format!("{label} is listed twice"));
                }
            }
        }
        check_text(
            &mut errors,
            &format!("{label}: groupName"),
            &group.group_name,
            MAX_GROUP_NAME,
            true,
        );
    }

    let mut product_nos = BTreeSet::new();
    for product in &file.products {
        let label = format!("product {}", product.product_no);
        if !is_product_no(&product.product_no) {
            errors.push(format!(
                "{label}: productNo must start with a letter or digit, contain only letters, \
                 digits, `_` and `.`, and be at most {MAX_PRODUCT_NO} characters"
            ));
        }
        if !product_nos.insert(product.product_no.as_str()) {
            errors.push(format!("{label} is listed twice"));
        }
        if !PURCHASE_TYPES.contains(&product.purchase_type.as_str()) {
            errors.push(format!(
                "{label}: purchaseType `{}` must be one of {}",
                product.purchase_type,
                PURCHASE_TYPES.join(", ")
            ));
        }
        if !["active", "inactive"].contains(&product.status.as_str()) {
            errors.push(format!(
                "{label}: status `{}` must be active or inactive (deleting products is not \
                 supported)",
                product.status
            ));
        }
        if product.default_locale.is_empty() {
            errors.push(format!("{label}: defaultLocale is empty"));
        }
        check_text(
            &mut errors,
            &format!("{label}: productName"),
            &product.product_name,
            MAX_PRODUCT_NAME,
            true,
        );
        check_text(
            &mut errors,
            &format!("{label}: productDesc"),
            &product.product_desc,
            MAX_PRODUCT_DESC,
            false,
        );
        let mut locales = BTreeSet::new();
        for lang in product.languages.iter().flatten() {
            let lang_label = format!("{label}: language {}", lang.locale);
            if lang.locale.is_empty() {
                errors.push(format!("{label}: a language has no locale"));
            } else if !locales.insert(lang.locale.as_str()) {
                errors.push(format!("{lang_label} is listed twice"));
            }
            check_text(
                &mut errors,
                &format!("{lang_label} productName"),
                &lang.product_name,
                MAX_PRODUCT_NAME,
                true,
            );
            check_text(
                &mut errors,
                &format!("{lang_label} productDesc"),
                &lang.product_desc,
                MAX_PRODUCT_DESC,
                false,
            );
        }
        match &product.default_price {
            Some(price) => {
                check_default_price(&mut errors, &label, price);
                let has_prices = product.prices.as_ref().is_some_and(|p| !p.is_empty());
                if price.price.is_none() && (has_prices || product.status == "active") {
                    errors.push(format!(
                        "{label}: defaultPrice.price is required for a product on sale and \
                         when prices per country are listed"
                    ));
                }
            }
            None => {
                if product.prices.as_ref().is_some_and(|p| !p.is_empty()) {
                    errors.push(format!(
                        "{label}: defaultPrice is required when prices per country are listed"
                    ));
                }
            }
        }
        let mut countries = BTreeSet::new();
        for price in product.prices.iter().flatten() {
            if !is_country(&price.country) {
                errors.push(format!(
                    "{label}: price country `{}` is not a two-letter country code such as RU",
                    price.country
                ));
            } else if !countries.insert(price.country.as_str()) {
                errors.push(format!(
                    "{label}: the price for {} is listed twice",
                    price.country
                ));
            }
            if let Some(currency) = &price.currency
                && !is_currency(currency)
            {
                errors.push(format!(
                    "{label}: currency `{currency}` of the price for {} is not a three-letter \
                     currency code such as RUB",
                    price.country
                ));
            }
            if !price.price.is_positive() {
                errors.push(format!(
                    "{label}: the price for {} `{}` is not a positive amount in minor units \
                     (cents)",
                    price.country, price.price.0
                ));
            }
        }
        if product.is_subscription() {
            if product.group_id.is_none() {
                errors.push(format!("{label}: a subscription needs groupId"));
            }
            if product.sub_period.is_none_or(|p| p == 0) {
                errors.push(format!(
                    "{label}: a subscription needs subPeriod (a positive number)"
                ));
            }
            if !product
                .sub_period_unit
                .as_deref()
                .is_some_and(|u| ["W", "M", "Y"].contains(&u))
            {
                errors.push(format!(
                    "{label}: a subscription needs subPeriodUnit W (week), M (month) or Y (year)"
                ));
            }
        } else if product.group_id.is_some()
            || product.sub_period.is_some()
            || product.sub_period_unit.is_some()
        {
            errors.push(format!(
                "{label}: groupId, subPeriod and subPeriodUnit are only for auto_subscription \
                 products"
            ));
        }
    }

    let mut promotion_ids = BTreeSet::new();
    for promotion in &file.promotions {
        let label = promotion.label();
        if let Some(id) = &promotion.promotion_id
            && !promotion_ids.insert(id.as_str())
        {
            errors.push(format!("{label} is listed twice"));
        }
        if promotion.product_no.is_empty() {
            errors.push(format!("{label}: productNo is empty"));
        }
        if !["trial", "introductory"].contains(&promotion.strategy.as_str()) {
            errors.push(format!(
                "{label}: strategy `{}` must be trial (free trial) or introductory \
                 (introductory price)",
                promotion.strategy
            ));
        }
        if !["active", "inactive"].contains(&promotion.sale_status()) {
            errors.push(format!(
                "{label}: promotionStatus `{}` must be active or inactive",
                promotion.sale_status()
            ));
        }
        if promotion.promotion_id.is_none() && promotion.sale_status() != "active" {
            errors.push(format!(
                "{label}: a new promotion is always created active (promotionStatus active)"
            ));
        }
        if promotion.promotion_title.is_empty() {
            errors.push(format!("{label}: promotionTitle is empty"));
        }
        if promotion.default_locale.is_empty() {
            errors.push(format!("{label}: defaultLocale is empty"));
        }
        if promotion.start_date <= 0 {
            errors.push(format!(
                "{label}: startDate must be a timestamp in milliseconds"
            ));
        }
        if let Some(end) = promotion.end_date
            && end <= promotion.start_date
        {
            errors.push(format!("{label}: endDate must be later than startDate"));
        }
        if let Some(unit) = &promotion.period_unit
            && !["D", "W", "M", "Y"].contains(&unit.as_str())
        {
            errors.push(format!("{label}: periodUnit `{unit}` must be D, W, M or Y"));
        }
        if promotion.period_unit.is_some() != promotion.number_of_units.is_some() {
            errors.push(format!("{label}: periodUnit and numberOfUnits go together"));
        }
        let introductory = promotion.strategy == "introductory";
        match &promotion.default_price {
            Some(price) => {
                check_default_price(&mut errors, &label, price);
                if introductory && price.price.is_none() {
                    errors.push(format!(
                        "{label}: an introductory price needs defaultPrice.price"
                    ));
                }
            }
            None if introductory => errors.push(format!(
                "{label}: an introductory price needs defaultPrice (country, currency, price)"
            )),
            None => {}
        }
        let mut countries = BTreeSet::new();
        for price in promotion.prices.iter().flatten() {
            if !is_country(&price.country) {
                errors.push(format!(
                    "{label}: price country `{}` is not a two-letter country code such as RU",
                    price.country
                ));
            } else if !countries.insert(price.country.as_str()) {
                errors.push(format!(
                    "{label}: the price for {} is listed twice",
                    price.country
                ));
            }
            match &price.price {
                Some(amount) if !amount.is_positive() => errors.push(format!(
                    "{label}: the price for {} `{}` is not a positive amount in minor units \
                     (cents)",
                    price.country, amount.0
                )),
                None if introductory => errors.push(format!(
                    "{label}: the introductory price for {} has no price",
                    price.country
                )),
                _ => {}
            }
        }
        let mut locales = BTreeSet::new();
        for lang in promotion.languages.iter().flatten() {
            if lang.locale.is_empty() {
                errors.push(format!("{label}: a language has no locale"));
            } else if !locales.insert(lang.locale.as_str()) {
                errors.push(format!("{label}: language {} is listed twice", lang.locale));
            }
            if lang.title.is_empty() {
                errors.push(format!("{label}: language {} has no title", lang.locale));
            }
        }
        if let Some(offer) = &promotion.offer_id
            && (offer.len() > MAX_OFFER_ID || !offer.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            errors.push(format!(
                "{label}: offerId must be up to {MAX_OFFER_ID} letters and digits"
            ));
        }
    }
    errors
}

/// Проверки акции, которым нужен товар (из файла или из AppGallery Connect): тип товара и
/// период акции по справочнику.
pub(crate) fn check_promotion_product(
    errors: &mut Vec<String>,
    promotion: &PromotionEntry,
    product: &ProductEntry,
) {
    let label = promotion.label();
    if !product.is_subscription() {
        errors.push(format!(
            "{label}: promotions are only for auto_subscription products, {} is {}",
            product.product_no, product.purchase_type
        ));
        return;
    }
    let (Some(units), Some(unit)) = (promotion.number_of_units, promotion.period_unit.as_deref())
    else {
        errors.push(format!(
            "{label}: periodUnit and numberOfUnits are required for a subscription promotion"
        ));
        return;
    };
    if promotion.strategy == "trial" {
        if !TRIAL_PERIODS.contains(&(units, unit)) {
            errors.push(format!(
                "{label}: a free trial lasts 3 days (3 D), 1 or 2 weeks (W), 1, 2, 3 or 6 months \
                 (M) or 1 year (1 Y), not {units} {unit}"
            ));
        }
        return;
    }
    // Вводная цена: кратно периоду подписки и не длиннее 12 месяцев.
    let (Some(sub_units), Some(sub_unit)) =
        (product.sub_period, product.sub_period_unit.as_deref())
    else {
        return;
    };
    let months = |n: u64, u: &str| match u {
        "M" => Some(n),
        "Y" => Some(n * 12),
        _ => None,
    };
    let fits = match (months(units, unit), months(sub_units, sub_unit)) {
        (Some(promo), Some(sub)) => sub > 0 && promo % sub == 0 && promo <= 12,
        _ => unit == "W" && sub_unit == "W" && sub_units > 0 && units % sub_units == 0,
    };
    if !fits {
        errors.push(format!(
            "{label}: the period of an introductory price must be a multiple of the \
             subscription period ({sub_units} {sub_unit}) and at most 12 months, not {units} \
             {unit}"
        ));
    }
}

/// Уровень доступа строкой для ответа.
pub(crate) fn level_name(level: AccessLevel) -> &'static str {
    match level {
        AccessLevel::Read => "read",
        AccessLevel::Write => "write",
        AccessLevel::Destructive => "destructive",
    }
}

/// Аргумент `country`: код страны поиска товаров.
pub(crate) fn country_arg(args: &Value) -> Result<String, ToolError> {
    match super::listing::str_arg(args, "country")? {
        None => Ok(DEFAULT_COUNTRY.to_owned()),
        Some(code) if is_country(code) => Ok(code.to_owned()),
        Some(code) => Err(ToolError::new(format!(
            "`country` `{code}` is not a two-letter country code such as CN"
        ))),
    }
}

/// Схема аргумента `country`.
pub(crate) fn country_schema() -> Value {
    json!({
        "type": "string",
        "description": "Two-letter country code that the product search of the PMS API \
            requires (it converts the list prices for that country; the file takes the prices \
            of each product as they are). Default: CN."
    })
}

#[cfg(test)]
mod tests;
