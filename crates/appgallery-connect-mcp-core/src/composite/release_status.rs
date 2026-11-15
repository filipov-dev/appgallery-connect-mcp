//! `release_status`: состояние выпуска приложения за один вызов.
//!
//! Основное чтение — `get_app_info` (версия, её состояние проверки и выпуска, выпущенная
//! версия, отзыв проверки): его сбой — ошибка инструмента. Вспомогательные — пометка в
//! `notes`, а не ошибка: `get_app_info` с `releaseType=3` (поэтапный выпуск: доля, сроки,
//! состояние; если поэтапного выпуска нет, API может ответить ошибкой), `list_packages`
//! (несколько новых пакетов, без ссылки на скачивание) и `get_package_compile_status` для
//! пакета текущей версии (общий опрос [`super::package_upload::wait_for_package`], один
//! запрос).
//!
//! Время запланированного выпуска API не отдаёт: известно только, что одобренная версия ждёт
//! выпуска (`releaseState` 3).

use std::time::Duration;

use appgallery_connect_api::AccessLevel;
use serde_json::{Map, Value, json};

use crate::composite::package_upload::{BuildStatus, fail_reason_text, wait_for_package};
use crate::composite::release::{
    self, Phased, RELEASE_TYPE_PHASED, STATE_RELEASING, Version, in_review, review_opinion,
};
use crate::composite::reviews_summary::app_id;
use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::ToolError;

/// Имя инструмента.
pub const NAME: &str = "release_status";

/// Пакетов в ответе по умолчанию и максимум.
const DEFAULT_PACKAGES: u64 = 5;
const MAX_PACKAGES: u64 = 20;

const DESCRIPTION: &str = "Release state of an app in one call: the version being prepared or \
reviewed (version name and code, review and release state such as draft, in_review, \
releasing or released, the latest review comment), the released version, whether an approved \
version waits for a scheduled release, the phased release (state RELEASE, SUSPEND, DRAFT or \
CANCEL, share of users, start and end), the newest packages and the processing state of the \
package of the current version (`readyToSubmit`). Read-only. Use it before release_submit and \
release_manage and to follow a submission. Parts that could not be read are explained in \
`notes`.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "releases",
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
            "maxPackages": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_PACKAGES,
                "description": "How many of the newest packages to list (default 5)."
            }
        }
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(release_status(ctx, args))
}

async fn release_status(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let app_id = app_id(ctx, args.get("appId"))?;
    let max_packages = match args.get("maxPackages") {
        None | Some(Value::Null) => DEFAULT_PACKAGES,
        Some(value) => value
            .as_u64()
            .filter(|count| *count <= MAX_PACKAGES)
            .ok_or_else(|| {
                ToolError::new(format!(
                    "`maxPackages` must be an integer from 0 to {MAX_PACKAGES}"
                ))
            })?,
    };
    let max_packages = usize::try_from(max_packages).unwrap_or(usize::MAX);
    let mut notes: Vec<String> = Vec::new();

    let body = release::app_info(ctx, &app_id, None)
        .await
        .map_err(|err| ToolError::new(format!("could not read the app (`get_app_info`): {err}")))?;
    let version = Version::from_body(&body);

    let mut out = Map::new();
    out.insert("appId".into(), json!(app_id));
    out.insert("version".into(), version.json());
    out.insert("released".into(), version.released_json());
    let mut review = Map::new();
    review.insert("inReview".into(), json!(version.state.map(in_review)));
    if let Some(opinion) = review_opinion(&body) {
        review.insert("opinion".into(), json!(opinion));
    }
    out.insert("review".into(), Value::Object(review));
    out.insert(
        "waitsForScheduledRelease".into(),
        json!(version.state.map(|state| state == STATE_RELEASING)),
    );

    // Поэтапный выпуск: из ответа с releaseType=3, иначе из основного ответа.
    let phased = match release::app_info(ctx, &app_id, Some(RELEASE_TYPE_PHASED)).await {
        Ok(phased_body) => Phased::from_body(&phased_body).or_else(|| Phased::from_body(&body)),
        Err(err) => {
            let fallback = Phased::from_body(&body);
            if fallback.is_none() {
                notes.push(format!(
                    "the phased release state could not be read (`get_app_info` with \
                     releaseType 3); the app may have no phased release: {err}"
                ));
            }
            fallback
        }
    };
    out.insert(
        "phasedRelease".into(),
        phased.as_ref().map_or(Value::Null, Phased::json),
    );

    match release::packages(ctx, &app_id).await {
        Ok(packages) => {
            let newest: Vec<Value> = packages
                .newest_first(max_packages)
                .into_iter()
                .map(release::Package::json)
                .collect();
            let mut list = Map::new();
            list.insert(
                "total".into(),
                json!(packages.total.unwrap_or(packages.items.len() as u64)),
            );
            list.insert("newest".into(), json!(newest));
            out.insert("packages".into(), Value::Object(list));
            match packages.current(version.code) {
                Some(package) => {
                    let wait =
                        wait_for_package(ctx, &app_id, &package.id, 0, Duration::ZERO, &mut notes)
                            .await;
                    if let Some(err) = &wait.error {
                        notes.push(format!(
                            "the processing state of package `{}` could not be read: {err}",
                            package.id
                        ));
                    }
                    let mut current = match package.json() {
                        Value::Object(map) => map,
                        _ => Map::new(),
                    };
                    current.insert("status".into(), json!(wait.status.as_str()));
                    if let Some(code) = wait.fail_reason {
                        current.insert(
                            "failReason".into(),
                            json!({"code": code, "description": fail_reason_text(code)}),
                        );
                    }
                    out.insert("currentPackage".into(), Value::Object(current));
                    let ready =
                        wait.status == BuildStatus::Ready && !version.state.is_some_and(in_review);
                    out.insert("readyToSubmit".into(), json!(ready));
                }
                None => {
                    out.insert("currentPackage".into(), Value::Null);
                    out.insert("readyToSubmit".into(), json!(false));
                    notes
                        .push("the app has no uploaded package; package_upload uploads one".into());
                }
            }
        }
        Err(err) => {
            notes.push(format!(
                "the packages could not be read (`list_packages`): {err}"
            ));
        }
    }
    if version.state == Some(STATE_RELEASING) {
        notes.push(
            "the version is approved and waits for its scheduled release; the API does not \
             return the scheduled time. release_manage changes it (set_release_time) or \
             releases the version now (release_now)"
                .into(),
        );
    }
    if !notes.is_empty() {
        out.insert("notes".into(), json!(notes));
    }
    Ok(Value::Object(out))
}
