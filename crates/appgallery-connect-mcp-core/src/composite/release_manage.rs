//! `release_manage`: изменение выпуска версии, которая уже прошла (или проходит) проверку.
//!
//! Действия:
//!
//! - `update_phased` — доля (`phasedPercent`, только растёт и меньше 100), конец
//!   (`phasedEnd`), начало (`phasedStart`, только пока поэтапный выпуск — черновик:
//!   справочник запрещает менять начало выпущенной версии) и описание поэтапного выпуска;
//! - `suspend` — приостановить поэтапный выпуск (`state: SUSPEND`, только из `RELEASE`);
//! - `resume` — продолжить приостановленный (`state: RELEASE`, только из `SUSPEND`);
//! - `complete` — перейти от поэтапного выпуска к выпуску на всех (`releaseType=1`, тело
//!   `{}`);
//! - `set_release_time` — перенести запланированный выпуск одобренной версии
//!   (`update_release_time`, `changeType 3`);
//! - `release_now` — выпустить одобренную версию сразу (`changeType 2`; поле `releaseTime`
//!   справочник требует и тут, уходит текущее время).
//!
//! Отменить поэтапный выпуск через API нельзя (состояние `CANCEL` только читается), уменьшать
//! долю — тоже: пользователи, которые уже получили версию, её не теряют. Такие переходы
//! отклоняются до запроса изменения. Текущее состояние читается `get_app_info` (для
//! поэтапного выпуска — с `releaseType=3`); если его прочитать не удалось, ничего не
//! меняется. `dryRun` показывает состояние и запрос, ничего не меняя.

use appgallery_connect_api::AccessLevel;
use serde_json::{Map, Value, json};

use crate::composite::release::{
    self, PHASED_UPDATE, Phased, RELEASE_TIME_UPDATE, RELEASE_TYPE_FULL, RELEASE_TYPE_PHASED,
    STATE_RELEASING, Version, api_time, describe_request, future_time, percent_arg, percent_text,
    release_state_name, text_arg,
};
use crate::composite::reviews_summary::{app_id, format_time, now_ms, operation};
use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};

/// Имя инструмента.
pub const NAME: &str = "release_manage";

const ACTIONS: [&str; 6] = [
    "update_phased",
    "suspend",
    "resume",
    "complete",
    "set_release_time",
    "release_now",
];

/// `changeType` у `update_release_time`.
const CHANGE_NOW: u8 = 2;
const CHANGE_TIME: u8 = 3;

const DESCRIPTION: &str = "Change how an app version is released on AppGallery: the result is \
visible to AppGallery users. Call it only when the user has explicitly asked for this change; \
run it with `dryRun: true` first and show the user the change. Actions: `update_phased` \
changes a phased release (`phasedPercent` can only grow and stays below 100, `phasedEnd`, \
`phasedDescription`, and `phasedStart` while the phased release has not started), `suspend` \
pauses a running phased release, `resume` continues a suspended one, `complete` turns the \
phased release into a release to all users, `set_release_time` moves the scheduled release of \
an approved version to `releaseTime`, `release_now` releases an approved version that waits \
for its scheduled time right away. The tool reads the current state first and refuses changes \
the API does not allow (a smaller share of users, cancelling a phased release, resuming a \
running one, changing the release time of a version that is not waiting for release), without \
changing anything. Times take a UTC offset, like 2026-12-01T10:00:00Z, and must be in the \
future. Check the state with `release_status` first.";

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
            "action": {
                "type": "string",
                "enum": ACTIONS,
                "description": "update_phased: change the share, end, start or description of \
                    a phased release. suspend: pause a phased release. resume: continue a \
                    suspended phased release. complete: release the phased version to all \
                    users. set_release_time: move the scheduled release to releaseTime. \
                    release_now: release a version that waits for its scheduled time now."
            },
            "appId": {
                "type": ["string", "integer"],
                "description": "Numeric app ID (like 100000001). Optional when the profile has \
                    exactly one default app."
            },
            "phasedPercent": {
                "type": "number",
                "exclusiveMinimum": 0,
                "exclusiveMaximum": 100,
                "description": "update_phased: new share of users in percent, larger than the \
                    current one, at most two decimal places. To release to everyone use \
                    complete."
            },
            "phasedStart": {
                "type": "string",
                "description": "update_phased: new start time, only while the phased release \
                    has not started (state DRAFT)."
            },
            "phasedEnd": {
                "type": "string",
                "description": "update_phased: new end time, in the future and after the \
                    start."
            },
            "phasedDescription": {
                "type": "string",
                "maxLength": 500,
                "description": "update_phased: new description of the phased release."
            },
            "releaseTime": {
                "type": "string",
                "description": "set_release_time (required): new release time, like \
                    2026-12-01T10:00:00Z."
            },
            "dryRun": {
                "type": "boolean",
                "description": "true: read the current state, check the change and show the \
                    request that would be sent, without changing anything."
            }
        },
        "required": ["action"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(release_manage(ctx, args))
}

/// Действие и его аргументы.
#[derive(Debug, Clone, PartialEq)]
enum Action {
    UpdatePhased {
        percent: Option<f64>,
        start: Option<i64>,
        end: Option<i64>,
        description: Option<String>,
    },
    Suspend,
    Resume,
    Complete,
    SetReleaseTime(i64),
    ReleaseNow,
}

impl Action {
    fn name(&self) -> &'static str {
        match self {
            Self::UpdatePhased { .. } => "update_phased",
            Self::Suspend => "suspend",
            Self::Resume => "resume",
            Self::Complete => "complete",
            Self::SetReleaseTime(_) => "set_release_time",
            Self::ReleaseNow => "release_now",
        }
    }

    fn is_phased(&self) -> bool {
        matches!(
            self,
            Self::UpdatePhased { .. } | Self::Suspend | Self::Resume | Self::Complete
        )
    }
}

async fn release_manage(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let now = now_ms()?;
    let app_id = app_id(ctx, args.get("appId"))?;
    let action = parse_action(&args, now)?;
    let dry_run = args.get("dryRun").and_then(Value::as_bool).unwrap_or(false);

    let (before, operation_id, call_args) = if action.is_phased() {
        let body = release::app_info(ctx, &app_id, Some(RELEASE_TYPE_PHASED))
            .await
            .map_err(|err| {
                ToolError::new(format!(
                    "could not read the phased release state (`get_app_info` with releaseType \
                     3), so nothing was changed: {err}"
                ))
            })?;
        let phased = Phased::from_body(&body).unwrap_or_default();
        check_phased(&action, &phased, now)?;
        (
            json!({"phasedRelease": phased.json()}),
            PHASED_UPDATE,
            phased_args(&app_id, &action),
        )
    } else {
        let body = release::app_info(ctx, &app_id, None).await.map_err(|err| {
            ToolError::new(format!(
                "could not read the version state (`get_app_info`), so nothing was changed: \
                 {err}"
            ))
        })?;
        let version = Version::from_body(&body);
        check_release_time(&version)?;
        (
            json!({"version": version.json()}),
            RELEASE_TIME_UPDATE,
            release_time_args(&app_id, &action, now),
        )
    };
    let op = operation(operation_id)?;
    let request = describe_request(op.tool_name, &call_args);

    let mut out = Map::new();
    out.insert("appId".into(), json!(app_id));
    out.insert("action".into(), json!(action.name()));
    out.insert("before".into(), before);
    out.insert("request".into(), request);
    if dry_run {
        out.insert("dryRun".into(), json!(true));
        out.insert(
            "note".into(),
            json!(
                "Nothing was changed. The change is allowed in the current state: call again \
                 without `dryRun` to apply it, but only if the user has confirmed it."
            ),
        );
        return Ok(Value::Object(out));
    }
    tracing::debug!(tool = NAME, action = action.name(), "изменение выпуска");
    ctx.api.call(op, &call_args).await.map_err(|err| {
        ToolError::new(format!(
            "AppGallery Connect did not accept the change (`{}`): {}. Check `release_status` \
             before trying again",
            op.tool_name,
            describe_api_error(&err)
        ))
    })?;
    out.insert("changed".into(), json!(true));
    out.insert(
        "next".into(),
        json!("`release_status` shows the new state; AppGallery Connect may take a few minutes to apply it."),
    );
    Ok(Value::Object(out))
}

/// Действие и аргументы — до запросов к API.
fn parse_action(args: &Value, now: i64) -> Result<Action, ToolError> {
    let name = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let percent = percent_arg(args, "phasedPercent")?;
    let start = future_time(args, "phasedStart", now)?;
    let end = future_time(args, "phasedEnd", now)?;
    let description = text_arg(args, "phasedDescription", 1, 500)?;
    let release_time = future_time(args, "releaseTime", now)?;
    let phased_given =
        percent.is_some() || start.is_some() || end.is_some() || description.is_some();
    let no_extra = |action: Action| -> Result<Action, ToolError> {
        if phased_given || release_time.is_some() {
            return Err(ToolError::new(format!(
                "`{name}` takes no phased release fields and no `releaseTime`"
            )));
        }
        Ok(action)
    };
    match name {
        "update_phased" => {
            if release_time.is_some() {
                return Err(ToolError::new(
                    "`releaseTime` is for set_release_time; a phased release has `phasedStart` \
                     and `phasedEnd`",
                ));
            }
            if !phased_given {
                return Err(ToolError::new(
                    "update_phased needs at least one of `phasedPercent`, `phasedEnd`, \
                     `phasedStart` or `phasedDescription`",
                ));
            }
            if let (Some(start), Some(end)) = (start, end)
                && end <= start
            {
                return Err(ToolError::new(
                    "`phasedEnd` must be later than `phasedStart`",
                ));
            }
            Ok(Action::UpdatePhased {
                percent,
                start,
                end,
                description,
            })
        }
        "suspend" => no_extra(Action::Suspend),
        "resume" => no_extra(Action::Resume),
        "complete" => no_extra(Action::Complete),
        "release_now" => no_extra(Action::ReleaseNow),
        "set_release_time" => {
            if phased_given {
                return Err(ToolError::new(
                    "set_release_time takes only `releaseTime`; phased release fields are for \
                     update_phased",
                ));
            }
            let time = release_time
                .ok_or_else(|| ToolError::new("set_release_time needs `releaseTime`"))?;
            Ok(Action::SetReleaseTime(time))
        }
        other => Err(ToolError::new(format!(
            "`action` must be one of {}, not `{other}`",
            ACTIONS.join(", ")
        ))),
    }
}

/// Переход поэтапного выпуска допустим в текущем состоянии.
fn check_phased(action: &Action, phased: &Phased, now: i64) -> Result<(), ToolError> {
    let state = phased.state.as_deref();
    let shown = state.unwrap_or("none");
    let refuse = |why: String| Err(ToolError::new(format!("{why}, so nothing was changed")));
    match state {
        None | Some("CANCEL") => {
            return refuse(format!(
                "the app has no active phased release (state {shown}); a phased release starts \
                 with release_submit and `releaseType: \"phased\"`"
            ));
        }
        Some("RELEASE" | "SUSPEND" | "DRAFT") => {}
        Some(other) => {
            return refuse(format!(
                "the phased release is in an unknown state `{other}`; check it with \
                 `release_status`"
            ));
        }
    }
    match action {
        Action::Suspend if state != Some("RELEASE") => refuse(format!(
            "only a running phased release (state RELEASE) can be suspended; it is {shown}"
        )),
        Action::Resume if state != Some("SUSPEND") => refuse(format!(
            "only a suspended phased release (state SUSPEND) can be resumed; it is {shown}"
        )),
        Action::UpdatePhased {
            percent,
            start,
            end,
            ..
        } => {
            if let (Some(new), Some(current)) = (percent, phased.percent)
                && *new <= current + 1e-9
            {
                return refuse(format!(
                    "the share of users can only grow: it is {} now, the new one is {}. \
                     Users who already got the version keep it",
                    percent_text(current),
                    percent_text(*new)
                ));
            }
            if start.is_some() && state != Some("DRAFT") {
                return refuse(format!(
                    "the start of a phased release cannot change after it has started (state \
                     {shown})"
                ));
            }
            if let Some(end) = end {
                let begin = start.or_else(|| phased.start_ms()).unwrap_or(now);
                if *end <= begin {
                    return refuse(format!(
                        "`phasedEnd` must be later than the start of the phased release ({})",
                        format_time(begin)
                    ));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Время выпуска меняется только у одобренной версии, которая ждёт выпуска.
fn check_release_time(version: &Version) -> Result<(), ToolError> {
    match version.state {
        Some(STATE_RELEASING) | None => Ok(()),
        Some(state) => Err(ToolError::new(format!(
            "the release time can be changed only while an approved version waits for its \
             scheduled release (state 3, releasing); the version {} is in state {state} ({}), \
             so nothing was changed",
            version.number.as_deref().unwrap_or("being prepared"),
            release_state_name(state)
        ))),
    }
}

/// Аргументы `update_phased_release`.
fn phased_args(app_id: &str, action: &Action) -> Value {
    let mut args = Map::new();
    args.insert("appId".into(), json!(app_id));
    let body = match action {
        Action::Suspend => json!({"state": "SUSPEND"}),
        Action::Resume => json!({"state": "RELEASE"}),
        Action::Complete => {
            args.insert("releaseType".into(), json!(RELEASE_TYPE_FULL));
            json!({})
        }
        Action::UpdatePhased {
            percent,
            start,
            end,
            description,
        } => {
            let mut body = Map::new();
            if let Some(start) = start {
                body.insert("phasedReleaseStartTime".into(), json!(api_time(*start)));
            }
            if let Some(end) = end {
                body.insert("phasedReleaseEndTime".into(), json!(api_time(*end)));
            }
            if let Some(percent) = percent {
                body.insert("phasedReleasePercent".into(), json!(percent_text(*percent)));
            }
            if let Some(description) = description {
                body.insert("phasedReleaseDescription".into(), json!(description));
            }
            Value::Object(body)
        }
        Action::SetReleaseTime(_) | Action::ReleaseNow => json!({}),
    };
    args.insert("body".into(), body);
    Value::Object(args)
}

/// Аргументы `update_release_time`.
fn release_time_args(app_id: &str, action: &Action, now: i64) -> Value {
    let (change, time) = match action {
        Action::SetReleaseTime(time) => (CHANGE_TIME, *time),
        _ => (CHANGE_NOW, now),
    };
    json!({
        "appId": app_id,
        "body": {
            "changeType": change,
            "releaseTime": api_time(time),
            "releaseType": RELEASE_TYPE_FULL,
        }
    })
}
