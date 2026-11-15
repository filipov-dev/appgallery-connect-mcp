//! `app_overview`: краткая сводка по приложению за один вызов.
//!
//! Основное чтение — `get_app_info` (без `lang`: все языки), его сбой — ошибка инструмента.
//! В ответ идут только короткие поля: название на языке по умолчанию, коды языков, категории,
//! платность, число стран, возрастной рейтинг, версия и её состояние, выпущенная версия,
//! отзыв проверки (обрезанный), поэтапный выпуск. Тексты описаний, адреса изображений и цены
//! по странам не отдаются: для страницы приложения есть отдельные инструменты.
//!
//! Рейтинг — из статистики оценок Comments API (`comments.rating.list`, общий помощник
//! [`super::reviews_summary::api_ratings`]) за последние 180 дней. Операция вызывается из
//! кода, даже если набор `reviews` выключен (уровень — чтение, как у самого инструмента).
//! API требует страны одного сайта AppGallery Connect: `countries` из аргумента, иначе
//! `RU` для профиля региона «Россия» или единственная страна выпуска приложения; без них
//! рейтинга нет, `notes` объясняет почему. Сбой запроса рейтинга — тоже пометка, не ошибка.

use appgallery_connect_api::{AccessLevel, Region};
use serde_json::{Map, Value, json};

use crate::composite::release::{self, Phased, Version, review_opinion, shorten, text};
use crate::composite::reviews_summary::{DAY_MS, api_ratings, app_id, now_ms};
use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::ToolError;

/// Имя инструмента.
pub const NAME: &str = "app_overview";

/// Окно статистики оценок, дней (API принимает не больше шести месяцев).
const RATING_DAYS: i64 = 180;
/// Самое длинное название в ответе.
const MAX_NAME: usize = 100;

const DESCRIPTION: &str = "Short overview of one app in a single call: name in the default \
language, default language and the list of language codes, categories, free or paid, number \
of release countries, content rating, the version being prepared or reviewed with its review \
and release state, the released version, the latest review comment, the phased release state, \
and the average user rating (`rating`, from the AppGallery Connect rating statistics for \
`countries`). Read-only and compact: no descriptions, images or prices; use the listing and \
release tools for details. Parts that could not be read (for example the rating) are \
explained in `notes`.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "apps",
        access: AccessLevel::Read,
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
                "description": "Numeric app ID (like 100000001). Optional when the profile has \
                    exactly one default app."
            },
            "countries": {
                "type": "array",
                "items": {"type": "string", "minLength": 2, "maxLength": 3},
                "minItems": 1,
                "maxItems": 100,
                "description": "Countries for the rating, like [\"RU\"], all served by the same \
                    AppGallery Connect site. Default: RU for a profile of the Russia region, or \
                    the only release country of the app."
            }
        }
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(app_overview(ctx, args))
}

async fn app_overview(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let app_id = app_id(ctx, args.get("appId"))?;
    let countries = countries_arg(&args)?;
    let mut notes: Vec<String> = Vec::new();

    let body = release::app_info(ctx, &app_id, None)
        .await
        .map_err(|err| ToolError::new(format!("could not read the app (`get_app_info`): {err}")))?;
    let info = body.get("appInfo").cloned().unwrap_or(Value::Null);
    let default_lang = text(info.get("defaultLang"));
    let languages: Vec<&Value> = body
        .get("languages")
        .and_then(Value::as_array)
        .map(|list| list.iter().collect())
        .unwrap_or_default();
    let codes: Vec<String> = languages
        .iter()
        .filter_map(|lang| text(lang.get("lang")))
        .collect();
    let name = languages
        .iter()
        .find(|lang| text(lang.get("lang")) == default_lang && default_lang.is_some())
        .or_else(|| languages.first())
        .and_then(|lang| text(lang.get("appName")))
        .map(|name| shorten(&name, MAX_NAME));
    let release_countries: Vec<String> = text(info.get("publishCountry"))
        .map(|list| {
            list.split(',')
                .map(str::trim)
                .filter(|code| !code.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let version = Version::from_body(&body);

    let mut out = Map::new();
    out.insert("appId".into(), json!(app_id));
    out.insert("name".into(), json!(name));
    out.insert("defaultLanguage".into(), json!(default_lang));
    out.insert("languages".into(), json!(codes));
    out.insert(
        "category".into(),
        json!({
            "parentType": info.get("parentType"),
            "childType": info.get("childType"),
            "grandChildType": info.get("grandChildType"),
        }),
    );
    out.insert("free".into(), json!(free_flag(info.get("isFree"))));
    out.insert("releaseCountryCount".into(), json!(release_countries.len()));
    if let Some(rating) = content_rating(info.get("contentRate")) {
        out.insert("contentRating".into(), rating);
    }
    out.insert("version".into(), version.json());
    out.insert("released".into(), version.released_json());
    if let Some(opinion) = review_opinion(&body) {
        out.insert("reviewOpinion".into(), json!(opinion));
    }
    if let Some(phased) = Phased::from_body(&body) {
        out.insert("phasedRelease".into(), phased.json());
    }

    let countries = countries.or_else(|| default_countries(ctx, &release_countries));
    let rating = match countries {
        Some(countries) => {
            let now = now_ms()?;
            let window = Some((now - RATING_DAYS * DAY_MS + 1, now));
            match api_ratings(ctx, &app_id, &countries, window).await {
                Ok(ratings) => json!({
                    "average": ratings.average,
                    "total": ratings.total,
                    "countries": countries,
                    "days": RATING_DAYS,
                }),
                Err(err) => {
                    notes.push(format!("the rating is not available: {err}"));
                    Value::Null
                }
            }
        }
        None => {
            notes.push(
                "the rating needs `countries` (countries served by the same AppGallery Connect \
                 site, like [\"RU\"]); pass them to get the rating"
                    .into(),
            );
            Value::Null
        }
    };
    out.insert("rating".into(), rating);
    if !notes.is_empty() {
        out.insert("notes".into(), json!(notes));
    }
    Ok(Value::Object(out))
}

/// Страны рейтинга из аргумента (верхний регистр, без повторов).
fn countries_arg(args: &Value) -> Result<Option<Vec<String>>, ToolError> {
    let Some(list) = args.get("countries").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut countries: Vec<String> = Vec::new();
    for item in list {
        let code = item
            .as_str()
            .map(|code| code.trim().to_ascii_uppercase())
            .filter(|code| {
                (2..=3).contains(&code.len()) && code.chars().all(|c| c.is_ascii_alphabetic())
            })
            .ok_or_else(|| {
                ToolError::new(format!(
                    "invalid country code {item}: expected a code like RU"
                ))
            })?;
        if !countries.contains(&code) {
            countries.push(code);
        }
    }
    Ok((!countries.is_empty()).then_some(countries))
}

/// Страны рейтинга по умолчанию: `RU` для региона «Россия», иначе единственная страна выпуска.
fn default_countries(ctx: &Context, release_countries: &[String]) -> Option<Vec<String>> {
    if ctx.api.config().region == Region::Ru {
        return Some(vec!["RU".to_owned()]);
    }
    match release_countries {
        [only] => Some(vec![only.to_ascii_uppercase()]),
        _ => None,
    }
}

/// `isFree`: 1 — бесплатное, 0 — платное.
fn free_flag(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Number(number) => number.as_i64().map(|flag| flag == 1),
        Value::String(text) => text.trim().parse::<i64>().ok().map(|flag| flag == 1),
        Value::Bool(flag) => Some(*flag),
        _ => None,
    }
}

/// Возрастной рейтинг: `contentRate` — JSON в строке (`{"HW":"3+"}`).
fn content_rating(value: Option<&Value>) -> Option<Value> {
    let raw = text(value)?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) if map.len() <= 10 => Some(Value::Object(map)),
        _ => Some(json!(shorten(&raw, 100))),
    }
}
