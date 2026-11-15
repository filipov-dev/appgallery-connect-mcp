//! Общее для инструментов выпуска (`release_submit`, `release_manage`, `release_status`) и
//! `app_overview`: чтение сведений о приложении и списка пакетов, состояние версии и
//! поэтапного выпуска, время и доля в форматах Publishing API.
//!
//! Время Publishing API — `yyyy-MM-ddTHH:mm:ssZZ` (`2015-01-01T01:01:01+0800`). Агент
//! передаёт время в RFC 3339 со смещением (`2026-12-01T10:00:00Z`, `+03:00` или `+0300`);
//! без смещения время отклоняется: часовой пояс угадывать нельзя. В запрос время уходит в
//! UTC (`+0000`).
//!
//! Доля поэтапного выпуска — строка с двумя знаками после точки, без знака процента
//! (`"25.00"`).
//!
//! Внутренние чтения (сведения о приложении целиком, со всеми языками) агенту не отдаются,
//! поэтому читаются с большим пределом размера ([`FILTER_INPUT_LIMIT`]), а в ответ
//! инструмента попадают только нужные поля. Ссылка на скачивание пакета (`packagePath`
//! списка пакетов) в ответ не попадает.

use appgallery_connect_api::ApiRequest;
use serde_json::{Map, Value, json};

use crate::composite::Context;
use crate::composite::reviews_summary::{
    DAY_MS, civil_from_days, format_time, int, operation, parse_date, uint,
};
use crate::error::{ToolError, describe_api_error};
use crate::jq::FILTER_INPUT_LIMIT;

/// Сведения о приложении.
pub(crate) const APP_INFO: &str = "publishing.appInfo.get";
/// Список пакетов приложения.
pub(crate) const PACKAGE_LIST: &str = "publishing.package.list";
/// Отправка на проверку.
pub(crate) const SUBMIT: &str = "publishing.app.submit";
/// Изменение поэтапного выпуска.
pub(crate) const PHASED_UPDATE: &str = "publishing.phasedRelease.update";
/// Изменение времени выпуска.
pub(crate) const RELEASE_TIME_UPDATE: &str = "publishing.releaseTime.update";

/// `releaseType` полного выпуска и поэтапного.
pub(crate) const RELEASE_TYPE_FULL: u8 = 1;
pub(crate) const RELEASE_TYPE_PHASED: u8 = 3;

/// `releaseState` «выпускается»: версия одобрена и ждёт времени выпуска.
pub(crate) const STATE_RELEASING: i64 = 3;

/// Сколько пакетов читается из списка (максимум API).
const PACKAGE_PAGE: u64 = 100;
/// Длина отзыва проверки в ответе, символов.
pub(crate) const MAX_OPINION: usize = 500;

/// Чтение операции таблицы для внутренних нужд инструмента. `Err` — текст для агента (уже
/// без секретов, через [`describe_api_error`]); вызывающий решает, ошибка это или пометка.
pub(crate) async fn read(ctx: &Context, operation_id: &str, args: &Value) -> Result<Value, String> {
    let op = operation(operation_id).map_err(|err| err.message)?;
    let request = ApiRequest::for_operation(op, args).map_err(|err| describe_api_error(&err))?;
    let response = ctx
        .api
        .execute_with_limit(request, FILTER_INPUT_LIMIT)
        .await
        .map_err(|err| describe_api_error(&err))?;
    Ok(response.body.json().cloned().unwrap_or(Value::Null))
}

/// Сведения о приложении (`get_app_info`) для полного (`None`) или поэтапного выпуска.
pub(crate) async fn app_info(
    ctx: &Context,
    app_id: &str,
    release_type: Option<u8>,
) -> Result<Value, String> {
    let mut args = Map::new();
    args.insert("appId".into(), json!(app_id));
    if let Some(kind) = release_type {
        args.insert("releaseType".into(), json!(kind));
    }
    read(ctx, APP_INFO, &Value::Object(args)).await
}

/// Название `releaseState` версии.
pub(crate) fn release_state_name(code: i64) -> &'static str {
    match code {
        0 => "released",
        1 => "release_rejected",
        2 => "removed",
        3 => "releasing",
        4 => "in_review",
        5 => "update_in_review",
        6 => "removal_requested",
        7 => "draft",
        8 => "update_rejected",
        9 => "removal_rejected",
        10 => "removed_by_developer",
        11 => "release_canceled",
        12 => "in_pre_review",
        13 => "pre_review_failed",
        _ => "unknown",
    }
}

/// Версия на проверке: отправлять снова нельзя.
pub(crate) fn in_review(code: i64) -> bool {
    matches!(code, 4 | 5 | 12)
}

/// Версия из `appInfo`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Version {
    pub(crate) state: Option<i64>,
    pub(crate) number: Option<String>,
    pub(crate) code: Option<i64>,
    pub(crate) id: Option<String>,
    pub(crate) updated: Option<String>,
    pub(crate) released_number: Option<String>,
    pub(crate) released_code: Option<i64>,
    pub(crate) released_id: Option<String>,
}

impl Version {
    pub(crate) fn from_body(body: &Value) -> Self {
        let info = body.get("appInfo").unwrap_or(&Value::Null);
        Self {
            state: int(info.get("releaseState")),
            number: text(info.get("versionNumber")),
            code: int(info.get("versionCode")),
            id: text(info.get("versionId")),
            updated: text(info.get("updateTime")),
            released_number: text(info.get("onShelfVersionNumber")),
            released_code: int(info.get("onShelfVersionCode")),
            released_id: text(info.get("onShelfVersionId")),
        }
    }

    /// У приложения есть выпущенная версия (поэтапный выпуск без неё невозможен).
    pub(crate) fn has_released(&self) -> bool {
        self.released_number.is_some() || self.released_code.is_some() || self.released_id.is_some()
    }

    pub(crate) fn state_json(&self) -> Value {
        match self.state {
            Some(code) => json!({"code": code, "name": release_state_name(code)}),
            None => Value::Null,
        }
    }

    /// Версия, которая готовится или проверяется.
    pub(crate) fn json(&self) -> Value {
        let mut out = Map::new();
        out.insert("versionNumber".into(), json!(self.number));
        out.insert("versionCode".into(), json!(self.code));
        if let Some(id) = &self.id {
            out.insert("versionId".into(), json!(id));
        }
        out.insert("state".into(), self.state_json());
        if let Some(updated) = &self.updated {
            out.insert("updated".into(), json!(updated));
        }
        Value::Object(out)
    }

    /// Выпущенная версия или `null`.
    pub(crate) fn released_json(&self) -> Value {
        if !self.has_released() {
            return Value::Null;
        }
        let mut out = Map::new();
        out.insert("versionNumber".into(), json!(self.released_number));
        out.insert("versionCode".into(), json!(self.released_code));
        if let Some(id) = &self.released_id {
            out.insert("versionId".into(), json!(id));
        }
        Value::Object(out)
    }
}

/// Отзыв проверки (`auditInfo.auditOpinion`), обрезанный.
pub(crate) fn review_opinion(body: &Value) -> Option<String> {
    let opinion = text(body.pointer("/auditInfo/auditOpinion"))?;
    Some(shorten(&opinion, MAX_OPINION))
}

/// Состояние поэтапного выпуска (`phasedReleaseInfo`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Phased {
    /// `RELEASE`, `SUSPEND`, `CANCEL` или `DRAFT` (в верхнем регистре).
    pub(crate) state: Option<String>,
    pub(crate) percent: Option<f64>,
    pub(crate) start: Option<String>,
    pub(crate) end: Option<String>,
    pub(crate) description: Option<String>,
}

impl Phased {
    /// Из тела ответа `get_app_info`; `None` — сведений о поэтапном выпуске нет.
    pub(crate) fn from_body(body: &Value) -> Option<Self> {
        let info = body.get("phasedReleaseInfo")?.as_object()?;
        let phased = Self {
            state: text(info.get("state")).map(|state| state.to_ascii_uppercase()),
            percent: text(info.get("phasedReleasePercent"))
                .and_then(|value| value.trim_end_matches('%').trim().parse::<f64>().ok())
                .filter(|value| value.is_finite()),
            start: text(info.get("phasedReleaseStartTime")),
            end: text(info.get("phasedReleaseEndTime")),
            description: text(info.get("phasedReleaseDescription")),
        };
        (phased != Self::default()).then_some(phased)
    }

    /// Начало или конец поэтапного выпуска в миллисекундах, если время разбирается.
    pub(crate) fn start_ms(&self) -> Option<i64> {
        self.start.as_deref().and_then(parse_time)
    }

    pub(crate) fn json(&self) -> Value {
        let mut out = Map::new();
        out.insert("state".into(), json!(self.state));
        out.insert(
            "percent".into(),
            json!(self.percent.map(|value| (value * 100.0).round() / 100.0)),
        );
        out.insert("start".into(), json!(self.start));
        out.insert("end".into(), json!(self.end));
        if let Some(description) = &self.description {
            out.insert("description".into(), json!(shorten(description, 200)));
        }
        Value::Object(out)
    }
}

/// Пакет из списка пакетов.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Package {
    pub(crate) id: String,
    pub(crate) file_name: Option<String>,
    pub(crate) version_code: Option<i64>,
    pub(crate) version_name: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) uploaded: Option<i64>,
}

impl Package {
    pub(crate) fn json(&self) -> Value {
        let mut out = Map::new();
        out.insert("pkgId".into(), json!(self.id));
        out.insert("fileName".into(), json!(self.file_name));
        out.insert("versionCode".into(), json!(self.version_code));
        out.insert("versionName".into(), json!(self.version_name));
        if let Some(size) = self.size {
            out.insert("bytes".into(), json!(size));
        }
        if let Some(uploaded) = self.uploaded {
            out.insert("uploaded".into(), json!(timestamp_text(uploaded)));
        }
        Value::Object(out)
    }
}

/// Пакеты приложения (первая страница, до [`PACKAGE_PAGE`]) и общее число по API.
#[derive(Debug, Clone, Default)]
pub(crate) struct Packages {
    pub(crate) items: Vec<Package>,
    pub(crate) total: Option<u64>,
}

impl Packages {
    /// Пакет текущей версии: с `versionCode` версии из `appInfo` (из нескольких — самый
    /// новый), иначе самый новый по времени загрузки, иначе первый в списке.
    pub(crate) fn current(&self, version_code: Option<i64>) -> Option<&Package> {
        if let Some(code) = version_code {
            let matching: Vec<&Package> = self
                .items
                .iter()
                .filter(|item| item.version_code == Some(code))
                .collect();
            if !matching.is_empty() {
                return newest(matching);
            }
        }
        newest(self.items.iter().collect())
    }

    /// Самые новые пакеты первыми.
    pub(crate) fn newest_first(&self, limit: usize) -> Vec<&Package> {
        let mut list: Vec<&Package> = self.items.iter().collect();
        list.sort_by_key(|item| std::cmp::Reverse(item.uploaded));
        list.truncate(limit);
        list
    }
}

/// Самый новый пакет по времени загрузки, иначе первый в списке.
fn newest(list: Vec<&Package>) -> Option<&Package> {
    let first = list.first().copied();
    list.into_iter()
        .filter(|item| item.uploaded.is_some())
        .max_by_key(|item| item.uploaded)
        .or(first)
}

/// Список пакетов (`list_packages`, первая страница).
pub(crate) async fn packages(ctx: &Context, app_id: &str) -> Result<Packages, String> {
    let body = read(
        ctx,
        PACKAGE_LIST,
        &json!({"appId": app_id, "fromRecCount": 1, "maxReqCount": PACKAGE_PAGE}),
    )
    .await?;
    let items = body
        .get("pkgList")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(parse_package).collect())
        .unwrap_or_default();
    Ok(Packages {
        items,
        total: uint(body.get("totalCount")),
    })
}

fn parse_package(item: &Value) -> Option<Package> {
    Some(Package {
        id: text(item.get("pkgVersion"))?,
        file_name: text(item.get("fileName")),
        version_code: int(item.get("versionCode")),
        version_name: text(item.get("versionName")),
        size: uint(item.get("packageSize")),
        uploaded: int(item.get("uploadTime")),
    })
}

/// Время загрузки пакета: миллисекунды или секунды Unix (справочник не уточняет).
fn timestamp_text(value: i64) -> String {
    if value.abs() >= 100_000_000_000 {
        format_time(value)
    } else {
        format_time(value.saturating_mul(1000))
    }
}

/// Непустая строка (числа — текстом).
pub(crate) fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Строка не длиннее `max` символов (с многоточием).
pub(crate) fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(max.saturating_sub(1)).collect();
    short.push('…');
    short
}

/// Время RFC 3339 со смещением — миллисекунды Unix. Принимаются `YYYY-MM-DDTHH:MM[:SS[.fff]]`
/// (или с пробелом вместо `T`) и `Z`, `±HH:MM`, `±HHMM` или `±HH`.
pub(crate) fn parse_time(text: &str) -> Option<i64> {
    let text = text.trim();
    let date = text.get(..10)?;
    let day = parse_date(date)?;
    let rest = text.get(10..)?;
    let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
    let (clock, offset_minutes) = if let Some(clock) = rest.strip_suffix('Z') {
        (clock, 0)
    } else {
        let at = rest.rfind(['+', '-'])?;
        let (clock, offset) = rest.split_at(at);
        (clock, parse_offset(offset)?)
    };
    let (clock, _fraction) = match clock.split_once('.') {
        Some((clock, fraction))
            if !fraction.is_empty() && fraction.chars().all(|c| c.is_ascii_digit()) =>
        {
            (clock, fraction)
        }
        Some(_) => return None,
        None => (clock, ""),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    let number = |part: &str, max: i64| -> Option<i64> {
        if part.len() != 2 || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse::<i64>().ok().filter(|value| *value <= max)
    };
    let (hours, minutes, seconds) = match parts.as_slice() {
        [h, m] => (number(h, 23)?, number(m, 59)?, 0),
        [h, m, s] => (number(h, 23)?, number(m, 59)?, number(s, 59)?),
        _ => return None,
    };
    let local = day * DAY_MS + ((hours * 60 + minutes) * 60 + seconds) * 1000;
    Some(local - offset_minutes * 60_000)
}

/// Смещение `+HH:MM`, `+HHMM` или `+HH` — минуты.
fn parse_offset(text: &str) -> Option<i64> {
    let (sign, digits) = match text.split_at_checked(1)? {
        ("+", rest) => (1, rest),
        ("-", rest) => (-1, rest),
        _ => return None,
    };
    let digits: String = match digits.len() {
        5 if digits.as_bytes().get(2) == Some(&b':') => digits.replace(':', ""),
        4 | 2 => digits.to_owned(),
        _ => return None,
    };
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits.get(..2)?.parse().ok()?;
    let minutes: i64 = digits
        .get(2..)
        .filter(|m| !m.is_empty())
        .map_or(Some(0), |m| m.parse().ok())?;
    if hours > 14 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

/// Время Unix в миллисекундах — формат Publishing API в UTC: `2026-12-01T10:00:00+0000`.
pub(crate) fn api_time(ms: i64) -> String {
    let days = ms.div_euclid(DAY_MS);
    let secs = ms.rem_euclid(DAY_MS) / 1000;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+0000",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// Время из аргумента: `None`, если не передано; ошибка — неверный формат или не в будущем.
pub(crate) fn future_time(args: &Value, name: &str, now: i64) -> Result<Option<i64>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let ms = parse_time(value).ok_or_else(|| {
                ToolError::new(format!(
                    "invalid `{name}` `{value}`: expected a date and time with a UTC offset, \
                     like 2026-12-01T10:00:00Z or 2026-12-01T13:00:00+03:00"
                ))
            })?;
            if ms <= now {
                return Err(ToolError::new(format!(
                    "`{name}` {value} is not in the future (now is {})",
                    format_time(now)
                )));
            }
            Ok(Some(ms))
        }
        Some(_) => Err(ToolError::new(format!("`{name}` must be a string"))),
    }
}

/// Доля поэтапного выпуска из аргумента: больше 0 и меньше 100, не больше двух знаков после
/// точки.
pub(crate) fn percent_arg(args: &Value, name: &str) -> Result<Option<f64>, ToolError> {
    let value = match args.get(name) {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let invalid = || {
        ToolError::new(format!(
            "`{name}` must be a number greater than 0 and less than 100 with at most two \
             decimal places, like 10 or 12.5 (the share of users in percent); 100 is a release \
             to all users"
        ))
    };
    let percent = value.as_f64().ok_or_else(invalid)?;
    if !(percent > 0.0 && percent < 100.0) {
        return Err(invalid());
    }
    let hundredths = percent * 100.0;
    if (hundredths - hundredths.round()).abs() > 1e-6 {
        return Err(invalid());
    }
    Ok(Some(hundredths.round() / 100.0))
}

/// Доля для Publishing API: `"12.50"`.
pub(crate) fn percent_text(percent: f64) -> String {
    format!("{percent:.2}")
}

/// Непустая строка аргумента в пределах длины.
pub(crate) fn text_arg(
    args: &Value,
    name: &str,
    min: usize,
    max: usize,
) -> Result<Option<String>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            let length = value.chars().count();
            if length < min.max(1) || length > max {
                return Err(ToolError::new(format!(
                    "`{name}` must be {}-{max} characters long",
                    min.max(1)
                )));
            }
            Ok(Some(value.to_owned()))
        }
        Some(_) => Err(ToolError::new(format!("`{name}` must be a string"))),
    }
}

/// Запрос изменения для агента: инструмент операции, аргументы строки запроса и тело.
pub(crate) fn describe_request(tool: &str, args: &Value) -> Value {
    let mut query = args.as_object().cloned().unwrap_or_default();
    let body = query.remove("body");
    let mut out = Map::new();
    out.insert("tool".into(), json!(tool));
    out.insert("query".into(), Value::Object(query));
    if let Some(body) = body {
        out.insert("body".into(), body);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests;
