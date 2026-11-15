//! `reviews_summary`: сводка отзывов одного приложения за произвольный период за один
//! вызов.
//!
//! Список отзывов Comments API (`comments.review.list`) принимает период не длиннее шести
//! месяцев и отдаёт до 100 отзывов на страницу (`page`/`limit`, признак `hasNext`). Поэтому
//! период режется на окна по [`WINDOW_DAYS`] дней (от новых к старым), в каждом окне
//! обходятся страницы, всего — не больше [`MAX_PAGES`] страниц. Отзывы на стыке окон
//! отбрасываются по `reviewId`.
//!
//! Фильтры, которые есть у API (`countries`, `ratings`, `langs`, `devReplyStates`),
//! передаются в запрос. Оценка, страна и наличие ответа дополнительно проверяются по полям
//! отзыва (`rating`, `countryCode`, `devReplyContent`): справочник местами неточен в именах
//! параметров, и если API фильтр не применил, сводка всё равно верна, а пометка `notes`
//! говорит, сколько отзывов отброшено. Языка в отзыве нет, его фильтрует только API.
//!
//! Средняя оценка — из статистики оценок API (`comments.rating.list`, поле `score` по
//! странам), если она пришла; иначе — по собранным отзывам с пометкой источника. Сбой
//! статистики оценок не ошибка инструмента: сводка строится по отзывам с пометкой. Сбой
//! чтения отзывов на любой странице — ошибка инструмента с числом уже собранных отзывов:
//! неполную сводку без явной пометки агент принял бы за полную.
//!
//! Отметки времени Comments API — миллисекунды «с 1970-01-01 08:00:00» по справочнику;
//! это начало эпохи Unix по пекинскому времени, то есть обычное время Unix в UTC.
//!
//! В ответ попадает только нужное для сводки: идентификатор отзыва (для `reply_review`),
//! оценка, дата, страна, версия приложения, текст и ответ разработчика (обрезанные). Модель
//! устройства и любые другие поля отзыва отбрасываются; тексты отзывов в логи не пишутся.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use appgallery_connect_api::spec::{OperationDef, operations};
use appgallery_connect_api::{AccessLevel, ApiError};
use serde_json::{Map, Value, json};

use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};

/// Имя инструмента.
pub const NAME: &str = "reviews_summary";

/// Список отзывов.
const REVIEWS_LIST: &str = "comments.review.list";
/// Оценки и общая статистика оценок.
const RATINGS_LIST: &str = "comments.rating.list";

/// Миллисекунд в сутках.
pub(crate) const DAY_MS: i64 = 86_400_000;
/// Длина окна запроса в днях: API принимает период не длиннее шести месяцев, а 180 дней
/// короче любых шести календарных месяцев.
const WINDOW_DAYS: i64 = 180;
/// Размер страницы: максимум API.
const PAGE_SIZE: u64 = 100;
/// Сколько страниц отзывов читается за вызов (все окна вместе).
const MAX_PAGES: usize = 50;
/// Период по умолчанию в днях.
const DEFAULT_DAYS: i64 = 30;
/// Самый длинный период в днях.
const MAX_PERIOD_DAYS: i64 = 730;
/// Отзывов в ответе по умолчанию и максимум.
const DEFAULT_MAX_REVIEWS: u64 = 50;
const MAX_REVIEWS: u64 = 500;
/// Длина текста отзыва и ответа в символах по умолчанию, минимум и максимум.
const DEFAULT_MAX_TEXT: u64 = 500;
const MIN_TEXT: u64 = 20;
const MAX_TEXT: u64 = 5000;
/// Коды `devReplyStates`: 0 — без ответа; 1 — ответил разработчик, 6 — пользователь
/// ответил на ответ разработчика, 3 — разработчик ответил снова.
const NOT_REPLIED_STATES: &str = "0";
const REPLIED_STATES: &str = "1,3,6";

const DESCRIPTION: &str = "Summary of the user reviews of one app for any period in a single \
call. Reads every page of `list_reviews`, splitting periods longer than six months into the \
windows the API accepts, and returns: the rating distribution of the matching reviews \
(`byRating`), the average rating (`averageRating`: from the AppGallery Connect rating \
statistics of `list_ratings` when available, otherwise computed from the collected reviews; \
`averageRatingSource` says which), the number of reviews without a developer reply \
(`unanswered`), and the newest reviews (`reviews`, at most `maxReviews`, texts cut to \
`maxTextLength` characters) with the review IDs that `get_review` and `reply_review` take. \
`countries` is required by the API, and one call can cover only countries served by the same \
AppGallery Connect site. Filters: `ratings`, `languages` (applied by AppGallery Connect only) \
and `replied`. The period is `days` (default 30, up to now or `to`) or `from`/`to` dates, at \
most two years. `truncated: true` means more reviews matched than are listed; `complete: \
false` means the scan stopped at the page limit, so the counts cover only the newest reviews: \
narrow the period or the filters then. `notes` explain anything else that is incomplete. \
Reviews contain data of real users: use them only for the task at hand; author data is not \
returned. Replies to reviews are public on AppGallery: this tool never replies; call \
`reply_review` (destructive access) only when the user asks for a reply and has approved its \
text.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "reviews",
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
                "description": "Country or region codes of the reviews, like [\"RU\", \"BY\"]. \
                    Required by the API; all of them must be served by the same AppGallery \
                    Connect site."
            },
            "days": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_PERIOD_DAYS,
                "description": "Length of the period in days, ending at the end of `to` or \
                    now. Default 30. Not combined with `from`."
            },
            "from": {
                "type": "string",
                "description": "First day of the period, YYYY-MM-DD (UTC), inclusive."
            },
            "to": {
                "type": "string",
                "description": "Last day of the period, YYYY-MM-DD (UTC), inclusive. Default: \
                    now."
            },
            "ratings": {
                "type": "array",
                "items": {"type": "integer", "minimum": 1, "maximum": 5},
                "minItems": 1,
                "maxItems": 5,
                "description": "Only reviews with these star ratings, like [1, 2]."
            },
            "languages": {
                "type": "array",
                "items": {"type": "string", "minLength": 2, "maxLength": 16},
                "minItems": 1,
                "maxItems": 50,
                "description": "Only reviews in these languages: a language and a country \
                    code joined by an underscore, like en_US or ru_RU. Filtered by \
                    AppGallery Connect only: reviews do not carry their language."
            },
            "replied": {
                "type": "boolean",
                "description": "true: only reviews with a developer reply; false: only \
                    reviews without one. Default: all reviews."
            },
            "maxReviews": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_REVIEWS,
                "description": "How many of the newest matching reviews to list. Default 50; \
                    0 returns only the summary."
            },
            "maxTextLength": {
                "type": "integer",
                "minimum": MIN_TEXT,
                "maximum": MAX_TEXT,
                "description": "Review and reply texts longer than this many characters are \
                    cut. Default 500."
            }
        },
        "required": ["countries"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(reviews_summary(ctx, args))
}

/// Разобранные аргументы.
#[derive(Debug, Clone, PartialEq)]
struct Request {
    app_id: String,
    countries: Vec<String>,
    ratings: Vec<u8>,
    languages: Vec<String>,
    replied: Option<bool>,
    /// Начало и конец периода, миллисекунды Unix, включительно.
    begin: i64,
    end: i64,
    max_reviews: usize,
    max_text: usize,
}

/// Отзыв: только поля, нужные сводке.
#[derive(Debug, Clone)]
struct Review {
    id: Option<String>,
    rating: Option<u8>,
    time: Option<i64>,
    country: Option<String>,
    version: Option<String>,
    text: String,
    reply: Option<String>,
}

/// Итог обхода страниц.
#[derive(Debug, Default)]
struct Scan {
    reviews: Vec<Review>,
    /// Отзывы, которые API вернул, но они не подошли под фильтры.
    dropped: usize,
    /// Обход остановлен лимитом страниц.
    stopped: bool,
    /// Сумма `total` первых страниц окон, если API сообщил его во всех окнах.
    reported_total: Option<u64>,
    windows_read: usize,
}

async fn reviews_summary(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let request = parse_request(ctx, &args, now_ms()?)?;
    let windows = windows(request.begin, request.end);
    tracing::debug!(
        tool = NAME,
        app_id = %request.app_id,
        windows = windows.len(),
        "сводка отзывов"
    );
    let scan = scan_reviews(ctx, &request, &windows).await?;
    let ratings = api_ratings(
        ctx,
        &request.app_id,
        &request.countries,
        windows.first().copied(),
    )
    .await;
    tracing::debug!(
        tool = NAME,
        matched = scan.reviews.len(),
        dropped = scan.dropped,
        stopped = scan.stopped,
        "отзывы собраны"
    );
    Ok(summary(&request, windows.len(), scan, ratings))
}

/// Разбор и проверка аргументов; `now` — текущее время, миллисекунды Unix.
fn parse_request(ctx: &Context, args: &Value, now: i64) -> Result<Request, ToolError> {
    let app_id = app_id(ctx, args.get("appId"))?;
    let countries = string_list(args, "countries")?
        .into_iter()
        .map(|code| {
            let code = code.trim().to_ascii_uppercase();
            if code.len() >= 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
                Ok(code)
            } else {
                Err(ToolError::new(format!(
                    "invalid country code `{code}`: expected a code like RU or CN"
                )))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if countries.is_empty() {
        return Err(ToolError::new(
            "`countries` is required: the API lists reviews only for given country or region \
             codes, like [\"RU\"]",
        ));
    }
    let languages = string_list(args, "languages")?
        .into_iter()
        .map(|lang| {
            let lang = lang.trim().replace('-', "_");
            if !lang.is_empty() && lang.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                Ok(lang)
            } else {
                Err(ToolError::new(format!(
                    "invalid language `{lang}`: expected a code like en_US"
                )))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut ratings: Vec<u8> = Vec::new();
    for value in args
        .get("ratings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let rating = value
            .as_u64()
            .filter(|r| (1..=5).contains(r))
            .and_then(|r| u8::try_from(r).ok())
            .ok_or_else(|| ToolError::new("`ratings` must be integers from 1 to 5"))?;
        if !ratings.contains(&rating) {
            ratings.push(rating);
        }
    }
    ratings.sort_unstable();
    let replied = args.get("replied").and_then(Value::as_bool);
    let (begin, end) = period(args, now)?;
    let max_reviews = bounded(args, "maxReviews", DEFAULT_MAX_REVIEWS, 0, MAX_REVIEWS)?;
    let max_text = bounded(args, "maxTextLength", DEFAULT_MAX_TEXT, MIN_TEXT, MAX_TEXT)?;
    Ok(Request {
        app_id,
        countries,
        ratings,
        languages,
        replied,
        begin,
        end,
        max_reviews,
        max_text,
    })
}

/// `appId` из аргумента или единственное приложение по умолчанию из профиля (общее и для
/// других составных инструментов).
pub(crate) fn app_id(ctx: &Context, value: Option<&Value>) -> Result<String, ToolError> {
    let explicit = match value {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.trim().is_empty() => None,
        Some(Value::String(text)) => Some(text.trim().to_owned()),
        Some(Value::Number(number)) => Some(number.to_string()),
        Some(_) => return Err(ToolError::new("`appId` must be a string or an integer")),
    };
    let id = match explicit {
        Some(id) => id,
        None => match ctx.profile.apps.as_slice() {
            [only] => only.trim().to_owned(),
            [] => {
                return Err(ToolError::new(
                    "`appId` is required: the profile has no default app. Pass the numeric \
                     app ID; `list_app_ids` finds it by package name",
                ));
            }
            several => {
                let list: Vec<String> = several.iter().map(|id| format!("`{id}`")).collect();
                return Err(ToolError::new(format!(
                    "`appId` is required: the profile has several default apps ({}); pass the \
                     one the user means",
                    list.join(", ")
                )));
            }
        },
    };
    if id.is_empty() || id.len() > 32 || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(ToolError::new(format!(
            "invalid appId `{id}`: expected a numeric app ID like 100000001, not a package name; \
             `list_app_ids` returns the app ID for a package name"
        )));
    }
    Ok(id)
}

/// Список строк аргумента (схема уже проверена).
fn string_list(args: &Value, name: &str) -> Result<Vec<String>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| ToolError::new(format!("`{name}` must be a list of strings")))
            })
            .collect(),
        Some(_) => Err(ToolError::new(format!(
            "`{name}` must be a list of strings"
        ))),
    }
}

/// Целый аргумент в границах со значением по умолчанию.
fn bounded(args: &Value, name: &str, default: u64, min: u64, max: u64) -> Result<usize, ToolError> {
    let value = match args.get(name) {
        None | Some(Value::Null) => default,
        Some(value) => value
            .as_u64()
            .filter(|v| (min..=max).contains(v))
            .ok_or_else(|| {
                ToolError::new(format!("`{name}` must be an integer from {min} to {max}"))
            })?,
    };
    usize::try_from(value).map_err(|_| ToolError::new(format!("`{name}` is too large")))
}

/// Период: начало и конец в миллисекундах Unix, включительно.
fn period(args: &Value, now: i64) -> Result<(i64, i64), ToolError> {
    let date_arg = |name: &str| -> Result<Option<i64>, ToolError> {
        match args.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => parse_date(text.trim()).map(Some).ok_or_else(|| {
                ToolError::new(format!(
                    "invalid `{name}` `{text}`: expected a date like 2026-01-31"
                ))
            }),
            Some(_) => Err(ToolError::new(format!("`{name}` must be a date string"))),
        }
    };
    let from = date_arg("from")?;
    let to = date_arg("to")?;
    let days = match args.get("days") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .filter(|d| (1..=MAX_PERIOD_DAYS).contains(d))
                .ok_or_else(|| {
                    ToolError::new(format!(
                        "`days` must be an integer from 1 to {MAX_PERIOD_DAYS}"
                    ))
                })?,
        ),
    };
    let end = match to {
        Some(day) => (day + 1) * DAY_MS - 1,
        None => now,
    };
    let begin = match (from, days) {
        (Some(_), Some(_)) => {
            return Err(ToolError::new(
                "pass either `from` or `days`, not both: `days` counts back from `to` or now",
            ));
        }
        (Some(day), None) => day * DAY_MS,
        (None, days) => end + 1 - days.unwrap_or(DEFAULT_DAYS) * DAY_MS,
    };
    if begin > end {
        return Err(ToolError::new(
            "the period is empty: `from` must not be later than `to`",
        ));
    }
    if end - begin >= MAX_PERIOD_DAYS * DAY_MS {
        return Err(ToolError::new(format!(
            "the period is longer than {MAX_PERIOD_DAYS} days; choose a shorter one"
        )));
    }
    Ok((begin, end))
}

/// Окна запроса от новых к старым: не длиннее [`WINDOW_DAYS`], без пересечений.
fn windows(begin: i64, end: i64) -> Vec<(i64, i64)> {
    let mut windows = Vec::new();
    let mut high = end;
    while high >= begin {
        let low = (high - WINDOW_DAYS * DAY_MS + 1).max(begin);
        windows.push((low, high));
        if low == begin {
            break;
        }
        high = low - 1;
    }
    windows
}

/// Операция из таблицы спецификации по `operationId` (общее и для других составных
/// инструментов).
pub(crate) fn operation(operation_id: &str) -> Result<&'static OperationDef, ToolError> {
    operations()
        .iter()
        .find(|op| op.operation_id == operation_id)
        .ok_or_else(|| {
            ToolError::new(format!(
                "operation `{operation_id}` is missing from the API table; this is a server bug"
            ))
        })
}

/// Обход окон и страниц списка отзывов.
async fn scan_reviews(
    ctx: &Context,
    request: &Request,
    windows: &[(i64, i64)],
) -> Result<Scan, ToolError> {
    let op = operation(REVIEWS_LIST)?;
    let mut scan = Scan {
        reported_total: Some(0),
        ..Scan::default()
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut pages = 0usize;
    'windows: for &(low, high) in windows {
        let mut page = 1u64;
        loop {
            if pages == MAX_PAGES {
                scan.stopped = true;
                break 'windows;
            }
            pages += 1;
            let args = review_args(request, low, high, page);
            let response = ctx
                .api
                .call(op, &args)
                .await
                .map_err(|err| scan_error(&err, page, low, high, scan.reviews.len()))?;
            let data = response
                .body
                .json()
                .and_then(|body| body.get("data"))
                .cloned()
                .unwrap_or(Value::Null);
            let list = data
                .get("reviewList")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if page == 1 {
                scan.reported_total = match (scan.reported_total, uint(data.get("total"))) {
                    (Some(sum), Some(total)) => Some(sum + total),
                    _ => None,
                };
            }
            for item in &list {
                let review = parse_review(item);
                if let Some(id) = &review.id
                    && !seen.insert(id.clone())
                {
                    continue;
                }
                if matches(request, &review) {
                    scan.reviews.push(review);
                } else {
                    scan.dropped += 1;
                }
            }
            let more = match data.get("hasNext") {
                Some(Value::Bool(flag)) => *flag,
                Some(value) => uint(Some(value)) == Some(1),
                None => list.len() as u64 >= PAGE_SIZE,
            };
            if !more || list.is_empty() {
                break;
            }
            page += 1;
        }
        scan.windows_read += 1;
    }
    Ok(scan)
}

/// Аргументы `list_reviews` для окна и страницы (имена — как в схеме инструмента).
fn review_args(request: &Request, low: i64, high: i64, page: u64) -> Value {
    let mut args = Map::new();
    args.insert("appId".into(), json!(request.app_id));
    args.insert("beginTime".into(), json!(low));
    args.insert("endTime".into(), json!(high));
    args.insert("countries".into(), json!(request.countries.join(",")));
    args.insert("sort".into(), json!(0));
    args.insert("page".into(), json!(page));
    args.insert("limit".into(), json!(PAGE_SIZE));
    if !request.ratings.is_empty() {
        let list: Vec<String> = request.ratings.iter().map(u8::to_string).collect();
        args.insert("ratings".into(), json!(list.join(",")));
    }
    if !request.languages.is_empty() {
        args.insert("langs".into(), json!(request.languages.join(",")));
    }
    match request.replied {
        Some(true) => {
            args.insert("devReplyStates".into(), json!(REPLIED_STATES));
        }
        Some(false) => {
            args.insert("devReplyStates".into(), json!(NOT_REPLIED_STATES));
        }
        None => {}
    }
    Value::Object(args)
}

/// Ошибка чтения отзывов: что собрано до сбоя и ошибка API.
fn scan_error(err: &ApiError, page: u64, low: i64, high: i64, collected: usize) -> ToolError {
    ToolError::new(format!(
        "reading reviews failed on page {page} of the window {} – {} after {collected} matching \
         review(s) had been collected, so no summary was returned (it would be incomplete). \
         Retry later, or narrow the period or the filters. Error: {}",
        format_time(low),
        format_time(high),
        describe_api_error(err)
    ))
}

/// Отзыв из ответа API: только нужные поля.
fn parse_review(item: &Value) -> Review {
    let text = |name: &str| {
        item.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let id = match item.get("reviewId") {
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    };
    Review {
        id,
        rating: uint(item.get("rating"))
            .filter(|r| (1..=5).contains(r))
            .and_then(|r| u8::try_from(r).ok()),
        time: int(item.get("dateline")),
        country: text("countryCode").map(|code| code.to_ascii_uppercase()),
        version: text("apkVersion"),
        text: text("content").unwrap_or_default(),
        reply: text("devReplyContent"),
    }
}

/// Подходит ли отзыв под фильтры, которые можно проверить по его полям.
fn matches(request: &Request, review: &Review) -> bool {
    if !request.ratings.is_empty() && !review.rating.is_some_and(|r| request.ratings.contains(&r)) {
        return false;
    }
    if let Some(country) = &review.country
        && !request.countries.contains(country)
    {
        return false;
    }
    if let Some(replied) = request.replied
        && replied != review.reply.is_some()
    {
        return false;
    }
    true
}

/// Статистика оценок API (её же показывает `app_overview`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApiRatings {
    pub(crate) average: f64,
    pub(crate) total: u64,
    by_rating: Option<[u64; 5]>,
    by_country: Vec<Value>,
}

/// Статистика оценок из `comments.rating.list` за окно (у сводки — последнее). `Err` —
/// пояснение для `notes`: сводка тогда строится по отзывам.
pub(crate) async fn api_ratings(
    ctx: &Context,
    app_id: &str,
    countries: &[String],
    window: Option<(i64, i64)>,
) -> Result<ApiRatings, String> {
    let (low, high) = window.ok_or_else(|| "the period is empty".to_owned())?;
    let op = operation(RATINGS_LIST).map_err(|err| err.message)?;
    let args = json!({
        "appId": app_id,
        "beginTime": low,
        "endTime": high,
        "countries": countries.join(","),
        "page": 1,
        "limit": 1,
    });
    let response = ctx.api.call(op, &args).await.map_err(|err| {
        format!(
            "the rating statistics of AppGallery Connect (`list_ratings`) are not available: \
             {err}"
        )
    })?;
    let scores = response
        .body
        .json()
        .and_then(|body| body.pointer("/data/score"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    parse_scores(&scores).ok_or_else(|| {
        "AppGallery Connect returned no rating statistics for these countries".to_owned()
    })
}

/// Общая оценка по странам: средняя, взвешенная по числу оценок.
fn parse_scores(scores: &[Value]) -> Option<ApiRatings> {
    const STARS: [&str; 5] = [
        "oneStarTimes",
        "twoStarTimes",
        "threeStarTimes",
        "fourStarTimes",
        "fiveStarTimes",
    ];
    let mut weighted = 0.0;
    let mut total = 0u64;
    let mut plain: Vec<f64> = Vec::new();
    let mut by_rating = [0u64; 5];
    let mut stars_known = false;
    let mut by_country = Vec::new();
    for score in scores {
        let Some(average) = float(score.get("averageScore")) else {
            continue;
        };
        let count = uint(score.get("totalRatings"));
        if let Some(count) = count {
            weighted += average * count as f64;
            total += count;
        }
        plain.push(average);
        for (index, field) in STARS.iter().enumerate() {
            if let (Some(slot), Some(value)) = (by_rating.get_mut(index), uint(score.get(*field))) {
                *slot += value;
                stars_known = true;
            }
        }
        let mut entry = Map::new();
        if let Some(country) = score.get("country").and_then(Value::as_str) {
            entry.insert("country".into(), json!(country));
        }
        entry.insert("averageRating".into(), json!(round2(average)));
        if let Some(count) = count {
            entry.insert("totalRatings".into(), json!(count));
        }
        by_country.push(Value::Object(entry));
    }
    let average = if total > 0 {
        weighted / total as f64
    } else if !plain.is_empty() {
        plain.iter().sum::<f64>() / plain.len() as f64
    } else {
        return None;
    };
    Some(ApiRatings {
        average: round2(average),
        total,
        by_rating: stars_known.then_some(by_rating),
        by_country,
    })
}

/// Ответ инструмента.
fn summary(
    request: &Request,
    window_count: usize,
    scan: Scan,
    ratings: Result<ApiRatings, String>,
) -> Value {
    let mut notes: Vec<String> = Vec::new();
    let mut reviews = scan.reviews;
    // Новые сначала: окна идут от новых к старым, но порядок внутри страницы не обещан.
    reviews.sort_by_key(|review| std::cmp::Reverse(review.time));

    let mut by_rating = [0u64; 5];
    for rating in reviews.iter().filter_map(|review| review.rating) {
        if let Some(slot) = usize::from(rating)
            .checked_sub(1)
            .and_then(|index| by_rating.get_mut(index))
        {
            *slot += 1;
        }
    }
    let rated: u64 = by_rating.iter().sum();
    let sample_average = (rated > 0).then(|| {
        let sum: u64 = by_rating
            .iter()
            .zip(1u64..)
            .map(|(count, star)| count * star)
            .sum();
        round2(sum as f64 / rated as f64)
    });
    let unanswered = reviews
        .iter()
        .filter(|review| review.reply.is_none())
        .count();

    let mut result = Map::new();
    result.insert("appId".into(), json!(request.app_id));
    result.insert(
        "period".into(),
        json!({
            "from": format_time(request.begin),
            "to": format_time(request.end),
            "windows": window_count,
        }),
    );
    let mut filters = Map::new();
    filters.insert("countries".into(), json!(request.countries));
    if !request.ratings.is_empty() {
        filters.insert("ratings".into(), json!(request.ratings));
    }
    if !request.languages.is_empty() {
        filters.insert("languages".into(), json!(request.languages));
    }
    if let Some(replied) = request.replied {
        filters.insert("replied".into(), json!(replied));
    }
    result.insert("filters".into(), Value::Object(filters));
    result.insert("reviewCount".into(), json!(reviews.len()));
    if let Some(total) = scan
        .reported_total
        .filter(|_| scan.windows_read == window_count)
    {
        result.insert("reportedTotal".into(), json!(total));
    }
    result.insert("byRating".into(), stars_object(&by_rating));
    match ratings {
        Ok(api) => {
            result.insert("averageRating".into(), json!(api.average));
            result.insert("averageRatingSource".into(), json!("api"));
            let mut stats = Map::new();
            stats.insert("averageRating".into(), json!(api.average));
            stats.insert("totalRatings".into(), json!(api.total));
            if let Some(stars) = api.by_rating {
                stats.insert("byRating".into(), stars_object(&stars));
            }
            stats.insert("byCountry".into(), Value::Array(api.by_country));
            result.insert("apiRatings".into(), Value::Object(stats));
            if !request.ratings.is_empty() {
                notes.push(
                    "averageRating and apiRatings are the AppGallery Connect rating statistics \
                     of all ratings in these countries; the `ratings` filter applies only to \
                     the reviews (see sampleAverageRating)"
                        .into(),
                );
            }
        }
        Err(note) => {
            result.insert("averageRating".into(), json!(sample_average));
            result.insert("averageRatingSource".into(), json!("sample"));
            notes.push(format!(
                "{note}; averageRating is computed from the collected reviews"
            ));
        }
    }
    result.insert("sampleAverageRating".into(), json!(sample_average));
    result.insert("unanswered".into(), json!(unanswered));

    let truncated = reviews.len() > request.max_reviews;
    let listed: Vec<Value> = reviews
        .iter()
        .take(request.max_reviews)
        .map(|review| review_json(review, request.max_text))
        .collect();
    result.insert("reviews".into(), Value::Array(listed));
    result.insert("truncated".into(), json!(truncated));
    result.insert("complete".into(), json!(!scan.stopped));
    if scan.stopped {
        notes.push(format!(
            "the scan stopped after {MAX_PAGES} pages ({} reviews); the counts cover only the \
             newest reviews of the period: narrow the period or the filters for complete numbers",
            MAX_PAGES as u64 * PAGE_SIZE
        ));
    }
    if scan.dropped > 0 {
        notes.push(format!(
            "{} review(s) returned by AppGallery Connect did not match the rating, country or \
             reply filter and were left out",
            scan.dropped
        ));
    }
    if !notes.is_empty() {
        result.insert("notes".into(), json!(notes));
    }
    Value::Object(result)
}

/// Распределение оценок `{"1": …, …, "5": …}`.
fn stars_object(counts: &[u64; 5]) -> Value {
    let mut object = Map::new();
    for (star, count) in (1..=5).zip(counts) {
        object.insert(star.to_string(), json!(count));
    }
    Value::Object(object)
}

/// Отзыв в ответе: обрезанные тексты, без лишних полей.
fn review_json(review: &Review, max_text: usize) -> Value {
    let mut object = Map::new();
    if let Some(id) = &review.id {
        object.insert("reviewId".into(), json!(id));
    }
    object.insert("rating".into(), json!(review.rating));
    if let Some(time) = review.time {
        object.insert("date".into(), json!(format_time(time)));
    }
    if let Some(country) = &review.country {
        object.insert("country".into(), json!(country));
    }
    if let Some(version) = &review.version {
        object.insert("version".into(), json!(version));
    }
    let (text, cut) = shorten(&review.text, max_text);
    object.insert("text".into(), json!(text));
    if cut {
        object.insert("textTruncated".into(), json!(true));
    }
    object.insert("replied".into(), json!(review.reply.is_some()));
    if let Some(reply) = &review.reply {
        let (reply, cut) = shorten(reply, max_text);
        object.insert("developerReply".into(), json!(reply));
        if cut {
            object.insert("replyTruncated".into(), json!(true));
        }
    }
    Value::Object(object)
}

/// Текст не длиннее `max` символов; обрезанный заканчивается многоточием.
fn shorten(text: &str, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text.to_owned(), false);
    }
    let mut short: String = text.chars().take(max.saturating_sub(1)).collect();
    short.truncate(short.trim_end().len());
    short.push('…');
    (short, true)
}

/// Неотрицательное целое из числа или строки.
pub(crate) fn uint(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Целое из числа или строки.
pub(crate) fn int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Число из числа или строки.
fn float(value: Option<&Value>) -> Option<f64> {
    let number: Option<f64> = match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    };
    number.filter(|f| f.is_finite())
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Текущее время, миллисекунды Unix.
pub(crate) fn now_ms() -> Result<i64, ToolError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ToolError::new("the system clock is set before 1970"))?;
    i64::try_from(elapsed.as_millis()).map_err(|_| ToolError::new("the system clock is invalid"))
}

/// Дата `YYYY-MM-DD` — номер дня от 1970-01-01.
pub(crate) fn parse_date(text: &str) -> Option<i64> {
    let mut parts = text.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    let year: i64 = year.parse().ok()?;
    let month: u32 = month.parse().ok()?;
    let day: u32 = day.parse().ok()?;
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > month_days {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// Номер дня от 1970-01-01 по григорианской дате (алгоритм Говарда Хиннанта).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Григорианская дата по номеру дня от 1970-01-01.
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Время Unix в миллисекундах — `YYYY-MM-DDTHH:MM:SSZ`.
pub(crate) fn format_time(ms: i64) -> String {
    let days = ms.div_euclid(DAY_MS);
    let secs = ms.rem_euclid(DAY_MS) / 1000;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use appgallery_connect_api::token::StaticToken;
    use appgallery_connect_api::{
        ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy,
    };
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::ProfileInfo;

    const REVIEWS: &str = "/api/reviews/v1/manage/dev/reviews";
    const RATINGS: &str = "/api/reviews/v1/manage/dev/ratings";
    const APP: &str = "100000001";

    fn context(server: &MockServer, apps: &[&str]) -> Context {
        let id = ["2718", "2818", "2845"].concat();
        let secret = ["QW12", "ER34", "TY56"].concat().repeat(4);
        let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
        let mut config = ClientConfig::new(Region::Ru, Some(credentials));
        config.base_url = Some(server.uri());
        config.retry = RetryPolicy {
            max_retries: 0,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
        };
        let token = ["reviews", "-test-", "token"].concat();
        let api = ApiClient::with_token_source(config, Arc::new(StaticToken::new(token))).unwrap();
        Context {
            api: Arc::new(api),
            files: Arc::new(crate::files::FilePolicy::disabled()),
            vault: Arc::new(crate::composite::MemoryVault::default()),
            profile: ProfileInfo {
                name: "work".into(),
                apps: apps.iter().map(|app| (*app).to_owned()).collect(),
                packages: Vec::new(),
            },
            access: AccessLevel::Read,
        }
    }

    fn ok(data: Value) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({"ret": {"rtnCode": 0, "rtnDesc": "success"}, "data": data}))
    }

    fn review(id: &str, rating: u64, time: i64) -> Value {
        json!({
            "reviewId": id,
            "rating": rating,
            "dateline": time,
            "countryCode": "RU",
            "apkVersion": "1.2.0",
            "phoneType": "Model X",
            "content": format!("text of {id}"),
            "likes": 1,
            "replies": 0,
        })
    }

    fn ms(date: &str) -> i64 {
        parse_date(date).unwrap() * DAY_MS
    }

    async fn mount_ratings(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(RATINGS))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn run_tool(ctx: &Context, args: Value) -> Result<Value, ToolError> {
        reviews_summary(ctx, args).await
    }

    #[test]
    fn dates_and_windows() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2024-02-29"), Some(19_782));
        for bad in [
            "2023-02-29",
            "2024-13-01",
            "24-01-01",
            "2024-1-01",
            "2024-01-01T00",
            "x",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
        assert_eq!(format_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_time(ms("2024-02-29") + 3_723_000),
            "2024-02-29T01:02:03Z"
        );
        for day in [0, 19_782, 20_000, 2_932_896] {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(
                days_from_civil(y, u32::try_from(m).unwrap(), u32::try_from(d).unwrap()),
                day
            );
        }

        let begin = ms("2025-01-01");
        let end = ms("2025-12-31") + DAY_MS - 1;
        let list = windows(begin, end);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].1, end);
        assert_eq!(list.last().unwrap().0, begin);
        for pair in list.windows(2) {
            assert_eq!(pair[1].1, pair[0].0 - 1);
        }
        for (low, high) in &list {
            assert!(high - low < WINDOW_DAYS * DAY_MS);
        }
        assert_eq!(windows(5, 5), vec![(5, 5)]);
    }

    #[tokio::test]
    async fn period_arguments() {
        let server = MockServer::start().await;
        let ctx = context(&server, &[APP]);
        let now = ms("2026-03-10") + 5_000;
        let parse = |args: Value| parse_request(&ctx, &args, now);

        let request = parse(json!({"countries": ["ru"]})).unwrap();
        assert_eq!(request.app_id, APP);
        assert_eq!(request.countries, vec!["RU"]);
        assert_eq!(request.end, now);
        assert_eq!(request.end - request.begin + 1, DEFAULT_DAYS * DAY_MS);
        assert_eq!(request.max_reviews, 50);

        let request =
            parse(json!({"countries": ["RU"], "from": "2026-01-01", "to": "2026-01-31"})).unwrap();
        assert_eq!(request.begin, ms("2026-01-01"));
        assert_eq!(request.end, ms("2026-02-01") - 1);

        let request = parse(json!({"countries": ["RU"], "to": "2026-01-31", "days": 7})).unwrap();
        assert_eq!(request.begin, ms("2026-01-25"));

        for (args, text) in [
            (
                json!({"countries": ["RU"], "from": "2026-01-01", "days": 3}),
                "not both",
            ),
            (
                json!({"countries": ["RU"], "from": "2026-02-01", "to": "2026-01-01"}),
                "empty",
            ),
            (
                json!({"countries": ["RU"], "from": "2020-01-01"}),
                "longer than",
            ),
            (
                json!({"countries": ["RU"], "from": "01.01.2026"}),
                "expected a date",
            ),
            (json!({"countries": ["R,U"]}), "invalid country"),
            (
                json!({"countries": ["RU"], "languages": ["en US"]}),
                "invalid language",
            ),
            (
                json!({"countries": ["RU"], "appId": "com.example.app"}),
                "numeric app ID",
            ),
        ] {
            let err = parse(args).unwrap_err();
            assert!(err.message().contains(text), "{err}");
        }
    }

    #[tokio::test]
    async fn default_app_comes_from_the_profile() {
        let server = MockServer::start().await;
        let args = json!({"countries": ["RU"]});
        let err = parse_request(&context(&server, &[]), &args, 0).unwrap_err();
        assert!(err.message().contains("no default app"), "{err}");
        assert!(err.message().contains("list_app_ids"), "{err}");

        let err = parse_request(&context(&server, &["1", "2"]), &args, 0).unwrap_err();
        assert!(
            err.message().contains("several default apps (`1`, `2`)"),
            "{err}"
        );

        let explicit = json!({"countries": ["RU"], "appId": 100000002});
        let request = parse_request(&context(&server, &["1", "2"]), &explicit, 0).unwrap();
        assert_eq!(request.app_id, "100000002");

        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .and(query_param("appId", APP))
            .respond_with(ok(
                json!({"reviewList": [review("r1", 5, 1)], "hasNext": 0}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        mount_ratings(&server, ok(json!({"score": []}))).await;
        let out = run_tool(&context(&server, &[APP]), json!({"countries": ["RU"]}))
            .await
            .unwrap();
        assert_eq!(out["appId"], APP);
        assert_eq!(out["reviewCount"], 1);
    }

    #[tokio::test]
    async fn pages_and_windows_are_walked() {
        let server = MockServer::start().await;
        // 200 дней: два окна, новое — с двумя страницами.
        let begin = ms("2025-01-01");
        let end = ms("2025-07-20") - 1;
        let list = windows(begin, end);
        assert_eq!(list.len(), 2);
        let (new_low, new_high) = list[0];
        let (old_low, old_high) = list[1];
        let page = |low: i64, number: &str| {
            Mock::given(method("GET"))
                .and(path(REVIEWS))
                .and(query_param("beginTime", low.to_string()))
                .and(query_param("page", number))
                .and(query_param("limit", "100"))
                .and(query_param("countries", "RU"))
        };
        page(new_low, "1")
            .and(query_param("endTime", new_high.to_string()))
            .respond_with(ok(json!({
                "reviewList": [review("a", 5, new_high - 10), review("b", 4, new_high - 20)],
                "hasNext": 1,
                "total": 3,
            })))
            .expect(1)
            .mount(&server)
            .await;
        page(new_low, "2")
            .respond_with(ok(
                json!({"reviewList": [review("c", 1, new_low + 10)], "hasNext": 0}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        page(old_low, "1")
            .and(query_param("endTime", old_high.to_string()))
            .respond_with(ok(json!({
                // Повтор отзыва на стыке окон отбрасывается.
                "reviewList": [review("c", 1, new_low + 10), review("d", 2, old_low + 10)],
                "hasNext": 0,
                "total": 2,
            })))
            .expect(1)
            .mount(&server)
            .await;
        mount_ratings(&server, ok(json!({"score": []}))).await;

        let ctx = context(&server, &[]);
        let out = run_tool(
            &ctx,
            json!({"appId": APP, "countries": ["RU"], "from": "2025-01-01", "to": "2025-07-19"}),
        )
        .await
        .unwrap();
        assert_eq!(out["reviewCount"], 4);
        assert_eq!(out["reportedTotal"], 5);
        assert_eq!(out["period"]["windows"], 2);
        assert_eq!(out["period"]["from"], "2025-01-01T00:00:00Z");
        assert_eq!(out["period"]["to"], "2025-07-19T23:59:59Z");
        assert_eq!(
            out["byRating"],
            json!({"1": 1, "2": 1, "3": 0, "4": 1, "5": 1})
        );
        assert_eq!(out["sampleAverageRating"], json!(3.0));
        assert_eq!(out["averageRatingSource"], "sample");
        assert_eq!(out["unanswered"], 4);
        assert_eq!(out["complete"], true);
        assert_eq!(out["truncated"], false);
        let ids: Vec<&str> = out["reviews"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["reviewId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        let first = &out["reviews"][0];
        assert_eq!(first["country"], "RU");
        assert_eq!(first["version"], "1.2.0");
        assert_eq!(first["replied"], false);
        // Лишние поля отзыва (модель устройства, лайки) в ответ не попадают.
        assert!(first.get("phoneType").is_none() && first.get("likes").is_none());
        assert!(
            out["notes"][0]
                .as_str()
                .unwrap()
                .contains("no rating statistics")
        );
    }

    #[tokio::test]
    async fn filters_go_to_the_api_and_are_checked_locally() {
        let server = MockServer::start().await;
        let mut replied = review("replied", 2, 30);
        replied["devReplyContent"] = json!("Thanks!");
        let mut foreign = review("foreign", 1, 20);
        foreign["countryCode"] = json!("DE");
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .and(query_param("countries", "RU,BY"))
            .and(query_param("ratings", "1,2"))
            .and(query_param("langs", "ru_RU,en_US"))
            .and(query_param("devReplyStates", "0"))
            .and(query_param("sort", "0"))
            .respond_with(ok(json!({
                "reviewList": [review("good", 1, 40), review("five", 5, 35), replied, foreign],
                "hasNext": 0,
            })))
            .expect(1)
            .mount(&server)
            .await;
        mount_ratings(&server, ok(json!({"score": []}))).await;
        let out = run_tool(
            &context(&server, &[APP]),
            json!({
                "countries": ["RU", "by"],
                "ratings": [2, 1, 2],
                "languages": ["ru-RU", "en_US"],
                "replied": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["reviewCount"], 1);
        assert_eq!(out["reviews"][0]["reviewId"], "good");
        assert_eq!(
            out["filters"],
            json!({"countries": ["RU", "BY"], "ratings": [1, 2],
                "languages": ["ru_RU", "en_US"], "replied": false})
        );
        let notes = out["notes"].to_string();
        assert!(notes.contains("3 review(s) returned"), "{notes}");
    }

    #[tokio::test]
    async fn replied_filter_and_ratings_from_the_api() {
        let server = MockServer::start().await;
        let mut answered = review("answered", 4, 10);
        answered["devReplyContent"] = json!("We fixed it in 1.3, thank you for the report.");
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .and(query_param("devReplyStates", "1,3,6"))
            .respond_with(ok(json!({"reviewList": [answered, review("plain", 3, 5)]})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(RATINGS))
            .and(query_param("appId", APP))
            .and(query_param("countries", "RU,BY"))
            .and(query_param("limit", "1"))
            .respond_with(ok(json!({"score": [
                {"country": "RU", "averageScore": 4.5, "totalRatings": 300,
                    "fiveStarTimes": 200, "fourStarTimes": 50, "threeStarTimes": 20,
                    "twoStarTimes": 10, "oneStarTimes": 20},
                {"country": "BY", "averageScore": "3.5", "totalRatings": "100"},
            ]})))
            .expect(1)
            .mount(&server)
            .await;
        let out = run_tool(
            &context(&server, &[APP]),
            json!({"countries": ["RU", "BY"], "replied": true, "maxTextLength": 20}),
        )
        .await
        .unwrap();
        assert_eq!(out["reviewCount"], 1);
        assert_eq!(out["unanswered"], 0);
        assert_eq!(out["averageRating"], json!(4.25));
        assert_eq!(out["averageRatingSource"], "api");
        assert_eq!(out["sampleAverageRating"], json!(4.0));
        assert_eq!(out["apiRatings"]["totalRatings"], 400);
        assert_eq!(out["apiRatings"]["byRating"]["5"], 200);
        assert_eq!(
            out["apiRatings"]["byCountry"][1]["averageRating"],
            json!(3.5)
        );
        let first = &out["reviews"][0];
        assert_eq!(first["replied"], true);
        assert_eq!(first["replyTruncated"], true);
        assert_eq!(
            first["developerReply"].as_str().unwrap().chars().count(),
            20
        );
        assert!(out.get("notes").is_none() || !out["notes"].to_string().contains("sample"));
    }

    #[tokio::test]
    async fn reviews_and_texts_are_limited() {
        let server = MockServer::start().await;
        let mut long = review("long", 2, 30);
        long["content"] = json!("Очень длинный отзыв о приложении, который не помещается");
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .respond_with(ok(json!({
                "reviewList": [long, review("mid", 3, 20), review("old", 4, 10)],
                "hasNext": 0,
            })))
            .mount(&server)
            .await;
        mount_ratings(&server, ResponseTemplate::new(500)).await;
        let out = run_tool(
            &context(&server, &[APP]),
            json!({"countries": ["RU"], "maxReviews": 2, "maxTextLength": 20}),
        )
        .await
        .unwrap();
        assert_eq!(out["reviewCount"], 3);
        assert_eq!(out["truncated"], true);
        let listed = out["reviews"].as_array().unwrap();
        assert_eq!(listed.len(), 2);
        let text = listed[0]["text"].as_str().unwrap();
        assert!(text.ends_with('…'), "{text}");
        assert!(text.chars().count() <= 20, "{text}");
        assert_eq!(listed[0]["textTruncated"], true);
        assert!(listed[1].get("textTruncated").is_none());
        // Статистика оценок недоступна: средняя — по выборке с пометкой.
        assert_eq!(out["averageRatingSource"], "sample");
        assert_eq!(out["averageRating"], json!(3.0));
        assert!(out["notes"].to_string().contains("not available"), "{out}");

        let out = run_tool(
            &context(&server, &[APP]),
            json!({"countries": ["RU"], "maxReviews": 0}),
        )
        .await
        .unwrap();
        assert_eq!(out["reviews"], json!([]));
        assert_eq!(out["truncated"], true);
    }

    #[tokio::test]
    async fn the_page_limit_marks_an_incomplete_scan() {
        let server = MockServer::start().await;
        let full: Vec<Value> = (0..100).map(|i| review(&format!("x{i}"), 5, i)).collect();
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .respond_with(ok(json!({"reviewList": full, "hasNext": 1})))
            .mount(&server)
            .await;
        mount_ratings(&server, ok(json!({"score": []}))).await;
        let out = run_tool(
            &context(&server, &[APP]),
            json!({"countries": ["RU"], "maxReviews": 1}),
        )
        .await
        .unwrap();
        assert_eq!(out["complete"], false);
        // На каждой странице те же идентификаторы: повторы отброшены.
        assert_eq!(out["reviewCount"], 100);
        assert!(out["notes"].to_string().contains("stopped after 50 pages"));
        let requests = server.received_requests().await.unwrap();
        let review_requests = requests.iter().filter(|r| r.url.path() == REVIEWS).count();
        assert_eq!(review_requests, MAX_PAGES);
    }

    #[tokio::test]
    async fn an_envelope_error_on_the_second_page_is_a_tool_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .and(query_param("page", "1"))
            .respond_with(ok(json!({
                "reviewList": [review("a", 5, 2), review("b", 1, 1)],
                "hasNext": 1,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"rtnCode": 204144660, "rtnDesc": "query failed"},
            })))
            .mount(&server)
            .await;
        mount_ratings(&server, ok(json!({"score": []}))).await;
        let err = run_tool(&context(&server, &[APP]), json!({"countries": ["RU"]}))
            .await
            .unwrap_err();
        let text = err.message();
        assert!(text.contains("page 2"), "{text}");
        assert!(text.contains("after 2 matching review(s)"), "{text}");
        assert!(text.contains("code 204144660: query failed"), "{text}");
        assert!(text.contains("no summary was returned"), "{text}");
        // Статистику оценок после сбоя не запрашиваем.
        let requests = server.received_requests().await.unwrap();
        assert!(requests.iter().all(|r| r.url.path() != RATINGS));
    }

    #[test]
    fn texts_are_cut_by_characters() {
        assert_eq!(shorten("short", 10), ("short".to_owned(), false));
        let (text, cut) = shorten("абвгдежзик", 5);
        assert!(cut);
        assert_eq!(text, "абвг…");
        let (text, _) = shorten("ab   cdefgh", 6);
        assert_eq!(text, "ab…");
    }

    #[test]
    fn tool_is_registered_as_read_only_in_reviews() {
        let tools = tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, NAME);
        assert_eq!(tools[0].toolset, "reviews");
        assert_eq!(tools[0].access, AccessLevel::Read);
        assert!(DESCRIPTION.contains("`reply_review`"));
        assert!(DESCRIPTION.contains("public"));
        let schema = input_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["countries"]));
        assert!(operation(REVIEWS_LIST).is_ok() && operation(RATINGS_LIST).is_ok());
    }
}
