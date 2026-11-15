//! Передача файлов на адреса, которые выдаёт сам AppGallery Connect API.
//!
//! Составным инструментам нужны два вида таких запросов:
//! - загрузка файла (сборки, изображения, части большого файла) на адрес из ответа Upload
//!   Management API: метод (`PUT`), адрес и заголовки приходят в `urlInfo` ответа, тело —
//!   содержимое файла ([`TransferClient::upload_file`]);
//! - скачивание по ссылке из ответа API, например `fileURL` отчёта Reports API, в
//!   локальный файл ([`TransferClient::download_to_file`]).
//!
//! Это отдельный клиент, а не [`crate::executor::ApiClient`]: путь и query таких адресов
//! задаёт Huawei, а заголовков авторизации Connect API (`client_id`, `Authorization:
//! Bearer <access token>`) у них быть не должно. Правила:
//! - только `https://` и только хосты Huawei из разрешённого списка
//!   ([`TRANSFER_HOST_SUFFIXES`]); порт — только 443; IP-адреса и адреса с именем
//!   пользователя отклоняются. Для тестов с локальным сервером список явно расширяется
//!   loopback-адресами ([`TransferHosts::HuaweiAndLoopback`]);
//! - редирект выполняется, только если ведёт на адрес, прошедший ту же проверку;
//! - токен Connect API на внешний адрес не отправляется. Заголовок `Authorization` в
//!   загрузке — это не токен Connect API, а подпись файлового хранилища из ответа API
//!   (`urlInfo.headers`): справочник требует передать её как есть, поэтому он разрешён;
//! - служебные заголовки ([`FORBIDDEN_UPLOAD_HEADERS`]) отклоняются; `Host` и
//!   `Content-Length` из ответа API не отправляются сами, а сверяются с адресом и
//!   размером файла;
//! - адрес (в нём подпись или идентификатор объекта), значения заголовков и тело ошибки
//!   хранилища с подписью не попадают в `Debug`, тексты ошибок и логи;
//! - повторов нет: адрес загрузки живёт около пяти минут, решение о повторе — за
//!   составным инструментом.
//!
//! Политику локальных путей (разрешённые каталоги) проверяет ядро: сюда приходит уже
//! проверенный абсолютный путь. Скачивание не перезаписывает существующий файл и удаляет
//! частично записанный.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::stream;
use reqwest::header::{self, HeaderName, HeaderValue};
use reqwest::redirect;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::envelope::detect_result;
use crate::executor::{ApiError, HttpError, HttpMethod};
use crate::http::{
    CONNECT_TIMEOUT, USER_AGENT, content_type, describe_client_error, describe_reqwest_error,
    is_loopback, lossy_prefix, read_limited, truncate,
};
use crate::scrub::scrub_secrets;

/// Суффиксы доменов Huawei, на которые разрешены загрузка и скачивание файлов.
///
/// Справочник не называет хосты файловых серверов: адрес загрузки описан как
/// `https://{domainname}.com/{objectId}` (Upload Management API), ссылка на отчёт — как
/// «путь к файлу» (Reports API). Список составлен по доменам, на которых Huawei держит
/// файлы AppGallery Connect:
/// - `.myhuaweicloud.com` — объектное хранилище OBS (адреса `…/for-obs`, отчёты);
/// - `.hicloud.com` — файловые серверы AppGallery (`developerfile*.hicloud.com`);
/// - `.dbankcloud.com`, `.dbankcloud.cn`, `.dbankcloud.ru`, `.dbankcloud.eu`,
///   `.dbankcloud.asia` — региональные серверы хранения и раздачи файлов AppGallery;
/// - `.huawei.com` — остальные хосты Huawei, в том числе сам Connect API.
///
/// Суффикс начинается с точки: `huawei.com.evil.example` и `evilhuawei.com` не проходят.
/// Если живая сверка покажет другой хост, он добавляется сюда.
pub const TRANSFER_HOST_SUFFIXES: &[&str] = &[
    ".huawei.com",
    ".hicloud.com",
    ".myhuaweicloud.com",
    ".dbankcloud.com",
    ".dbankcloud.cn",
    ".dbankcloud.ru",
    ".dbankcloud.eu",
    ".dbankcloud.asia",
];

/// Заголовки (в нижнем регистре), которые нельзя задать загрузке. Кроме них отклоняются
/// имена с префиксом `proxy-`.
pub const FORBIDDEN_UPLOAD_HEADERS: &[&str] = &[
    "client_id",
    "oauth2token",
    "cookie",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "expect",
];

/// Таймаут одной передачи файла по умолчанию (весь запрос, включая тело).
pub const DEFAULT_TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Лимит размера скачиваемого файла по умолчанию, байт.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Сколько байт тела ответа загрузки читается (успешного и ошибки).
const TRANSFER_RESPONSE_LIMIT: usize = 16 * 1024;

/// Сколько символов текста хранилища попадает в ошибку.
const TRANSFER_ERROR_TEXT_LIMIT: usize = 300;

/// Размер куска файла при загрузке.
const UPLOAD_CHUNK_BYTES: u64 = 64 * 1024;

/// Максимальное число редиректов.
const MAX_REDIRECTS: usize = 5;

/// На какие хосты разрешена передача файлов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferHosts {
    /// Только `https` на хосты Huawei ([`TRANSFER_HOST_SUFFIXES`]).
    #[default]
    Huawei,
    /// Дополнительно `http` и `https` на loopback-адреса: только для тестов с локальным
    /// сервером (`wiremock`).
    HuaweiAndLoopback,
}

/// Настройки передачи файлов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferConfig {
    pub hosts: TransferHosts,
    /// Таймаут одной передачи (весь запрос, включая тело).
    pub timeout: Duration,
    /// Лимит размера скачиваемого файла, байт.
    pub max_download_bytes: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            hosts: TransferHosts::Huawei,
            timeout: DEFAULT_TRANSFER_TIMEOUT,
            max_download_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
        }
    }
}

/// Проверяет адрес загрузки или скачивания: `https`, хост Huawei из разрешённого списка,
/// порт 443, без имени пользователя. Текст ошибки содержит только причину и имя хоста.
pub fn check_transfer_url(url: &str, hosts: TransferHosts) -> Result<url::Url, ApiError> {
    let parsed = url::Url::parse(url.trim())
        .map_err(|_| ApiError::HostNotAllowed("it is not an absolute URL".into()))?;
    check_parsed_url(&parsed, hosts).map_err(ApiError::HostNotAllowed)?;
    Ok(parsed)
}

fn check_parsed_url(url: &url::Url, hosts: TransferHosts) -> Result<(), String> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err("credentials in the URL are not allowed".into());
    }
    if hosts == TransferHosts::HuaweiAndLoopback
        && is_loopback(url)
        && matches!(url.scheme(), "http" | "https")
    {
        return Ok(());
    }
    if url.scheme() != "https" {
        return Err("only https URLs are allowed".into());
    }
    let Some(url::Host::Domain(host)) = url.host() else {
        return Err("the host must be a Huawei domain name, not an IP address".into());
    };
    if url.port().is_some_and(|port| port != 443) {
        return Err("only the default https port is allowed".into());
    }
    if !is_huawei_file_host(host) {
        return Err(format!(
            "host `{}` is not a Huawei file host",
            truncate(host, 100)
        ));
    }
    Ok(())
}

/// Хост оканчивается на один из суффиксов [`TRANSFER_HOST_SUFFIXES`].
pub fn is_huawei_file_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    TRANSFER_HOST_SUFFIXES
        .iter()
        .any(|suffix| host.len() > suffix.len() && host.ends_with(suffix))
}

/// Загрузка локального файла на адрес из ответа API.
#[derive(Clone)]
pub struct UploadRequest {
    /// Метод из ответа API: `PUT` или `POST`.
    pub method: HttpMethod,
    /// Абсолютный адрес загрузки. В `Debug` не выводится.
    pub url: String,
    /// Заголовки из ответа API (`urlInfo.headers`): подпись хранилища, `x-amz-*`,
    /// `Content-Type`. Значения в `Debug` не выводятся.
    pub headers: Vec<(String, String)>,
    /// Абсолютный путь к файлу (уже проверенный политикой локальных путей).
    pub file: PathBuf,
    /// С какого байта файла читать (загрузка частями).
    pub offset: u64,
    /// Сколько байт отправить; `None` — до конца файла.
    pub length: Option<u64>,
}

impl UploadRequest {
    /// Загрузка файла целиком.
    pub fn new(method: HttpMethod, url: impl Into<String>, file: impl Into<PathBuf>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            file: file.into(),
            offset: 0,
            length: None,
        }
    }

    /// Добавляет заголовок из ответа API.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Ограничивает загрузку частью файла: `length` байт начиная с `offset`.
    pub fn with_range(mut self, offset: u64, length: u64) -> Self {
        self.offset = offset;
        self.length = Some(length);
        self
    }
}

impl fmt::Debug for UploadRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("UploadRequest")
            .field("method", &self.method)
            .field("url", &"***")
            .field("headers", &names)
            .field("file", &self.file)
            .field("offset", &self.offset)
            .field("length", &self.length)
            .finish()
    }
}

/// Ответ хранилища на загрузку (статус 2xx).
#[derive(Debug, Clone, PartialEq)]
pub struct UploadResponse {
    pub status: u16,
    /// Сколько байт отправлено.
    pub bytes: u64,
    /// Заголовок `ETag` ответа: нужен для объединения частей файла.
    pub etag: Option<String>,
    /// JSON-тело ответа, если оно есть и разбирается.
    pub body: Option<Value>,
}

/// Скачанный файл.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedFile {
    pub path: PathBuf,
    /// Сколько байт записано.
    pub bytes: u64,
    pub content_type: Option<String>,
}

/// Клиент для адресов, выданных API. Без учётных данных Connect API.
#[derive(Debug, Clone)]
pub struct TransferClient {
    http: reqwest::Client,
    config: TransferConfig,
}

impl TransferClient {
    pub fn new(config: TransferConfig) -> Result<Self, ApiError> {
        let hosts = config.hosts;
        // Редирект — только на адрес, прошедший ту же проверку хоста.
        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if check_parsed_url(attempt.url(), hosts).is_ok() {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(config.timeout)
            .connect_timeout(CONNECT_TIMEOUT.min(config.timeout))
            .redirect(policy)
            .build()
            .map_err(|e| ApiError::InvalidConfig(describe_client_error(&e)))?;
        Ok(Self { http, config })
    }

    pub fn config(&self) -> &TransferConfig {
        &self.config
    }

    /// Загружает файл (или его часть) на адрес из ответа API. Тело — содержимое файла
    /// потоком, `Content-Length` — размер части. Ответ не-2xx — [`ApiError::Http`] с кодом
    /// и текстом хранилища без подписи.
    pub async fn upload_file(&self, request: UploadRequest) -> Result<UploadResponse, ApiError> {
        if !matches!(request.method, HttpMethod::Put | HttpMethod::Post) {
            return Err(ApiError::InvalidRequest(
                "files are uploaded with PUT or POST".into(),
            ));
        }
        let url = check_transfer_url(&request.url, self.config.hosts)?;

        let (mut file, size) = open_upload_file(&request.file).await?;
        let length = match request.length {
            Some(length) => length,
            None => size.saturating_sub(request.offset),
        };
        if request
            .offset
            .checked_add(length)
            .is_none_or(|end| end > size)
        {
            return Err(ApiError::InvalidRequest(format!(
                "the part to upload (offset {}, length {length}) is outside the file of {size} \
                 bytes",
                request.offset
            )));
        }

        let mut builder = self.http.request(request.method.to_reqwest(), url.clone());
        let mut has_content_type = false;
        for (name, value) in &request.headers {
            let lower = name.trim().to_ascii_lowercase();
            if FORBIDDEN_UPLOAD_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
                return Err(ApiError::InvalidRequest(format!(
                    "header `{}` cannot be set on an upload request",
                    truncate(&lower, 60)
                )));
            }
            match lower.as_str() {
                // `Host` задаёт адрес: значение из ответа API обязано с ним совпадать.
                "host" => {
                    if !host_matches(&url, value) {
                        return Err(ApiError::InvalidRequest(
                            "the Host header of the upload does not match the upload URL".into(),
                        ));
                    }
                    continue;
                }
                // `Content-Length` считает клиент по размеру части.
                "content-length" => {
                    if value.trim().parse::<u64>().ok() != Some(length) {
                        return Err(ApiError::InvalidRequest(
                            "the Content-Length header of the upload does not match the size \
                             of the file part"
                                .into(),
                        ));
                    }
                    continue;
                }
                "content-type" => has_content_type = true,
                _ => {}
            }
            let header_name = HeaderName::from_bytes(lower.as_bytes())
                .map_err(|_| ApiError::InvalidRequest("invalid name of an upload header".into()))?;
            let mut header_value = HeaderValue::from_str(value).map_err(|_| {
                ApiError::InvalidRequest(format!("invalid value of upload header `{header_name}`"))
            })?;
            // Значения — подпись хранилища: не должны попасть в отладочный вывод.
            header_value.set_sensitive(true);
            builder = builder.header(header_name, header_value);
        }
        if !has_content_type {
            builder = builder.header(header::CONTENT_TYPE, "application/octet-stream");
        }

        file.seek(std::io::SeekFrom::Start(request.offset))
            .await
            .map_err(|e| io_error("could not read the file", &request.file, &e))?;
        let body = if length == 0 {
            reqwest::Body::from(Vec::new())
        } else {
            reqwest::Body::wrap_stream(file_stream(file, length))
        };
        tracing::debug!(
            method = request.method.as_str(),
            bytes = length,
            "uploading a file to the AppGallery Connect file host"
        );
        let mut response = builder
            .header(header::CONTENT_LENGTH, length)
            .body(body)
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_reqwest_error(&e.without_url())))?;
        let status = response.status();
        if !status.is_success() {
            return Err(transfer_http_error(response).await);
        }
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let body = match read_limited(&mut response, TRANSFER_RESPONSE_LIMIT).await {
            Ok((bytes, false)) => serde_json::from_slice::<Value>(&bytes).ok(),
            _ => None,
        };
        Ok(UploadResponse {
            status: status.as_u16(),
            bytes: length,
            etag,
            body,
        })
    }

    /// Скачивает файл по ссылке из ответа API в `destination` с лимитом размера из
    /// настроек ([`TransferConfig::max_download_bytes`]).
    pub async fn download_to_file(
        &self,
        url: &str,
        destination: &Path,
    ) -> Result<DownloadedFile, ApiError> {
        self.download_to_file_with_limit(url, destination, self.config.max_download_bytes)
            .await
    }

    /// Скачивает файл по ссылке из ответа API в `destination` потоком, не больше
    /// `max_bytes`. Путь должен быть абсолютным, а файла с таким именем быть не должно:
    /// существующие файлы не перезаписываются. При ошибке и превышении лимита частично
    /// записанный файл удаляется.
    pub async fn download_to_file_with_limit(
        &self,
        url: &str,
        destination: &Path,
        max_bytes: u64,
    ) -> Result<DownloadedFile, ApiError> {
        let url = check_transfer_url(url, self.config.hosts)?;
        if !destination.is_absolute() {
            return Err(ApiError::InvalidRequest(format!(
                "the path to save the file must be absolute: `{}`",
                destination.display()
            )));
        }
        if tokio::fs::symlink_metadata(destination).await.is_ok() {
            return Err(ApiError::InvalidRequest(format!(
                "the file already exists and is not overwritten: `{}`",
                destination.display()
            )));
        }
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_reqwest_error(&e.without_url())))?;
        if !response.status().is_success() {
            return Err(transfer_http_error(response).await);
        }
        if response.content_length().is_some_and(|len| len > max_bytes) {
            return Err(ApiError::DownloadTooLarge { limit: max_bytes });
        }
        let content_type = content_type(&response);

        // create_new: не перезаписываем существующий файл и не идём по симлинку, даже если
        // он появился после проверки.
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .await
            .map_err(|e| io_error("could not create the file", destination, &e))?;
        let result = async {
            let mut written: u64 = 0;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| ApiError::Transport(describe_reqwest_error(&e.without_url())))?
            {
                written += chunk.len() as u64;
                if written > max_bytes {
                    return Err(ApiError::DownloadTooLarge { limit: max_bytes });
                }
                file.write_all(&chunk)
                    .await
                    .map_err(|e| io_error("could not write the file", destination, &e))?;
            }
            file.flush()
                .await
                .map_err(|e| io_error("could not write the file", destination, &e))?;
            Ok(written)
        }
        .await;
        drop(file);
        match result {
            Ok(bytes) => Ok(DownloadedFile {
                path: destination.to_owned(),
                bytes,
                content_type,
            }),
            Err(err) => {
                // Частично записанный файл бесполезен и вводит в заблуждение — удаляем.
                let _ = tokio::fs::remove_file(destination).await;
                Err(err)
            }
        }
    }
}

/// Открывает файл для загрузки и возвращает его размер.
async fn open_upload_file(path: &Path) -> Result<(tokio::fs::File, u64), ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::InvalidRequest(format!(
            "the path of the file to upload must be absolute: `{}`",
            path.display()
        )));
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| io_error("could not open the file", path, &e))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|e| io_error("could not read the file metadata", path, &e))?;
    if !metadata.is_file() {
        return Err(ApiError::InvalidRequest(format!(
            "`{}` is not a regular file",
            path.display()
        )));
    }
    Ok((file, metadata.len()))
}

/// Поток из `length` байт файла с текущей позиции. Если файл укоротили во время загрузки,
/// поток завершается ошибкой, а не отправляет меньше заявленного.
fn file_stream(
    file: tokio::fs::File,
    length: u64,
) -> impl futures_util::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static {
    stream::try_unfold((file, length), |(mut file, remaining)| async move {
        if remaining == 0 {
            return Ok(None);
        }
        let mut buffer = vec![0u8; remaining.min(UPLOAD_CHUNK_BYTES) as usize];
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file became shorter during the upload",
            ));
        }
        buffer.truncate(read);
        Ok(Some((buffer, (file, remaining - read as u64))))
    })
}

/// Значение заголовка `Host` из ответа API совпадает с хостом (и портом) адреса.
fn host_matches(url: &url::Url, value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let port = url.port_or_known_default();
    value == host || port.is_some_and(|port| value == format!("{host}:{port}"))
}

/// Ошибка хранилища. Тело может содержать подпись и строку для подписи (XML-ошибки
/// объектного хранилища), поэтому из XML берутся только `Code` и `Message`, из JSON —
/// конверт результата, а прочий текст обрезается и проходит вырезание секретов.
async fn transfer_http_error(mut response: reqwest::Response) -> ApiError {
    let status = response.status();
    let mut error = HttpError {
        status: status.as_u16(),
        ..HttpError::default()
    };
    if status.is_redirection() {
        error.body = "a redirect to a host that is not allowed was not followed".into();
        return ApiError::Http(Box::new(error));
    }
    let clean = |text: &str| truncate(&scrub_secrets(text.trim()), TRANSFER_ERROR_TEXT_LIMIT);
    if let Ok((bytes, _)) = read_limited(&mut response, TRANSFER_RESPONSE_LIMIT).await {
        let text = lossy_prefix(&bytes);
        let trimmed = text.trim();
        if trimmed.starts_with('<') {
            error.code = xml_tag(trimmed, "Code").map(clean);
            error.message = xml_tag(trimmed, "Message").map(clean);
        } else if let Some(result) = serde_json::from_str::<Value>(trimmed)
            .ok()
            .as_ref()
            .and_then(detect_result)
        {
            error.code = Some(clean(&result.code));
            error.message = result.message.as_deref().map(clean);
        } else {
            error.body = clean(trimmed);
        }
    }
    ApiError::Http(Box::new(error))
}

/// Текст первого элемента `<tag>…</tag>`.
fn xml_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end]).filter(|value| !value.trim().is_empty())
}

fn io_error(action: &str, path: &Path, err: &std::io::Error) -> ApiError {
    ApiError::Io(format!("{action} `{}`: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_huawei_https_hosts_only() {
        for url in [
            "https://developerfile7.hicloud.com/FileServer/uploadFile",
            "https://developerfile7.hicloud.com:443/FileServer/uploadFile",
            "https://bucket.obs.cn-north-4.myhuaweicloud.com/obj/1?X-Amz-Signature=abc",
            "https://store-dre.dbankcloud.eu/report.csv",
            "https://store.dbankcloud.ru/report.csv",
            "https://connect-api.cloud.huawei.com/api/x",
            "https://FILES.Huawei.COM/a",
            "https://files.huawei.com./a",
        ] {
            assert!(
                check_transfer_url(url, TransferHosts::Huawei).is_ok(),
                "{url}"
            );
        }
        for url in [
            "http://developerfile7.hicloud.com/FileServer/uploadFile",
            "https://developerfile7.hicloud.com:8443/x",
            "https://huawei.com.evil.example/x",
            "https://evilhuawei.com/x",
            "https://huawei.com/x",
            "https://.huawei.com/x",
            "https://evil.example/x?host=files.huawei.com",
            "https://evil.example/files.huawei.com",
            "https://files.huawei.com@evil.example/x",
            "https://user:pw@files.huawei.com/x",
            "https://127.0.0.1/x",
            "http://127.0.0.1:8080/x",
            "http://localhost/x",
            "https://[::1]/x",
            "ftp://files.huawei.com/x",
            "file:///etc/passwd",
            "//files.huawei.com/x",
            "/api/x",
            "",
        ] {
            let err = check_transfer_url(url, TransferHosts::Huawei).unwrap_err();
            assert!(matches!(err, ApiError::HostNotAllowed(_)), "{url}: {err:?}");
        }
    }

    #[test]
    fn loopback_is_allowed_only_explicitly() {
        let hosts = TransferHosts::HuaweiAndLoopback;
        assert!(check_transfer_url("http://127.0.0.1:8080/x", hosts).is_ok());
        assert!(check_transfer_url("http://localhost:1/x", hosts).is_ok());
        assert!(check_transfer_url("https://files.huawei.com/x", hosts).is_ok());
        assert!(check_transfer_url("http://files.huawei.com/x", hosts).is_err());
        assert!(check_transfer_url("http://192.168.1.10/x", hosts).is_err());
        assert!(check_transfer_url("http://evil.example/x", hosts).is_err());
        assert!(check_transfer_url("http://user@127.0.0.1/x", hosts).is_err());
    }

    #[test]
    fn errors_and_debug_do_not_contain_the_url() {
        let signature = ["Sig", "nature", "Value", "0123456789"].concat();
        let url = format!("https://evil.example/obj/secret-object?X-Amz-Signature={signature}");
        let err = check_transfer_url(&url, TransferHosts::Huawei)
            .unwrap_err()
            .to_string();
        assert!(err.contains("evil.example"), "{err}");
        assert!(!err.contains(&signature), "{err}");
        assert!(!err.contains("secret-object"), "{err}");

        let request = UploadRequest::new(HttpMethod::Put, url, "/tmp/app.apk")
            .with_header("Authorization", signature.clone());
        let debug = format!("{request:?}");
        assert!(!debug.contains(&signature), "{debug}");
        assert!(!debug.contains("secret-object"), "{debug}");
        assert!(debug.contains("Authorization"), "{debug}");
    }

    #[test]
    fn host_header_must_match_the_url() {
        let url = url::Url::parse("https://Bucket.obs.myhuaweicloud.com/obj").unwrap();
        assert!(host_matches(&url, "bucket.obs.myhuaweicloud.com"));
        assert!(host_matches(&url, "bucket.obs.myhuaweicloud.com:443"));
        assert!(!host_matches(&url, "other.obs.myhuaweicloud.com"));
        assert!(!host_matches(&url, "bucket.obs.myhuaweicloud.com:8443"));
    }

    #[test]
    fn xml_errors_keep_only_code_and_message() {
        let xml = "<Error><Code>SignatureDoesNotMatch</Code><Message>bad signature</Message>\
                   <StringToSign>secret</StringToSign></Error>";
        assert_eq!(xml_tag(xml, "Code"), Some("SignatureDoesNotMatch"));
        assert_eq!(xml_tag(xml, "Message"), Some("bad signature"));
        assert_eq!(xml_tag(xml, "RequestId"), None);
    }
}
