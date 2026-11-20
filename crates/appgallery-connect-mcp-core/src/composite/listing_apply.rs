//! `listing_apply`: тексты страницы приложения из локального каталога (формат —
//! [`super::listing`]) в AppGallery Connect.
//!
//! Порядок: чтение каталога и проверка длин (до запросов) → `publishing.appInfo.get` →
//! проверка кодов языков и плана (до записи) → запись. Изменяются только языки, в
//! которых есть разница по полям; поле, файла которого нет, не трогается. Порядок записи:
//! языки (`publishing.languageInfo.update`, он же добавляет новый язык) → основные сведения
//! (`publishing.appInfo.update`, только изменённые поля и обязательный `privacyPolicy`) →
//! удаления языков (`publishing.languageInfo.delete`). Так новый язык по умолчанию
//! появляется раньше, чем становится языком по умолчанию, а прежний удаляется последним.
//!
//! Удаление языков, которых нет в каталоге, — только с `delete_missing_languages` и только
//! на сервере с уровнем удаления: иначе отказ до любой записи с подсказкой
//! `--allow-destructive`. Язык по умолчанию не удаляется.
//!
//! Тело `languageInfo.update` содержит все известные поля языка (из каталога, остальные —
//! текущие непустые значения AppGallery Connect): справочник не говорит, сохраняет ли
//! AppGallery Connect опущенные поля.

use std::collections::BTreeSet;

use appgallery_connect_api::AccessLevel;
use serde_json::{Map, Value, json};

use super::listing::{
    self, APP_FIELDS, APP_INFO_UPDATE, LANGUAGE_DELETE, LANGUAGE_FIELDS, LANGUAGE_UPDATE,
    LocalListing, Remote, TOOLSET, app_args, app_id_schema, bool_arg, is_known_language,
    release_type, release_type_schema, required_str, suggest_language, text_diff, text_of,
};
use super::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};
use crate::registry::{access_action, access_hint, access_requirement};

/// Имя инструмента.
pub const NAME: &str = "listing_apply";

const DESCRIPTION: &str = "Apply the texts of a local listing directory (as written by \
listing_export) to the AppGallery store listing: the app name, short and full descriptions \
and release notes per language, and the default language, privacy policy, developer website \
and privacy rights URL. Only fields whose file exists and differs are changed, only changed \
languages are updated; a language directory that the app does not have is added (title.txt \
is required). Lengths (title 64, short description 80, full description 8000, release notes \
500 characters) and language codes are checked before anything is sent. `dry_run` defaults \
to true and returns the plan with the differences; call again with dry_run false to apply. \
Languages that exist in AppGallery Connect but not in the directory are only listed; with \
delete_missing_languages they are deleted, which needs a server started with \
--allow-destructive (the default language is never deleted). The changes apply to the draft \
version and reach users only after the app is submitted and approved (submit_app). Images and \
videos: image_sync. Local paths are allowed only inside the server's allowed directories.";

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
            "appId": app_id_schema(),
            "dir": {
                "type": "string",
                "description": "Listing directory (inside the allowed directories), as written \
                    by listing_export."
            },
            "dry_run": {
                "type": "boolean",
                "description": "Only return the plan with the differences. Default: true."
            },
            "delete_missing_languages": {
                "type": "boolean",
                "description": "Delete the languages that exist in AppGallery Connect but have \
                    no directory (except the default language). Needs a server started with \
                    --allow-destructive. Default: false."
            },
            "apply_app_info": {
                "type": "boolean",
                "description": "Apply default_language.txt, privacy_policy.txt, \
                    developer_website.txt and privacy_rights_url.txt. Default: true."
            },
            "releaseType": release_type_schema()
        },
        "required": ["dir"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(apply(ctx, args))
}

/// Действие плана.
#[derive(Debug, Clone)]
enum Action {
    /// Обновление или добавление языка: полное тело `LanguageInfo`.
    Language {
        language: String,
        added: bool,
        body: Map<String, Value>,
        diff: Vec<Value>,
    },
    /// Основные сведения: изменённые поля и `privacyPolicy`.
    AppInfo {
        body: Map<String, Value>,
        diff: Vec<Value>,
    },
    Delete {
        language: String,
    },
}

impl Action {
    fn describe(&self) -> Value {
        match self {
            Self::Language {
                language,
                added,
                diff,
                ..
            } => json!({
                "action": if *added { "add_language" } else { "update_language" },
                "language": language,
                "changes": diff,
            }),
            Self::AppInfo { diff, .. } => json!({"action": "update_app_info", "changes": diff}),
            Self::Delete { language } => json!({"action": "delete_language", "language": language}),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Language {
                language,
                added: true,
                ..
            } => format!("added {language}"),
            Self::Language { language, .. } => format!("updated {language}"),
            Self::AppInfo { .. } => "updated the app information".into(),
            Self::Delete { language } => format!("deleted {language}"),
        }
    }
}

/// План: действия, неизменённые и отсутствующие в каталоге языки.
struct Plan {
    actions: Vec<Action>,
    unchanged: Vec<String>,
    missing: Vec<String>,
}

fn plan(
    local: &LocalListing,
    remote: &Remote,
    apply_app_info: bool,
    delete_missing: bool,
) -> Result<Plan, ToolError> {
    let mut errors = Vec::new();
    let mut actions = Vec::new();
    let mut unchanged = Vec::new();

    for language in &local.languages {
        let code = &language.language;
        let existing = remote.languages.get(code);
        if existing.is_none() && !is_known_language(code) {
            let hint = match suggest_language(code) {
                Some(known) => format!("; did you mean `{known}`?"),
                None => "; use an AppGallery Connect language code such as en-US, ru-RU, fil, \
                         es-419 or he_IL"
                    .into(),
            };
            errors.push(format!(
                "{code}/: not an AppGallery Connect language code{hint}"
            ));
            continue;
        }
        let mut body = Map::new();
        body.insert("lang".into(), json!(code));
        let mut diff = Vec::new();
        for (_, field, _) in LANGUAGE_FIELDS {
            let old = existing.map(|l| text_of(l, field)).unwrap_or_default();
            match language.fields.get(field) {
                Some(new) => {
                    if *new != old {
                        diff.push(text_diff(field, &old, new));
                    }
                    if existing.is_some() || !new.is_empty() {
                        body.insert((*field).into(), json!(new));
                    }
                }
                None if !old.is_empty() => {
                    body.insert((*field).into(), json!(old));
                }
                None => {}
            }
        }
        match existing {
            None => {
                if !language.fields.contains_key("appName") {
                    errors.push(format!(
                        "{code}/title.txt is missing: AppGallery Connect needs the app name to \
                         add a language"
                    ));
                    continue;
                }
                actions.push(Action::Language {
                    language: code.clone(),
                    added: true,
                    body,
                    diff,
                });
            }
            Some(_) if diff.is_empty() => unchanged.push(code.clone()),
            Some(_) => actions.push(Action::Language {
                language: code.clone(),
                added: false,
                body,
                diff,
            }),
        }
    }

    let local_codes: BTreeSet<&str> = local
        .languages
        .iter()
        .map(|l| l.language.as_str())
        .collect();
    let mut default_language = remote.app_text("defaultLang");
    if apply_app_info {
        let mut body = Map::new();
        let mut diff = Vec::new();
        for (_, field, _) in APP_FIELDS {
            let Some(new) = local.app.get(field) else {
                continue;
            };
            let old = remote.app_text(field);
            if *new != old {
                diff.push(text_diff(field, &old, new));
                body.insert((*field).into(), json!(new));
            }
        }
        if let Some(new_default) = local.app.get("defaultLang") {
            if !remote.languages.contains_key(new_default)
                && !local_codes.contains(new_default.as_str())
            {
                errors.push(format!(
                    "default_language.txt: `{new_default}` is neither a language of the app nor \
                     a language directory"
                ));
            }
            default_language = new_default.clone();
        }
        if !diff.is_empty() {
            let privacy = local
                .app
                .get("privacyPolicy")
                .cloned()
                .unwrap_or_else(|| remote.app_text("privacyPolicy"));
            if privacy.is_empty() {
                errors.push(
                    "privacy_policy.txt: AppGallery Connect needs the privacy policy URL with \
                     every app information update and the app has none; add the file"
                        .into(),
                );
            }
            body.insert("privacyPolicy".into(), json!(privacy));
            actions.push(Action::AppInfo { body, diff });
        }
    }

    let missing: Vec<String> = remote
        .languages
        .keys()
        .filter(|code| !local_codes.contains(code.as_str()))
        .cloned()
        .collect();
    if delete_missing {
        for code in &missing {
            if *code == default_language {
                errors.push(format!(
                    "{code}: the default language cannot be deleted; add its directory or \
                     change default_language.txt"
                ));
                continue;
            }
            actions.push(Action::Delete {
                language: code.clone(),
            });
        }
    }

    if !errors.is_empty() {
        return Err(ToolError::new(format!(
            "the listing directory does not pass the AppGallery Connect checks; nothing was \
             sent:\n- {}",
            errors.join("\n- ")
        )));
    }
    Ok(Plan {
        actions,
        unchanged,
        missing,
    })
}

async fn apply(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let app_id = listing::app_id(ctx, &args)?;
    let dir = required_str(&args, "dir")?;
    let dry_run = bool_arg(&args, "dry_run")?.unwrap_or(true);
    let delete_missing = bool_arg(&args, "delete_missing_languages")?.unwrap_or(false);
    let apply_app_info = bool_arg(&args, "apply_app_info")?.unwrap_or(true);
    let release_type = release_type(&args)?;
    if !dry_run && !ctx.access.allows(AccessLevel::Write) {
        return Err(ToolError::new(format!(
            "applying the listing needs {}; dry_run works without. {}",
            access_requirement(AccessLevel::Write, ctx.http),
            access_hint(AccessLevel::Write, ctx.http),
        )));
    }

    let local = listing::read_listing(ctx, dir)?;
    let remote = listing::fetch_remote(ctx, &app_id, release_type).await?;
    let plan = plan(&local, &remote, apply_app_info, delete_missing)?;

    let deletes = plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::Delete { .. }));
    let can_delete = ctx.access.allows(AccessLevel::Destructive);
    if deletes && !can_delete && !dry_run {
        return Err(ToolError::new(format!(
            "deleting languages needs {}; nothing was sent: {}, or call again without \
             delete_missing_languages to apply the other changes",
            access_requirement(AccessLevel::Destructive, ctx.http),
            access_action(AccessLevel::Destructive, ctx.http),
        )));
    }

    let mut result = Map::new();
    result.insert("appId".into(), json!(app_id));
    result.insert("dry_run".into(), json!(dry_run));
    result.insert(
        "changes".into(),
        Value::Array(plan.actions.iter().map(Action::describe).collect()),
    );
    result.insert("unchanged_languages".into(), json!(plan.unchanged));
    if !plan.missing.is_empty() {
        result.insert("not_in_directory".into(), json!(plan.missing));
    }
    if !local.skipped.is_empty() {
        result.insert("skipped".into(), json!(local.skipped));
    }
    let mut notes = Vec::new();
    if !plan.missing.is_empty() && !delete_missing {
        notes.push(format!(
            "the languages in not_in_directory stay as they are; pass \
             delete_missing_languages to delete them (needs {})",
            access_requirement(AccessLevel::Destructive, ctx.http),
        ));
    }
    if deletes && !can_delete {
        notes.push(format!(
            "deleting languages needs {}; applying this plan is refused here",
            access_requirement(AccessLevel::Destructive, ctx.http),
        ));
    }
    if !notes.is_empty() {
        result.insert("notes".into(), json!(notes));
    }

    if plan.actions.is_empty() {
        result.insert(
            "next_step".into(),
            json!(
                "The listing in AppGallery Connect already matches the directory; nothing to apply."
            ),
        );
        return Ok(Value::Object(result));
    }
    if dry_run {
        result.insert(
            "next_step".into(),
            json!("Show the changes to the user; call listing_apply again with dry_run false to apply them."),
        );
        return Ok(Value::Object(result));
    }

    // Языки → основные сведения → удаления (план уже в этом порядке).
    let mut ordered: Vec<&Action> = Vec::new();
    ordered.extend(
        plan.actions
            .iter()
            .filter(|a| matches!(a, Action::Language { .. })),
    );
    ordered.extend(
        plan.actions
            .iter()
            .filter(|a| matches!(a, Action::AppInfo { .. })),
    );
    ordered.extend(
        plan.actions
            .iter()
            .filter(|a| matches!(a, Action::Delete { .. })),
    );
    let mut done: Vec<String> = Vec::new();
    for action in ordered {
        if let Err(err) = execute(ctx, &app_id, release_type, action).await {
            let applied = if done.is_empty() {
                "Nothing was applied before the failure.".to_owned()
            } else {
                format!("Already applied: {}.", done.join(", "))
            };
            return Err(ToolError::new(format!(
                "{err}\n\n{applied} Call listing_apply again with dry_run true to see what is \
                 left."
            )));
        }
        done.push(action.label());
    }
    result.insert("applied".into(), json!(done));
    result.insert(
        "next_step".into(),
        json!("The changes are in the draft version; they reach users after the app is submitted for review (submit_app) and approved."),
    );
    Ok(Value::Object(result))
}

async fn execute(
    ctx: &Context,
    app_id: &str,
    release_type: Option<u64>,
    action: &Action,
) -> Result<(), ToolError> {
    let (operation, args, what) = match action {
        Action::Language { language, body, .. } => {
            let mut args = app_args(app_id, release_type);
            args.insert("body".into(), Value::Object(body.clone()));
            (LANGUAGE_UPDATE, args, format!("updating {language}"))
        }
        Action::AppInfo { body, .. } => {
            let mut args = app_args(app_id, release_type);
            args.insert("body".into(), Value::Object(body.clone()));
            (
                APP_INFO_UPDATE,
                args,
                "updating the app information".to_owned(),
            )
        }
        Action::Delete { language } => {
            let mut args = app_args(app_id, release_type);
            args.insert("lang".into(), json!(language));
            (LANGUAGE_DELETE, args, format!("deleting {language}"))
        }
    };
    let op = listing::operation(operation)?;
    ctx.api
        .call(op, &Value::Object(args))
        .await
        .map_err(|err| ToolError::new(format!("{what} failed: {}", describe_api_error(&err))))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::listing::testing::{
        APP_INFO, LANGUAGE_INFO, context, mount_app, ok, writes,
    };

    async fn mount_writes(server: &MockServer) {
        for verb in ["PUT", "DELETE"] {
            Mock::given(method(verb))
                .and(path(LANGUAGE_INFO))
                .respond_with(ok(json!({})))
                .mount(server)
                .await;
        }
        Mock::given(method("PUT"))
            .and(path(APP_INFO))
            .respond_with(ok(json!({})))
            .mount(server)
            .await;
    }

    async fn exported(server: &MockServer, dir: &std::path::Path) {
        let ctx = context(server, dir, AccessLevel::Read);
        let export = crate::composite::listing_export::tools()[0].run;
        export(&ctx, json!({"dir": "listing", "include_images": false}))
            .await
            .unwrap();
    }

    fn bodies(requests: &[wiremock::Request], verb: &str, at: &str) -> Vec<Value> {
        requests
            .iter()
            .filter(|r| r.method.as_str() == verb && r.url.path() == at)
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect()
    }

    #[tokio::test]
    async fn export_then_apply_has_no_changes() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        exported(&server, dir.path()).await;
        let ctx = context(&server, dir.path(), AccessLevel::Destructive);
        for dry_run in [true, false] {
            let result = apply(&ctx, json!({"dir": "listing", "dry_run": dry_run}))
                .await
                .unwrap();
            assert_eq!(result["changes"], json!([]), "{result}");
            assert_eq!(result["unchanged_languages"], json!(["en-US", "ru-RU"]));
        }
        assert!(writes(&server).await.is_empty());
    }

    #[tokio::test]
    async fn updates_only_the_changed_language() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        mount_writes(&server).await;
        let dir = tempfile::tempdir().unwrap();
        exported(&server, dir.path()).await;
        let short = dir.path().join("listing/ru-RU/short_description.txt");
        std::fs::remove_file(&short).unwrap();
        std::fs::write(&short, "Новое краткое\r\n").unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);

        // По умолчанию — только план.
        let plan = apply(&ctx, json!({"dir": "listing"})).await.unwrap();
        assert_eq!(plan["dry_run"], true);
        assert_eq!(
            plan["changes"],
            json!([{"action": "update_language", "language": "ru-RU", "changes": [
                {"field": "briefInfo", "old": "Кратко", "new": "Новое краткое"}
            ]}]),
            "{plan}"
        );
        assert_eq!(plan["unchanged_languages"], json!(["en-US"]));
        assert!(writes(&server).await.is_empty());

        let result = apply(&ctx, json!({"dir": "listing", "dry_run": false}))
            .await
            .unwrap();
        assert_eq!(result["applied"], json!(["updated ru-RU"]), "{result}");
        let requests = server.received_requests().await.unwrap();
        let updates = bodies(&requests, "PUT", LANGUAGE_INFO);
        assert_eq!(
            updates,
            vec![json!({
                "lang": "ru-RU",
                "appName": "Пример",
                "briefInfo": "Новое краткое",
                "appDesc": "Строка",
                "newFeatures": "Исправления",
            })]
        );
        assert!(bodies(&requests, "PUT", APP_INFO).is_empty());
        assert_eq!(writes(&server).await.len(), 1);
    }

    #[tokio::test]
    async fn adds_a_language_and_updates_the_app_information() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        mount_writes(&server).await;
        let dir = tempfile::tempdir().unwrap();
        exported(&server, dir.path()).await;
        let root = dir.path().join("listing");
        std::fs::create_dir(root.join("de-DE")).unwrap();
        std::fs::write(root.join("de-DE/title.txt"), "Beispiel\n").unwrap();
        std::fs::write(
            root.join("privacy_rights_url.txt"),
            "https://example.com/rights",
        )
        .unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        let result = apply(&ctx, json!({"dir": "listing", "dry_run": false}))
            .await
            .unwrap();
        assert_eq!(
            result["applied"],
            json!(["added de-DE", "updated the app information"]),
            "{result}"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            bodies(&requests, "PUT", LANGUAGE_INFO),
            vec![json!({"lang": "de-DE", "appName": "Beispiel"})]
        );
        assert_eq!(
            bodies(&requests, "PUT", APP_INFO),
            vec![json!({
                "privacyRightsUrl": "https://example.com/rights",
                "privacyPolicy": "https://example.com/privacy",
            })]
        );
    }

    #[tokio::test]
    async fn http_mode_asks_for_an_access_token_instead_of_flags() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        exported(&server, dir.path()).await;
        std::fs::remove_dir_all(dir.path().join("listing/ru-RU")).unwrap();

        let mut read = context(&server, dir.path(), AccessLevel::Read);
        read.http = true;
        let err = apply(&read, json!({"dir": "listing", "dry_run": false}))
            .await
            .unwrap_err();
        let message = err.message();
        assert!(
            message.contains("access token with the write level"),
            "{message}"
        );
        assert!(message.contains("server operator"), "{message}");
        assert!(!message.contains("--allow-"), "{message}");

        let mut write = context(&server, dir.path(), AccessLevel::Write);
        write.http = true;
        let args = json!({"dir": "listing", "delete_missing_languages": true});
        let plan = apply(&write, args.clone()).await.unwrap();
        let notes = plan["notes"].to_string();
        assert!(
            notes.contains("access token with the destructive level"),
            "{notes}"
        );
        assert!(!notes.contains("--allow-"), "{notes}");
        let mut run = args;
        run["dry_run"] = json!(false);
        let err = apply(&write, run).await.unwrap_err();
        let message = err.message();
        assert!(message.contains("server operator"), "{message}");
        assert!(!message.contains("--allow-"), "{message}");
        assert!(writes(&server).await.is_empty());
    }

    #[tokio::test]
    async fn deletes_only_with_the_flag_and_the_destructive_level() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        mount_writes(&server).await;
        let dir = tempfile::tempdir().unwrap();
        exported(&server, dir.path()).await;
        let ru = dir.path().join("listing/ru-RU");
        std::fs::remove_dir_all(&ru).unwrap();

        // Без флага — только список.
        let ctx = context(&server, dir.path(), AccessLevel::Destructive);
        let result = apply(&ctx, json!({"dir": "listing", "dry_run": false}))
            .await
            .unwrap();
        assert_eq!(result["changes"], json!([]), "{result}");
        assert_eq!(result["not_in_directory"], json!(["ru-RU"]));
        assert!(writes(&server).await.is_empty());

        // С флагом на уровне записи: план с пометкой, применение — отказ до записи.
        let write = context(&server, dir.path(), AccessLevel::Write);
        let args = json!({"dir": "listing", "delete_missing_languages": true});
        let plan = apply(&write, args.clone()).await.unwrap();
        assert_eq!(
            plan["changes"],
            json!([{"action": "delete_language", "language": "ru-RU"}])
        );
        assert!(plan["notes"].to_string().contains("--allow-destructive"));
        let mut run = args.clone();
        run["dry_run"] = json!(false);
        let err = apply(&write, run.clone()).await.unwrap_err();
        assert!(err.message().contains("--allow-destructive"), "{err}");
        assert!(writes(&server).await.is_empty());

        // С флагом на уровне удаления.
        let result = apply(&ctx, run).await.unwrap();
        assert_eq!(result["applied"], json!(["deleted ru-RU"]), "{result}");
        let requests = server.received_requests().await.unwrap();
        let deletes: Vec<_> = requests
            .iter()
            .filter(|r| r.method.as_str() == "DELETE")
            .collect();
        assert_eq!(deletes.len(), 1);
        assert!(deletes[0].url.query().unwrap().contains("lang=ru-RU"));

        // Язык по умолчанию не удаляется.
        std::fs::remove_dir_all(dir.path().join("listing/en-US")).unwrap();
        std::fs::create_dir(&ru).unwrap();
        std::fs::write(ru.join("title.txt"), "Пример").unwrap();
        std::fs::remove_file(dir.path().join("listing/default_language.txt")).unwrap();
        let err = apply(
            &ctx,
            json!({"dir": "listing", "delete_missing_languages": true}),
        )
        .await
        .unwrap_err();
        assert!(
            err.message().contains("default language cannot be deleted"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn checks_fail_before_writes() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("listing");
        std::fs::create_dir_all(root.join("en-US")).unwrap();
        std::fs::write(root.join("en-US/title.txt"), "x".repeat(65)).unwrap();
        std::fs::write(root.join("en-US/release_notes.txt"), "я".repeat(501)).unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        let err = apply(&ctx, json!({"dir": "listing", "dry_run": false}))
            .await
            .unwrap_err();
        let message = err.message();
        assert!(
            message.contains("en-US/title.txt: 65 characters"),
            "{message}"
        );
        assert!(
            message.contains("release_notes.txt: 501 characters"),
            "{message}"
        );
        // Длины — до любых запросов.
        assert!(server.received_requests().await.unwrap().is_empty());

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("ru_RU")).unwrap();
        std::fs::write(root.join("ru_RU/title.txt"), "Пример").unwrap();
        std::fs::create_dir_all(root.join("xx-YY")).unwrap();
        std::fs::write(root.join("xx-YY/title.txt"), "X").unwrap();
        std::fs::create_dir_all(root.join("fr-FR")).unwrap();
        std::fs::write(root.join("fr-FR/short_description.txt"), "Court").unwrap();
        let err = apply(&ctx, json!({"dir": "listing", "dry_run": false}))
            .await
            .unwrap_err();
        let message = err.message();
        assert!(message.contains("did you mean `ru-RU`"), "{message}");
        assert!(
            message.contains("xx-YY/: not an AppGallery Connect language"),
            "{message}"
        );
        assert!(message.contains("fr-FR/title.txt is missing"), "{message}");
        assert!(writes(&server).await.is_empty());

        // Без политики конфиденциальности сведения не обновить.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(APP_INFO))
            .and(query_param("appId", "100000001"))
            .respond_with(ok(
                json!({"appInfo": {"defaultLang": "en-US"}, "languages": [
                    {"lang": "en-US", "appName": "Example"}
                ]}),
            ))
            .mount(&server)
            .await;
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("en-US")).unwrap();
        std::fs::write(root.join("en-US/title.txt"), "Example").unwrap();
        std::fs::write(root.join("developer_website.txt"), "https://example.com").unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        let err = apply(&ctx, json!({"dir": "listing"})).await.unwrap_err();
        assert!(err.message().contains("privacy_policy.txt"), "{err}");
    }

    #[tokio::test]
    async fn failure_lists_what_was_applied() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        Mock::given(method("PUT"))
            .and(path(LANGUAGE_INFO))
            .respond_with(ok(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(APP_INFO))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"ret": {"code": 204144647, "msg": "[cds]update app info failed"}}),
            ))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        exported(&server, dir.path()).await;
        let root = dir.path().join("listing");
        std::fs::remove_file(root.join("en-US/title.txt")).unwrap();
        std::fs::write(root.join("en-US/title.txt"), "Example 2").unwrap();
        std::fs::write(root.join("privacy_rights_url.txt"), "https://example.com/r").unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        let err = apply(&ctx, json!({"dir": "listing", "dry_run": false}))
            .await
            .unwrap_err();
        let message = err.message();
        assert!(
            message.contains("updating the app information failed"),
            "{message}"
        );
        assert!(
            message.contains("Already applied: updated en-US."),
            "{message}"
        );
    }
}
