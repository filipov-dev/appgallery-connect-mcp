//! `products_apply`: файл товаров (формат — [`super::products`]) в AppGallery Connect.
//!
//! Порядок: чтение и проверка файла (до запросов) → текущее состояние
//! ([`super::products::fetch_remote`]: группы, список товаров и акций, подробно — только
//! записи из файла) → проверки, которым нужно текущее состояние, и план (до записи) →
//! запись. Запись, которой нет в файле, не трогается; языки и цены по странам, которых нет
//! у записи файла, остаются как есть.
//!
//! Уровни правок: названия и описания товаров, их языки и названия групп подписок — запись;
//! цены (по умолчанию и по странам), создание товара, активация и деактивация, любое
//! изменение акции, её создание, запуск и снятие — удаление (последствия для пользователей
//! и денег; изменение акции переотправляет её цены и период). Инструмент регистрируется на
//! уровне записи, как `listing_apply`: план (`dry_run`, по умолчанию) строится всегда, а
//! применение плана с правками уровня удаления на сервере без `--allow-destructive` —
//! отказ до первой записи с перечнем таких правок.
//!
//! Запись: группы (`pms.productGroup.update`, по одной) → товары
//! (`pms.product.batchUpdate` частями по 100: создание и изменение; у изменения — текущее
//! состояние, текущая цена по умолчанию и только изменённые цены по странам, потому что
//! справочник меняет только перечисленные цены, а без цены по умолчанию ставит `CN`/`CNY`) →
//! деактивация и активация товаров → акции: создание, изменение (тело целиком: без цен
//! акция по справочнику не действует), запуск и снятие (по 100 за запрос — предел API).
//! Ответ пакетной операции с `failedNumber` больше нуля — сбой. Сбой на середине — ошибка
//! со списком уже применённого.

use std::collections::{BTreeMap, BTreeSet};

use appgallery_connect_api::AccessLevel;
use serde_json::{Map, Value, json};

use super::listing::{self, bool_arg, required_str};
use super::products::{
    self, CountryPrice, DefaultPrice, GROUP_UPDATE, MAX_FILE_BYTES, PRODUCT_ACTIVATE,
    PRODUCT_DEACTIVATE, PRODUCT_IMPORT, PROMOTION_CREATE, PROMOTION_RELEASE, PROMOTION_REMOVE,
    PROMOTION_UPDATE, ProductEntry, ProductsFile, PromotionEntry, Remote, Scope, TOOLSET,
    check_promotion_product, country_arg, country_schema, level_name, pms_args,
};
use super::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};

/// Имя инструмента.
pub const NAME: &str = "products_apply";

/// Сколько записей в одном пакетном запросе.
const BATCH: usize = 100;
/// Сколько ключей перечислять в `not_in_file`.
const MAX_LISTED: usize = 50;

const DESCRIPTION: &str = "Apply a products file (as written by products_export) to the in-app \
products of an app: compare it with AppGallery Connect and update only what differs, in \
batches. `dry_run` defaults to true and returns the plan: every change with its fields (old and \
new values) and its access level. Call again with dry_run false to apply. Levels: product names \
and descriptions (also per language) and subscription group names are write changes; prices \
(the default price and prices per country), creating a product, activating or deactivating \
products, and creating, changing, releasing or removing promotions are destructive changes \
(they affect what users pay) and need a server started with --allow-destructive: without it a \
plan with such changes is refused before anything is sent, with the list of them. Products, \
promotions and groups that are not in the file are left as they are, and so are languages and \
country prices missing from an entry. The file is checked before any request: amounts in minor \
units (cents) as positive integers, two-letter country and three-letter currency codes, name \
lengths, free trial periods (3 days, 1 or 2 weeks, 1, 2, 3 or 6 months, 1 year) and \
introductory price periods (a multiple of the subscription period, up to 12 months). The \
product type, subscription group and period and the promotion type cannot be changed. With \
force_existing_subscribers a new subscription price also applies to existing subscribers. \
Local paths are allowed only inside the server's allowed directories.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: TOOLSET,
        access: AccessLevel::Write,
        description: DESCRIPTION,
        input_schema,
        run,
    }]
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "appId": {
                "type": ["string", "integer"],
                "description": "Numeric app ID (like 100000001). Optional: by default the \
                    appId of the file, or the only default app of the profile. Must match the \
                    appId of the file."
            },
            "file": {
                "type": "string",
                "description": "Products file (inside the allowed directories), as written by \
                    products_export."
            },
            "dry_run": {
                "type": "boolean",
                "description": "Only return the plan with the differences. Default: true."
            },
            "force_existing_subscribers": {
                "type": "boolean",
                "description": "Apply changed subscription prices to existing subscribers too \
                    (force 1 of the PMS API). Default: false."
            },
            "country": country_schema()
        },
        "required": ["file"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(apply(ctx, args))
}

/// Изменение плана для агента.
#[derive(Debug, Clone)]
struct Change {
    action: &'static str,
    /// Поля, которые называют запись (`productNo`, `promotionId`, `groupId`).
    key: Map<String, Value>,
    level: AccessLevel,
    fields: Vec<Value>,
    /// Короткая подпись для отказа и списка применённого.
    label: String,
}

impl Change {
    fn describe(&self) -> Value {
        let mut out = Map::new();
        out.insert("action".into(), json!(self.action));
        out.extend(self.key.clone());
        out.insert("level".into(), json!(level_name(self.level)));
        if !self.fields.is_empty() {
            out.insert("changes".into(), Value::Array(self.fields.clone()));
        }
        Value::Object(out)
    }
}

/// Шаг записи.
#[derive(Debug, Clone)]
enum Step {
    /// Один запрос: операция, тело (без `requestId`), подпись.
    Single {
        operation: &'static str,
        body: Map<String, Value>,
        label: String,
    },
    /// Пакетный запрос: операция, поле списка, поле ключа в `resultInfo`, элементы
    /// (ключ, элемент, подпись).
    Batch {
        operation: &'static str,
        field: &'static str,
        key: &'static str,
        items: Vec<(String, Value, String)>,
    },
}

/// План: изменения для агента и шаги записи в порядке выполнения.
#[derive(Debug, Default)]
struct Plan {
    changes: Vec<Change>,
    steps: Vec<Step>,
    unchanged_products: usize,
    unchanged_promotions: usize,
    unchanged_groups: usize,
    /// Языки и цены, которые есть в AppGallery Connect, но не в записях файла.
    kept: Vec<String>,
}

fn diff(fields: &mut Vec<Value>, field: &str, old: Value, new: Value) {
    if old != new {
        fields.push(json!({"field": field, "old": old, "new": new}));
    }
}

fn price_text(price: Option<&products::Amount>) -> Value {
    price.map_or(Value::Null, |p| json!(p.normalized()))
}

fn default_price_value(price: Option<&DefaultPrice>) -> Value {
    price.map_or(Value::Null, DefaultPrice::describe_normalized)
}

impl DefaultPrice {
    fn describe_normalized(&self) -> Value {
        json!({
            "country": self.country,
            "currency": self.currency,
            "price": price_text(self.price.as_ref()),
        })
    }
}

/// Элемент `pms.product.batchUpdate` (`ProductImportInfo`).
fn import_item(entry: &ProductEntry, status: &str, prices: &[&CountryPrice], force: bool) -> Value {
    let mut item = Map::new();
    item.insert("productNo".into(), json!(entry.product_no));
    item.insert("status".into(), json!(status));
    item.insert("productName".into(), json!(entry.product_name));
    item.insert("productDesc".into(), json!(entry.product_desc));
    item.insert("purchaseType".into(), json!(entry.purchase_type));
    item.insert("defaultLocale".into(), json!(entry.default_locale));
    if let Some(group) = &entry.group_id {
        item.insert("subGroupId".into(), json!(group));
    }
    if let Some(period) = entry.sub_period {
        // В `ProductImportInfo` период — строка.
        item.insert("subPeriod".into(), json!(period.to_string()));
    }
    if let Some(unit) = &entry.sub_period_unit {
        item.insert("subPeriodUnit".into(), json!(unit));
    }
    if let Some(price) = &entry.default_price {
        let mut info = Map::new();
        info.insert("country".into(), json!(price.country));
        info.insert("currency".into(), json!(price.currency));
        if let Some(amount) = &price.price {
            info.insert("price".into(), json!(amount.normalized()));
        }
        item.insert("defaultPriceInfo".into(), Value::Object(info));
    }
    let languages: Vec<Value> = entry
        .languages
        .iter()
        .flatten()
        .map(|lang| {
            json!({
                "locale": lang.locale,
                "productName": lang.product_name,
                "productDesc": lang.product_desc,
            })
        })
        .collect();
    if !languages.is_empty() {
        item.insert("languages".into(), Value::Array(languages));
    }
    if !prices.is_empty() {
        let prices: Vec<Value> = prices
            .iter()
            .map(|p| json!({"country": p.country, "price": p.price.normalized()}))
            .collect();
        item.insert("prices".into(), Value::Array(prices));
    }
    if force {
        item.insert("force".into(), json!(1));
    }
    Value::Object(item)
}

/// Ресурс `pms.promotion.create`/`update` (`ProductPromotionInfo`).
fn promotion_resource(entry: &PromotionEntry, status: &str) -> Value {
    let mut item = Map::new();
    if let Some(id) = &entry.promotion_id {
        item.insert("promotionId".into(), json!(id));
    }
    item.insert("productNo".into(), json!(entry.product_no));
    item.insert("startDate".into(), json!(entry.start_date));
    if let Some(end) = entry.end_date {
        item.insert("endDate".into(), json!(end));
    }
    item.insert("status".into(), json!(status));
    item.insert("strategy".into(), json!(entry.strategy));
    item.insert("promotionTitle".into(), json!(entry.promotion_title));
    item.insert("defaultLocale".into(), json!(entry.default_locale));
    item.insert("promotionDesc".into(), json!(entry.promotion_desc));
    if let Some(unit) = &entry.period_unit {
        item.insert("periodUnit".into(), json!(unit));
    }
    if let Some(units) = entry.number_of_units {
        item.insert("numberOfUnits".into(), json!(units));
    }
    if let Some(price) = &entry.default_price {
        item.insert("country".into(), json!(price.country));
        item.insert("currency".into(), json!(price.currency));
        if let Some(amount) = &price.price {
            item.insert("defaultPrice".into(), json!(amount.normalized()));
        }
    }
    let prices: Vec<Value> = entry
        .prices
        .iter()
        .flatten()
        .map(|p| match &p.price {
            Some(amount) => json!({"country": p.country, "price": amount.normalized()}),
            None => json!({"country": p.country}),
        })
        .collect();
    item.insert("prices".into(), Value::Array(prices));
    let languages: Vec<Value> = entry
        .languages
        .iter()
        .flatten()
        .map(|lang| json!({"locale": lang.locale, "title": lang.title, "desc": lang.desc}))
        .collect();
    item.insert("languages".into(), Value::Array(languages));
    if let Some(kind) = entry.promotion_type {
        item.insert("promotionType".into(), json!(kind));
    }
    if let Some(offer) = &entry.offer_id {
        item.insert("offerId".into(), json!(offer));
    }
    Value::Object(item)
}

fn key(name: &str, value: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(name.into(), json!(value));
    map
}

/// Изменения товара, который уже есть: поля, уровень, тело и смена состояния.
struct ProductDiff {
    fields: Vec<Value>,
    level: AccessLevel,
    merged: ProductEntry,
    changed_prices: Vec<CountryPrice>,
}

fn product_diff(
    errors: &mut Vec<String>,
    kept: &mut Vec<String>,
    local: &ProductEntry,
    remote: &ProductEntry,
) -> ProductDiff {
    let label = format!("product {}", local.product_no);
    let fixed = [
        (
            "purchaseType",
            json!(local.purchase_type),
            json!(remote.purchase_type),
        ),
        ("groupId", json!(local.group_id), json!(remote.group_id)),
        (
            "subPeriod",
            json!(local.sub_period),
            json!(remote.sub_period),
        ),
        (
            "subPeriodUnit",
            json!(local.sub_period_unit),
            json!(remote.sub_period_unit),
        ),
    ];
    for (field, new, old) in fixed {
        if new != old {
            errors.push(format!(
                "{label}: {field} cannot be changed (AppGallery Connect has {old}, the file \
                 has {new})"
            ));
        }
    }

    let mut merged = remote.clone();
    let mut fields = Vec::new();
    let mut level = AccessLevel::Write;
    diff(
        &mut fields,
        "productName",
        json!(remote.product_name),
        json!(local.product_name),
    );
    diff(
        &mut fields,
        "productDesc",
        json!(remote.product_desc),
        json!(local.product_desc),
    );
    diff(
        &mut fields,
        "defaultLocale",
        json!(remote.default_locale),
        json!(local.default_locale),
    );
    merged.product_name.clone_from(&local.product_name);
    merged.product_desc.clone_from(&local.product_desc);
    merged.default_locale.clone_from(&local.default_locale);

    if let Some(languages) = &local.languages {
        let mut map: BTreeMap<String, (String, String)> = remote
            .languages
            .iter()
            .flatten()
            .map(|l| {
                (
                    l.locale.clone(),
                    (l.product_name.clone(), l.product_desc.clone()),
                )
            })
            .collect();
        let local_locales: BTreeSet<&str> = languages.iter().map(|l| l.locale.as_str()).collect();
        for locale in map.keys() {
            if !local_locales.contains(locale.as_str()) {
                kept.push(format!("{label}: language {locale}"));
            }
        }
        for lang in languages {
            let new = (lang.product_name.clone(), lang.product_desc.clone());
            let old = map.get(&lang.locale);
            if old != Some(&new) {
                fields.push(json!({
                    "field": "languages",
                    "locale": lang.locale,
                    "old": old.map(|(name, desc)| json!({"productName": name, "productDesc": desc})),
                    "new": {"productName": new.0, "productDesc": new.1},
                }));
            }
            map.insert(lang.locale.clone(), new);
        }
        merged.languages = Some(
            map.into_iter()
                .map(
                    |(locale, (product_name, product_desc))| products::ProductLanguage {
                        locale,
                        product_name,
                        product_desc,
                    },
                )
                .collect(),
        );
    }

    if let Some(price) = &local.default_price {
        let same = remote.default_price.as_ref().is_some_and(|r| r.same(price));
        if !same {
            fields.push(json!({
                "field": "defaultPrice",
                "old": default_price_value(remote.default_price.as_ref()),
                "new": price.describe_normalized(),
            }));
            level = AccessLevel::Destructive;
            merged.default_price = Some(price.clone());
        }
    }

    let mut changed_prices = Vec::new();
    if let Some(prices) = &local.prices {
        let current: BTreeMap<&str, &CountryPrice> = remote
            .prices
            .iter()
            .flatten()
            .map(|p| (p.country.as_str(), p))
            .collect();
        let local_countries: BTreeSet<&str> = prices.iter().map(|p| p.country.as_str()).collect();
        for country in current.keys() {
            if !local_countries.contains(country) {
                kept.push(format!("{label}: price for {country}"));
            }
        }
        for price in prices {
            let old = current.get(price.country.as_str());
            if let (Some(new_currency), Some(old_currency)) =
                (&price.currency, old.and_then(|o| o.currency.as_ref()))
                && new_currency != old_currency
            {
                errors.push(format!(
                    "{label}: the currency for {} is {old_currency} in AppGallery Connect; the \
                     currency of a country cannot be changed, fix `currency` in the file",
                    price.country
                ));
            }
            let old_amount = old.map(|o| o.price.normalized());
            if old_amount.as_deref() != Some(price.price.normalized().as_str()) {
                fields.push(json!({
                    "field": "prices",
                    "country": price.country,
                    "old": old_amount,
                    "new": price.price.normalized(),
                }));
                level = AccessLevel::Destructive;
                changed_prices.push(price.clone());
            }
        }
    }
    if !changed_prices.is_empty()
        && merged
            .default_price
            .as_ref()
            .is_none_or(|p| p.price.is_none())
    {
        errors.push(format!(
            "{label}: prices per country need the default price, and neither the file nor \
             AppGallery Connect has one; add defaultPrice"
        ));
    }
    ProductDiff {
        fields,
        level,
        merged,
        changed_prices,
    }
}

/// Акция с полями, которых нет в файле, из текущего состояния.
fn merge_promotion(local: &PromotionEntry, remote: &PromotionEntry) -> PromotionEntry {
    let mut merged = local.clone();
    merged.promotion_status = remote.promotion_status.clone();
    merged.state.clone_from(&remote.state);
    if merged.end_date.is_none() {
        merged.end_date = remote.end_date;
    }
    if merged.period_unit.is_none() && merged.number_of_units.is_none() {
        merged.period_unit.clone_from(&remote.period_unit);
        merged.number_of_units = remote.number_of_units;
    }
    if merged.default_price.is_none() {
        merged.default_price.clone_from(&remote.default_price);
    }
    if merged.promotion_type.is_none() {
        merged.promotion_type = remote.promotion_type;
    }
    if merged.offer_id.is_none() {
        merged.offer_id.clone_from(&remote.offer_id);
    }
    // Цены и языки: текущие, поверх — из файла.
    let mut prices: BTreeMap<String, Option<products::Amount>> = remote
        .prices
        .iter()
        .flatten()
        .map(|p| (p.country.clone(), p.price.clone()))
        .collect();
    for price in local.prices.iter().flatten() {
        prices.insert(price.country.clone(), price.price.clone());
    }
    merged.prices = Some(
        prices
            .into_iter()
            .map(|(country, price)| products::PromotionPrice { country, price })
            .collect(),
    );
    let mut languages: BTreeMap<String, (String, String)> = remote
        .languages
        .iter()
        .flatten()
        .map(|l| (l.locale.clone(), (l.title.clone(), l.desc.clone())))
        .collect();
    for lang in local.languages.iter().flatten() {
        languages.insert(lang.locale.clone(), (lang.title.clone(), lang.desc.clone()));
    }
    merged.languages = Some(
        languages
            .into_iter()
            .map(|(locale, (title, desc))| products::PromotionLanguage {
                locale,
                title,
                desc,
            })
            .collect(),
    );
    merged
}

fn promotion_fields(old: &PromotionEntry, new: &PromotionEntry) -> Vec<Value> {
    let mut fields = Vec::new();
    diff(
        &mut fields,
        "promotionTitle",
        json!(old.promotion_title),
        json!(new.promotion_title),
    );
    diff(
        &mut fields,
        "promotionDesc",
        json!(old.promotion_desc),
        json!(new.promotion_desc),
    );
    diff(
        &mut fields,
        "defaultLocale",
        json!(old.default_locale),
        json!(new.default_locale),
    );
    diff(
        &mut fields,
        "startDate",
        json!(old.start_date),
        json!(new.start_date),
    );
    diff(
        &mut fields,
        "endDate",
        json!(old.end_date),
        json!(new.end_date),
    );
    diff(
        &mut fields,
        "periodUnit",
        json!(old.period_unit),
        json!(new.period_unit),
    );
    diff(
        &mut fields,
        "numberOfUnits",
        json!(old.number_of_units),
        json!(new.number_of_units),
    );
    diff(
        &mut fields,
        "defaultPrice",
        default_price_value(old.default_price.as_ref()),
        default_price_value(new.default_price.as_ref()),
    );
    let prices = |entry: &PromotionEntry| -> Value {
        entry
            .prices
            .iter()
            .flatten()
            .map(|p| (p.country.clone(), price_text(p.price.as_ref())))
            .collect::<Map<String, Value>>()
            .into()
    };
    let (old_prices, new_prices) = (prices(old), prices(new));
    if old_prices != new_prices
        && let (Value::Object(old_map), Value::Object(new_map)) = (&old_prices, &new_prices)
    {
        for (country, new_price) in new_map {
            let old_price = old_map.get(country).cloned().unwrap_or(Value::Null);
            if old_price != *new_price {
                fields.push(json!({
                    "field": "prices", "country": country, "old": old_price, "new": new_price,
                }));
            }
        }
    }
    let languages = |entry: &PromotionEntry| -> BTreeMap<String, (String, String)> {
        entry
            .languages
            .iter()
            .flatten()
            .map(|l| (l.locale.clone(), (l.title.clone(), l.desc.clone())))
            .collect()
    };
    let old_languages = languages(old);
    for (locale, new_text) in languages(new) {
        let old_text = old_languages.get(&locale);
        if old_text != Some(&new_text) {
            fields.push(json!({
                "field": "languages",
                "locale": locale,
                "old": old_text.map(|(title, desc)| json!({"title": title, "desc": desc})),
                "new": {"title": new_text.0, "desc": new_text.1},
            }));
        }
    }
    diff(
        &mut fields,
        "promotionType",
        json!(old.promotion_type),
        json!(new.promotion_type),
    );
    diff(
        &mut fields,
        "offerId",
        json!(old.offer_id),
        json!(new.offer_id),
    );
    fields
}

fn build_plan(file: &ProductsFile, remote: &Remote, force: bool) -> Result<Plan, ToolError> {
    let mut errors = Vec::new();
    let mut plan = Plan::default();

    for group in &file.subscription_groups {
        let Some(id) = &group.group_id else { continue };
        match remote.groups.get(id) {
            None => errors.push(format!(
                "subscription group {id} does not exist in AppGallery Connect"
            )),
            Some(current) if current.group_name == group.group_name => plan.unchanged_groups += 1,
            Some(current) => {
                plan.changes.push(Change {
                    action: "update_group",
                    key: key("groupId", id),
                    level: AccessLevel::Write,
                    fields: vec![json!({
                        "field": "groupName", "old": current.group_name, "new": group.group_name,
                    })],
                    label: format!("subscription group {id} name"),
                });
                let mut body = Map::new();
                body.insert(
                    "resource".into(),
                    json!({"groupId": id, "groupName": group.group_name}),
                );
                plan.steps.push(Step::Single {
                    operation: GROUP_UPDATE,
                    body,
                    label: format!("updated subscription group {id}"),
                });
            }
        }
    }

    let mut imports = Vec::new();
    let mut activate = Vec::new();
    let mut deactivate = Vec::new();
    for product in &file.products {
        let no = &product.product_no;
        if let Some(group) = &product.group_id
            && !remote.groups.contains_key(group)
        {
            errors.push(format!(
                "product {no}: subscription group {group} does not exist in AppGallery Connect"
            ));
        }
        let Some(summary) = remote.products.get(no) else {
            // Нового товара в AppGallery Connect нет: создаётся пакетным импортом сразу в
            // нужном состоянии.
            let prices: Vec<&CountryPrice> = product.prices.iter().flatten().collect();
            let item = import_item(product, &product.status, &prices, false);
            let mut fields = vec![json!({"field": "status", "new": product.status})];
            if let Some(price) = &product.default_price {
                fields.push(json!({"field": "defaultPrice", "new": price.describe_normalized()}));
            }
            if !prices.is_empty() {
                fields.push(json!({"field": "prices", "countries": prices.len()}));
            }
            plan.changes.push(Change {
                action: "create_product",
                key: key("productNo", no),
                level: AccessLevel::Destructive,
                fields,
                label: format!("create product {no}"),
            });
            imports.push((no.clone(), item, format!("created product {no}")));
            continue;
        };
        let Some(current) = remote.details.get(no) else {
            errors.push(format!(
                "product {no} is listed by AppGallery Connect but its details were not read"
            ));
            continue;
        };
        let found = product_diff(&mut errors, &mut plan.kept, product, current);
        let mut unchanged = true;
        if !found.fields.is_empty() {
            unchanged = false;
            let prices: Vec<&CountryPrice> = found.changed_prices.iter().collect();
            let item = import_item(
                &found.merged,
                &summary.status,
                &prices,
                force && current.is_subscription() && found.level == AccessLevel::Destructive,
            );
            let what = if found.level == AccessLevel::Destructive {
                "prices"
            } else {
                "texts"
            };
            plan.changes.push(Change {
                action: "update_product",
                key: key("productNo", no),
                level: found.level,
                fields: found.fields,
                label: format!("update product {no} ({what})"),
            });
            imports.push((no.clone(), item, format!("updated product {no}")));
        }
        if product.status != summary.status {
            unchanged = false;
            let (action, list, done) = if product.status == "active" {
                ("activate_product", &mut activate, "activated")
            } else {
                ("deactivate_product", &mut deactivate, "deactivated")
            };
            plan.changes.push(Change {
                action,
                key: key("productNo", no),
                level: AccessLevel::Destructive,
                fields: vec![json!({
                    "field": "status", "old": summary.status, "new": product.status,
                })],
                label: format!("{} product {no}", action.trim_end_matches("_product")),
            });
            list.push((no.clone(), json!(no), format!("{done} product {no}")));
        }
        if unchanged {
            plan.unchanged_products += 1;
        }
    }
    if !imports.is_empty() {
        plan.steps.push(Step::Batch {
            operation: PRODUCT_IMPORT,
            field: "products",
            key: "productNo",
            items: imports,
        });
    }
    for (operation, items) in [
        (PRODUCT_DEACTIVATE, deactivate),
        (PRODUCT_ACTIVATE, activate),
    ] {
        if !items.is_empty() {
            plan.steps.push(Step::Batch {
                operation,
                field: "products",
                key: "productNo",
                items,
            });
        }
    }

    let local_products: BTreeMap<&str, &ProductEntry> = file
        .products
        .iter()
        .map(|p| (p.product_no.as_str(), p))
        .collect();
    let mut release = Vec::new();
    let mut remove = Vec::new();
    for promotion in &file.promotions {
        let label = promotion.label();
        let product = local_products
            .get(promotion.product_no.as_str())
            .copied()
            .or_else(|| remote.details.get(&promotion.product_no));
        let Some(product) = product else {
            errors.push(format!(
                "{label}: product {} is neither in the file nor in AppGallery Connect",
                promotion.product_no
            ));
            continue;
        };
        let Some(id) = &promotion.promotion_id else {
            check_promotion_product(&mut errors, promotion, product);
            plan.changes.push(Change {
                action: "create_promotion",
                key: key("productNo", &promotion.product_no),
                level: AccessLevel::Destructive,
                fields: vec![json!({
                    "field": "promotionTitle", "new": promotion.promotion_title,
                })],
                label: format!("create {label}"),
            });
            let mut body = Map::new();
            body.insert("resource".into(), promotion_resource(promotion, "active"));
            plan.steps.push(Step::Single {
                operation: PROMOTION_CREATE,
                body,
                label: format!("created {label}"),
            });
            continue;
        };
        let Some(current) = remote.promotion_details.get(id) else {
            errors.push(format!(
                "{label} does not exist in AppGallery Connect (or was deleted); remove \
                 promotionId to create a new promotion"
            ));
            continue;
        };
        for (field, new, old) in [
            ("productNo", &promotion.product_no, &current.product_no),
            ("strategy", &promotion.strategy, &current.strategy),
        ] {
            if new != old {
                errors.push(format!(
                    "{label}: {field} cannot be changed (AppGallery Connect has `{old}`, the \
                     file has `{new}`)"
                ));
            }
        }
        let merged = merge_promotion(promotion, current);
        check_promotion_product(&mut errors, &merged, product);
        let fields = promotion_fields(current, &merged);
        let mut unchanged = true;
        if !fields.is_empty() {
            unchanged = false;
            plan.changes.push(Change {
                action: "update_promotion",
                key: key("promotionId", id),
                level: AccessLevel::Destructive,
                fields,
                label: format!("update {label}"),
            });
            let mut body = Map::new();
            body.insert(
                "resource".into(),
                promotion_resource(&merged, current.sale_status()),
            );
            plan.steps.push(Step::Single {
                operation: PROMOTION_UPDATE,
                body,
                label: format!("updated {label}"),
            });
        }
        if promotion.sale_status() != current.sale_status() {
            unchanged = false;
            let (action, list, done) = if promotion.sale_status() == "active" {
                ("release_promotion", &mut release, "released")
            } else {
                ("remove_promotion", &mut remove, "removed")
            };
            plan.changes.push(Change {
                action,
                key: key("promotionId", id),
                level: AccessLevel::Destructive,
                fields: vec![json!({
                    "field": "promotionStatus",
                    "old": current.sale_status(),
                    "new": promotion.sale_status(),
                })],
                label: format!("{} {label}", action.trim_end_matches("_promotion")),
            });
            list.push((id.clone(), json!(id), format!("{done} {label}")));
        }
        if unchanged {
            plan.unchanged_promotions += 1;
        }
    }
    for (operation, items) in [(PROMOTION_RELEASE, release), (PROMOTION_REMOVE, remove)] {
        if !items.is_empty() {
            plan.steps.push(Step::Batch {
                operation,
                field: "promotions",
                key: "promotionId",
                items,
            });
        }
    }

    if !errors.is_empty() {
        return Err(ToolError::new(format!(
            "the products file does not match AppGallery Connect; nothing was sent:\n- {}",
            errors.join("\n- ")
        )));
    }
    Ok(plan)
}

/// Читает и проверяет файл (до запросов к API).
fn read_file(ctx: &Context, path: &str) -> Result<ProductsFile, ToolError> {
    let canonical = ctx.files.resolve_read(path)?;
    let size = std::fs::metadata(&canonical)
        .map_err(|e| ToolError::new(format!("cannot read `{path}`: {e}")))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(ToolError::new(format!(
            "`{path}` is {size} bytes; a products file is at most {MAX_FILE_BYTES} bytes"
        )));
    }
    let bytes = std::fs::read(&canonical)
        .map_err(|e| ToolError::new(format!("cannot read `{path}`: {e}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::new(format!("`{path}` is not UTF-8 text")))?;
    let file = products::parse_file(&text)?;
    let errors = products::check_file(&file);
    if !errors.is_empty() {
        return Err(ToolError::new(format!(
            "the products file does not pass the AppGallery Connect checks; nothing was \
             sent:\n- {}",
            errors.join("\n- ")
        )));
    }
    Ok(file)
}

/// `appId`: аргумент, иначе из файла, иначе единственное приложение профиля. Должен
/// совпадать с `appId` файла.
fn resolve_app_id(ctx: &Context, args: &Value, file: &ProductsFile) -> Result<String, ToolError> {
    let explicit = match args.get("appId") {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(_) => true,
    };
    let app_id = match (&file.app_id, explicit) {
        (Some(id), false) => id.clone(),
        _ => listing::app_id(ctx, args)?,
    };
    if let Some(file_app) = &file.app_id
        && *file_app != app_id
    {
        return Err(ToolError::new(format!(
            "the products file belongs to app {file_app}, not to app {app_id}; nothing was sent. \
             Export the products of app {app_id} with products_export, or fix appId in the file"
        )));
    }
    Ok(app_id)
}

/// Подписи первых записей и число остальных.
fn listed(keys: Vec<String>) -> Value {
    let total = keys.len();
    let mut shown: Vec<String> = keys.into_iter().take(MAX_LISTED).collect();
    if total > MAX_LISTED {
        shown.push(format!("... and {} more", total - MAX_LISTED));
    }
    json!(shown)
}

async fn apply(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let path = required_str(&args, "file")?;
    let dry_run = bool_arg(&args, "dry_run")?.unwrap_or(true);
    let force = bool_arg(&args, "force_existing_subscribers")?.unwrap_or(false);
    let country = country_arg(&args)?;
    if !dry_run && !ctx.access.allows(AccessLevel::Write) {
        return Err(ToolError::new(
            "applying the products file needs a server started with --allow-write; dry_run \
             works without",
        ));
    }
    let file = read_file(ctx, path)?;
    let app_id = resolve_app_id(ctx, &args, &file)?;

    let mut wanted_products: BTreeSet<String> =
        file.products.iter().map(|p| p.product_no.clone()).collect();
    wanted_products.extend(file.promotions.iter().map(|p| p.product_no.clone()));
    let wanted_promotions: BTreeSet<String> = file
        .promotions
        .iter()
        .filter_map(|p| p.promotion_id.clone())
        .collect();
    let remote = products::fetch_remote(
        ctx,
        &app_id,
        &country,
        Scope {
            products: Some(&wanted_products),
            promotions: Some(&wanted_promotions),
            include_finished: true,
        },
    )
    .await?;
    let plan = build_plan(&file, &remote, force)?;

    let destructive: Vec<&Change> = plan
        .changes
        .iter()
        .filter(|c| c.level == AccessLevel::Destructive)
        .collect();
    let can_destroy = ctx.access.allows(AccessLevel::Destructive);
    if !dry_run && !destructive.is_empty() && !can_destroy {
        let list: Vec<&str> = destructive.iter().map(|c| c.label.as_str()).collect();
        return Err(ToolError::new(format!(
            "the products file needs changes of the destructive level (they affect what users \
             pay or what they can buy), and this server was not started with \
             --allow-destructive; nothing was sent. Destructive changes:\n- {}\n\nAsk the user \
             to restart the server with --allow-destructive, or remove these changes from the \
             file to apply the rest (names and descriptions)",
            list.join("\n- ")
        )));
    }

    let mut result = Map::new();
    result.insert("appId".into(), json!(app_id));
    result.insert("dry_run".into(), json!(dry_run));
    result.insert(
        "changes".into(),
        Value::Array(plan.changes.iter().map(Change::describe).collect()),
    );
    result.insert(
        "unchanged".into(),
        json!({
            "subscriptionGroups": plan.unchanged_groups,
            "products": plan.unchanged_products,
            "promotions": plan.unchanged_promotions,
        }),
    );
    let file_groups: BTreeSet<&str> = file
        .subscription_groups
        .iter()
        .filter_map(|g| g.group_id.as_deref())
        .collect();
    let file_products: BTreeSet<&str> = file
        .products
        .iter()
        .map(|p| p.product_no.as_str())
        .collect();
    let mut not_in_file = Map::new();
    let groups: Vec<String> = remote
        .groups
        .keys()
        .filter(|id| !file_groups.contains(id.as_str()))
        .cloned()
        .collect();
    let products_left: Vec<String> = remote
        .products
        .keys()
        .filter(|no| !file_products.contains(no.as_str()))
        .cloned()
        .collect();
    let promotions_left: Vec<String> = remote
        .promotions
        .iter()
        .filter(|(id, summary)| summary.state != Some(2) && !wanted_promotions.contains(*id))
        .map(|(id, _)| id.clone())
        .collect();
    for (name, keys) in [
        ("subscriptionGroups", groups),
        ("products", products_left),
        ("promotions", promotions_left),
    ] {
        if !keys.is_empty() {
            not_in_file.insert(name.into(), listed(keys));
        }
    }
    if !not_in_file.is_empty() {
        result.insert("not_in_file".into(), Value::Object(not_in_file));
    }

    let mut notes = Vec::new();
    if !destructive.is_empty() && !can_destroy {
        notes.push(format!(
            "{} change(s) are of the destructive level and need a server started with \
             --allow-destructive; applying this plan is refused on this server",
            destructive.len()
        ));
    }
    if !plan.kept.is_empty() {
        notes.push(format!(
            "languages and country prices that are in AppGallery Connect but not in the file \
             stay as they are: {}",
            listed(plan.kept.clone())
        ));
    }
    if plan.changes.iter().any(|c| c.action == "update_product") {
        notes.push(
            "a product update also sends the current status, default price and languages of \
             the product, because the PMS API needs them; only the listed changes differ"
                .into(),
        );
    }
    if !notes.is_empty() {
        result.insert("notes".into(), json!(notes));
    }

    if plan.changes.is_empty() {
        result.insert(
            "next_step".into(),
            json!("The products in AppGallery Connect already match the file; nothing to apply."),
        );
        return Ok(Value::Object(result));
    }
    if dry_run {
        result.insert(
            "next_step".into(),
            json!("Show the changes to the user; call products_apply again with dry_run false to apply them."),
        );
        return Ok(Value::Object(result));
    }

    let mut done: Vec<String> = Vec::new();
    for step in &plan.steps {
        if let Err(err) = execute(ctx, &app_id, step, &mut done).await {
            let applied = if done.is_empty() {
                "Nothing was applied before the failure.".to_owned()
            } else {
                format!("Already applied: {}.", done.join(", "))
            };
            return Err(ToolError::new(format!(
                "{err}\n\n{applied} Call products_apply again with dry_run true to see what is \
                 left."
            )));
        }
    }
    result.insert("applied".into(), json!(done));
    result.insert(
        "next_step".into(),
        json!("The changes are applied in AppGallery Connect; export again with products_export to check them."),
    );
    Ok(Value::Object(result))
}

/// Запрос записи PMS API; `Err` — текст для агента.
async fn write(
    ctx: &Context,
    operation: &str,
    app_id: &str,
    body: Map<String, Value>,
) -> Result<Value, String> {
    let op = listing::operation(operation).map_err(|err| err.message)?;
    let response = ctx
        .api
        .call(op, &pms_args(app_id, body))
        .await
        .map_err(|err| describe_api_error(&err))?;
    Ok(response.body.json().cloned().unwrap_or(Value::Null))
}

/// Отказы пакетной операции: `ключ: причина`. Пусто — все записи приняты.
fn batch_failures(ctx: &Context, response: &Value, key: &str) -> Vec<(String, String)> {
    let mut failures: Vec<(String, String)> = response
        .get("resultInfo")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            let code = item.get("errorCode").and_then(Value::as_i64);
            let reason = item
                .get("errorReason")
                .and_then(Value::as_str)
                .unwrap_or("");
            code.is_some_and(|c| c != 0) || code.is_none() && !reason.trim().is_empty()
        })
        .map(|item| {
            let id = item
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned();
            let reason = item
                .get("errorReason")
                .and_then(Value::as_str)
                .map(|r| ctx.api.scrub(r.trim()))
                .unwrap_or_else(|| "no reason given".into());
            (id, reason)
        })
        .collect();
    let failed = response
        .get("failedNumber")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if failures.is_empty() && failed > 0 {
        failures.push((
            "?".into(),
            format!("{failed} record(s) failed, no details given"),
        ));
    }
    failures
}

async fn execute(
    ctx: &Context,
    app_id: &str,
    step: &Step,
    done: &mut Vec<String>,
) -> Result<(), String> {
    match step {
        Step::Single {
            operation,
            body,
            label,
        } => {
            write(ctx, operation, app_id, body.clone())
                .await
                .map_err(|err| format!("{} failed: {err}", label_action(label)))?;
            done.push(label.clone());
        }
        Step::Batch {
            operation,
            field,
            key,
            items,
        } => {
            for chunk in items.chunks(BATCH) {
                let mut body = Map::new();
                body.insert(
                    (*field).into(),
                    Value::Array(chunk.iter().map(|(_, item, _)| item.clone()).collect()),
                );
                let response = write(ctx, operation, app_id, body).await.map_err(|err| {
                    let keys: Vec<&str> = chunk.iter().map(|(k, ..)| k.as_str()).collect();
                    format!("{operation} for {} failed: {err}", keys.join(", "))
                })?;
                let failures = batch_failures(ctx, &response, key);
                let failed: BTreeSet<&str> = failures.iter().map(|(k, _)| k.as_str()).collect();
                let unknown = failed.contains("?");
                for (k, _, label) in chunk {
                    if !unknown && !failed.contains(k.as_str()) {
                        done.push(label.clone());
                    }
                }
                if !failures.is_empty() {
                    let list: Vec<String> = failures
                        .iter()
                        .map(|(k, reason)| format!("{k}: {reason}"))
                        .collect();
                    return Err(format!(
                        "{operation} rejected some records: {}",
                        list.join("; ")
                    ));
                }
            }
        }
    }
    Ok(())
}

/// «updated product X» → «updating product X» для текста ошибки.
fn label_action(label: &str) -> String {
    for (done, doing) in [
        ("updated ", "updating "),
        ("created ", "creating "),
        ("released ", "releasing "),
        ("removed ", "removing "),
    ] {
        if let Some(rest) = label.strip_prefix(done) {
            return format!("{doing}{rest}");
        }
    }
    label.to_owned()
}
