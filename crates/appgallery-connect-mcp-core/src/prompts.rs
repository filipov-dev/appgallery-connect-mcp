//! MCP-промпты: готовые сценарии для частых задач AppGallery Connect (подготовка и отправка
//! выпуска, поэтапный выпуск, разбор отзывов, обзор товаров и подписок, месячный отчёт,
//! обновление страницы приложения).
//!
//! Контракт с сервером: [`all`] возвращает все промпты, [`available`] — те, что регистрируются
//! при уровне доступа сервера ([`PromptDef::min_access`]). Сервер отдаёт их в `prompts/list`
//! ([`info`]) и рендерит текст в `prompts/get` ([`get`]: неизвестные и недостающие обязательные
//! аргументы отклоняются до рендера). Промпты только описывают шаги для агента и называют
//! инструменты; сами они запросов не делают.
//!
//! Текст зависит от настроек сервера ([`PromptSetup`]): уровня доступа, режима динамических
//! наборов, включённых наборов и приложений профиля. Если уровня доступа не хватает на весь
//! сценарий ([`PromptDef::full_access`]), промпт работает в режиме чтения: инструменты выше
//! уровня сервера помечаются «not available», а агенту велено подготовить остальное и сказать
//! пользователю, какой флаг нужен. Перед действиями уровня удаления промпт требует явного
//! подтверждения пользователя.
//!
//! Тексты — для агента, на английском. Имена инструментов в тексте пишутся в обратных
//! кавычках и (кроме инструментов чтения) с пометкой уровня — через [`Render::tool`]. Тесты
//! проверяют, что каждое такое имя существует, входит в набор из строки «Toolsets used here»,
//! а пометка совпадает с уровнем инструмента и доступностью при уровне сервера.

use std::collections::BTreeMap;

use appgallery_connect_api::spec::{self, AccessLevel};
use rmcp::ErrorData;
use rmcp::model::{
    GetPromptRequestParams, GetPromptResult, JsonObject, Prompt, PromptArgument, PromptMessage,
    Role,
};
use serde_json::Value;

use crate::composite;
use crate::registry::{access_action, access_name, access_requirement};
use crate::toolsets::{ENABLE_TOOLSET_TOOL, LIST_TOOLSETS_TOOL};

/// Аргумент промпта.
#[derive(Debug, Clone, Copy)]
pub struct PromptArg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// Описание промпта.
#[derive(Clone, Copy)]
pub struct PromptDef {
    /// Имя промпта (snake_case).
    pub name: &'static str,
    /// Заголовок для клиента.
    pub title: &'static str,
    /// Описание для клиента и агента (английский).
    pub description: &'static str,
    pub arguments: &'static [PromptArg],
    /// Уровень доступа, без которого промпт не регистрируется: ниже него от сценария не
    /// остаётся полезной части.
    pub min_access: AccessLevel,
    /// Уровень доступа, при котором выполним весь сценарий. От `min_access` до него промпт
    /// работает в режиме чтения (или без шагов уровня удаления).
    pub full_access: AccessLevel,
    /// Наборы инструментов, которые называет промпт.
    pub toolsets: &'static [&'static str],
    /// Текст сообщения пользователя. Обязательные аргументы проверяет [`get`] до вызова.
    pub render: fn(&Render<'_>) -> String,
}

impl std::fmt::Debug for PromptDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptDef")
            .field("name", &self.name)
            .field("min_access", &self.min_access)
            .finish_non_exhaustive()
    }
}

/// Настройки сервера, от которых зависит текст промпта.
#[derive(Debug, Clone)]
pub struct PromptSetup {
    /// Уровень доступа сервера.
    pub access: AccessLevel,
    /// Режим динамических наборов: наборы включает `enable_toolset`.
    pub dynamic: bool,
    /// Наборы, включённые сейчас (в динамическом режиме — в этой сессии).
    pub enabled_toolsets: Vec<String>,
    /// `appId` приложений по умолчанию из профиля.
    pub apps: Vec<String>,
    /// Сервер работает по HTTP (`serve --http`): уровень доступа и наборы задаёт оператор
    /// сервера, поэтому подсказки говорят о токене доступа, а не о флагах запуска.
    pub http: bool,
}

/// Все промпты сервера.
pub fn all() -> Vec<PromptDef> {
    vec![
        PREPARE_RELEASE,
        PHASED_RELEASE,
        TRIAGE_REVIEWS,
        PRODUCTS_OVERVIEW,
        MONTHLY_REPORT,
        LISTING_UPDATE,
    ]
}

/// Промпты, которые регистрируются при уровне доступа `access`.
pub fn available(prompts: &[PromptDef], access: AccessLevel) -> impl Iterator<Item = &PromptDef> {
    prompts
        .iter()
        .filter(move |prompt| access.allows(prompt.min_access))
}

/// Описание промпта для `prompts/list`.
pub fn info(prompt: &PromptDef) -> Prompt {
    let arguments = prompt
        .arguments
        .iter()
        .map(|arg| {
            PromptArgument::new(arg.name)
                .with_description(arg.description)
                .with_required(arg.required)
        })
        .collect();
    let mut info = Prompt::new(prompt.name, Some(prompt.description), Some(arguments));
    if !prompt.title.is_empty() {
        info = info.with_title(prompt.title);
    }
    info
}

/// `prompts/get`: находит промпт по имени среди доступных при уровне сервера, проверяет
/// аргументы и рендерит текст. Промпт выше уровня доступа — ошибка с флагом, который его
/// включает.
pub fn get(
    prompts: &[PromptDef],
    setup: &PromptSetup,
    request: GetPromptRequestParams,
) -> Result<GetPromptResult, ErrorData> {
    let Some(prompt) = prompts.iter().find(|p| p.name == request.name) else {
        let known: Vec<String> = available(prompts, setup.access)
            .map(|p| format!("`{}`", p.name))
            .collect();
        return Err(ErrorData::invalid_params(
            format!(
                "unknown prompt `{}`; known prompts: {}",
                request.name,
                if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                }
            ),
            None,
        ));
    };
    if !setup.access.allows(prompt.min_access) {
        return Err(ErrorData::invalid_params(
            format!(
                "prompt `{}` needs {} access: the server runs with {} access; {}",
                prompt.name,
                access_name(prompt.min_access),
                access_name(setup.access),
                access_action(prompt.min_access, setup.http)
            ),
            None,
        ));
    }
    let values = arguments(prompt, request.arguments)?;
    let text = (prompt.render)(&Render {
        prompt,
        args: &values,
        setup,
    });
    let mut result = GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]);
    result.description = Some(prompt.description.to_owned());
    Ok(result)
}

/// Аргументы промпта строками: неизвестные и недостающие обязательные отклоняются, пустые
/// значения считаются отсутствующими.
fn arguments(
    prompt: &PromptDef,
    args: Option<JsonObject>,
) -> Result<BTreeMap<String, String>, ErrorData> {
    let mut values = BTreeMap::new();
    for (name, value) in args.unwrap_or_default() {
        if !prompt.arguments.iter().any(|arg| arg.name == name) {
            let allowed: Vec<String> = prompt
                .arguments
                .iter()
                .map(|arg| format!("`{}`", arg.name))
                .collect();
            return Err(ErrorData::invalid_params(
                format!(
                    "prompt `{}` has no argument `{name}`; its arguments: {}",
                    prompt.name,
                    if allowed.is_empty() {
                        "none".to_owned()
                    } else {
                        allowed.join(", ")
                    }
                ),
                None,
            ));
        }
        let text = match value {
            Value::Null => continue,
            Value::String(text) => text,
            other => other.to_string(),
        };
        if !text.trim().is_empty() {
            values.insert(name, text);
        }
    }
    let missing: Vec<String> = prompt
        .arguments
        .iter()
        .filter(|arg| arg.required && !values.contains_key(arg.name))
        .map(|arg| format!("`{}`", arg.name))
        .collect();
    if !missing.is_empty() {
        return Err(ErrorData::invalid_params(
            format!(
                "prompt `{}` needs argument(s): {}",
                prompt.name,
                missing.join(", ")
            ),
            None,
        ));
    }
    Ok(values)
}

/// Уровень доступа и набор инструмента: операция таблицы или составной инструмент.
fn tool_info(name: &str) -> Option<(AccessLevel, &'static str)> {
    spec::find(name)
        .map(|op| (op.access, op.toolset))
        .or_else(|| {
            composite::all()
                .into_iter()
                .find(|tool| tool.name == name)
                .map(|tool| (tool.access, tool.toolset))
        })
}

/// Контекст рендера: промпт, его аргументы и настройки сервера.
pub struct Render<'a> {
    prompt: &'a PromptDef,
    args: &'a BTreeMap<String, String>,
    setup: &'a PromptSetup,
}

impl Render<'_> {
    /// Значение аргумента без пробелов по краям; пустая строка — аргумента нет.
    fn arg(&self, name: &str) -> Option<&str> {
        self.args
            .get(name)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    }

    /// Обязательный аргумент. Сервер проверяет его до рендера; если его всё же нет, в тексте
    /// остаётся просьба уточнить значение у пользователя.
    fn required(&self, name: &str) -> &str {
        self.arg(name).unwrap_or("<ask the user>")
    }

    fn allows(&self, level: AccessLevel) -> bool {
        self.setup.access.allows(level)
    }

    /// Имя инструмента в обратных кавычках с пометкой уровня: у инструментов чтения пометки
    /// нет, у недоступных при уровне сервера — «not available» и флаг.
    fn tool(&self, name: &str) -> String {
        match tool_info(name) {
            Some((AccessLevel::Read, _)) | None => format!("`{name}`"),
            Some((level, _)) if self.allows(level) => format!("`{name}` ({})", level.as_str()),
            Some((level, _)) => format!(
                "`{name}` ({}; not available on this server, needs {})",
                level.as_str(),
                self.needs(level)
            ),
        }
    }

    /// Где работают локальные файлы: в HTTP-режиме — каталоги на сервере.
    fn files_note(&self) -> &'static str {
        if self.setup.http {
            HTTP_FILES_NOTE
        } else {
            FILES_NOTE
        }
    }

    /// Что нужно для уровня `level` («needs …»): права профиля сервера или, в HTTP-режиме, токен
    /// доступа с этим уровнем.
    fn needs(&self, level: AccessLevel) -> String {
        access_requirement(level, self.setup.http)
    }

    /// Приложение сценария для первой фразы.
    fn app(&self) -> String {
        match (self.arg("appId"), self.setup.apps.as_slice()) {
            (Some(id), _) => format!("app {id}"),
            (None, [id]) => format!("the profile's default app {id}"),
            (None, _) => "the app the user names".to_owned(),
        }
    }

    /// Абзац о том, как передавать `appId`.
    fn app_note(&self) -> String {
        match (self.arg("appId"), self.setup.apps.as_slice()) {
            (Some(id), _) => format!("App: pass appId {id} to every tool below."),
            (None, [id]) => format!(
                "App: the tools below use the profile's default app {id} when appId is omitted."
            ),
            (None, []) => format!(
                "App: ask the user which app, or find its numeric appId by app name or package \
                 name with {}; pass appId to every tool below.",
                self.tool("search_apps")
            ),
            (None, apps) => format!(
                "App: the profile has several default apps ({}); ask the user which one (or find \
                 it with {}) and pass appId to every tool below.",
                apps.join(", "),
                self.tool("search_apps")
            ),
        }
    }

    /// Наборы, уровень доступа и правило подтверждения: общее начало всех промптов.
    fn header(&self) -> String {
        let prompt = self.prompt;
        let mut out = format!("Toolsets used here: {}.", prompt.toolsets.join(", "));
        let missing: Vec<&str> = prompt
            .toolsets
            .iter()
            .copied()
            .filter(|slug| !self.setup.enabled_toolsets.iter().any(|t| t == slug))
            .collect();
        if !missing.is_empty() {
            let list = missing.join(", ");
            if self.setup.dynamic {
                out.push_str(&format!(
                    " Not enabled yet: {list}. Call {} with toolset \"{}\" before using their \
                     tools ({} shows what is enabled).",
                    self.tool(ENABLE_TOOLSET_TOOL),
                    missing.join(","),
                    self.tool(LIST_TOOLSETS_TOOL)
                ));
            } else if self.setup.http {
                out.push_str(&format!(
                    " Not enabled on this server: {list}, so their tools are not registered. The \
                     server operator sets the toolsets of this remote server; until they change \
                     them call their operations with {} (composite tools cannot be called that \
                     way).",
                    self.tool("api_request")
                ));
            } else {
                out.push_str(&format!(
                    " Not enabled on this server: {list}, so their tools are not registered. Ask \
                     the user to restart the server with --toolsets that include them (or \
                     --toolsets all); until then call their operations with {} (composite tools \
                     cannot be called that way).",
                    self.tool("api_request")
                ));
            }
        }

        let full = prompt.full_access;
        let current = self.setup.access;
        out.push_str(&format!(
            "\n\nAccess: the whole scenario needs {} access{}; this server runs with {} access.",
            access_name(full),
            if full == AccessLevel::Read {
                String::new()
            } else {
                format!(" ({})", self.needs(full))
            },
            access_name(current)
        ));
        if !current.allows(full) {
            out.push_str(
                " Tools marked \"not available\" are not registered here: do the steps you can, \
                 prepare the rest (texts, values, the exact arguments) and tell the user that \
                 these steps need what is shown in parentheses, or that they can do them \
                 in AppGallery Connect. Do not try to get around the access level with other \
                 tools.",
            );
        }
        if current.allows(AccessLevel::Destructive) && full == AccessLevel::Destructive {
            out.push(' ');
            out.push_str(CONFIRM_NOTE);
        }
        out.push_str("\n\n");
        out.push_str(&self.app_note());
        out
    }
}

const CONFIRM_NOTE: &str = "Destructive steps change what AppGallery users see or pay: call a \
destructive tool only after the user's explicit confirmation of that exact change in this \
conversation, run it with dryRun true first when the tool has it and show the result, and never \
repeat a change that already succeeded.";

const FILES_NOTE: &str = "Local files work only inside the directories the server allows (the \
working directory, the temporary directory and the extra directories of the server's \
profile); existing files are never overwritten.";

const HTTP_FILES_NOTE: &str = "Files are on the remote server, not on the user's machine, and \
work only inside the directories the server operator allows; existing files are never \
overwritten.";

const APP_ID: PromptArg = PromptArg {
    name: "appId",
    description: "Numeric AppGallery app ID, like 100000001. Optional when the profile has \
                  exactly one default app; otherwise the agent asks or finds it with \
                  search_apps.",
    required: false,
};

const COUNTRIES: PromptArg = PromptArg {
    name: "countries",
    description: "Country or region codes of the reviews, comma-separated, like RU,BY; default — \
                  ask the user or take the release countries of the app.",
    required: false,
};

/// Страны отзывов: из аргумента или как их найти.
fn countries_text(r: &Render<'_>) -> String {
    match r.arg("countries") {
        Some(c) => format!("countries [{c}]"),
        None => format!(
            "countries the user names (ask; the release countries are in publishCountry of {}; \
             one call covers only countries served by the same AppGallery Connect site)",
            r.tool("get_app_info")
        ),
    }
}

// ---------------------------------------------------------------------------------------
// prepare_release
// ---------------------------------------------------------------------------------------

const PREPARE_RELEASE: PromptDef = PromptDef {
    name: "prepare_release",
    title: "Prepare and submit an AppGallery release",
    description: "Check the release state, upload the package, update the release notes in \
                  every language, check that the version is ready and submit it for review only \
                  after the user's explicit confirmation. On a read-only server it checks \
                  readiness and prepares the steps.",
    arguments: &[
        APP_ID,
        PromptArg {
            name: "file",
            description: "Path to the package to upload (.apk, .aab, .rpk, .app or .exe); \
                          default — ask the user, or use a package already uploaded.",
            required: false,
        },
        PromptArg {
            name: "release_type",
            description: "full (default): release to all users after approval; phased: release \
                          to a share of users first (the app must already have a released \
                          version).",
            required: false,
        },
    ],
    min_access: AccessLevel::Read,
    full_access: AccessLevel::Destructive,
    toolsets: &["releases", "listing", "apps"],
    render: render_prepare_release,
};

fn render_prepare_release(r: &Render<'_>) -> String {
    let app = r.app();
    let phased = r
        .arg("release_type")
        .is_some_and(|t| t.eq_ignore_ascii_case("phased"));
    let (file, file_arg) = match r.arg("file") {
        Some(f) => (format!("the file {f}"), format!("file {f}")),
        None => (
            "the package the user names (ask for the path)".to_owned(),
            "file set to the path the user gives".to_owned(),
        ),
    };
    let header = r.header();
    let release_status = r.tool("release_status");
    let package_upload = r.tool("package_upload");
    let list_packages = r.tool("list_packages");
    let compile_status = r.tool("get_package_compile_status");
    let listing_export = r.tool("listing_export");
    let listing_apply = r.tool("listing_apply");
    let release_submit = r.tool("release_submit");
    let no_write = if r.allows(AccessLevel::Write) {
        ""
    } else {
        " Without write access: check with the package list whether the user has already \
         uploaded the build in AppGallery Connect; otherwise tell the user how to upload it."
    };
    let no_write_notes = if r.allows(AccessLevel::Write) {
        ""
    } else {
        " Without write access prepare the texts per language and give them to the user."
    };
    let mode = if phased {
        "releaseType phased with phasedPercent, phasedStart and phasedEnd (ask the user; times \
         with a UTC offset and in the future); the app must already have a released version"
    } else {
        "releaseType full (and releaseTime if the user wants a scheduled release)"
    };
    let no_destructive = if r.allows(AccessLevel::Destructive) {
        String::new()
    } else {
        format!(
            " Without destructive access: tell the user that the version is ready and that \
             submitting needs {}, or the submit button in AppGallery Connect.",
            r.needs(AccessLevel::Destructive)
        )
    };
    let files_note = r.files_note();
    format!(
        "Prepare the next AppGallery release of {app} from {file} and submit it for review. \
Report every step as done / missing / needs the user.

{header}

{files_note}

1. Current state: call {release_status}. Note the version being prepared (name, code, review \
and release state, the latest review comment), the released version, the phased release and \
readyToSubmit. If a version is in review, stop and tell the user: {release_submit} refuses a \
version in review, so wait until the review ends. If the latest review comment reports a rejection, show \
it to the user first.
2. Package: upload the build with {package_upload}, {file_arg}: first with dryRun true to \
check the file, then with dryRun false. For AAB the tool waits until AppGallery Connect has \
processed the package (waitSeconds, default 300). Check packageStatus and readyForReview in the \
result. If the build is already uploaded, find it with {list_packages}; {compile_status} shows \
its processing state.{no_write}
3. Release notes (\"What's new\", at most 500 characters per language): ask the user for the \
notes or take them from the changelog they point to, one text per store language. Export the \
listing with {listing_export} into a new directory, edit <lang>/release_notes.txt, then call \
{listing_apply} with that directory and dry_run true to show the differences and, after the \
user approves the texts, with dry_run false. The texts apply to the version being \
prepared.{no_write_notes}
4. Readiness: call {release_status} again. readyToSubmit must be true and the package of the \
current version processed; if it is still processing, wait and check again instead of \
submitting.
5. Summary: show the user what will be submitted: app, version name and code, package, release \
notes languages, release mode and time. Ask for an explicit confirmation.
6. Submit (only after the user's explicit yes): {release_submit} with {mode}. Run it with \
dryRun true first and show the request it would send; then with dryRun false. A submission \
cannot be withdrawn through the API, and after approval the version reaches AppGallery \
users.{no_destructive}
7. Afterwards: follow the review with {release_status}. For a phased release continue with \
the phased_release prompt."
    )
}

// ---------------------------------------------------------------------------------------
// phased_release
// ---------------------------------------------------------------------------------------

const PHASED_RELEASE: PromptDef = PromptDef {
    name: "phased_release",
    title: "Run a phased release",
    description: "Follow a phased release on AppGallery: check reviews and installation \
                  failures between the steps, widen the share of users step by step, pause it \
                  when problems grow and release to everyone at the end. Every change is \
                  confirmed with the user; on a server without destructive access it only \
                  monitors and recommends.",
    arguments: &[
        APP_ID,
        PromptArg {
            name: "steps",
            description: "Shares of users in percent, comma-separated (default 1, 5, 10, 20, \
                          50), then release to everyone.",
            required: false,
        },
        COUNTRIES,
    ],
    min_access: AccessLevel::Read,
    full_access: AccessLevel::Destructive,
    toolsets: &["releases", "reviews", "reports", "apps"],
    render: render_phased_release,
};

fn render_phased_release(r: &Render<'_>) -> String {
    let app = r.app();
    let steps = r.arg("steps").unwrap_or("1, 5, 10, 20, 50");
    let countries = countries_text(r);
    let header = r.header();
    let release_status = r.tool("release_status");
    let release_submit = r.tool("release_submit");
    let release_manage = r.tool("release_manage");
    let reviews_summary = r.tool("reviews_summary");
    let get_review = r.tool("get_review");
    let report_download = r.tool("report_download");
    let no_destructive = if r.allows(AccessLevel::Destructive) {
        String::new()
    } else {
        format!(
            "\n\nThis server cannot change the release: monitor it, recommend the next step and \
             tell the user to make the change in AppGallery Connect or with {}.",
            r.needs(AccessLevel::Destructive)
        )
    };
    format!(
        "Run the phased release of {app}: {steps} percent of users, then everyone.

{header}{no_destructive}

1. State: call {release_status}. Note the phased release state (DRAFT — not started yet, \
RELEASE — running, SUSPEND — paused, CANCEL — cancelled), its share of users, start and end, \
and the version. If there is no phased release yet: a version being prepared starts one when \
it is submitted with {release_submit} and releaseType phased (the app must already have a \
released version; see the prepare_release prompt); a version in review has to wait for the \
review.
2. Baseline: call {reviews_summary} with {countries} and days 14: the rating distribution, the \
average rating and the 1-2 star reviews by app version. Call {report_download} with report \
install_failures and with report installs for the last 7 days and note the totals (failed \
installations, uninstalls). This server has no crash statistics: reviews and \
installation failures are the signals.
3. Wait: agree with the user how long each step runs (usually one to three days) before the \
next check.
4. Check: repeat {reviews_summary} for the days since the last step and {report_download} \
with report install_failures, and compare the new version with the baseline. Read single \
reviews with {get_review} when the summary is not enough.
5. Stop rule: if the new version gets clearly worse ratings, new complaints about crashes or \
broken features, or installation failures grow, recommend pausing: {release_manage} with \
action suspend (dryRun true first, then for real after the user's explicit yes). Continue \
later with action resume once the cause is fixed. The API cannot cancel a phased release.
6. Next step: if the signals are fine, propose the next share from the list and call \
{release_manage} with action update_phased and phasedPercent (it can only grow and stays below \
100): dryRun true first, then for real after the user's explicit yes. If phasedEnd is close, \
move it later with phasedEnd in the same action.
7. Last step: {release_manage} with action complete releases the version to everyone (after \
the user's explicit yes).
8. After every step report the share of users, the signals compared with the baseline and the \
time of the next check. Never widen the release without the user's confirmation."
    )
}

// ---------------------------------------------------------------------------------------
// triage_reviews
// ---------------------------------------------------------------------------------------

const TRIAGE_REVIEWS: PromptDef = PromptDef {
    name: "triage_reviews",
    title: "Triage user reviews",
    description: "Summarize recent AppGallery reviews, group them by theme, rating and app \
                  version, and draft replies. Posts a reply only after the user approves its \
                  exact text and only on a server with destructive access; otherwise hands the \
                  drafts to the user.",
    arguments: &[
        APP_ID,
        COUNTRIES,
        PromptArg {
            name: "days",
            description: "Period in days, ending now (default 30).",
            required: false,
        },
    ],
    min_access: AccessLevel::Read,
    full_access: AccessLevel::Destructive,
    toolsets: &["reviews", "releases", "apps"],
    render: render_triage_reviews,
};

fn render_triage_reviews(r: &Render<'_>) -> String {
    let app = r.app();
    let days = r.arg("days").unwrap_or("30");
    let countries = countries_text(r);
    let header = r.header();
    let reviews_summary = r.tool("reviews_summary");
    let list_reviews = r.tool("list_reviews");
    let get_review = r.tool("get_review");
    let release_status = r.tool("release_status");
    let reply_review = r.tool("reply_review");
    let posting = if r.allows(AccessLevel::Destructive) {
        format!(
            "6. Posting (only approved texts): {reply_review} with requestId (a new unique value \
for every call, like a UUID) and body with appId, reviewId, countryCode (the country of the \
review), lang (the language of the reply joined with a country code by an underscore, like \
ru_RU) and devReplyContent (the approved text); to change an earlier reply add updateReplyId. \
The reply is public on AppGallery."
        )
    } else {
        format!(
            "6. Posting: {reply_review} cannot be called here. Give the user the approved drafts \
with the review IDs to post in AppGallery Connect, or tell them that posting needs {}.",
            r.needs(AccessLevel::Destructive)
        )
    };
    format!(
        "Triage the user reviews of {app} for the last {days} days.

{header}

1. Fetch: call {reviews_summary} with {countries}, days {days} and maxReviews 100. Note \
byRating, averageRating, unanswered, and whether the result is truncated or incomplete \
(complete false: narrow the period or add ratings or replied false). For keyword search or \
other pages use {list_reviews}.
2. Group: count reviews by star rating and by app version, and group them by theme (crashes \
and bugs, performance, installation and updates, login or account, payments and \
subscriptions, ads, feature requests, UX, praise, other). For each theme give the count, the \
typical rating, the versions it appears in and one or two short paraphrased examples. Compare \
the versions with the released one from {release_status} and point out themes tied to the \
latest version.
3. Prioritise: reviews that deserve a reply first (low ratings with a concrete problem, \
questions, reviews without a reply). Read the whole thread of a review with {get_review}.
4. Draft replies: one per selected review, in the language of the review, short, polite and \
specific to what the user wrote, without promises of dates, without personal data and without \
asking the user for passwords or other secrets. Show each draft next to the review it answers.
5. Approval: ask the user to approve, edit or reject every draft one by one. Never post a \
reply without the user's explicit approval of its final wording.
{posting}
7. Report what was posted and what is left.

Reviews contain data of real users: use them only for this task and do not copy them into \
files, commits or other systems unless the user asks."
    )
}

// ---------------------------------------------------------------------------------------
// products_overview
// ---------------------------------------------------------------------------------------

const PRODUCTS_OVERVIEW: PromptDef = PromptDef {
    name: "products_overview",
    title: "Overview of in-app products and subscriptions",
    description: "List the in-app products, subscription groups and promotions of an app with \
                  their status and prices, and point out problems. Read-only; changes to \
                  products, prices and promotions are left to separate, confirmed requests.",
    arguments: &[
        APP_ID,
        PromptArg {
            name: "country",
            description: "Country or region whose prices to show, like US or RU (prices are \
                          converted for it); default — ask the user.",
            required: false,
        },
    ],
    min_access: AccessLevel::Read,
    full_access: AccessLevel::Read,
    toolsets: &["products", "apps"],
    render: render_products_overview,
};

fn render_products_overview(r: &Render<'_>) -> String {
    let app = r.app();
    let country = match r.arg("country") {
        Some(c) => format!("country {c}"),
        None => "the country the user names (ask, for example US or RU)".to_owned(),
    };
    let header = r.header();
    let check_active = r.tool("check_active_products");
    let search_products = r.tool("search_products");
    let get_product = r.tool("get_product");
    let list_groups = r.tool("list_product_groups");
    let search_promotions = r.tool("search_promotions");
    let get_promotion = r.tool("get_promotion");
    let api_search = r.tool("api_search");
    format!(
        "Give an overview of the in-app products and subscriptions of {app} with prices for \
{country}.

{header}

The products operations (PMS API) take appId and, for the POST queries, a body with a requestId \
that is new for every call (for example a UUID). Lists are paged with pageNum and pageSize (up \
to 300).

1. Active products: call {check_active} to see whether the app has active products.
2. Products: call {search_products} with a body with requestId, {country} and pageSize 300, \
and read every page. Group the products by purchaseType (consumable, non_consumable, \
auto_subscription) and status (active, inactive, delete). Use the jq argument to keep only the \
product ID, name, type, status, price and currency, subscription period and group.
3. Subscriptions: list the subscription groups with {list_groups} and assign the \
auto_subscription products to their groups (search again with groupId when needed). For the \
details of one product (names per language, prices, subscription settings) call \
{get_product} with productNo and {country}.
4. Promotions: call {search_promotions}, with strategy trial for free trials or introductory \
for introductory prices if the user asks for one kind, and read single promotions with {get_promotion}: product, type, status \
(ongoing, not started, finished), dates and price.
5. Report: a table per type with ID, name, status and price, the subscription groups with \
their products, the running and planned promotions, and problems: inactive products that are \
still referenced, subscriptions without a group, promotions that ended or overlap, missing \
names in some languages.

Changes are not part of this overview: products, prices and promotions change what users pay, \
so their operations are destructive-level. If the user asks for a change, find the operation \
with {api_search} (for example \"activate product\", \"product price\" or \"promotion\"), show the \
exact request and call it only after the user's explicit confirmation; it needs {needs}.",
        needs = r.needs(AccessLevel::Destructive)
    )
}

// ---------------------------------------------------------------------------------------
// monthly_report
// ---------------------------------------------------------------------------------------

const MONTHLY_REPORT: PromptDef = PromptDef {
    name: "monthly_report",
    title: "Monthly installs and revenue report",
    description: "Summarize one month of an app on AppGallery: installs and uninstalls, \
                  in-app purchase revenue, paid app sales and installation failures, from the \
                  AppGallery Connect reports. Read-only.",
    arguments: &[
        APP_ID,
        PromptArg {
            name: "month",
            description: "Month as YYYY-MM; default — the previous month.",
            required: false,
        },
        PromptArg {
            name: "currency",
            description: "Currency of the in-app purchase amounts, like USD (default) or RUB.",
            required: false,
        },
        PromptArg {
            name: "save_dir",
            description: "Local directory for the report files; default — only summaries, no \
                          files.",
            required: false,
        },
    ],
    min_access: AccessLevel::Read,
    full_access: AccessLevel::Read,
    toolsets: &["reports", "apps"],
    render: render_monthly_report,
};

fn render_monthly_report(r: &Render<'_>) -> String {
    let app = r.app();
    let month = match r.arg("month") {
        Some(m) => format!("the month {m}"),
        None => "the previous month (work out YYYY-MM from today's date)".to_owned(),
    };
    let currency = r.arg("currency").unwrap_or("USD");
    let save = match r.arg("save_dir") {
        Some(dir) => format!(
            "Also save every report as a new file in {dir} with save_to (for example \
             installs-YYYY-MM.csv); a summary needs a separate call without save_to."
        ),
        None => "Use the summaries (no save_to); save a file only if the user asks.".to_owned(),
    };
    let header = r.header();
    let app_overview = r.tool("app_overview");
    let report_download = r.tool("report_download");
    let files_note = r.files_note();
    format!(
        "Prepare a report of installs and revenue of {app} for {month}.

{header}

{files_note} {save}

1. App: call {app_overview} to see whether the app is free or paid and which version is \
released.
2. Installs: {report_download} with report installs, from the first and to the last day of \
the month. Note the totals: impressions, page views, new and update downloads, uninstalls.
3. In-app purchases: {report_download} with report in_app_purchases, the same period and \
currency {currency}: paying users and amounts (totalsByCurrency).
4. Paid app: only for a paid app, {report_download} with report paid_app for the same period.
5. Installation failures: {report_download} with report install_failures; name the most \
frequent causes.
6. Comparison (if the user wants it): the same reports for the previous month.
7. Report a short summary with the numbers, the files saved (paths) and caveats: data of the \
current month is incomplete, empty true means AppGallery Connect has no data for the period, \
amounts are in {currency}.

Financial reports are confidential: keep the numbers in this conversation and the saved files \
unless the user asks to share them."
    )
}

// ---------------------------------------------------------------------------------------
// listing_update
// ---------------------------------------------------------------------------------------

const LISTING_UPDATE: PromptDef = PromptDef {
    name: "listing_update",
    title: "Update the store listing from local files",
    description: "Back up the current AppGallery store listing, apply texts and images from a \
                  local directory, show the differences first and apply them only after the \
                  user's confirmation. Needs write access.",
    arguments: &[
        APP_ID,
        PromptArg {
            name: "dir",
            description: "Local listing directory as written by listing_export: \
                          <lang>/title.txt, short_description.txt, full_description.txt, \
                          release_notes.txt and images.",
            required: true,
        },
    ],
    min_access: AccessLevel::Write,
    full_access: AccessLevel::Write,
    toolsets: &["listing", "releases", "apps"],
    render: render_listing_update,
};

fn render_listing_update(r: &Render<'_>) -> String {
    let app = r.app();
    let dir = r.required("dir");
    let header = r.header();
    let release_status = r.tool("release_status");
    let listing_export = r.tool("listing_export");
    let listing_apply = r.tool("listing_apply");
    let image_sync = r.tool("image_sync");
    let delete_languages = if r.allows(AccessLevel::Destructive) {
        "They are kept unless delete_missing_languages is true, which deletes them on AppGallery \
         (destructive): pass it only after the user's explicit confirmation."
            .to_owned()
    } else {
        format!(
            "They are kept: deleting them (delete_missing_languages) needs {}.",
            r.needs(AccessLevel::Destructive)
        )
    };
    let files_note = r.files_note();
    format!(
        "Update the AppGallery store listing of {app} from the directory {dir}.

{header}

{files_note}

1. State: call {release_status}. The listing changes apply to the version being prepared; if \
a version is in review, tell the user before changing anything.
2. Backup: export the current listing with {listing_export} into a new directory next to {dir} \
(for example with the date in its name), with images. Tell the user where it is.
3. Texts: call {listing_apply} with dir {dir} and dry_run true. Show the user the differences \
per language: title (64 characters), short description (80), full description (8000), release \
notes (500) and, with apply_app_info, the default language, privacy policy and websites. Point \
out the languages that exist in AppGallery Connect but not in the directory. \
{delete_languages}
4. Images: call {image_sync} with dir {dir} and dry_run true (its default is false). Show \
which icons, screenshots and videos will be uploaded.
5. Confirm: ask the user to approve the text and image changes. Fix rejected texts in the \
files, not in the requests.
6. Apply (after the user's yes): {listing_apply} with dry_run false, then {image_sync} with \
dry_run false, one after another.
7. Report the applied changes per language. Users see them only after the version is submitted \
and approved: suggest the prepare_release prompt."
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Служебные инструменты сервера (не операции и не составные).
    const SERVER_TOOLS: [&str; 5] = [
        "api_search",
        "api_request",
        LIST_TOOLSETS_TOOL,
        ENABLE_TOOLSET_TOOL,
        "disable_toolset",
    ];

    const LEVELS: [AccessLevel; 3] = [
        AccessLevel::Read,
        AccessLevel::Write,
        AccessLevel::Destructive,
    ];

    fn setup(access: AccessLevel, dynamic: bool, toolsets: &[&str], apps: &[&str]) -> PromptSetup {
        PromptSetup {
            access,
            dynamic,
            enabled_toolsets: toolsets.iter().map(|s| s.to_string()).collect(),
            apps: apps.iter().map(|s| s.to_string()).collect(),
            http: false,
        }
    }

    fn render(prompt: &PromptDef, setup: &PromptSetup, pairs: &[(&str, &str)]) -> String {
        let args: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (prompt.render)(&Render {
            prompt,
            args: &args,
            setup,
        })
    }

    /// Тексты промпта при всех уровнях доступа, в обоих режимах наборов, с приложением и
    /// без, только с обязательными аргументами и со всеми.
    fn renders(prompt: &PromptDef) -> Vec<(PromptSetup, String)> {
        let mut required = Vec::new();
        let mut all_args = Vec::new();
        for arg in prompt.arguments {
            all_args.push((arg.name, "42"));
            if arg.required {
                required.push((arg.name, "42"));
            }
        }
        let mut out = Vec::new();
        for access in LEVELS {
            if !access.allows(prompt.min_access) {
                continue;
            }
            for (dynamic, toolsets) in [
                (false, &["apps", "releases", "reviews"][..]),
                (true, &[][..]),
                (
                    false,
                    &[
                        "apps", "releases", "listing", "reviews", "reports", "products",
                    ][..],
                ),
            ] {
                for apps in [&[][..], &["100000001"][..], &["1", "2"][..]] {
                    let setup = setup(access, dynamic, toolsets, apps);
                    for pairs in [&required, &all_args] {
                        out.push((setup.clone(), render(prompt, &setup, pairs)));
                    }
                }
            }
        }
        out
    }

    /// Слова в обратных кавычках, похожие на имена инструментов (флаги `--...` не берутся),
    /// с текстом после закрывающей кавычки.
    fn backticked(text: &str) -> Vec<(String, String)> {
        let parts: Vec<&str> = text.split('`').collect();
        parts
            .iter()
            .enumerate()
            .skip(1)
            .step_by(2)
            .filter(|(_, w)| {
                !w.is_empty()
                    && w.starts_with(|c: char| c.is_ascii_lowercase())
                    && w.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            })
            .map(|(i, w)| {
                let after = parts.get(i + 1).copied().unwrap_or_default();
                (w.to_string(), after.to_string())
            })
            .collect()
    }

    #[test]
    fn prompts_have_unique_names_and_arguments() {
        let prompts = all();
        let names: BTreeSet<_> = prompts.iter().map(|p| p.name).collect();
        assert_eq!(names.len(), prompts.len());
        let slugs: BTreeSet<&str> = spec::toolsets().iter().map(|t| t.slug).collect();
        for prompt in &prompts {
            assert!(!prompt.title.is_empty() && !prompt.description.is_empty());
            assert!(prompt.full_access >= prompt.min_access, "{}", prompt.name);
            let args: BTreeSet<_> = prompt.arguments.iter().map(|a| a.name).collect();
            assert_eq!(args.len(), prompt.arguments.len(), "{}", prompt.name);
            // appId необязателен: в профиле может быть приложение по умолчанию.
            assert!(
                prompt
                    .arguments
                    .iter()
                    .any(|a| a.name == "appId" && !a.required),
                "{}",
                prompt.name
            );
            for slug in prompt.toolsets {
                assert!(
                    slugs.contains(slug),
                    "{}: unknown toolset {slug}",
                    prompt.name
                );
            }
        }
    }

    #[test]
    fn prompt_tool_names_exist() {
        let known: BTreeSet<String> = spec::operations()
            .iter()
            .map(|op| op.tool_name.to_string())
            .chain(composite::all().iter().map(|t| t.name.to_string()))
            .chain(SERVER_TOOLS.iter().map(|s| s.to_string()))
            .collect();
        for prompt in all() {
            let mut found = BTreeSet::new();
            let mut texts: Vec<String> = renders(&prompt).into_iter().map(|(_, t)| t).collect();
            texts.push(prompt.description.to_owned());
            for text in texts {
                assert!(!text.contains("<ask the user>"), "{}", prompt.name);
                for (name, _) in backticked(&text) {
                    assert!(
                        known.contains(&name),
                        "prompt {} mentions unknown tool `{name}`",
                        prompt.name
                    );
                    found.insert(name);
                }
            }
            assert!(
                found.len() > 4,
                "prompt {} mentions too few tools",
                prompt.name
            );
        }
    }

    /// Инструменты из промпта входят в наборы из строки «Toolsets used here».
    #[test]
    fn prompt_tools_belong_to_listed_toolsets() {
        for prompt in all() {
            for (_, text) in renders(&prompt) {
                let line = text
                    .split("Toolsets used here:")
                    .nth(1)
                    .and_then(|rest| rest.split('.').next())
                    .unwrap_or_else(|| panic!("prompt {} lists no toolsets", prompt.name));
                let listed: BTreeSet<&str> = line.split(',').map(str::trim).collect();
                for (name, _) in backticked(&text) {
                    if let Some((_, toolset)) = tool_info(&name) {
                        assert!(
                            listed.contains(toolset),
                            "prompt {}: `{name}` is in toolset {toolset}, not in: {line}",
                            prompt.name
                        );
                    }
                }
            }
        }
    }

    /// Пометка после имени совпадает с уровнем инструмента; инструменты выше уровня сервера
    /// помечены «not available»; при доступных инструментах удаления текст требует явного
    /// подтверждения, а уровень доступа назван всегда.
    #[test]
    fn access_marks_match_access_levels() {
        let mut marked = 0;
        let mut unavailable = 0;
        for prompt in all() {
            for (setup, text) in renders(&prompt) {
                assert!(
                    text.contains("Access: the whole scenario needs"),
                    "{}",
                    prompt.name
                );
                let mut destructive = false;
                for (name, after) in backticked(&text) {
                    let Some((access, _)) = tool_info(&name) else {
                        continue;
                    };
                    if access == AccessLevel::Read {
                        assert!(!after.starts_with(" ("), "prompt {}: `{name}`", prompt.name);
                        continue;
                    }
                    let mark = after
                        .strip_prefix(" (")
                        .and_then(|rest| rest.split(')').next())
                        .unwrap_or_else(|| {
                            panic!("prompt {}: `{name}` needs an access mark", prompt.name)
                        });
                    assert!(
                        mark.starts_with(access.as_str()),
                        "prompt {}: `{name}` marked {mark}",
                        prompt.name
                    );
                    marked += 1;
                    if setup.access.allows(access) {
                        assert!(!mark.contains("not available"), "{}: {name}", prompt.name);
                        destructive |= access == AccessLevel::Destructive;
                    } else {
                        assert!(mark.contains("not available"), "{}: {name}", prompt.name);
                        unavailable += 1;
                    }
                }
                if destructive {
                    assert!(
                        text.contains("explicit confirmation") && text.contains("dryRun true"),
                        "prompt {} must ask for an explicit confirmation",
                        prompt.name
                    );
                }
                if !setup.access.allows(prompt.full_access) {
                    assert!(text.contains("\"not available\""), "{}", prompt.name);
                }
            }
        }
        assert!(marked > 20 && unavailable > 10);
    }

    #[test]
    fn toolset_hints_follow_the_mode() {
        let prompt = PREPARE_RELEASE;
        let dynamic = render(
            &prompt,
            &setup(AccessLevel::Read, true, &["apps"], &[]),
            &[],
        );
        assert!(
            dynamic.contains("Not enabled yet: releases, listing."),
            "{dynamic}"
        );
        assert!(dynamic.contains("`enable_toolset` with toolset \"releases,listing\""));
        let fixed = render(
            &prompt,
            &setup(AccessLevel::Read, false, &["apps", "releases"], &[]),
            &[],
        );
        assert!(
            fixed.contains("Not enabled on this server: listing"),
            "{fixed}"
        );
        assert!(!fixed.contains("enable_toolset"));
        let all_on = render(
            &prompt,
            &setup(
                AccessLevel::Read,
                false,
                &["apps", "releases", "listing"],
                &[],
            ),
            &[],
        );
        assert!(!all_on.contains("Not enabled"));
    }

    #[test]
    fn render_uses_arguments_and_defaults() {
        let read = setup(
            AccessLevel::Read,
            false,
            &["apps", "releases", "reviews"],
            &[],
        );
        let destructive = setup(AccessLevel::Destructive, false, &[], &["100000001"]);

        let release = render(&PREPARE_RELEASE, &read, &[]);
        assert!(release.contains("the app the user names"));
        assert!(release.contains("`search_apps`"));
        assert!(release.contains("`release_submit` (destructive; not available"));
        assert!(release.contains("releaseType full"));
        let release = render(
            &PREPARE_RELEASE,
            &destructive,
            &[("file", "build/app.aab"), ("release_type", "phased")],
        );
        assert!(release.contains("the profile's default app 100000001"));
        assert!(release.contains("from the file build/app.aab"));
        assert!(release.contains("`release_submit` (destructive) with releaseType phased"));
        assert!(release.contains("explicit confirmation"));

        let phased = render(&PHASED_RELEASE, &destructive, &[]);
        assert!(phased.contains("1, 5, 10, 20, 50 percent"));
        assert!(phased.contains("`get_app_info`"));
        let phased = render(
            &PHASED_RELEASE,
            &read,
            &[("appId", "7"), ("steps", "10, 50"), ("countries", "RU,BY")],
        );
        assert!(phased.contains("app 7: 10, 50 percent"));
        assert!(phased.contains("countries [RU,BY]"));
        assert!(phased.contains("This server cannot change the release"));

        let reviews = render(&TRIAGE_REVIEWS, &read, &[]);
        assert!(reviews.contains("last 30 days"));
        assert!(reviews.contains("cannot be called here"));
        let reviews = render(&TRIAGE_REVIEWS, &destructive, &[("days", "7")]);
        assert!(reviews.contains("last 7 days"));
        assert!(reviews.contains("`reply_review` (destructive) with requestId"));

        let products = render(&PRODUCTS_OVERVIEW, &read, &[("country", "RU")]);
        assert!(products.contains("country RU"));
        assert!(products.contains("`api_search`"));
        assert!(!products.contains("not available"));

        let report = render(&MONTHLY_REPORT, &read, &[]);
        assert!(report.contains("previous month"));
        assert!(report.contains("currency USD"));
        let report = render(
            &MONTHLY_REPORT,
            &read,
            &[("month", "2026-10"), ("save_dir", "reports")],
        );
        assert!(report.contains("the month 2026-10"));
        assert!(report.contains("new file in reports with save_to"));

        let write = setup(AccessLevel::Write, false, &[], &[]);
        let listing = render(&LISTING_UPDATE, &write, &[("dir", "meta")]);
        assert!(listing.contains("directory meta"));
        assert!(listing.contains("needs destructive access for the server's profile"));
        let listing = render(&LISTING_UPDATE, &destructive, &[("dir", "meta")]);
        assert!(listing.contains("only after the user's explicit confirmation"));
    }

    #[test]
    fn http_mode_speaks_about_access_tokens() {
        // В HTTP-режиме уровень и наборы задаёт оператор сервера: флаги запуска в тексте
        // промпта агенту не помогут.
        for prompt in all() {
            for access in [
                AccessLevel::Read,
                AccessLevel::Write,
                AccessLevel::Destructive,
            ] {
                if !access.allows(prompt.min_access) {
                    continue;
                }
                let mut http = setup(access, false, &[], &[]);
                http.http = true;
                let text = render(&prompt, &http, &[("dir", "meta"), ("file", "app.aab")]);
                for flag in [
                    "--allow-write",
                    "--allow-destructive",
                    "--toolsets",
                    "--allow-path",
                ] {
                    assert!(
                        !text.contains(flag),
                        "{} at {access:?} mentions {flag}: {text}",
                        prompt.name
                    );
                }
            }
        }
        let mut http = setup(AccessLevel::Write, false, &[], &[]);
        http.http = true;
        let listing = render(&LISTING_UPDATE, &http, &[("dir", "meta")]);
        assert!(listing.contains("an access token with the destructive level"));
        assert!(listing.contains("server operator"));
        let err = get(
            &all(),
            &PromptSetup {
                access: AccessLevel::Read,
                http: true,
                ..setup(AccessLevel::Read, false, &[], &[])
            },
            request("listing_update", serde_json::json!({"dir": "meta"})),
        )
        .unwrap_err();
        assert!(err.message.contains("access token"), "{}", err.message);
    }

    #[test]
    fn missing_required_arg_is_visible() {
        // `get` отклоняет вызов без обязательного аргумента; если рендер всё же вызван,
        // текст не должен молча подставить пустое значение.
        let write = setup(AccessLevel::Write, false, &[], &[]);
        assert!(render(&LISTING_UPDATE, &write, &[]).contains("<ask the user>"));
        assert!(render(&LISTING_UPDATE, &write, &[("dir", "  ")]).contains("<ask the user>"));
    }

    fn request(name: &str, value: Value) -> GetPromptRequestParams {
        let mut request = GetPromptRequestParams::new(name);
        if let Value::Object(map) = value {
            request = request.with_arguments(map);
        }
        request
    }

    #[test]
    fn get_checks_arguments_and_access() {
        let prompts = all();
        let write = setup(AccessLevel::Write, false, &[], &[]);
        let ok = get(
            &prompts,
            &write,
            request("listing_update", serde_json::json!({"dir": "m"})),
        )
        .unwrap();
        assert_eq!(ok.messages.len(), 1);
        assert!(get(&prompts, &write, request("prepare_release", Value::Null)).is_ok());

        for (name, args, expected) in [
            ("listing_update", serde_json::json!({}), "`dir`"),
            ("listing_update", serde_json::json!({"dir": " "}), "`dir`"),
            (
                "triage_reviews",
                serde_json::json!({"appId": "1", "x": "1"}),
                "no argument `x`",
            ),
            ("nope", Value::Null, "unknown prompt"),
        ] {
            let err = get(&prompts, &write, request(name, args)).unwrap_err();
            assert!(err.message.contains(expected), "{}", err.message);
        }

        let read = setup(AccessLevel::Read, false, &[], &[]);
        assert_eq!(
            available(&prompts, AccessLevel::Read).count(),
            prompts.len() - 1
        );
        let err = get(
            &prompts,
            &read,
            request("listing_update", serde_json::json!({"dir": "m"})),
        )
        .unwrap_err();
        assert!(err.message.contains("write access"), "{}", err.message);
        let err = get(&prompts, &read, request("nope", Value::Null)).unwrap_err();
        assert!(!err.message.contains("listing_update"), "{}", err.message);
    }
}
