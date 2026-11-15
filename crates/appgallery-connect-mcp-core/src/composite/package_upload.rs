//! `package_upload`: загрузка пакета приложения (APK, AAB, RPK, APP, EXE) в AppGallery
//! Connect и привязка его к приложению за один вызов.
//!
//! Порядок:
//! 1. До любых запросов: путь через [`crate::composite::LocalFiles`] (только чтение в
//!    разрешённых каталогах), расширение и размер по ограничениям справочника («Uploading a
//!    File»: APK — до 4 GB, AAB — до 150 MB, RPK — до 30 MB, APP и EXE — до 4 GB), остальные
//!    аргументы. `dryRun` на этом останавливается и говорит, что было бы сделано.
//! 2. Загрузка через Upload Management API — общий помощник [`super::upload`]: целиком
//!    (с SHA-256 для проверки целостности) или частями для больших APK, AAB и APP, с
//!    повтором сбойной части.
//! 3. Привязка файла к приложению: `publishing.fileInfo.update` (`update_app_file_info`)
//!    с `fileType = 5` (пакет приложения); в ответе `pkgVersion` — идентификатор пакета.
//! 4. Ожидание обработки пакета: `publishing.packageCompileStatus.get`
//!    (`get_package_compile_status`) до готовности, сбоя или срока `waitSeconds` (для AAB по
//!    умолчанию [`DEFAULT_AAB_WAIT_SECS`], для остальных пакетов — без ожидания).
//!
//! Отправка версии на проверку — не здесь: это отдельный инструмент уровня удаления.
//!
//! В ответ попадают имя файла, размер, тип, SHA-256, идентификатор пакета, состояние
//! обработки и число частей и повторов. Адреса и заголовки загрузки, идентификатор объекта
//! в хранилище и `nspUploadId` не попадают ни в ответ, ни в ошибки, ни в логи.

use std::path::Path;
use std::time::Duration;

use appgallery_connect_api::AccessLevel;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::composite::reviews_summary::{app_id, operation};
use crate::composite::upload::{
    self, MIB, MultipartFileType, UploadSpec, UploadTuning, UploadedFile,
};
use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};
use crate::files::display_path;

/// Имя инструмента.
pub const NAME: &str = "package_upload";

/// Привязка загруженного файла к приложению.
const FILE_INFO_UPDATE: &str = "publishing.fileInfo.update";
/// Состояние обработки (компиляции) пакета.
const COMPILE_STATUS: &str = "publishing.packageCompileStatus.get";

/// `fileType` пакета приложения в `update_app_file_info`.
const FILE_TYPE_PACKAGE: u8 = 5;
/// Ожидание обработки AAB по умолчанию, секунд.
pub const DEFAULT_AAB_WAIT_SECS: u64 = 300;
/// Самое долгое ожидание, секунд.
const MAX_WAIT_SECS: u64 = 1800;
/// Пауза между запросами состояния пакета.
const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// После стольких сбоев запроса состояния подряд ожидание прекращается.
const MAX_STATUS_FAILURES: u32 = 3;
/// Самое длинное имя файла (`fileName`, String(256)).
const MAX_FILE_NAME: usize = 256;
const GIB: u64 = 1024 * MIB;

const DESCRIPTION: &str = "Upload an app package to AppGallery Connect and attach it to the \
app in one call: `file` is a local APK, AAB, RPK, APP (HarmonyOS) or EXE (Windows) file in the \
allowed directories. The tool checks the file type and size limits (APK, APP, EXE up to 4 GB, \
AAB up to 150 MB, RPK up to 30 MB) before any request, uploads the file (in chunks with retries \
when it is larger than 100 MB), attaches it as the app package (`update_app_file_info`, file \
type 5) and, for AAB by default, waits up to `waitSeconds` until AppGallery Connect has \
processed the package. Returns the file name, size, type and SHA-256, the package ID \
(`pkgId`), `packageStatus` (`ready`, `processing`, `failed`, `not_checked` or `unknown`), \
`readyForReview`, and how many parts and retries the upload took. The package replaces the \
package of the version being prepared; it is not sent for review: submitting the version \
(`submit_app`, destructive access) is a separate step, only when the user asks for it. \
`dryRun: true` checks the file and the arguments and tells what would be done, without any \
request. Upload URLs and signatures are never returned.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "releases",
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
            "file": {
                "type": "string",
                "minLength": 1,
                "description": "Path to the package file (.apk, .aab, .rpk, .app or .exe), \
                    absolute or relative to the server's working directory, inside the allowed \
                    directories."
            },
            "appId": {
                "type": ["string", "integer"],
                "description": "Numeric app ID (like 100000001). Optional when the profile has \
                    exactly one default app."
            },
            "dryRun": {
                "type": "boolean",
                "description": "true: only check the file and the arguments and describe the \
                    upload; nothing is uploaded or changed."
            },
            "waitSeconds": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_WAIT_SECS,
                "description": "How long to wait for AppGallery Connect to process the \
                    package. Default 300 for AAB (it is compiled into APKs), 0 for other \
                    packages; 0 does not wait."
            },
            "releaseType": {
                "type": "integer",
                "enum": [1, 3],
                "description": "Release type of the version the package belongs to: 1 full \
                    release (default), 3 phased release."
            },
            "chineseMainland": {
                "type": "boolean",
                "description": "Whether the package is also distributed in the Chinese \
                    mainland. Default false. Required by AppGallery Connect for accounts \
                    registered outside the Chinese mainland."
            },
            "versionName": {
                "type": "string",
                "minLength": 1,
                "description": "EXE packages only (required for them): version of the Windows \
                    app."
            },
            "displayName": {
                "type": "string",
                "minLength": 1,
                "description": "EXE packages only (required for them): name shown in the \
                    Windows registry that identifies the app."
            }
        },
        "required": ["file"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(async move { package_upload(ctx, args, &Tuning::default()).await })
}

/// Время и размеры, которые тесты подменяют.
#[derive(Debug, Clone, Copy)]
struct Tuning {
    upload: UploadTuning,
    poll_interval: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            upload: UploadTuning::default(),
            poll_interval: POLL_INTERVAL,
        }
    }
}

/// Тип пакета по расширению.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageType {
    Apk,
    Aab,
    Rpk,
    App,
    Exe,
}

impl PackageType {
    const ALL: [Self; 5] = [Self::Apk, Self::Aab, Self::Rpk, Self::App, Self::Exe];

    fn extension(self) -> &'static str {
        match self {
            Self::Apk => "apk",
            Self::Aab => "aab",
            Self::Rpk => "rpk",
            Self::App => "app",
            Self::Exe => "exe",
        }
    }

    fn from_file_name(name: &str) -> Option<Self> {
        let (_, extension) = name.rsplit_once('.')?;
        Self::ALL
            .into_iter()
            .find(|kind| extension.eq_ignore_ascii_case(kind.extension()))
    }

    /// Предел размера по справочнику («Uploading a File»). У RPK — предел быстрых игр
    /// (30 MB; быстрых приложений — 20 MB), у APP — приложений HarmonyOS (4 GB; атомарных
    /// сервисов — 10 MB): по файлу их не различить, точнее проверит AppGallery Connect.
    fn max_bytes(self) -> u64 {
        match self {
            Self::Apk | Self::App | Self::Exe => 4 * GIB,
            Self::Aab => 150 * MIB,
            Self::Rpk => 30 * MIB,
        }
    }

    /// Вид файла для загрузки частями; RPK маленькие, для EXE способ не описан.
    fn multipart(self) -> Option<MultipartFileType> {
        match self {
            Self::Apk | Self::Aab => Some(MultipartFileType::AndroidPackage),
            Self::App => Some(MultipartFileType::HarmonyApp),
            Self::Rpk | Self::Exe => None,
        }
    }

    fn default_wait(self) -> u64 {
        match self {
            Self::Aab => DEFAULT_AAB_WAIT_SECS,
            _ => 0,
        }
    }
}

/// Разобранные и проверенные аргументы.
#[derive(Debug)]
struct Request {
    app_id: String,
    path: std::path::PathBuf,
    file_name: String,
    size: u64,
    kind: PackageType,
    dry_run: bool,
    wait_secs: u64,
    release_type: Option<u64>,
    chinese_mainland: bool,
    version_name: Option<String>,
    display_name: Option<String>,
}

async fn package_upload(ctx: &Context, args: Value, tuning: &Tuning) -> Result<Value, ToolError> {
    let request = parse_request(ctx, &args)?;
    let plan = upload::plan(request.size, request.kind.multipart(), &tuning.upload);
    if request.dry_run {
        return Ok(dry_run(&request, &plan));
    }
    tracing::debug!(
        tool = NAME,
        bytes = request.size,
        kind = request.kind.extension(),
        multipart = plan.multipart,
        "загрузка пакета"
    );

    // SHA-256 нужен только загрузке целиком: хранилище сверит с ним файл.
    let sha256 = if plan.multipart {
        None
    } else {
        Some(upload::sha256_file(&request.path).await?)
    };
    let spec = UploadSpec {
        app_id: &request.app_id,
        path: &request.path,
        file_name: &request.file_name,
        size: request.size,
        sha256: sha256.as_deref(),
        chinese_mainland: Some(request.chinese_mainland),
        multipart: request.kind.multipart(),
    };
    let uploaded = upload::upload(ctx, &spec, &tuning.upload).await?;
    let pkg_id = attach(ctx, &request, &uploaded).await?;

    let mut notes: Vec<String> = Vec::new();
    let state = match &pkg_id {
        Some(pkg_id) if request.wait_secs > 0 => {
            wait_for_package(
                ctx,
                &request.app_id,
                pkg_id,
                request.wait_secs,
                tuning.poll_interval,
                &mut notes,
            )
            .await
        }
        Some(_) => Wait::not_checked(),
        None => {
            notes.push(
                "AppGallery Connect returned no package ID (`pkgVersion`), so the processing \
                 state was not checked; `list_packages` lists the packages of the app"
                    .into(),
            );
            Wait::unknown()
        }
    };
    tracing::debug!(
        tool = NAME,
        parts = uploaded.parts,
        retries = uploaded.retries,
        status = state.status.as_str(),
        "пакет загружен"
    );
    Ok(result(
        &request,
        &uploaded,
        sha256.as_deref(),
        pkg_id.as_deref(),
        state,
        notes,
    ))
}

/// Разбор и проверка аргументов и файла, без запросов к API.
fn parse_request(ctx: &Context, args: &Value) -> Result<Request, ToolError> {
    let app_id = app_id(ctx, args.get("appId"))?;
    let dry_run = args.get("dryRun").and_then(Value::as_bool).unwrap_or(false);
    let chinese_mainland = args
        .get("chineseMainland")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let release_type = match args.get("releaseType") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|kind| matches!(kind, 1 | 3))
                .ok_or_else(|| {
                    ToolError::new("`releaseType` must be 1 (full release) or 3 (phased release)")
                })?,
        ),
    };
    let wait = match args.get("waitSeconds") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|secs| *secs <= MAX_WAIT_SECS)
                .ok_or_else(|| {
                    ToolError::new(format!(
                        "`waitSeconds` must be an integer from 0 to {MAX_WAIT_SECS}"
                    ))
                })?,
        ),
    };
    let text_arg = |name: &str| -> Result<Option<String>, ToolError> {
        match args.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) if !text.trim().is_empty() => {
                Ok(Some(text.trim().to_owned()))
            }
            Some(_) => Err(ToolError::new(format!(
                "`{name}` must be a non-empty string"
            ))),
        }
    };
    let version_name = text_arg("versionName")?;
    let display_name = text_arg("displayName")?;

    let file = match args.get("file") {
        Some(Value::String(path)) if !path.trim().is_empty() => path.as_str(),
        _ => {
            return Err(ToolError::new(
                "`file` is required: the path to the APK, AAB, RPK, APP or EXE file",
            ));
        }
    };
    let file_name = Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_default();
    let kind = PackageType::from_file_name(&file_name).ok_or_else(|| {
        ToolError::new(format!(
            "`{file}` is not an app package: the file name must end with .apk, .aab, .rpk, .app \
             or .exe. Images and other files are uploaded by other tools"
        ))
    })?;
    if file_name.chars().count() > MAX_FILE_NAME {
        return Err(ToolError::new(format!(
            "the file name is longer than {MAX_FILE_NAME} characters; rename the file"
        )));
    }
    let path = ctx.files.resolve_read(file)?;
    // Имя и расширение — по настоящему файлу: символическая ссылка могла вести на другой.
    let real_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_default();
    if PackageType::from_file_name(&real_name) != Some(kind) {
        return Err(ToolError::new(format!(
            "`{file}` points to `{}`, which does not have the .{} extension",
            display_path(&path),
            kind.extension()
        )));
    }
    let size = std::fs::metadata(&path)
        .map_err(|err| {
            ToolError::new(format!(
                "could not read the size of `{}`: {err}",
                display_path(&path)
            ))
        })?
        .len();
    if size == 0 {
        return Err(ToolError::new(format!("`{file}` is empty")));
    }
    if size > kind.max_bytes() {
        return Err(ToolError::new(format!(
            "`{file}` is {size} bytes; AppGallery Connect accepts {} packages up to {} bytes \
             ({})",
            kind.extension().to_ascii_uppercase(),
            kind.max_bytes(),
            human_size(kind.max_bytes())
        )));
    }
    match kind {
        PackageType::Exe if version_name.is_none() || display_name.is_none() => {
            return Err(ToolError::new(
                "EXE packages need `versionName` and `displayName` (the version and the \
                 registry name of the Windows app)",
            ));
        }
        PackageType::Exe => {}
        _ if version_name.is_some() || display_name.is_some() => {
            return Err(ToolError::new(
                "`versionName` and `displayName` are only for EXE packages",
            ));
        }
        _ => {}
    }
    Ok(Request {
        app_id,
        path,
        file_name: real_name,
        size,
        kind,
        dry_run,
        wait_secs: wait.unwrap_or_else(|| kind.default_wait()),
        release_type,
        chinese_mainland,
        version_name,
        display_name,
    })
}

/// Ответ `dryRun`: что было бы сделано.
fn dry_run(request: &Request, plan: &upload::UploadPlan) -> Value {
    let mut steps = Vec::new();
    if plan.multipart {
        steps.push(format!(
            "upload the file in {} parts of up to {} bytes (each part retried up to {} times)",
            plan.parts,
            plan.part_bytes,
            upload::DEFAULT_RETRIES
        ));
    } else {
        steps.push(format!(
            "upload the file in one request with its SHA-256 for an integrity check (retried up \
             to {} times)",
            upload::DEFAULT_RETRIES
        ));
    }
    steps.push(format!(
        "attach it to app {} as the app package (`update_app_file_info`, file type 5{})",
        request.app_id,
        match request.release_type {
            Some(3) => ", phased release",
            _ => "",
        }
    ));
    if request.wait_secs > 0 {
        steps.push(format!(
            "wait up to {} seconds until AppGallery Connect has processed the package \
             (`get_package_compile_status`)",
            request.wait_secs
        ));
    }
    json!({
        "dryRun": true,
        "appId": request.app_id,
        "fileName": request.file_name,
        "bytes": request.size,
        "packageType": request.kind.extension(),
        "upload": {
            "chunked": plan.multipart,
            "parts": plan.parts,
            "partBytes": plan.part_bytes,
        },
        "waitSeconds": request.wait_secs,
        "steps": steps,
        "note": "Nothing was uploaded or changed. Call again without `dryRun` to upload; the \
            package is not sent for review.",
    })
}

/// Привязка файла к приложению; возвращает идентификатор пакета из `pkgVersion`.
async fn attach(
    ctx: &Context,
    request: &Request,
    uploaded: &UploadedFile,
) -> Result<Option<String>, ToolError> {
    let op = operation(FILE_INFO_UPDATE)?;
    let mut file = Map::new();
    file.insert("fileName".into(), json!(request.file_name));
    file.insert("fileDestUrl".into(), json!(uploaded.object_id()));
    if let Some(version) = &request.version_name {
        file.insert("versionName".into(), json!(version));
    }
    if let Some(name) = &request.display_name {
        file.insert("displayName".into(), json!(name));
    }
    let mut args = Map::new();
    args.insert("appId".into(), json!(request.app_id));
    if let Some(release_type) = request.release_type {
        args.insert("releaseType".into(), json!(release_type));
    }
    args.insert(
        "body".into(),
        json!({"fileType": FILE_TYPE_PACKAGE, "files": [Value::Object(file)]}),
    );
    let response = ctx
        .api
        .call(op, &Value::Object(args))
        .await
        .map_err(|err| {
            ToolError::new(uploaded.scrub(&format!(
                "the file was uploaded, but attaching it to the app (`update_app_file_info`) \
                 failed, so the app was not changed: {}. Fix the cause and run package_upload \
                 again",
                describe_api_error(&err)
            )))
        })?;
    let pkg_id = response
        .body
        .json()
        .and_then(|body| body.get("pkgVersion"))
        .and_then(|list| match list {
            Value::Array(items) => items.first().cloned(),
            other => Some(other.clone()),
        })
        .and_then(|id| match id {
            Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        });
    Ok(pkg_id)
}

/// Состояние обработки пакета.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildStatus {
    Ready,
    Processing,
    Failed,
    NotChecked,
    Unknown,
}

impl BuildStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Processing => "processing",
            Self::Failed => "failed",
            Self::NotChecked => "not_checked",
            Self::Unknown => "unknown",
        }
    }
}

/// Итог ожидания.
#[derive(Debug, Clone)]
pub(crate) struct Wait {
    pub(crate) status: BuildStatus,
    pub(crate) checks: u32,
    pub(crate) waited: Duration,
    pub(crate) timed_out: bool,
    pub(crate) fail_reason: Option<i64>,
    /// Текст последнего сбоя запроса состояния, если после него успешного запроса не было.
    pub(crate) error: Option<String>,
}

impl Wait {
    fn not_checked() -> Self {
        Self {
            status: BuildStatus::NotChecked,
            checks: 0,
            waited: Duration::ZERO,
            timed_out: false,
            fail_reason: None,
            error: None,
        }
    }

    fn unknown() -> Self {
        Self {
            status: BuildStatus::Unknown,
            ..Self::not_checked()
        }
    }
}

/// Опрос состояния пакета до готовности, сбоя или срока `wait_secs` (0 — один запрос). Сбой
/// запроса состояния — не ошибка (у `package_upload` пакет уже привязан), а пометка в
/// `notes` после [`MAX_STATUS_FAILURES`] сбоев подряд и текст в [`Wait::error`]. Общий для
/// `package_upload`, `release_submit` и `release_status`.
pub(crate) async fn wait_for_package(
    ctx: &Context,
    app_id: &str,
    pkg_id: &str,
    wait_secs: u64,
    interval: Duration,
    notes: &mut Vec<String>,
) -> Wait {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(wait_secs);
    let mut wait = Wait::unknown();
    let op = match operation(COMPILE_STATUS) {
        Ok(op) => op,
        Err(err) => {
            notes.push(err.message);
            return wait;
        }
    };
    let args = json!({"appId": app_id, "pkgIds": pkg_id});
    let mut failures = 0u32;
    loop {
        wait.checks += 1;
        match ctx.api.call(op, &args).await {
            Ok(response) => {
                failures = 0;
                wait.error = None;
                let state = response
                    .body
                    .json()
                    .and_then(|body| package_state(body, pkg_id));
                match state {
                    Some((status, fail_reason)) => {
                        wait.status = status;
                        wait.fail_reason = fail_reason;
                    }
                    None => wait.status = BuildStatus::Unknown,
                }
                if matches!(wait.status, BuildStatus::Ready | BuildStatus::Failed) {
                    break;
                }
            }
            Err(err) => {
                failures += 1;
                wait.error = Some(describe_api_error(&err));
                if failures >= MAX_STATUS_FAILURES {
                    notes.push(format!(
                        "the processing state could not be read ({MAX_STATUS_FAILURES} failed \
                         requests in a row), so the tool stopped waiting: {}",
                        describe_api_error(&err)
                    ));
                    wait.status = BuildStatus::Unknown;
                    break;
                }
            }
        }
        if Instant::now() + interval > deadline {
            wait.timed_out = true;
            break;
        }
        tokio::time::sleep(interval).await;
    }
    wait.waited = started.elapsed();
    wait
}

/// Состояние пакета из `pkgStateList` (массив или объект): статус и код причины сбоя AAB.
///
/// `successStatus` (0 — готов, 1 — разбирается, 2 — сбой) — основное поле; устаревающие
/// `aabCompileStatus` (0 — не отправлен, 1 — компилируется, 2 — готов, 3 — сбой) и
/// `failReason` ещё приходят для AAB и учитываются осторожно: компиляция или её сбой
/// перевешивают «готов».
fn package_state(body: &Value, pkg_id: &str) -> Option<(BuildStatus, Option<i64>)> {
    let entries: Vec<&Value> = match body.get("pkgStateList")? {
        Value::Array(items) => items.iter().collect(),
        object @ Value::Object(_) => vec![object],
        _ => return None,
    };
    let id_of = |entry: &Value| match entry.get("pkgId") {
        Some(Value::String(text)) => Some(text.trim().to_owned()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    };
    let entry = entries
        .iter()
        .find(|entry| id_of(entry).as_deref() == Some(pkg_id))
        .or_else(|| {
            if entries.len() == 1 {
                entries.first()
            } else {
                None
            }
        })?;
    let success = int(entry.get("successStatus"));
    let compile = int(entry.get("aabCompileStatus"));
    let status = if success == Some(2) || compile == Some(3) {
        BuildStatus::Failed
    } else if success == Some(1) || compile == Some(1) {
        BuildStatus::Processing
    } else if success == Some(0) || compile == Some(2) {
        BuildStatus::Ready
    } else {
        BuildStatus::Unknown
    };
    let fail_reason = (status == BuildStatus::Failed)
        .then(|| int(entry.get("failReason")))
        .flatten();
    Some((status, fail_reason))
}

/// Причины сбоя компиляции AAB (`failReason` справочника).
pub(crate) fn fail_reason_text(code: i64) -> &'static str {
    match code {
        6 => "the app uses dynamic features but does not integrate the Dynamic Ability SDK",
        7 => "universal.apk cannot run because of invalid module configurations",
        8 => "Asset Packs are not supported",
        9 => "AppGallery Connect could not download the AAB",
        10 => "the AAB is empty",
        11 => "the AAB could not be decompressed: the format is invalid",
        12 => "bundletool could not parse the AAB",
        13..=18 => "the AAB could not be compiled",
        19 => "the AAB is larger than 150 MB",
        _ => "see the failReason codes of get_package_compile_status in the API reference",
    }
}

/// Ответ инструмента.
fn result(
    request: &Request,
    uploaded: &UploadedFile,
    sha256: Option<&str>,
    pkg_id: Option<&str>,
    wait: Wait,
    mut notes: Vec<String>,
) -> Value {
    let mut out = Map::new();
    out.insert("appId".into(), json!(request.app_id));
    out.insert("fileName".into(), json!(request.file_name));
    out.insert("bytes".into(), json!(request.size));
    out.insert("packageType".into(), json!(request.kind.extension()));
    if let Some(sha256) = sha256 {
        out.insert("sha256".into(), json!(sha256));
    }
    out.insert(
        "upload".into(),
        json!({
            "chunked": uploaded.multipart,
            "parts": uploaded.parts,
            "retries": uploaded.retries,
        }),
    );
    out.insert("attached".into(), json!(true));
    out.insert("pkgId".into(), json!(pkg_id));
    out.insert("packageStatus".into(), json!(wait.status.as_str()));
    let ready = wait.status == BuildStatus::Ready;
    out.insert("readyForReview".into(), json!(ready));
    if wait.checks > 0 {
        out.insert("statusChecks".into(), json!(wait.checks));
        out.insert("waitedSeconds".into(), json!(wait.waited.as_secs()));
        out.insert("timedOut".into(), json!(wait.timed_out));
    }
    if let Some(code) = wait.fail_reason {
        out.insert(
            "failReason".into(),
            json!({"code": code, "description": fail_reason_text(code)}),
        );
    }
    let check = match pkg_id {
        Some(id) => format!("`get_package_compile_status` with pkgIds `{id}`"),
        None => "`list_packages`".to_owned(),
    };
    let next = match wait.status {
        BuildStatus::Ready => "The package is attached to the version being prepared and \
            AppGallery Connect has processed it. Sending the version for review is a separate \
            step (`submit_app`, destructive access): do it only when the user asks."
            .to_owned(),
        BuildStatus::Failed => "AppGallery Connect could not process the package: fix it and \
            upload a new one. Do not submit the version for review with this package."
            .to_owned(),
        BuildStatus::Processing if wait.timed_out => format!(
            "AppGallery Connect is still processing the package. Check {check} later (or call \
             package_upload with a longer `waitSeconds` next time) before submitting the \
             version for review."
        ),
        _ => format!(
            "AppGallery Connect processes a new package for a few minutes. Check {check} \
             until it is ready before submitting the version for review."
        ),
    };
    if wait.status == BuildStatus::Processing && wait.timed_out {
        notes.push(format!(
            "the package was still being processed after {} seconds",
            wait.waited.as_secs()
        ));
    }
    out.insert("next".into(), json!(next));
    if !notes.is_empty() {
        out.insert("notes".into(), json!(notes));
    }
    Value::Object(out)
}

/// Целое из числа или строки.
fn int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Размер для сообщений: `150 MB`, `4 GB` (двоичные, как в справочнике).
fn human_size(bytes: u64) -> String {
    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        format!("{} GB", bytes / GIB)
    } else {
        format!("{} MB", bytes / MIB)
    }
}

#[cfg(test)]
mod tests;
