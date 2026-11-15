//! `release_submit`: отправка версии приложения на проверку и выпуск — полный (сразу после
//! одобрения или в заданное время) или поэтапный (доля пользователей и сроки).
//!
//! Порядок:
//! 1. До любых запросов — аргументы: вид выпуска, время в будущем и в формате со смещением,
//!    доля больше 0 и меньше 100 с двумя знаками, конец поэтапного выпуска позже начала,
//!    длина примечания и описания.
//! 2. Чтение: `get_app_info` (состояние версии: на проверке отправлять нельзя; для
//!    поэтапного выпуска нужна уже выпущенная версия), `list_packages` (пакет текущей
//!    версии или `pkgId`), `get_package_compile_status` через общий опрос
//!    [`super::package_upload::wait_for_package`]. Пакета нет, он обрабатывается, не собрался
//!    или его состояние неизвестно — отказ с объяснением, без отправки.
//! 3. `dryRun` на этом останавливается и показывает запрос, который был бы отправлен.
//! 4. `submit_app` (`publishing.app.submit`): `releaseTime` и `remark` — в строке запроса,
//!    для поэтапного выпуска `releaseType=3` и поля `phasedRelease*` в корне тела (так в
//!    справочнике; живой сверкой не проверено).
//!
//! Отправка видна пользователям AppGallery после одобрения и через API не отменяется, поэтому
//! инструмент — уровня удаления.

use std::time::Duration;

use appgallery_connect_api::AccessLevel;
use serde_json::{Map, Value, json};

use crate::composite::package_upload::{BuildStatus, Wait, fail_reason_text, wait_for_package};
use crate::composite::release::{
    self, Package, RELEASE_TYPE_PHASED, SUBMIT, Version, api_time, describe_request, future_time,
    in_review, percent_arg, percent_text, release_state_name, text_arg,
};
use crate::composite::reviews_summary::{app_id, format_time, now_ms, operation};
use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};

/// Имя инструмента.
pub const NAME: &str = "release_submit";

/// Самое долгое ожидание сборки пакета, секунд (как у `package_upload`).
const MAX_WAIT_SECS: u64 = 1800;
/// Пауза между запросами состояния пакета.
const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Описание поэтапного выпуска по умолчанию (поле обязательно для поэтапного выпуска).
const DEFAULT_PHASED_DESCRIPTION: &str = "Phased release";

const DESCRIPTION: &str = "Submit the app version being prepared for review and release in \
one call. The result is visible to AppGallery users: after approval the version is released \
to them, and a submission cannot be withdrawn through the API. Call it only when the user has \
explicitly asked to submit or release this app version; run it with `dryRun: true` first and \
show the user what will be sent. Modes: `releaseType: \"full\"` (default) releases the version \
to all users right after approval, or at `releaseTime` (a scheduled release); `releaseType: \
\"phased\"` releases it to `phasedPercent` percent of users from `phasedStart` to `phasedEnd` \
(the app must already have a released version). Before submitting, the tool checks that the \
version is not already in review and that its package (the newest package of the version, or \
`pkgId`) is uploaded and processed by AppGallery Connect (`waitSeconds` waits for processing); \
otherwise it refuses and explains why, without submitting. Times take a UTC offset, like \
2026-12-01T10:00:00Z or 2026-12-01T13:00:00+03:00, and must be in the future. Returns what was \
sent, the package that was checked and the next step; `release_status` shows the review and \
release state afterwards, `release_manage` changes a phased release or the release time.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "releases",
        access: AccessLevel::Destructive,
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
            "releaseType": {
                "type": "string",
                "enum": ["full", "phased"],
                "description": "full (default): release to all users after approval or at \
                    releaseTime. phased: release to phasedPercent percent of users between \
                    phasedStart and phasedEnd."
            },
            "releaseTime": {
                "type": "string",
                "description": "full only: release the approved version at this time instead \
                    of right after approval, like 2026-12-01T10:00:00Z. Must be in the future."
            },
            "phasedPercent": {
                "type": "number",
                "exclusiveMinimum": 0,
                "exclusiveMaximum": 100,
                "description": "phased only (required): share of users in percent, like 10 or \
                    12.5, at most two decimal places."
            },
            "phasedStart": {
                "type": "string",
                "description": "phased only (required): start of the phased release, like \
                    2026-12-01T10:00:00Z. Must be in the future."
            },
            "phasedEnd": {
                "type": "string",
                "description": "phased only (required): end of the phased release, later than \
                    phasedStart."
            },
            "phasedDescription": {
                "type": "string",
                "maxLength": 500,
                "description": "phased only: description of the phased release (default \
                    \"Phased release\")."
            },
            "remark": {
                "type": "string",
                "minLength": 10,
                "maxLength": 300,
                "description": "Optional note for the reviewers, 10-300 characters."
            },
            "pkgId": {
                "type": "string",
                "minLength": 1,
                "description": "Package ID (pkgVersion from package_upload or list_packages) \
                    to check before submitting. Default: the newest package of the version \
                    being prepared."
            },
            "waitSeconds": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_WAIT_SECS,
                "description": "How long to wait for AppGallery Connect to finish processing \
                    the package before submitting. Default 0: check once."
            },
            "dryRun": {
                "type": "boolean",
                "description": "true: run the checks and show the request that would be sent, \
                    without submitting anything."
            }
        }
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(async move { release_submit(ctx, args, POLL_INTERVAL).await })
}

/// Вид выпуска.
#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// Полный: сразу после одобрения (`None`) или в заданное время.
    Full { release_time: Option<i64> },
    /// Поэтапный.
    Phased {
        percent: f64,
        start: i64,
        end: i64,
        description: String,
    },
}

/// Разобранные и проверенные аргументы.
#[derive(Debug)]
struct Request {
    app_id: String,
    mode: Mode,
    remark: Option<String>,
    pkg_id: Option<String>,
    wait_secs: u64,
    dry_run: bool,
}

async fn release_submit(
    ctx: &Context,
    args: Value,
    interval: Duration,
) -> Result<Value, ToolError> {
    let request = parse_request(ctx, &args, now_ms()?)?;
    let mut notes: Vec<String> = Vec::new();

    let body = release::app_info(ctx, &request.app_id, None)
        .await
        .map_err(|err| {
            ToolError::new(format!(
                "could not read the app state (`get_app_info`), so nothing was submitted: {err}"
            ))
        })?;
    let version = Version::from_body(&body);
    check_version(&request, &version)?;

    let (package, wait) = check_package(ctx, &request, &version, interval, &mut notes).await?;
    let op = operation(SUBMIT)?;
    let call_args = submit_args(&request);
    let sent = describe_request(op.tool_name, &call_args);

    if request.dry_run {
        let mut out = summary(&request, &version, &package, &wait, sent);
        out.insert("dryRun".into(), json!(true));
        out.insert(
            "note".into(),
            json!(
                "Nothing was submitted. The checks passed: call again without `dryRun` to \
                 submit, but only if the user has confirmed it."
            ),
        );
        add_notes(&mut out, notes);
        return Ok(Value::Object(out));
    }

    tracing::debug!(
        tool = NAME,
        phased = matches!(request.mode, Mode::Phased { .. }),
        "отправка на проверку"
    );
    ctx.api.call(op, &call_args).await.map_err(|err| {
        ToolError::new(format!(
            "AppGallery Connect did not accept the submission (`submit_app`): {}. The checks \
             before it passed (package `{}` is processed). Check `release_status` before \
             trying again",
            describe_api_error(&err),
            package.id
        ))
    })?;
    let mut out = summary(&request, &version, &package, &wait, sent);
    out.insert("submitted".into(), json!(true));
    out.insert(
        "next".into(),
        json!(
            "The version is in review now. `release_status` shows the review and release \
             state; `release_manage` changes a phased release or a scheduled release time \
             after approval."
        ),
    );
    add_notes(&mut out, notes);
    Ok(Value::Object(out))
}

/// Разбор и проверка аргументов, без запросов к API.
fn parse_request(ctx: &Context, args: &Value, now: i64) -> Result<Request, ToolError> {
    let app_id = app_id(ctx, args.get("appId"))?;
    let phased = match args.get("releaseType").and_then(Value::as_str) {
        None | Some("full") => false,
        Some("phased") => true,
        Some(other) => {
            return Err(ToolError::new(format!(
                "`releaseType` must be `full` or `phased`, not `{other}`"
            )));
        }
    };
    let release_time = future_time(args, "releaseTime", now)?;
    let percent = percent_arg(args, "phasedPercent")?;
    let start = future_time(args, "phasedStart", now)?;
    let end = future_time(args, "phasedEnd", now)?;
    let description = text_arg(args, "phasedDescription", 1, 500)?;
    let mode = if phased {
        if release_time.is_some() {
            return Err(ToolError::new(
                "`releaseTime` is only for a full release; a phased release starts at \
                 `phasedStart`",
            ));
        }
        let (Some(percent), Some(start), Some(end)) = (percent, start, end) else {
            return Err(ToolError::new(
                "a phased release needs `phasedPercent`, `phasedStart` and `phasedEnd`",
            ));
        };
        if end <= start {
            return Err(ToolError::new(
                "`phasedEnd` must be later than `phasedStart`",
            ));
        }
        Mode::Phased {
            percent,
            start,
            end,
            description: description.unwrap_or_else(|| DEFAULT_PHASED_DESCRIPTION.to_owned()),
        }
    } else {
        if percent.is_some() || start.is_some() || end.is_some() || description.is_some() {
            return Err(ToolError::new(
                "`phasedPercent`, `phasedStart`, `phasedEnd` and `phasedDescription` are only \
                 for `releaseType: \"phased\"`",
            ));
        }
        Mode::Full { release_time }
    };
    let remark = text_arg(args, "remark", 10, 300)?;
    let pkg_id = text_arg(args, "pkgId", 1, 64)?;
    let wait_secs = match args.get("waitSeconds") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_u64()
            .filter(|secs| *secs <= MAX_WAIT_SECS)
            .ok_or_else(|| {
                ToolError::new(format!(
                    "`waitSeconds` must be an integer from 0 to {MAX_WAIT_SECS}"
                ))
            })?,
    };
    Ok(Request {
        app_id,
        mode,
        remark,
        pkg_id,
        wait_secs,
        dry_run: args.get("dryRun").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Состояние версии позволяет отправку.
fn check_version(request: &Request, version: &Version) -> Result<(), ToolError> {
    if let Some(state) = version.state.filter(|state| in_review(*state)) {
        return Err(ToolError::new(format!(
            "the version {} is already in review (state {state}, {}), so it was not submitted \
             again. Wait for the review result; `release_status` shows it",
            version.number.as_deref().unwrap_or("being prepared"),
            release_state_name(state)
        )));
    }
    if matches!(request.mode, Mode::Phased { .. }) && !version.has_released() {
        return Err(ToolError::new(
            "a phased release needs a version that is already released, and AppGallery \
             Connect reports none for this app. Submit the first version as a full release",
        ));
    }
    Ok(())
}

/// Пакет загружен и собран. Иначе — отказ с объяснением.
async fn check_package(
    ctx: &Context,
    request: &Request,
    version: &Version,
    interval: Duration,
    notes: &mut Vec<String>,
) -> Result<(Package, Wait), ToolError> {
    let packages = release::packages(ctx, &request.app_id)
        .await
        .map_err(|err| {
            ToolError::new(format!(
                "could not read the packages of the app (`list_packages`), so nothing was \
                 submitted: {err}"
            ))
        })?;
    if packages.items.is_empty() {
        return Err(ToolError::new(
            "the app has no uploaded package, so nothing was submitted. Upload the package \
             with package_upload first",
        ));
    }
    let package = match &request.pkg_id {
        Some(id) => packages
            .items
            .iter()
            .find(|item| &item.id == id)
            .ok_or_else(|| {
                ToolError::new(format!(
                    "package `{id}` is not among the packages of the app (`list_packages`), so \
                     nothing was submitted. Pass the pkgId that package_upload returned"
                ))
            })?,
        None => packages.current(version.code).ok_or_else(|| {
            ToolError::new("could not choose the package of the version; pass `pkgId`")
        })?,
    }
    .clone();
    if request.pkg_id.is_none() && version.code.is_some() && package.version_code != version.code {
        notes.push(format!(
            "no package has the version code of the version being prepared ({}); the newest \
             package `{}` was checked",
            version.code.unwrap_or_default(),
            package.id
        ));
    }

    let wait = wait_for_package(
        ctx,
        &request.app_id,
        &package.id,
        request.wait_secs,
        interval,
        notes,
    )
    .await;
    let name = package.file_name.as_deref().unwrap_or("of the version");
    match wait.status {
        BuildStatus::Ready => Ok((package, wait)),
        BuildStatus::Processing => Err(ToolError::new(format!(
            "package `{}` ({name}) is still being processed by AppGallery Connect, so nothing \
             was submitted. Try again in a few minutes or pass `waitSeconds`",
            package.id
        ))),
        BuildStatus::Failed => {
            let reason = wait
                .fail_reason
                .map(|code| format!(": {} (failReason {code})", fail_reason_text(code)))
                .unwrap_or_default();
            Err(ToolError::new(format!(
                "AppGallery Connect could not process package `{}` ({name}){reason}, so nothing \
                 was submitted. Fix the package and upload a new one with package_upload",
                package.id
            )))
        }
        BuildStatus::NotChecked | BuildStatus::Unknown => {
            let cause = match &wait.error {
                Some(err) => format!("the request failed: {err}"),
                None => "AppGallery Connect did not report its state".to_owned(),
            };
            Err(ToolError::new(format!(
                "could not confirm that package `{}` ({name}) is processed ({cause}), so \
                 nothing was submitted. Check it with `get_package_compile_status` and try \
                 again",
                package.id
            )))
        }
    }
}

/// Аргументы `submit_app`.
fn submit_args(request: &Request) -> Value {
    let mut args = Map::new();
    args.insert("appId".into(), json!(request.app_id));
    if let Some(remark) = &request.remark {
        args.insert("remark".into(), json!(remark));
    }
    match &request.mode {
        Mode::Full { release_time } => {
            if let Some(time) = release_time {
                args.insert("releaseTime".into(), json!(api_time(*time)));
            }
        }
        Mode::Phased {
            percent,
            start,
            end,
            description,
        } => {
            args.insert("releaseType".into(), json!(RELEASE_TYPE_PHASED));
            args.insert(
                "body".into(),
                json!({
                    "phasedReleaseStartTime": api_time(*start),
                    "phasedReleaseEndTime": api_time(*end),
                    "phasedReleasePercent": percent_text(*percent),
                    "phasedReleaseDescription": description,
                }),
            );
        }
    }
    Value::Object(args)
}

/// Общая часть ответа: что отправлено (или было бы отправлено) и что проверено.
fn summary(
    request: &Request,
    version: &Version,
    package: &Package,
    wait: &Wait,
    sent: Value,
) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("appId".into(), json!(request.app_id));
    match &request.mode {
        Mode::Full { release_time } => {
            out.insert("releaseType".into(), json!("full"));
            out.insert(
                "release".into(),
                json!(match release_time {
                    Some(time) => format!("at {} (after approval)", format_time(*time)),
                    None => "right after approval".to_owned(),
                }),
            );
        }
        Mode::Phased {
            percent,
            start,
            end,
            ..
        } => {
            out.insert("releaseType".into(), json!("phased"));
            out.insert(
                "phased".into(),
                json!({
                    "percent": percent,
                    "start": format_time(*start),
                    "end": format_time(*end),
                }),
            );
        }
    }
    out.insert("version".into(), version.json());
    let mut checked = match package.json() {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    checked.insert("status".into(), json!(wait.status.as_str()));
    out.insert("package".into(), Value::Object(checked));
    out.insert("request".into(), sent);
    out
}

fn add_notes(out: &mut Map<String, Value>, notes: Vec<String>) {
    if !notes.is_empty() {
        out.insert("notes".into(), json!(notes));
    }
}
