//! Общий помощник загрузки локального файла в файловое хранилище AppGallery Connect через
//! Upload Management API. Им пользуются составные инструменты, которые загружают файлы:
//! `package_upload` (пакеты приложения), изображения и видео страницы приложения.
//!
//! Помощник только загружает файл и возвращает идентификатор объекта в хранилище
//! ([`UploadedFile::object_id`]); привязку к приложению (`publishing.fileInfo.update` с
//! `fileDestUrl` = идентификатор объекта и нужным `fileType`) делает сам инструмент.
//!
//! Два способа ([`plan`]):
//! - целиком (файл не больше [`UploadTuning::multipart_threshold`] или способ частями для
//!   вида файла не описан): `upload.uploadUrl.get` даёт `urlInfo` (`objectId`, `url`,
//!   `method`, `headers`), затем файл отправляется на `url` одним запросом
//!   ([`appgallery_connect_api::ApiClient::upload_file`]);
//! - частями: `upload.multipart.init` (`objectId`, `nspUploadId`, допустимый размер части
//!   `nspPartMinSize`…`nspPartMaxSize`), для каждой части `upload.multipart.parts` с одной
//!   частью `additionalProp<N>` (адрес живёт около четырёх минут, поэтому адрес берётся
//!   прямо перед отправкой части, а не сразу для всех), отправка части (ответ — `ETag`),
//!   в конце `upload.multipart.compose` с `partObjectId` и `etag` всех частей.
//!
//! Сбой отправки (сеть, таймаут, ответы хранилища 403, 408, 429, 5xx) повторяется не больше
//! [`UploadTuning::retries`] раз на файл или на часть: перед повтором берётся новый адрес
//! (истёкшая подпись — тоже 403). Сбой вызова Connect API не повторяется: его повторы уже
//! сделал исполнитель для идемпотентных методов, а `POST` повторять нельзя.
//!
//! Безопасность: адреса загрузки, значения заголовков (подпись хранилища), идентификатор
//! объекта и `nspUploadId` — секрет. В ответ, ошибки и логи они не попадают: ошибки
//! строятся из номера части, размеров и кода хранилища, а перед возвратом ещё и проходят
//! маскирование всех известных помощнику значений ([`UploadedFile::scrub`] — для ошибок
//! после загрузки). Логи — только счётчики и размеры.
//!
//! Операции Upload Management API исключены из таблицы инструментов (`callable = false`):
//! помощник берёт их метод и путь из таблицы исключённых операций, а конверт результата у
//! группы `upload` общий — `ret.code`/`ret.msg`.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use appgallery_connect_api::envelope::RESULT_RET_CODE;
use appgallery_connect_api::executor::{ApiError, ApiRequest, ApiResponse, ResultCheck};
use appgallery_connect_api::spec::{ExcludedOperationDef, find_excluded};
use appgallery_connect_api::{HttpMethod, UploadRequest};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::composite::Context;
use crate::error::{ToolError, describe_api_error};

/// Адрес загрузки файла целиком.
pub const UPLOAD_URL: &str = "upload.uploadUrl.get";
/// Начало загрузки частями.
pub const MULTIPART_INIT: &str = "upload.multipart.init";
/// Адреса частей.
pub const MULTIPART_PARTS: &str = "upload.multipart.parts";
/// Слияние частей.
pub const MULTIPART_COMPOSE: &str = "upload.multipart.compose";

/// Мебибайт: справочник Huawei считает «MB» двоичными (пример части — 5242880 байт).
pub const MIB: u64 = 1024 * 1024;
/// Самая большая часть по справочнику («The maximum size of a single chunk is 100 MB»).
pub const MAX_PART_BYTES: u64 = 100 * MIB;
/// Порог загрузки частями по умолчанию: файл, который помещается в одну часть, загружается
/// целиком.
pub const MULTIPART_THRESHOLD: u64 = MAX_PART_BYTES;
/// Размер части по умолчанию: меньше максимума, чтобы повтор части стоил меньше; не меньше
/// минимума хранилища (`nspPartMinSize`, по справочнику 5 MB).
pub const DEFAULT_PART_BYTES: u64 = 64 * MIB;
/// Повторов на файл или на часть по умолчанию.
pub const DEFAULT_RETRIES: u32 = 3;
/// Пауза перед первым повтором по умолчанию (дальше удваивается).
pub const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Больше частей объектное хранилище не принимает.
const MAX_PARTS: u64 = 10_000;
/// Размер буфера при подсчёте SHA-256.
const HASH_BUFFER: usize = 1024 * 1024;
/// Значения короче не маскируются: короткие строки встречаются в обычном тексте.
const MIN_MASKED_LEN: usize = 6;

/// Настройки загрузки. По умолчанию — значения справочника; тесты задают маленькие части
/// и короткие паузы.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadTuning {
    /// Файлы больше порога загружаются частями (если вид файла это допускает).
    pub multipart_threshold: u64,
    /// Желаемый размер части; итог ограничен `nspPartMinSize`/`nspPartMaxSize` ответа
    /// инициализации и [`MAX_PART_BYTES`].
    pub part_bytes: u64,
    /// Сколько раз повторить отправку файла или одной части после сбоя.
    pub retries: u32,
    /// Пауза перед первым повтором (дальше удваивается).
    pub retry_delay: Duration,
}

impl Default for UploadTuning {
    fn default() -> Self {
        Self {
            multipart_threshold: MULTIPART_THRESHOLD,
            part_bytes: DEFAULT_PART_BYTES,
            retries: DEFAULT_RETRIES,
            retry_delay: DEFAULT_RETRY_DELAY,
        }
    }
}

/// Вид файла для загрузки частями (`fileType` инициализации).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultipartFileType {
    /// `1`: пакет HarmonyOS (APP) или атомарного сервиса.
    HarmonyApp,
    /// `2`: файл расширения APK (OBB).
    Obb,
    /// `3`: пакет Android (APK и AAB).
    AndroidPackage,
}

impl MultipartFileType {
    /// Код `fileType` справочника.
    pub fn code(self) -> u8 {
        match self {
            Self::HarmonyApp => 1,
            Self::Obb => 2,
            Self::AndroidPackage => 3,
        }
    }
}

/// Что загрузить.
#[derive(Debug, Clone)]
pub struct UploadSpec<'a> {
    /// Числовой `appId`.
    pub app_id: &'a str,
    /// Канонический путь к файлу, уже проверенный [`crate::composite::LocalFiles`].
    pub path: &'a Path,
    /// Имя файла с расширением (`fileName`): по нему хранилище определяет тип файла.
    pub file_name: &'a str,
    /// Размер файла в байтах (`contentLength`); отправляется ровно столько.
    pub size: u64,
    /// SHA-256 файла (64 шестнадцатеричных символа) для проверки целостности при загрузке
    /// целиком ([`sha256_file`]); `None` — без проверки.
    pub sha256: Option<&'a str>,
    /// `chineseMainlandFlag`: распространять ли пакет в материковом Китае. Справочник
    /// требует его, если кабинет зарегистрирован не в материковом Китае; `None` — не
    /// передавать (изображения и видео).
    pub chinese_mainland: Option<bool>,
    /// Вид файла для загрузки частями; `None` — только целиком (изображения, видео, RPK, EXE).
    pub multipart: Option<MultipartFileType>,
}

/// Как будет загружен файл (без запросов к API).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadPlan {
    /// Частями.
    pub multipart: bool,
    /// Размер части (оценка: хранилище может сузить его ответом инициализации).
    pub part_bytes: u64,
    /// Число частей (1 при загрузке целиком).
    pub parts: u64,
}

/// План загрузки файла размера `size`.
pub fn plan(size: u64, multipart: Option<MultipartFileType>, tuning: &UploadTuning) -> UploadPlan {
    if multipart.is_none() || size <= tuning.multipart_threshold {
        return UploadPlan {
            multipart: false,
            part_bytes: size,
            parts: 1,
        };
    }
    let part_bytes = tuning.part_bytes.clamp(1, MAX_PART_BYTES);
    UploadPlan {
        multipart: true,
        part_bytes,
        parts: size.div_ceil(part_bytes),
    }
}

/// Загруженный файл.
pub struct UploadedFile {
    /// Идентификатор объекта в хранилище: `fileDestUrl` при привязке файла к приложению.
    object_id: String,
    /// Сколько байт загружено.
    pub bytes: u64,
    /// Загружен частями.
    pub multipart: bool,
    /// Сколько частей отправлено (1 при загрузке целиком).
    pub parts: usize,
    /// Сколько всего было повторов отправки.
    pub retries: u32,
    masks: Masks,
}

impl UploadedFile {
    /// Идентификатор объекта для `fileDestUrl`. Агенту его не показывают: он часть адреса
    /// загрузки.
    pub fn object_id(&self) -> &str {
        &self.object_id
    }

    /// Убирает из текста (например, ошибки привязки файла) адреса, заголовки и
    /// идентификаторы этой загрузки.
    pub fn scrub(&self, text: &str) -> String {
        self.masks.apply(text)
    }
}

impl fmt::Debug for UploadedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadedFile")
            .field("object_id", &"***")
            .field("bytes", &self.bytes)
            .field("multipart", &self.multipart)
            .field("parts", &self.parts)
            .field("retries", &self.retries)
            .finish()
    }
}

/// Загружает файл в хранилище AppGallery Connect. Ошибка — текст для агента без адресов,
/// заголовков и идентификаторов загрузки; файл к приложению при этом не привязан.
pub async fn upload(
    ctx: &Context,
    spec: &UploadSpec<'_>,
    tuning: &UploadTuning,
) -> Result<UploadedFile, ToolError> {
    let mut masks = Masks::default();
    let planned = plan(spec.size, spec.multipart, tuning);
    let result = match spec.multipart {
        Some(kind) if planned.multipart => {
            upload_multipart(ctx, spec, kind, tuning, &mut masks).await
        }
        _ => upload_single(ctx, spec, tuning, &mut masks).await,
    };
    match result {
        Ok(mut file) => {
            file.masks = masks;
            Ok(file)
        }
        Err(err) => Err(ToolError::new(masks.apply(err.message()))),
    }
}

/// SHA-256 файла шестнадцатеричной строкой (чтение в отдельном потоке: файл бывает в
/// несколько гигабайт).
pub async fn sha256_file(path: &Path) -> Result<String, ToolError> {
    let path: PathBuf = path.to_owned();
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let mut file = std::fs::File::open(&path).map_err(|err| err.to_string())?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; HASH_BUFFER];
        loop {
            let read = file.read(&mut buffer).map_err(|err| err.to_string())?;
            if read == 0 {
                break;
            }
            hasher.update(buffer.get(..read).unwrap_or_default());
        }
        Ok(hex(&hasher.finalize()))
    })
    .await
    .map_err(|_| ToolError::new("computing the SHA-256 of the file stopped unexpectedly"))?
    .map_err(|err| ToolError::new(format!("could not read the file to upload: {err}")))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// Загрузка целиком: адрес из `upload.uploadUrl.get`, один запрос с файлом.
async fn upload_single(
    ctx: &Context,
    spec: &UploadSpec<'_>,
    tuning: &UploadTuning,
    masks: &mut Masks,
) -> Result<UploadedFile, ToolError> {
    let op = excluded(UPLOAD_URL)?;
    let mut retries = 0u32;
    loop {
        let mut request = api_request(op)
            .with_query("appId", spec.app_id)
            .with_query("fileName", spec.file_name)
            .with_query("contentLength", spec.size.to_string());
        if let Some(sha256) = spec.sha256 {
            request = request.with_query("sha256", sha256);
        }
        if let Some(flag) = spec.chinese_mainland {
            request = request.with_query("chineseMainlandFlag", if flag { "1" } else { "0" });
        }
        let response = execute(
            ctx,
            request,
            "getting the upload URL (`upload.uploadUrl.get`)",
        )
        .await?;
        let info = json_body(&response)
            .get("urlInfo")
            .cloned()
            .unwrap_or(Value::Null);
        let object_id = required_text(&info, "objectId", masks)
            .ok_or_else(|| invalid_response("the upload URL response has no `urlInfo.objectId`"))?;
        let target = parse_target(&info, masks)
            .map_err(|reason| invalid_response(&format!("the upload URL is unusable: {reason}")))?;
        let request = target.request(spec.path, 0, spec.size);
        match ctx.api.upload_file(request).await {
            Ok(_) => {
                tracing::debug!(bytes = spec.size, retries, "файл загружен целиком");
                return Ok(UploadedFile {
                    object_id,
                    bytes: spec.size,
                    multipart: false,
                    parts: 1,
                    retries,
                    masks: Masks::default(),
                });
            }
            Err(err) if is_retryable(&err) && retries < tuning.retries => {
                retries += 1;
                tracing::debug!(
                    retry = retries,
                    status = err.status().unwrap_or(0),
                    "повтор загрузки файла с новым адресом"
                );
                tokio::time::sleep(backoff(tuning.retry_delay, retries)).await;
            }
            Err(err) => {
                return Err(ToolError::new(format!(
                    "sending the file ({} bytes) to the AppGallery Connect file storage failed{}: \
                     {}. Nothing was attached to the app; run the tool again later",
                    spec.size,
                    after_retries(retries),
                    storage_error(&err)
                )));
            }
        }
    }
}

/// Часть, готовая к слиянию.
struct ComposedPart {
    key: String,
    part_object_id: String,
    etag: String,
}

/// Загрузка частями: инициализация, адрес и отправка каждой части, слияние.
async fn upload_multipart(
    ctx: &Context,
    spec: &UploadSpec<'_>,
    kind: MultipartFileType,
    tuning: &UploadTuning,
    masks: &mut Masks,
) -> Result<UploadedFile, ToolError> {
    let init = excluded(MULTIPART_INIT)?;
    let parts_op = excluded(MULTIPART_PARTS)?;
    let compose = excluded(MULTIPART_COMPOSE)?;

    let mut request = api_request(init)
        .with_query("appId", spec.app_id)
        .with_query("fileName", spec.file_name)
        .with_query("fileType", kind.code().to_string());
    if let Some(flag) = spec.chinese_mainland {
        request = request.with_query("chineseMainlandFlag", if flag { "1" } else { "0" });
    }
    let response = execute(
        ctx,
        request,
        "starting the chunked upload (`upload.multipart.init`)",
    )
    .await?;
    let body = json_body(&response);
    let object_id = required_text(body, "objectId", masks)
        .ok_or_else(|| invalid_response("the chunked upload response has no `objectId`"))?;
    let upload_id = required_text(body, "nspUploadId", masks)
        .ok_or_else(|| invalid_response("the chunked upload response has no `nspUploadId`"))?;
    let part_bytes = part_size(
        tuning.part_bytes,
        uint(body.get("nspPartMinSize")),
        uint(body.get("nspPartMaxSize")),
    );
    let count = spec.size.div_ceil(part_bytes);
    if count > MAX_PARTS {
        return Err(ToolError::new(format!(
            "the file of {} bytes needs {count} parts of {part_bytes} bytes, more than the \
             storage accepts ({MAX_PARTS})",
            spec.size
        )));
    }
    tracing::debug!(
        bytes = spec.size,
        part_bytes,
        parts = count,
        "загрузка частями"
    );

    let mut composed: Vec<ComposedPart> = Vec::new();
    let mut retries = 0u32;
    for number in 1..=count {
        let offset = (number - 1) * part_bytes;
        let length = part_bytes.min(spec.size - offset);
        let key = format!("additionalProp{number}");
        let mut part_retries = 0u32;
        loop {
            let mut part = Map::new();
            part.insert(key.clone(), json!({"length": length}));
            let request = api_request(parts_op)
                .with_query("objectId", object_id.as_str())
                .with_query("nspUploadId", upload_id.as_str())
                .with_json(Value::Object(part));
            let step = format!(
                "getting the upload URL of part {number} of {count} (`upload.multipart.parts`)"
            );
            let response = execute(ctx, request, &step).await?;
            let info = part_info(json_body(&response), &key).ok_or_else(|| {
                invalid_response(&format!(
                    "the chunk URL response has no `uploadInfoMap.{key}`"
                ))
            })?;
            let part_object_id = info
                .get("partObjectId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            masks.add(&part_object_id);
            let target = parse_target(&info, masks).map_err(|reason| {
                invalid_response(&format!("the URL of part {number} is unusable: {reason}"))
            })?;
            match ctx
                .api
                .upload_file(target.request(spec.path, offset, length))
                .await
            {
                Ok(uploaded) => {
                    let etag = uploaded
                        .etag
                        .as_deref()
                        .map(|etag| etag.trim().trim_matches('"').to_owned())
                        .filter(|etag| !etag.is_empty())
                        .ok_or_else(|| {
                            ToolError::new(format!(
                                "the file storage returned no ETag for part {number} of \
                                 {count}, so the parts cannot be merged. Nothing was attached \
                                 to the app"
                            ))
                        })?;
                    tracing::debug!(
                        part = number,
                        parts = count,
                        bytes = length,
                        "часть загружена"
                    );
                    composed.push(ComposedPart {
                        key,
                        part_object_id,
                        etag,
                    });
                    break;
                }
                Err(err) if is_retryable(&err) && part_retries < tuning.retries => {
                    part_retries += 1;
                    retries += 1;
                    tracing::debug!(
                        part = number,
                        retry = part_retries,
                        status = err.status().unwrap_or(0),
                        "повтор части с новым адресом"
                    );
                    tokio::time::sleep(backoff(tuning.retry_delay, part_retries)).await;
                }
                Err(err) => {
                    return Err(ToolError::new(format!(
                        "uploading part {number} of {count} (bytes {offset}-{}) to the \
                         AppGallery Connect file storage failed{}: {}. The parts were not \
                         merged and nothing was attached to the app; run the tool again later",
                        offset + length - 1,
                        after_retries(part_retries),
                        storage_error(&err)
                    )));
                }
            }
        }
    }

    let mut parts = Map::new();
    for part in &composed {
        parts.insert(
            part.key.clone(),
            json!({"partObjectId": part.part_object_id, "etag": part.etag}),
        );
    }
    let request = api_request(compose)
        .with_query("objectId", object_id.as_str())
        .with_query("nspUploadId", upload_id.as_str())
        .with_json(Value::Object(parts));
    execute(
        ctx,
        request,
        "merging the uploaded parts (`upload.multipart.compose`)",
    )
    .await?;
    tracing::debug!(parts = count, retries, "части объединены");
    Ok(UploadedFile {
        object_id,
        bytes: spec.size,
        multipart: true,
        parts: composed.len(),
        retries,
        masks: Masks::default(),
    })
}

/// Размер части: желаемый, но в пределах ответа инициализации и справочника.
fn part_size(wanted: u64, min: Option<u64>, max: Option<u64>) -> u64 {
    let mut size = wanted.min(MAX_PART_BYTES);
    if let Some(max) = max.filter(|max| *max > 0) {
        size = size.min(max);
    }
    if let Some(min) = min {
        size = size.max(min);
    }
    size.max(1)
}

/// Исключённая операция Upload Management API по `operationId`.
fn excluded(operation_id: &str) -> Result<&'static ExcludedOperationDef, ToolError> {
    find_excluded(operation_id).ok_or_else(|| {
        ToolError::new(format!(
            "operation `{operation_id}` is missing from the API table; this is a server bug"
        ))
    })
}

/// Запрос операции Upload Management API (путь без параметров, конверт `ret.code`).
fn api_request(op: &'static ExcludedOperationDef) -> ApiRequest {
    ApiRequest::new(op.method, op.path)
        .with_result(ResultCheck::Envelope(RESULT_RET_CODE))
        .with_operation(op.operation_id)
}

async fn execute(ctx: &Context, request: ApiRequest, step: &str) -> Result<ApiResponse, ToolError> {
    ctx.api.execute(request).await.map_err(|err| {
        ToolError::new(format!(
            "{step} failed, nothing was attached to the app: {}",
            describe_api_error(&err)
        ))
    })
}

fn json_body(response: &ApiResponse) -> &Value {
    static NULL: Value = Value::Null;
    response.body.json().unwrap_or(&NULL)
}

fn invalid_response(reason: &str) -> ToolError {
    ToolError::new(format!(
        "AppGallery Connect returned an unexpected upload response ({reason}); nothing was \
         attached to the app"
    ))
}

/// Непустая строка поля; значение сразу добавляется в маски.
fn required_text(value: &Value, field: &str, masks: &mut Masks) -> Option<String> {
    let text = match value.get(field)? {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    masks.add(&text);
    Some(text)
}

/// Сведения о части из `uploadInfoMap`: по ключу части или единственная запись.
fn part_info(body: &Value, key: &str) -> Option<Value> {
    let map = body.get("uploadInfoMap")?.as_object()?;
    match map.get(key) {
        Some(info) => Some(info.clone()),
        None if map.len() == 1 => map.values().next().cloned(),
        None => None,
    }
}

/// Куда и как отправить файл: из `urlInfo` или `FilePartUploadInfo`.
struct Target {
    method: HttpMethod,
    url: String,
    headers: Vec<(String, String)>,
}

impl Target {
    fn request(&self, path: &Path, offset: u64, length: u64) -> UploadRequest {
        let mut request = UploadRequest::new(self.method, self.url.clone(), path);
        request.headers = self.headers.clone();
        request.with_range(offset, length)
    }
}

/// Разбирает адрес, метод и заголовки; адрес и значения заголовков добавляются в маски до
/// любых проверок. Текст ошибки — без значений.
fn parse_target(info: &Value, masks: &mut Masks) -> Result<Target, String> {
    let url = info
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or("no URL")?
        .to_owned();
    masks.add_url(&url);

    // Справочник описывает `headers` то объектом, то строкой с JSON внутри.
    let parsed;
    let headers = match info.get("headers") {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => Some(map),
        Some(Value::String(text)) if text.trim().is_empty() => None,
        Some(Value::String(text)) => {
            parsed = serde_json::from_str::<Value>(text).map_err(|_| "malformed headers")?;
            Some(parsed.as_object().ok_or("malformed headers")?)
        }
        Some(_) => return Err("malformed headers".into()),
    };
    let mut list = Vec::new();
    for (name, value) in headers.into_iter().flatten() {
        let value = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            _ => return Err("malformed headers".into()),
        };
        let lower = name.to_ascii_lowercase();
        if lower != "content-type" && lower != "content-length" {
            masks.add(&value);
        }
        list.push((name.clone(), value));
    }

    let method = match info.get("method").and_then(Value::as_str).map(str::trim) {
        None | Some("") => HttpMethod::Put,
        Some(method) if method.eq_ignore_ascii_case("PUT") => HttpMethod::Put,
        Some(method) if method.eq_ignore_ascii_case("POST") => HttpMethod::Post,
        Some(_) => return Err("unsupported upload method".into()),
    };
    Ok(Target {
        method,
        url,
        headers: list,
    })
}

/// Стоит ли повторить отправку с новым адресом.
fn is_retryable(err: &ApiError) -> bool {
    match err {
        ApiError::Transport(_) => true,
        ApiError::Http(http) => matches!(http.status, 403 | 408 | 429 | 500..=599),
        _ => false,
    }
}

/// Текст ошибки хранилища: статус, код и текст хранилища, без адреса.
fn storage_error(err: &ApiError) -> String {
    let Some(http) = err.http() else {
        return describe_api_error(err);
    };
    let mut text = format!("the storage returned HTTP {}", http.status);
    match (&http.code, &http.message) {
        (Some(code), Some(message)) => text.push_str(&format!(" ({code}: {message})")),
        (Some(code), None) => text.push_str(&format!(" ({code})")),
        (None, Some(message)) => text.push_str(&format!(" ({message})")),
        (None, None) if !http.body.trim().is_empty() => {
            text.push_str(&format!(" ({})", http.body.trim()));
        }
        (None, None) => {}
    }
    text
}

fn after_retries(retries: u32) -> String {
    match retries {
        0 => String::new(),
        1 => " after 1 retry".to_owned(),
        n => format!(" after {n} retries"),
    }
}

/// Пауза перед повтором `attempt` (с 1): `delay`, `2·delay`, `4·delay`, …
fn backoff(delay: Duration, attempt: u32) -> Duration {
    delay.saturating_mul(1u32 << attempt.saturating_sub(1).min(5))
}

/// Неотрицательное целое из числа или строки.
fn uint(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Значения, которые нельзя показывать: адреса, подписи, идентификаторы загрузки.
#[derive(Debug, Default, Clone)]
struct Masks(Vec<String>);

impl Masks {
    fn add(&mut self, value: &str) {
        let value = value.trim();
        if value.len() >= MIN_MASKED_LEN && !self.0.iter().any(|known| known == value) {
            self.0.push(value.to_owned());
        }
    }

    /// Адрес целиком, путь, строка запроса и значения её параметров.
    fn add_url(&mut self, url: &str) {
        self.add(url);
        let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
        let (location, query) = rest.split_once('?').unwrap_or((rest, ""));
        if let Some((_, path)) = location.split_once('/') {
            self.add(path);
        }
        self.add(query);
        for pair in query.split('&') {
            if let Some((_, value)) = pair.split_once('=') {
                self.add(value);
            }
        }
    }

    fn apply(&self, text: &str) -> String {
        let mut values: Vec<&String> = self.0.iter().collect();
        // Сначала длинные: адрес целиком раньше своего пути.
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        let mut text = text.to_owned();
        for value in values {
            if text.contains(value.as_str()) {
                text = text.replace(value.as_str(), "***");
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_follow_the_threshold() {
        let tuning = UploadTuning::default();
        let small = plan(
            MULTIPART_THRESHOLD,
            Some(MultipartFileType::AndroidPackage),
            &tuning,
        );
        assert!(!small.multipart);
        assert_eq!(small.parts, 1);
        let large = plan(
            MULTIPART_THRESHOLD + 1,
            Some(MultipartFileType::AndroidPackage),
            &tuning,
        );
        assert!(large.multipart);
        assert_eq!(large.part_bytes, DEFAULT_PART_BYTES);
        assert_eq!(large.parts, 2);
        // Без вида файла для частей — всегда целиком.
        assert!(!plan(4096 * MIB, None, &tuning).multipart);
    }

    #[test]
    fn part_size_stays_within_limits() {
        assert_eq!(
            part_size(64 * MIB, Some(5 * MIB), Some(1024 * MIB)),
            64 * MIB
        );
        assert_eq!(part_size(1024 * MIB, None, None), MAX_PART_BYTES);
        assert_eq!(part_size(MIB, Some(5 * MIB), None), 5 * MIB);
        assert_eq!(part_size(64 * MIB, None, Some(10 * MIB)), 10 * MIB);
        assert_eq!(part_size(0, None, Some(0)), 1);
    }

    #[test]
    fn masks_hide_urls_and_parts_of_them() {
        let signature = ["Sig", "nature", "0123456789"].concat();
        let mut masks = Masks::default();
        masks.add_url(&format!(
            "https://bucket.obs.example.com/CN/obj-1234.apk?uploadId=up-98765&X-Amz-Signature={signature}"
        ));
        masks.add("abc");
        let text = masks.apply(&format!(
            "error at /CN/obj-1234.apk with {signature} and up-98765, abc"
        ));
        assert!(!text.contains(&signature), "{text}");
        assert!(!text.contains("obj-1234"), "{text}");
        assert!(!text.contains("up-98765"), "{text}");
        assert!(text.contains("abc"), "{text}");
    }

    #[test]
    fn targets_take_headers_as_object_or_string() {
        let mut masks = Masks::default();
        let target = parse_target(
            &json!({"url": "https://h.example.com/o", "method": "put",
                "headers": "{\"Authorization\":\"AWS4-secret-value\",\"Content-Type\":\"application/octet-stream\"}"}),
            &mut masks,
        )
        .unwrap();
        assert_eq!(target.method, HttpMethod::Put);
        assert_eq!(target.headers.len(), 2);
        assert!(masks.apply("AWS4-secret-value").contains("***"));
        assert!(!masks.apply("application/octet-stream").contains("***"));

        let target = parse_target(&json!({"url": "https://h.example.com/o"}), &mut masks).unwrap();
        assert!(target.headers.is_empty());
        for bad in [
            json!({}),
            json!({"url": "https://h.example.com/o", "method": "GET"}),
            json!({"url": "https://h.example.com/o", "headers": [1]}),
            json!({"url": "https://h.example.com/o", "headers": "not json"}),
        ] {
            let err = parse_target(&bad, &mut masks).err().unwrap();
            assert!(!err.contains("h.example.com"), "{err}");
        }
    }

    #[test]
    fn retries_only_what_a_new_url_can_fix() {
        use appgallery_connect_api::HttpError;
        let http = |status| {
            ApiError::Http(Box::new(HttpError {
                status,
                ..HttpError::default()
            }))
        };
        assert!(is_retryable(&ApiError::Transport("reset".into())));
        for status in [403, 408, 429, 500, 503] {
            assert!(is_retryable(&http(status)), "{status}");
        }
        for status in [400, 404, 413] {
            assert!(!is_retryable(&http(status)), "{status}");
        }
        assert!(!is_retryable(&ApiError::Io("x".into())));
        assert!(!is_retryable(&ApiError::HostNotAllowed("x".into())));
        assert_eq!(backoff(Duration::from_secs(2), 1), Duration::from_secs(2));
        assert_eq!(backoff(Duration::from_secs(2), 3), Duration::from_secs(8));
    }

    #[test]
    fn upload_operations_are_in_the_excluded_table() {
        for id in [
            UPLOAD_URL,
            MULTIPART_INIT,
            MULTIPART_PARTS,
            MULTIPART_COMPOSE,
        ] {
            let op = excluded(id).unwrap();
            assert!(!op.callable, "{id}");
            assert!(op.path.starts_with("/api/publish/v2/"), "{id}");
        }
        assert_eq!(excluded(UPLOAD_URL).unwrap().method, HttpMethod::Get);
        assert_eq!(excluded(MULTIPART_PARTS).unwrap().method, HttpMethod::Post);
    }

    #[test]
    fn hex_is_lowercase() {
        assert_eq!(hex(&[0x00, 0xab, 0x1f]), "00ab1f");
    }
}
