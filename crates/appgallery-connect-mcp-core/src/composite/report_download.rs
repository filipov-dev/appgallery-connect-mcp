//! `report_download`: отчёт Reports API в локальный файл или краткую сводку за один вызов.
//!
//! Reports API не отдаёт данные отчёта в ответе: каждая операция возвращает временную
//! ссылку на файл (`fileURL`, CSV или Excel). Инструмент сопоставляет вид отчёта с
//! операцией, проверяет аргументы и ограничения периода из справочника до запроса, получает
//! ссылку через [`ApiClient::call`](appgallery_connect_api::ApiClient::call) (так
//! проверяется и конверт результата: ненулевой `ret.code` при HTTP 200 — ошибка) и скачивает
//! файл только с хостов Huawei ([`appgallery_connect_api::ApiClient::download_to_file`]):
//!
//! - с `save_to` — в новый файл по правилам [`crate::files::FilePolicy`] (путь проверяется
//!   до запроса, существующий файл не перезаписывается); агенту — путь, размер и тип;
//! - без `save_to` — во временный файл в собственном каталоге (права 0700 на Unix) с
//!   лимитом [`SUMMARY_MAX_BYTES`], разбирается как CSV ([`table`]) и удаляется; агенту —
//!   число строк, колонки, итоги по метрикам и первые строки. Excel без `save_to` не
//!   разбирается: для `.xlsx` нужны распаковка и XML, а это тяжёлые зависимости ради
//!   сводки; такой вызов отклоняется до запроса, а файл Excel, пришедший вместо CSV, —
//!   после скачивания с подсказкой про `save_to`.
//!
//! Поиск ссылки в ответе — [`savable_values`] по полям `[[secrets]]` операции (как у
//! `save_to` инструментов операций). Ссылка не попадает ни в ответ, ни в ошибки, ни в логи:
//! ответ собирается из отдельных полей, ошибки скачивания исполнитель пишет без адреса, в
//! логах — только вид отчёта, приложение и размер.
//!
//! Ответ с кодом 0 и пустой ссылкой — не ошибка: так AppGallery Connect, по-видимому,
//! отвечает на период без данных; агент получает `empty: true` и пояснение.

mod table;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use appgallery_connect_api::spec::{OperationDef, SecretKind};
use appgallery_connect_api::{AccessLevel, ApiError};
use serde_json::{Map, Value, json};

use crate::composite::reviews_summary::{
    DAY_MS, app_id, civil_from_days, now_ms, operation, parse_date,
};
use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};
use crate::files::{SAVE_TO_ARG, display_path};
use crate::redact::savable_values;

use table::{Metric, Totals};

/// Имя инструмента.
pub const NAME: &str = "report_download";

/// Лимит файла, который разбирается для сводки без `save_to`, байт.
pub const SUMMARY_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Период по умолчанию в днях.
const DEFAULT_DAYS: i64 = 30;
/// Первые строки в сводке по умолчанию и максимум.
const DEFAULT_MAX_ROWS: u64 = 10;
const MAX_ROWS: u64 = 100;

/// Языки отчёта по справочнику.
const LANGUAGES: &[&str] = &["en-US", "ru-RU", "zh-CN"];
/// Валюты отчёта о покупках в приложении.
const CURRENCIES: &[&str] = &["USD", "EUR", "CNY"];

/// Аргументы-фильтры инструмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    Countries,
    BusinessType,
    AppVersions,
    DeviceName,
    DownloadType,
    OrderStatus,
    OrderId,
    DateType,
}

impl Filter {
    const ALL: [Filter; 8] = [
        Filter::Countries,
        Filter::BusinessType,
        Filter::AppVersions,
        Filter::DeviceName,
        Filter::DownloadType,
        Filter::OrderStatus,
        Filter::OrderId,
        Filter::DateType,
    ];

    fn arg(self) -> &'static str {
        match self {
            Self::Countries => "countries",
            Self::BusinessType => "businessType",
            Self::AppVersions => "appVersions",
            Self::DeviceName => "deviceName",
            Self::DownloadType => "downloadType",
            Self::OrderStatus => "orderStatus",
            Self::OrderId => "orderId",
            Self::DateType => "dateType",
        }
    }
}

/// Вид отчёта: операция, ограничения и аргументы по справочнику.
#[derive(Debug)]
struct Kind {
    /// Значение аргумента `report`.
    name: &'static str,
    operation_id: &'static str,
    title: &'static str,
    /// Самый длинный период, дней включительно.
    max_days: i64,
    /// Значения `groupBy`; пустой список — группировки нет.
    group_by: &'static [&'static str],
    /// `groupBy` — список (несколько измерений).
    multi_group: bool,
    /// Есть `exportType` (CSV или Excel).
    excel: bool,
    /// Нужен `currency`.
    currency: bool,
    filters: &'static [Filter],
    totals: Totals,
}

const CURRENCY_COLUMN: Metric = Metric {
    en: "Currency",
    ru: "Валюта",
};

/// Виды отчётов. Периоды и параметры — по страницам справочника Reports API.
const KINDS: &[Kind] = &[
    Kind {
        name: "installs",
        operation_id: "reports.downloadInstall.export",
        title: "download and installation",
        max_days: 180,
        group_by: &["date", "countryId", "businessType", "appVersion"],
        multi_group: false,
        excel: true,
        currency: false,
        filters: &[Filter::Countries, Filter::BusinessType, Filter::AppVersions],
        totals: Totals {
            metrics: &[
                Metric {
                    en: "Impressions",
                    ru: "Впечатления",
                },
                Metric {
                    en: "App details page views",
                    ru: "Просмотры страницы сведений",
                },
                Metric {
                    en: "Total downloads",
                    ru: "Общее количество загрузок",
                },
                Metric {
                    en: "Update downloads",
                    ru: "Загрузки обновлений",
                },
                Metric {
                    en: "New downloads",
                    ru: "Новые загрузки",
                },
                Metric {
                    en: "Uninstalls",
                    ru: "Удаления",
                },
            ],
            currency: None,
        },
    },
    Kind {
        name: "in_app_purchases",
        operation_id: "reports.iap.export",
        title: "in-app purchases",
        max_days: 180,
        group_by: &["date", "countryId", "businessType"],
        multi_group: false,
        excel: true,
        currency: true,
        filters: &[Filter::Countries, Filter::BusinessType],
        totals: Totals {
            metrics: &[
                Metric {
                    en: "Payment amount",
                    ru: "Сумма оплаты",
                },
                Metric {
                    en: "Payment times",
                    ru: "Кол-во оплат",
                },
                Metric {
                    en: "New paying users",
                    ru: "Новые платящие пользователи",
                },
            ],
            currency: None,
        },
    },
    Kind {
        name: "paid_app",
        operation_id: "reports.paidApp.export",
        title: "paid app",
        max_days: 365,
        group_by: &[],
        multi_group: false,
        excel: true,
        currency: false,
        filters: &[Filter::Countries],
        totals: Totals {
            metrics: &[Metric {
                en: "Purchase price",
                ru: "Цена покупки",
            }],
            currency: Some(CURRENCY_COLUMN),
        },
    },
    Kind {
        name: "paid_app_details",
        operation_id: "reports.paidAppDetails.export",
        title: "paid app details",
        max_days: 180,
        group_by: &[],
        multi_group: false,
        excel: false,
        currency: false,
        filters: &[
            Filter::Countries,
            Filter::OrderStatus,
            Filter::OrderId,
            Filter::DateType,
        ],
        totals: Totals {
            metrics: &[Metric {
                en: "Purchase price",
                ru: "Цена покупки",
            }],
            currency: Some(CURRENCY_COLUMN),
        },
    },
    Kind {
        name: "install_failures",
        operation_id: "reports.installFailure.export",
        title: "installation failure",
        max_days: 180,
        group_by: &[
            "date",
            "deviceName",
            "downloadType",
            "appVersion",
            "countryId",
        ],
        multi_group: true,
        excel: true,
        currency: false,
        filters: &[
            Filter::Countries,
            Filter::AppVersions,
            Filter::DeviceName,
            Filter::DownloadType,
        ],
        totals: Totals {
            metrics: &[
                Metric {
                    en: "Devices with app installation failures",
                    ru: "Устройства со сбоями установок",
                },
                Metric {
                    en: "Installation failures",
                    ru: "Сбои установки",
                },
            ],
            currency: None,
        },
    },
];

const DESCRIPTION: &str = "Download one AppGallery Connect report of an app and either save \
it to a local file (`save_to`) or get a short summary of it, in one call. Reports (`report`): \
`installs` (impressions, page views, new and update downloads, uninstalls; up to 180 days), \
`in_app_purchases` (paying users, payment amounts in `currency`, default USD; up to 180 days), \
`paid_app` (paid app purchases; up to 365 days), `paid_app_details` (individual paid app orders \
of one country: order IDs and statuses of real users; up to 180 days; CSV only), \
`install_failures` (installation failures with result codes and causes; up to 180 days). The \
period is `days` (default 30, ending at `to` or yesterday, UTC) or `from`/`to` dates; it is \
checked before any request. `groupBy`, `format` (csv or excel) and the filters apply only to \
the reports that support them; the error says which. With `save_to` the server downloads the \
file (only from Huawei hosts) to that new local file and returns its path, size and type; \
existing files are never overwritten. Without `save_to` the report must be CSV: the server reads \
it (up to 32 MiB) and returns `summary`: `rows`, `columns`, `totals` (sums of the main metric \
columns; money per currency in `totalsByCurrency`), `firstRows` (the first `maxRows` rows as \
arrays in column order) and `truncated`; the temporary file is deleted. Use `save_to` for the \
full data or Excel. `empty: true` means AppGallery Connect has no data for this period and \
these filters. The temporary report link is never returned.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "reports",
        access: AccessLevel::Read,
        description: DESCRIPTION,
        input_schema,
        run,
    }]
}

fn input_schema() -> Value {
    let kinds: Vec<&str> = KINDS.iter().map(|kind| kind.name).collect();
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "report": {
                "type": "string",
                "enum": kinds,
                "description": "Report to download: installs, in_app_purchases, paid_app, \
                    paid_app_details or install_failures."
            },
            "appId": {
                "type": ["string", "integer"],
                "description": "Numeric app ID (like 100000001). Optional when the profile has \
                    exactly one default app."
            },
            "days": {
                "type": "integer",
                "minimum": 1,
                "maximum": 365,
                "description": "Length of the period in days, ending at `to` (inclusive) or \
                    yesterday. Default 30. Not combined with `from`. At most 180 days (365 for \
                    paid_app)."
            },
            "from": {
                "type": "string",
                "description": "First day of the period, YYYY-MM-DD (UTC), inclusive."
            },
            "to": {
                "type": "string",
                "description": "Last day of the period, YYYY-MM-DD (UTC), inclusive. Default: \
                    yesterday, the last full day."
            },
            "language": {
                "type": "string",
                "enum": LANGUAGES,
                "description": "Language of the column names: en-US (default), ru-RU or zh-CN."
            },
            "groupBy": {
                "type": ["string", "array"],
                "items": {"type": "string"},
                "maxItems": 5,
                "description": "Rows of the report: installs — date (default), countryId, \
                    businessType or appVersion; in_app_purchases — date, countryId or \
                    businessType; install_failures — one or more of date, deviceName, \
                    downloadType, appVersion, countryId. Not available for paid_app and \
                    paid_app_details."
            },
            "format": {
                "type": "string",
                "enum": ["csv", "excel"],
                "description": "File format: csv (default) or excel. Excel needs `save_to`; \
                    paid_app_details is CSV only."
            },
            "currency": {
                "type": "string",
                "enum": CURRENCIES,
                "description": "in_app_purchases only: currency of the amounts, USD (default), \
                    EUR or CNY; other currencies are converted at the exchange rate."
            },
            "countries": {
                "type": "array",
                "items": {"type": "string", "minLength": 2, "maxLength": 6},
                "minItems": 1,
                "maxItems": 250,
                "description": "Country or region codes, like [\"RU\", \"BY\"]. installs and \
                    in_app_purchases also take \"all\" (global data) and \"others\". \
                    paid_app_details requires exactly one country."
            },
            "businessType": {
                "type": "string",
                "enum": ["ALL", "AppGallery", "AppTouch"],
                "description": "installs and in_app_purchases: channel, ALL, AppGallery or \
                    AppTouch."
            },
            "appVersions": {
                "type": "array",
                "items": {"type": "string", "minLength": 1, "maxLength": 64},
                "minItems": 1,
                "maxItems": 50,
                "description": "installs and install_failures: only these app versions, like \
                    [\"1.0.0\", \"2.0.0\"]."
            },
            "deviceName": {
                "type": "string",
                "minLength": 1,
                "maxLength": 100,
                "description": "install_failures: only this device model, like P40-lite."
            },
            "downloadType": {
                "type": "string",
                "enum": ["new", "update"],
                "description": "install_failures: new downloads or update downloads only."
            },
            "orderStatus": {
                "type": "string",
                "enum": ["paid", "refunded", "payment_failed", "refund_failed", "refunding"],
                "description": "paid_app_details: only orders in this status."
            },
            "orderId": {
                "type": "string",
                "minLength": 1,
                "maxLength": 128,
                "description": "paid_app_details: only this order."
            },
            "dateType": {
                "type": "string",
                "enum": ["purchase", "refund"],
                "description": "paid_app_details: select orders by purchase date (default) or \
                    by refund date."
            },
            "maxRows": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_ROWS,
                "description": "Summary only: how many first rows to return. Default 10."
            },
            SAVE_TO_ARG: {
                "type": "string",
                "minLength": 1,
                "description": "Path of a new local file to save the report to (absolute, or \
                    relative to the server's working directory; only inside the allowed \
                    directories listed in the server instructions). The parent directory must \
                    exist; existing files are never overwritten. Without it the tool returns a \
                    summary of a CSV report."
            }
        },
        "required": ["report"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(report_download(ctx, args))
}

/// Разобранные аргументы.
#[derive(Debug, Clone, PartialEq)]
struct Request {
    kind: &'static str,
    app_id: String,
    /// Первый и последний день периода (номера дней от 1970-01-01), включительно.
    first_day: i64,
    last_day: i64,
    language: String,
    group_by: Vec<String>,
    excel: bool,
    currency: Option<String>,
    /// Пары `filterCondition` и `filterConditionValue` в порядке передачи.
    filters: Vec<(&'static str, String)>,
    query_type: Option<u8>,
    max_rows: usize,
    save_to: Option<String>,
}

impl Request {
    fn kind(&self) -> Result<&'static Kind, ToolError> {
        find_kind(self.kind)
    }
}

fn find_kind(name: &str) -> Result<&'static Kind, ToolError> {
    KINDS.iter().find(|kind| kind.name == name).ok_or_else(|| {
        let names: Vec<&str> = KINDS.iter().map(|kind| kind.name).collect();
        ToolError::new(format!(
            "unknown report `{name}`; use one of: {}",
            names.join(", ")
        ))
    })
}

async fn report_download(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let request = parse_request(ctx, &args, now_ms()? / DAY_MS)?;
    let kind = request.kind()?;
    let op = operation(kind.operation_id)?;
    // Путь проверяется до запроса: ошибка в пути не должна стоить вызова API.
    let target = match &request.save_to {
        Some(path) => Some(ctx.files.resolve_write(path)?),
        None => None,
    };
    tracing::debug!(
        tool = NAME,
        report = kind.name,
        app_id = %request.app_id,
        save_to = target.is_some(),
        "запрос ссылки на отчёт"
    );
    let response = ctx
        .api
        .call(op, &operation_args(&request))
        .await
        .map_err(|err| request_error(kind, &err))?;
    let body = response.body.json().cloned().unwrap_or(Value::Null);
    let mut result = describe(&request, kind);
    if let Some(package) = body
        .get("packName")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
    {
        result.insert("packageName".into(), json!(package.trim()));
    }
    let Some(link) = report_link(op, &body)? else {
        result.insert("empty".into(), json!(true));
        result.insert(
            "notes".into(),
            json!([format!(
                "AppGallery Connect returned no {} report file for this period and these \
                 filters (`fileURL` is empty): there is probably no data. Nothing was \
                 downloaded{}",
                kind.title,
                if target.is_some() { " or saved" } else { "" }
            )]),
        );
        return Ok(Value::Object(result));
    };
    match target {
        Some(target) => save(ctx, kind, &link, &target, result).await,
        None => summarize(ctx, kind, &request, &link, result).await,
    }
}

/// Ссылка на файл из ответа: значение секретного поля операции (`fileURL`). `None` —
/// ссылки нет (пустая строка или поля нет).
fn report_link(op: &'static OperationDef, body: &Value) -> Result<Option<String>, ToolError> {
    let found = savable_values(op, body);
    match found.as_slice() {
        [] => Ok(None),
        [(field, value)] if field.kind == SecretKind::DownloadUrl => match value.as_str() {
            Some(link) => Ok(Some(link.trim().to_owned())),
            None => Err(ToolError::new(format!(
                "`{}` in the AppGallery Connect response is not a link",
                field.path
            ))),
        },
        _ => Err(ToolError::new(
            "the AppGallery Connect response has an unexpected report file field; this is a \
             server bug",
        )),
    }
}

/// Сохранение отчёта в файл `save_to`.
async fn save(
    ctx: &Context,
    kind: &Kind,
    link: &str,
    target: &Path,
    mut result: Map<String, Value>,
) -> Result<Value, ToolError> {
    let file = ctx
        .api
        .download_to_file(link, target)
        .await
        .map_err(|err| download_error(kind, &err))?;
    let file_type = sniff(&file.path).await;
    tracing::debug!(
        tool = NAME,
        report = kind.name,
        bytes = file.bytes,
        file_type,
        "отчёт сохранён"
    );
    let mut saved = Map::new();
    saved.insert("path".into(), json!(display_path(&file.path)));
    saved.insert("bytes".into(), json!(file.bytes));
    saved.insert("fileType".into(), json!(file_type));
    if let Some(content_type) = file.content_type {
        saved.insert("content_type".into(), json!(content_type));
    }
    result.insert("saved".into(), Value::Object(saved));
    if file.bytes == 0 {
        result.insert("empty".into(), json!(true));
        result.insert(
            "notes".into(),
            json!(["the report file is empty: there is no data for this period and these filters"]),
        );
    }
    Ok(Value::Object(result))
}

/// Сводка CSV-отчёта через временный файл.
async fn summarize(
    ctx: &Context,
    kind: &'static Kind,
    request: &Request,
    link: &str,
    mut result: Map<String, Value>,
) -> Result<Value, ToolError> {
    let dir = PrivateTempDir::create()?;
    let path = dir.path().join("report");
    let file = ctx
        .api
        .transfer()
        .download_to_file_with_limit(link, &path, SUMMARY_MAX_BYTES)
        .await
        .map_err(|err| match err {
            ApiError::DownloadTooLarge { .. } => ToolError::new(format!(
                "the {} report is larger than {} MiB, too large to summarize; call again with \
                 `save_to` to save the file",
                kind.title,
                SUMMARY_MAX_BYTES / 1024 / 1024
            )),
            other => download_error(kind, &other),
        })?;
    let max_rows = request.max_rows;
    let totals = kind.totals;
    let parsed = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let bytes = std::fs::read(&path).map_err(|err| err.to_string())?;
        if let Some(format) = excel_signature(&bytes) {
            return Ok(Err(format));
        }
        Ok(Ok(table::summarize(&bytes, totals, max_rows)))
    })
    .await
    .map_err(|_| ToolError::new("reading the report failed unexpectedly"))?
    .map_err(|err| ToolError::new(format!("could not read the downloaded report: {err}")))?;
    // Временный каталог удаляется сразу, до ответа.
    drop(dir);
    let summary = match parsed {
        Ok(summary) => summary,
        Err(format) => {
            return Err(ToolError::new(format!(
                "AppGallery Connect returned the {} report as an {format} file, which this tool \
                 does not summarize; call again with `save_to` to save the file",
                kind.title
            )));
        }
    };
    let rows = summary.value.get("rows").and_then(|rows| rows.as_u64());
    tracing::debug!(
        tool = NAME,
        report = kind.name,
        bytes = file.bytes,
        rows,
        "сводка отчёта"
    );
    if summary.value.get("empty") == Some(&Value::Bool(true)) {
        result.insert("empty".into(), json!(true));
    }
    result.insert("bytes".into(), json!(file.bytes));
    result.insert("summary".into(), Value::Object(summary.value));
    if !summary.notes.is_empty() {
        result.insert("notes".into(), json!(summary.notes));
    }
    Ok(Value::Object(result))
}

/// Общая часть ответа: что запрошено.
fn describe(request: &Request, kind: &Kind) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("report".into(), json!(kind.name));
    result.insert("appId".into(), json!(request.app_id));
    result.insert(
        "period".into(),
        json!({
            "from": format_day(request.first_day),
            "to": format_day(request.last_day),
            "days": request.last_day - request.first_day + 1,
        }),
    );
    result.insert("language".into(), json!(request.language));
    result.insert(
        "format".into(),
        json!(if request.excel { "excel" } else { "csv" }),
    );
    if !request.group_by.is_empty() {
        result.insert("groupBy".into(), json!(request.group_by));
    }
    if let Some(currency) = &request.currency {
        result.insert("currency".into(), json!(currency));
    }
    if !request.filters.is_empty() {
        let mut filters = Map::new();
        for (name, value) in &request.filters {
            filters.insert((*name).to_owned(), json!(value));
        }
        result.insert("filters".into(), Value::Object(filters));
    }
    if let Some(query_type) = request.query_type {
        result.insert(
            "dateType".into(),
            json!(if query_type == 1 {
                "refund"
            } else {
                "purchase"
            }),
        );
    }
    result
}

/// Аргументы инструмента операции (имена — как в схеме операции).
fn operation_args(request: &Request) -> Value {
    let mut args = Map::new();
    args.insert("appId".into(), json!(request.app_id));
    args.insert("language".into(), json!(request.language));
    args.insert("startTime".into(), json!(api_day(request.first_day)));
    args.insert("endTime".into(), json!(api_day(request.last_day)));
    if let Some(kind) = KINDS.iter().find(|kind| kind.name == request.kind) {
        if kind.multi_group && !request.group_by.is_empty() {
            args.insert("groupBy".into(), json!(request.group_by));
        } else if let Some(group) = request.group_by.first() {
            args.insert("groupBy".into(), json!(group));
        }
        if kind.excel {
            args.insert(
                "exportType".into(),
                json!(if request.excel { "EXCEL" } else { "CSV" }),
            );
        }
    }
    if let Some(currency) = &request.currency {
        args.insert("currency".into(), json!(currency));
    }
    if let Some(query_type) = request.query_type {
        args.insert("queryType".into(), json!(query_type));
    }
    if !request.filters.is_empty() {
        let names: Vec<&str> = request.filters.iter().map(|(name, _)| *name).collect();
        let values: Vec<&str> = request
            .filters
            .iter()
            .map(|(_, value)| value.as_str())
            .collect();
        args.insert("filterCondition".into(), json!(names));
        args.insert("filterConditionValue".into(), json!(values));
    }
    Value::Object(args)
}

/// Разбор и проверка аргументов; `today` — текущий день UTC (номер дня от 1970-01-01).
fn parse_request(ctx: &Context, args: &Value, today: i64) -> Result<Request, ToolError> {
    let name = args
        .get("report")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new("`report` is required"))?;
    let kind = find_kind(name)?;
    // Аргументы, которых у вида отчёта нет: ошибка, а не молчаливый пропуск.
    for filter in Filter::ALL {
        if present(args, filter.arg()) && !kind.filters.contains(&filter) {
            return Err(not_supported(kind, filter.arg()));
        }
    }
    if present(args, "groupBy") && kind.group_by.is_empty() {
        return Err(not_supported(kind, "groupBy"));
    }
    if present(args, "currency") && !kind.currency {
        return Err(not_supported(kind, "currency"));
    }
    let save_to = match args.get(SAVE_TO_ARG) {
        None | Some(Value::Null) => None,
        Some(Value::String(path)) if !path.trim().is_empty() => Some(path.clone()),
        Some(_) => {
            return Err(ToolError::new(
                "`save_to` must be a non-empty path of a new local file",
            ));
        }
    };
    let excel = match str_arg(args, "format")? {
        None | Some("csv") => false,
        Some("excel") if kind.excel => true,
        Some("excel") => {
            return Err(ToolError::new(format!(
                "the {} report is available only as CSV; omit `format`",
                kind.title
            )));
        }
        Some(other) => {
            return Err(ToolError::new(format!(
                "invalid `format` `{other}`: use csv or excel"
            )));
        }
    };
    if excel && save_to.is_none() {
        return Err(ToolError::new(
            "an Excel report cannot be summarized: pass `save_to` to save the file, or omit \
             `format` to get a CSV summary",
        ));
    }
    if present(args, "maxRows") && save_to.is_some() {
        return Err(ToolError::new(
            "`maxRows` applies only to the summary; it cannot be combined with `save_to`",
        ));
    }

    let app_id = app_id(ctx, args.get("appId"))?;
    let (first_day, last_day) = period(args, today, kind)?;
    let language = match str_arg(args, "language")? {
        None => "en-US".to_owned(),
        Some(lang) if LANGUAGES.contains(&lang) => lang.to_owned(),
        Some(lang) => {
            return Err(ToolError::new(format!(
                "invalid `language` `{lang}`: use one of {}",
                LANGUAGES.join(", ")
            )));
        }
    };
    let group_by = group_by(args, kind)?;
    let currency = if kind.currency {
        match str_arg(args, "currency")? {
            None => Some("USD".to_owned()),
            Some(code) if CURRENCIES.contains(&code) => Some(code.to_owned()),
            Some(code) => {
                return Err(ToolError::new(format!(
                    "invalid `currency` `{code}`: use one of {}",
                    CURRENCIES.join(", ")
                )));
            }
        }
    } else {
        None
    };
    let filters = filters(args, kind)?;
    let query_type = if kind.filters.contains(&Filter::DateType) {
        match str_arg(args, "dateType")? {
            None | Some("purchase") => Some(0),
            Some("refund") => Some(1),
            Some(other) => {
                return Err(ToolError::new(format!(
                    "invalid `dateType` `{other}`: use purchase or refund"
                )));
            }
        }
    } else {
        None
    };
    let max_rows = match args.get("maxRows") {
        None | Some(Value::Null) => DEFAULT_MAX_ROWS,
        Some(value) => value
            .as_u64()
            .filter(|rows| *rows <= MAX_ROWS)
            .ok_or_else(|| {
                ToolError::new(format!("`maxRows` must be an integer from 0 to {MAX_ROWS}"))
            })?,
    };
    Ok(Request {
        kind: kind.name,
        app_id,
        first_day,
        last_day,
        language,
        group_by,
        excel,
        currency,
        filters,
        query_type,
        max_rows: usize::try_from(max_rows).unwrap_or(usize::MAX),
        save_to,
    })
}

fn present(args: &Value, name: &str) -> bool {
    args.get(name).is_some_and(|value| !value.is_null())
}

fn not_supported(kind: &Kind, arg: &str) -> ToolError {
    ToolError::new(format!(
        "`{arg}` is not available for the {} report (`{}`)",
        kind.title, kind.name
    ))
}

/// Строковый аргумент (схема уже проверена).
fn str_arg<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.trim())),
        Some(_) => Err(ToolError::new(format!("`{name}` must be a string"))),
    }
}

/// Список строк аргумента (схема уже проверена).
fn str_list(args: &Value, name: &str) -> Result<Vec<String>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|text| text.trim().to_owned())
                    .ok_or_else(|| ToolError::new(format!("`{name}` must be a list of strings")))
            })
            .collect(),
        Some(_) => Err(ToolError::new(format!(
            "`{name}` must be a list of strings"
        ))),
    }
}

/// Период: первый и последний день включительно. Ограничение длины — из справочника.
fn period(args: &Value, today: i64, kind: &Kind) -> Result<(i64, i64), ToolError> {
    let date_arg = |name: &str| -> Result<Option<i64>, ToolError> {
        match str_arg(args, name)? {
            None => Ok(None),
            Some(text) => parse_day(text).map(Some).ok_or_else(|| {
                ToolError::new(format!(
                    "invalid `{name}` `{text}`: expected a date like 2026-01-31"
                ))
            }),
        }
    };
    let from = date_arg("from")?;
    let to = date_arg("to")?;
    let days = match args.get("days") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_i64().filter(|d| *d >= 1).ok_or_else(|| {
            ToolError::new(format!(
                "`days` must be an integer from 1 to {}",
                kind.max_days
            ))
        })?),
    };
    let last = to.unwrap_or(today - 1);
    let first = match (from, days) {
        (Some(_), Some(_)) => {
            return Err(ToolError::new(
                "pass either `from` or `days`, not both: `days` counts back from `to` or \
                 yesterday",
            ));
        }
        (Some(day), None) => day,
        (None, days) => last + 1 - days.unwrap_or(DEFAULT_DAYS.min(kind.max_days)),
    };
    if first > last {
        return Err(ToolError::new(
            "the period is empty: `from` must not be later than `to`",
        ));
    }
    if last > today {
        return Err(ToolError::new(format!(
            "`to` {} is in the future; the reports end at today ({}) at the latest",
            format_day(last),
            format_day(today)
        )));
    }
    let length = last - first + 1;
    if length > kind.max_days {
        return Err(ToolError::new(format!(
            "the period {} – {} is {length} days long; AppGallery Connect returns the {} report \
             for at most {} days per request: choose a shorter period and download the rest \
             with another call",
            format_day(first),
            format_day(last),
            kind.title,
            kind.max_days
        )));
    }
    Ok((first, last))
}

/// Дата `YYYY-MM-DD` или `YYYYMMDD` — номер дня.
fn parse_day(text: &str) -> Option<i64> {
    if text.len() == 8 && text.chars().all(|c| c.is_ascii_digit()) {
        let (year, rest) = text.split_at(4);
        let (month, day) = rest.split_at(2);
        return parse_date(&format!("{year}-{month}-{day}"));
    }
    parse_date(text)
}

/// `groupBy`: значения вида отчёта; несколько — только там, где это список.
fn group_by(args: &Value, kind: &Kind) -> Result<Vec<String>, ToolError> {
    let values = match args.get("groupBy") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(text)) => vec![text.trim().to_owned()],
        Some(Value::Array(_)) => str_list(args, "groupBy")?,
        Some(_) => {
            return Err(ToolError::new(
                "`groupBy` must be a string or a list of strings",
            ));
        }
    };
    let mut list: Vec<String> = Vec::new();
    for value in values {
        let Some(known) = kind
            .group_by
            .iter()
            .find(|name| name.eq_ignore_ascii_case(&value))
        else {
            return Err(ToolError::new(format!(
                "invalid `groupBy` `{value}` for the {} report: use {}",
                kind.title,
                kind.group_by.join(", ")
            )));
        };
        if !list.iter().any(|item| item == known) {
            list.push((*known).to_owned());
        }
    }
    if list.len() > 1 && !kind.multi_group {
        return Err(ToolError::new(format!(
            "the {} report is grouped by one dimension; pass one of {}",
            kind.title,
            kind.group_by.join(", ")
        )));
    }
    Ok(list)
}

/// Фильтры: пары `filterCondition` и `filterConditionValue` по справочнику.
fn filters(args: &Value, kind: &Kind) -> Result<Vec<(&'static str, String)>, ToolError> {
    let mut filters = Vec::new();
    let countries = str_list(args, "countries")?;
    if !countries.is_empty() {
        let special = matches!(kind.name, "installs" | "in_app_purchases");
        let mut codes: Vec<String> = Vec::new();
        for code in countries {
            let lower = code.to_ascii_lowercase();
            let group = lower == "all" || lower == "others";
            let normalized = if special && group {
                lower
            } else if !group
                && (2..=3).contains(&code.len())
                && code.chars().all(|c| c.is_ascii_alphabetic())
            {
                code.to_ascii_uppercase()
            } else {
                return Err(ToolError::new(format!(
                    "invalid country code `{code}`: expected a code like RU or CN"
                )));
            };
            if !codes.contains(&normalized) {
                codes.push(normalized);
            }
        }
        if kind.name == "paid_app_details" && codes.len() != 1 {
            return Err(ToolError::new(
                "the paid app details report covers one country or region per request: pass \
                 exactly one code in `countries` and call again for the others",
            ));
        }
        filters.push(("countryId", codes.join(",")));
    } else if kind.name == "paid_app_details" {
        return Err(ToolError::new(
            "`countries` is required for the paid app details report: pass one country or \
             region code, like [\"RU\"]",
        ));
    }
    if let Some(channel) = str_arg(args, "businessType")? {
        filters.push(("businessType", channel.to_owned()));
    }
    let versions = str_list(args, "appVersions")?;
    if !versions.is_empty() {
        if let Some(bad) = versions
            .iter()
            .find(|v| v.is_empty() || v.contains(',') || v.chars().any(char::is_whitespace))
        {
            return Err(ToolError::new(format!(
                "invalid app version `{bad}`: pass each version as a separate item, like \
                 [\"1.0.0\", \"2.0.0\"]"
            )));
        }
        filters.push(("appVersion", versions.join(",")));
    }
    if let Some(device) = str_arg(args, "deviceName")?.filter(|d| !d.is_empty()) {
        filters.push(("deviceName", device.to_owned()));
    }
    if let Some(download) = str_arg(args, "downloadType")? {
        let code = match download {
            "new" => "1",
            "update" => "2",
            other => {
                return Err(ToolError::new(format!(
                    "invalid `downloadType` `{other}`: use new or update"
                )));
            }
        };
        filters.push(("downloadType", code.to_owned()));
    }
    if let Some(order) = str_arg(args, "orderId")?.filter(|o| !o.is_empty()) {
        filters.push(("orderID", order.to_owned()));
    }
    if let Some(status) = str_arg(args, "orderStatus")? {
        let code = match status {
            "paid" => "0",
            "refunded" => "1",
            "payment_failed" => "2",
            "refund_failed" => "3",
            "refunding" => "5",
            other => {
                return Err(ToolError::new(format!(
                    "invalid `orderStatus` `{other}`: use paid, refunded, payment_failed, \
                     refund_failed or refunding"
                )));
            }
        };
        filters.push(("orderStatus", code.to_owned()));
    }
    Ok(filters)
}

/// Ошибка запроса ссылки: текст исполнителя (ссылки в нём нет — до неё дело не дошло).
fn request_error(kind: &Kind, err: &ApiError) -> ToolError {
    ToolError::new(format!(
        "requesting the {} report failed: {}",
        kind.title,
        describe_api_error(err)
    ))
}

/// Ошибка скачивания: исполнитель пишет только причину и имя хоста, без адреса и query.
fn download_error(kind: &Kind, err: &ApiError) -> ToolError {
    let hint = match err {
        ApiError::HostNotAllowed(_) => {
            " The server downloads reports only from Huawei hosts; report this to the user"
        }
        ApiError::Io(_) | ApiError::InvalidRequest(_) => "",
        _ => " The link is valid only for a short time: call the tool again",
    };
    ToolError::new(format!(
        "could not download the {} report: {err}.{hint}",
        kind.title
    ))
}

/// Excel по первым байтам: `xlsx` (zip) или `xls` (OLE2).
fn excel_signature(head: &[u8]) -> Option<&'static str> {
    match head {
        [0x50, 0x4B, 0x03, 0x04, ..] => Some("Excel (xlsx)"),
        [0xD0, 0xCF, 0x11, 0xE0, ..] => Some("Excel (xls)"),
        _ => None,
    }
}

/// Тип сохранённого файла по первым байтам: `excel` или `csv` (текст).
async fn sniff(path: &Path) -> &'static str {
    use tokio::io::AsyncReadExt as _;
    let mut head = [0u8; 8];
    let read = match tokio::fs::File::open(path).await {
        Ok(mut file) => file.read(&mut head).await.unwrap_or(0),
        Err(_) => 0,
    };
    match head.get(..read) {
        Some(bytes) if excel_signature(bytes).is_some() => "excel",
        Some([]) | None => "empty",
        Some(_) => "csv",
    }
}

/// День для API: `YYYYMMDD`.
fn api_day(day: i64) -> String {
    let (year, month, date) = civil_from_days(day);
    format!("{year:04}{month:02}{date:02}")
}

/// День для агента: `YYYY-MM-DD`.
fn format_day(day: i64) -> String {
    let (year, month, date) = civil_from_days(day);
    format!("{year:04}-{month:02}-{date:02}")
}

/// Собственный временный каталог для скачанного отчёта: на Unix доступен только владельцу
/// процесса (общий `/tmp` на Linux видят другие пользователи). Удаляется при `drop`, в том
/// числе при ошибке.
struct PrivateTempDir(PathBuf);

impl PrivateTempDir {
    fn create() -> Result<Self, ToolError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let mut last_error = None;
        for _ in 0..16 {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "agc-report-{}-{nanos}-{unique}",
                std::process::id()
            ));
            match private_dir_builder().create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_error = Some(err);
                }
                Err(err) => {
                    return Err(ToolError::new(format!(
                        "could not create a temporary directory for the report: {err}"
                    )));
                }
            }
        }
        Err(ToolError::new(format!(
            "could not create a temporary directory for the report: {}",
            last_error.map_or_else(|| "no free name".to_owned(), |err| err.to_string())
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

/// Каталог только для владельца (0700).
#[cfg(unix)]
fn private_dir_builder() -> std::fs::DirBuilder {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
}

/// На Windows временный каталог и так свой у каждого пользователя (`%TEMP%` в профиле), а
/// новый каталог наследует его права.
#[cfg(not(unix))]
fn private_dir_builder() -> std::fs::DirBuilder {
    std::fs::DirBuilder::new()
}

impl Drop for PrivateTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use appgallery_connect_api::token::StaticToken;
    use appgallery_connect_api::{ApiClient, ApiClientCredentials, ClientConfig, Region};

    use super::*;
    use crate::composite::ProfileInfo;

    fn context(apps: &[&str]) -> Context {
        let id = ["3141", "5926", "5358"].concat();
        let secret = ["ZX12", "CV34", "BN56"].concat().repeat(4);
        let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
        let config = ClientConfig::new(Region::Ru, Some(credentials));
        let token = ["report", "-unit-", "token"].concat();
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

    fn day(text: &str) -> i64 {
        parse_date(text).unwrap()
    }

    #[test]
    fn kinds_match_operations_of_the_reports_toolset() {
        for kind in KINDS {
            let op = operation(kind.operation_id).unwrap();
            assert_eq!(op.access, AccessLevel::Read, "{}", kind.name);
            assert!(op.saves_file(), "{}", kind.name);
            assert_eq!(
                op.param("exportType").is_some(),
                kind.excel,
                "{}: exportType",
                kind.name
            );
            assert_eq!(op.param("groupBy").is_some(), !kind.group_by.is_empty());
            if let Some(param) = op.param("groupBy") {
                assert_eq!(param.repeated, kind.multi_group, "{}", kind.name);
            }
            assert_eq!(op.param("currency").is_some(), kind.currency);
            assert_eq!(
                op.param("queryType").is_some(),
                kind.filters.contains(&Filter::DateType)
            );
            // Ограничение периода совпадает с описанием параметра в справочнике.
            let start = op.param("startTime").unwrap().description;
            assert!(
                start.contains(&format!("cannot exceed {} days", kind.max_days)),
                "{}: {start}",
                kind.name
            );
        }
        let tools = tools();
        assert_eq!(tools[0].name, NAME);
        assert_eq!(tools[0].toolset, "reports");
        assert_eq!(tools[0].access, AccessLevel::Read);
        assert_eq!(input_schema()["additionalProperties"], false);
    }

    #[test]
    fn periods_are_checked_before_the_request() {
        let ctx = context(&["100000001"]);
        let today = day("2026-03-10");
        let parse = |args: Value| parse_request(&ctx, &args, today);

        let request = parse(json!({"report": "installs"})).unwrap();
        assert_eq!(request.last_day, day("2026-03-09"));
        assert_eq!(request.last_day - request.first_day + 1, 30);
        assert_eq!(request.language, "en-US");

        let request =
            parse(json!({"report": "paid_app", "from": "2025-03-10", "to": "20260309"})).unwrap();
        assert_eq!(request.last_day - request.first_day + 1, 365);
        let request =
            parse(json!({"report": "installs", "to": "2026-01-31", "days": 180})).unwrap();
        assert_eq!(format_day(request.first_day), "2025-08-05");
        assert_eq!(api_day(request.first_day), "20250805");

        for (args, text) in [
            (
                json!({"report": "installs", "days": 181}),
                "at most 180 days",
            ),
            (
                json!({"report": "paid_app", "from": "2025-01-01", "to": "2026-01-01"}),
                "at most 365 days",
            ),
            (
                json!({"report": "install_failures", "from": "2026-01-01", "days": 3}),
                "not both",
            ),
            (
                json!({"report": "installs", "from": "2026-02-01", "to": "2026-01-01"}),
                "empty",
            ),
            (json!({"report": "installs", "to": "2026-03-11"}), "future"),
            (
                json!({"report": "installs", "from": "01.01.2026"}),
                "expected a date",
            ),
        ] {
            let err = parse(args.clone()).unwrap_err();
            assert!(err.message().contains(text), "{args}: {err}");
        }
    }

    #[test]
    fn arguments_are_checked_per_report() {
        let ctx = context(&[]);
        let parse = |args: Value| parse_request(&ctx, &args, day("2026-03-10"));
        for (args, text) in [
            (json!({"report": "installs"}), "no default app"),
            (
                json!({"report": "paid_app", "appId": "1", "groupBy": "date"}),
                "`groupBy` is not available",
            ),
            (
                json!({"report": "installs", "appId": "1", "currency": "EUR"}),
                "`currency` is not available",
            ),
            (
                json!({"report": "installs", "appId": "1", "orderStatus": "paid"}),
                "`orderStatus` is not available",
            ),
            (
                json!({"report": "installs", "appId": "1", "groupBy": ["date", "countryId"]}),
                "one dimension",
            ),
            (
                json!({"report": "installs", "appId": "1", "groupBy": "deviceName"}),
                "invalid `groupBy`",
            ),
            (
                json!({"report": "paid_app_details", "appId": "1"}),
                "`countries` is required",
            ),
            (
                json!({"report": "paid_app_details", "appId": "1", "countries": ["RU", "BY"]}),
                "exactly one",
            ),
            (
                json!({"report": "paid_app_details", "appId": "1", "countries": ["RU"],
                    "format": "excel", "save_to": "x.xlsx"}),
                "only as CSV",
            ),
            (
                json!({"report": "installs", "appId": "1", "format": "excel"}),
                "pass `save_to`",
            ),
            (
                json!({"report": "installs", "appId": "1", "maxRows": 5, "save_to": "x.csv"}),
                "`maxRows`",
            ),
            (
                json!({"report": "paid_app", "appId": "1", "countries": ["all"]}),
                "invalid country code",
            ),
            (
                json!({"report": "installs", "appId": "1", "appVersions": ["1.0,2.0"]}),
                "separate item",
            ),
        ] {
            let err = parse(args.clone()).unwrap_err();
            assert!(err.message().contains(text), "{args}: {err}");
        }

        let request = parse(json!({
            "report": "install_failures",
            "appId": 100000002,
            "groupBy": ["DATE", "deviceName", "date"],
            "countries": ["ru"],
            "appVersions": ["1.0.0", "2.0.0"],
            "deviceName": "P40-lite",
            "downloadType": "update",
        }))
        .unwrap();
        assert_eq!(request.app_id, "100000002");
        assert_eq!(request.group_by, ["date", "deviceName"]);
        let args = operation_args(&request);
        assert_eq!(args["groupBy"], json!(["date", "deviceName"]));
        assert_eq!(args["exportType"], "CSV");
        assert_eq!(
            args["filterCondition"],
            json!(["countryId", "appVersion", "deviceName", "downloadType"])
        );
        assert_eq!(
            args["filterConditionValue"],
            json!(["RU", "1.0.0,2.0.0", "P40-lite", "2"])
        );
        assert!(args.get("currency").is_none() && args.get("queryType").is_none());
    }

    #[test]
    fn excel_files_are_recognized() {
        assert_eq!(excel_signature(b"PK\x03\x04rest"), Some("Excel (xlsx)"));
        assert_eq!(
            excel_signature(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1]),
            Some("Excel (xls)")
        );
        assert_eq!(excel_signature(b"a,b\n"), None);
    }

    #[test]
    fn temporary_directories_are_private_and_removed() {
        let dir = PrivateTempDir::create().unwrap();
        let path = dir.path().to_path_buf();
        assert!(path.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        std::fs::write(path.join("report"), "a,b").unwrap();
        drop(dir);
        assert!(!path.exists());
    }
}
