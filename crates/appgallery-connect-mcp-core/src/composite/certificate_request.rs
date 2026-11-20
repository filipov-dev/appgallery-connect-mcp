//! `certificate_request`: сертификат подписи HarmonyOS по запросу на подпись (CSR) из
//! локального файла.
//!
//! Операция Provisioning API `provisioning.cert.create` принимает CSR строкой в теле
//! (`csr`, до 2048 символов). Чтобы агент не вставлял CSR (а по ошибке — приватный ключ)
//! текстом в аргументы, операция исключена из инструментов и из `api_request`
//! (`[[exclude]]` с `callable = false` в дополнении спецификации), а этот инструмент
//! читает CSR только из файла через [`Context::files`] и проверяет его форму до запроса:
//!
//! - файл — текст UTF-8 не больше [`MAX_FILE_BYTES`];
//! - в нём нет приватного ключа (`PRIVATE KEY` в любой строке `-----BEGIN …-----`): такой
//!   файл отклоняется целиком, без запроса и без пересказа содержимого;
//! - ровно один блок PEM `CERTIFICATE REQUEST` или `NEW CERTIFICATE REQUEST` (второй вид
//!   пишут DevEco Studio и `keytool`), без заголовков блока, с base64 внутри; данные DER
//!   начинаются с `SEQUENCE` (первый символ base64 — `M`);
//! - блок после нормализации (концы строк `\n`, пробелы по краям строк убраны) не длиннее
//!   [`MAX_CSR_CHARS`] — ограничения поля `csr` в справочнике.
//!
//! Путь `save_to` проверяется до чтения CSR и до запроса. Запрос `POST` не повторяется
//! (неидемпотентный): повтор создал бы второй сертификат. Ответ собирается из выбранных
//! полей `certInfo` (идентификатор, имя, тип, время создания и окончания); временная
//! ссылка `certDownloadUrl` в ответ не попадает: с `save_to` сервер скачивает файл
//! сертификата сам ([`ApiClient::download_to_file`], только с хостов Huawei), без него —
//! пометка говорит, как получить файл позже (`list_certificates` с `certIds` и `save_to`).
//! Если сертификат создан, а файл не сохранён, это не ошибка инструмента: повторять запрос
//! нельзя, пометка объясняет, что вышло.
//!
//! [`ApiClient::download_to_file`]: appgallery_connect_api::ApiClient::download_to_file

use appgallery_connect_api::AccessLevel;
use appgallery_connect_api::envelope::RESULT_RET_CODE;
use appgallery_connect_api::executor::{ApiError, ApiRequest, ResultCheck};
use appgallery_connect_api::spec::{ExcludedOperationDef, find_excluded};
use serde_json::{Map, Value, json};

use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};
use crate::files::display_path;

/// Имя инструмента.
pub const NAME: &str = "certificate_request";

/// `provisioning.cert.create`: исключена из инструментов и `api_request`.
pub const CERT_CREATE: &str = "provisioning.cert.create";

/// Поле ответа со сведениями о сертификате.
const CERT_INFO: &str = "certInfo";
/// Временная ссылка на файл сертификата внутри [`CERT_INFO`]. То же поле у
/// `list_certificates` указано в `[[secrets]]`, поэтому его имя скрывается и в любом тексте.
const DOWNLOAD_URL: &str = "certDownloadUrl";
/// Поля `certInfo`, которые попадают в ответ агенту.
const CERT_FIELDS: &[&str] = &["id", "certName", "certType", "createTime", "expireTime"];

/// Самый большой файл CSR, который инструмент читает: настоящий CSR — около килобайта.
pub const MAX_FILE_BYTES: u64 = 16 * 1024;
/// Длина CSR в запросе: `csr` — `String(2048)` по справочнику.
pub const MAX_CSR_CHARS: usize = 2048;
/// Длина имени сертификата: `certName` — `String(100)`.
const MAX_NAME_CHARS: usize = 100;

/// Виды блоков PEM, которые принимаются как CSR.
const CSR_LABELS: &[&str] = &["CERTIFICATE REQUEST", "NEW CERTIFICATE REQUEST"];

/// Результаты справочника, к которым есть подсказка.
const CSR_INVALID: &str = "204144766";
const DUPLICATE_NAME: &str = "204144767";
const TOO_MANY_CERTS: &str = "204144765";

const DESCRIPTION: &str = "Requests a HarmonyOS signing certificate (Provisioning API) from a \
certificate signing request (CSR) in a local file and saves the certificate file (.cer) to \
`save_to`. `csr_file` is the .csr file the user generated with the signing key (DevEco Studio: \
Build > Generate Key and CSR); the tool checks that it is one PEM certificate signing request \
and refuses files with a private key: never pass the private key or the keystore (.p12), and \
never paste a CSR or a key into the conversation. `certType`: 1 debug, 2 release, 3 in-house \
release, 4 binary (only for accounts that Huawei allowed). Each call creates a new certificate \
and the account has a limit of certificates, so call it only when the user asks for a new \
certificate and do not repeat a call that succeeded. Pass `save_to` in the same call: the \
download link expires after about five minutes; later `list_certificates` with `certIds` and \
`save_to` saves the file again. The result has the certificate ID, name, type and times; the \
download link is never returned.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "provisioning",
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
            "csr_file": {
                "type": "string",
                "minLength": 1,
                "description": "Path of the local certificate signing request file (PEM, \
                    usually .csr; absolute, or relative to the server's working directory; \
                    only inside the allowed directories listed in the server instructions). \
                    Files with a private key are refused."
            },
            "certName": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_NAME_CHARS,
                "description": "Certificate name, unique in the account (up to 100 characters)."
            },
            "certType": {
                "type": "integer",
                "enum": [1, 2, 3, 4],
                "description": "Certificate type: 1 debug, 2 release, 3 in-house release, 4 \
                    binary certificate (for signing binary programs; available only to accounts \
                    that Huawei allowed)."
            },
            "save_to": {
                "type": "string",
                "minLength": 1,
                "description": "Path of a new local file for the certificate (.cer; absolute, \
                    or relative to the server's working directory; only inside the allowed \
                    directories). The parent directory must exist; existing files are never \
                    overwritten."
            }
        },
        "required": ["csr_file", "certName", "certType"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(certificate_request(ctx, args))
}

async fn certificate_request(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let csr_file = text_arg(&args, "csr_file")?;
    let cert_name = text_arg(&args, "certName")?.trim().to_owned();
    if cert_name.is_empty() || cert_name.chars().count() > MAX_NAME_CHARS {
        return Err(ToolError::new(format!(
            "`certName` must be 1 to {MAX_NAME_CHARS} characters"
        )));
    }
    let cert_type = match args.get("certType").and_then(Value::as_i64) {
        Some(value @ 1..=4) => value,
        _ => {
            return Err(ToolError::new(
                "`certType` must be 1 (debug), 2 (release), 3 (in-house release) or 4 (binary)",
            ));
        }
    };
    // Пути проверяются до чтения файла и до запроса: ошибка в пути не должна стоить
    // сертификата из лимита кабинета.
    let target = match args.get("save_to") {
        Some(Value::String(path)) if !path.trim().is_empty() => {
            Some(ctx.files.resolve_write(path)?)
        }
        Some(Value::String(_)) => {
            return Err(ToolError::new("`save_to` must not be empty"));
        }
        _ => None,
    };
    let csr_path = ctx.files.resolve_read(csr_file)?;
    let csr = read_csr(&csr_path, csr_file)?;
    let op = excluded(CERT_CREATE)?;
    tracing::debug!(
        tool = NAME,
        cert_type,
        save_to = target.is_some(),
        "запрос сертификата"
    );
    let request = ApiRequest::new(op.method, op.path)
        .with_result(ResultCheck::Envelope(RESULT_RET_CODE))
        .with_operation(op.operation_id)
        .with_json(json!({"csr": csr, "certName": cert_name, "certType": cert_type}));
    let response = ctx.api.execute(request).await.map_err(|err| {
        ToolError::new(format!(
            "the certificate was not created: {}{}",
            describe_api_error(&err),
            error_hint(&err)
        ))
    })?;

    let info = response
        .body
        .json()
        .and_then(|body| body.get(CERT_INFO))
        .and_then(Value::as_object);
    let mut certificate = Map::new();
    if let Some(info) = info {
        for field in CERT_FIELDS {
            if let Some(value) = info.get(*field) {
                certificate.insert((*field).to_owned(), value.clone());
            }
        }
    }
    let cert_id = certificate.get("id").and_then(id_text);
    let later = match &cert_id {
        Some(id) => format!(
            "call `list_certificates` with `body: {{\"certIds\": [\"{id}\"]}}` and `save_to` to \
             save the file with a new link"
        ),
        None => "find the certificate with `list_certificates` and save its file with `certIds` \
                 and `save_to`"
            .to_owned(),
    };
    let mut notes = Vec::new();
    let mut result = json!({
        "created": true,
        "certificate": Value::Object(certificate),
        "csrFile": display_path(&csr_path),
    });
    let url = info
        .and_then(|info| info.get(DOWNLOAD_URL))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty());
    if info.is_none() {
        notes.push(format!(
            "The request succeeded, but the response has no certificate information; do not \
             repeat the request: {later}."
        ));
    }
    match (target, url) {
        (Some(target), Some(url)) => match ctx.api.download_to_file(url, &target).await {
            Ok(file) => {
                tracing::debug!(tool = NAME, bytes = file.bytes, "файл сертификата сохранён");
                let mut saved = json!({"path": display_path(&file.path), "bytes": file.bytes});
                if let Some(content_type) = file.content_type {
                    saved["content_type"] = Value::String(content_type);
                }
                result["saved"] = saved;
            }
            Err(err) => notes.push(format!(
                "The certificate was created, but its file was not saved to `save_to`: could not \
                 download the file: {}. Do not request the certificate again: {later}.",
                ctx.api.scrub(&err.to_string())
            )),
        },
        (Some(_), None) => notes.push(format!(
            "The certificate was created, but the response has no download link, so nothing was \
             saved to `save_to`. Do not request the certificate again: {later}."
        )),
        (None, _) => notes.push(format!(
            "The download link of the certificate file is hidden; it is valid for about five \
             minutes. To get the file, {later}."
        )),
    }
    notes.push(
        "The certificate is not secret, but keep the private key of the CSR private: never \
         paste it into the conversation or commit it."
            .to_owned(),
    );
    result["notes"] = json!(notes);
    Ok(result)
}

/// Непустая строка аргумента.
fn text_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    match args.get(name) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text),
        _ => Err(ToolError::new(format!("`{name}` is required"))),
    }
}

/// Идентификатор строкой (в ответах Huawei он бывает и числом).
fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Исключённая операция по `operationId`.
fn excluded(operation_id: &str) -> Result<&'static ExcludedOperationDef, ToolError> {
    find_excluded(operation_id).ok_or_else(|| {
        ToolError::new(format!(
            "operation `{operation_id}` is missing from the API table; this is a server bug"
        ))
    })
}

/// Подсказка к известным кодам результата Provisioning API.
fn error_hint(err: &ApiError) -> &'static str {
    let ApiError::Result(result) = err else {
        return "";
    };
    match result.code.as_str() {
        CSR_INVALID => {
            " AppGallery Connect rejected the CSR: ask the user to generate a new key and CSR \
             (DevEco Studio: Build > Generate Key and CSR) and pass the new .csr file."
        }
        DUPLICATE_NAME => " A certificate with this name exists: choose another `certName`.",
        TOO_MANY_CERTS => {
            " The account has the maximum number of certificates of this type: the user can \
             delete an unused one (`delete_certificates`, destructive) after checking that no \
             profile uses it."
        }
        _ => "",
    }
}

/// Читает и проверяет файл CSR. Ошибки не пересказывают содержимое файла.
fn read_csr(path: &std::path::Path, shown: &str) -> Result<String, ToolError> {
    let size = std::fs::metadata(path)
        .map_err(|err| ToolError::new(format!("could not read `{shown}`: {err}")))?
        .len();
    if size == 0 {
        return Err(ToolError::new(format!("`{shown}` is empty")));
    }
    if size > MAX_FILE_BYTES {
        return Err(ToolError::new(format!(
            "`{shown}` is {size} bytes, which is too large for a certificate signing request \
             (up to {MAX_FILE_BYTES} bytes are read): pass the .csr file"
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|err| ToolError::new(format!("could not read `{shown}`: {err}")))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        ToolError::new(format!(
            "`{shown}` is not a text PEM file: pass the .csr file, not a keystore (.p12) or a \
             binary file"
        ))
    })?;
    parse_csr(&text).map_err(|reason| ToolError::new(format!("`{shown}` {reason}")))
}

/// Блок PEM CSR в нормализованном виде или причина отказа (продолжение фразы после имени
/// файла).
fn parse_csr(text: &str) -> Result<String, String> {
    let lines: Vec<&str> = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .collect();
    let mut blocks: Vec<(usize, &str)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(label) = pem_label(line, "BEGIN") {
            if label.contains("PRIVATE KEY") {
                return Err(
                    "holds a private key, not a certificate signing request: nothing was sent. \
                     Pass the .csr file; never pass or paste the private key, and keep the key \
                     file private."
                        .to_owned(),
                );
            }
            blocks.push((index, label));
        }
    }
    let (start, label) = match blocks.as_slice() {
        [] => {
            return Err(
                "is not a PEM certificate signing request: there is no `-----BEGIN CERTIFICATE \
                 REQUEST-----` or `-----BEGIN NEW CERTIFICATE REQUEST-----` line"
                    .to_owned(),
            );
        }
        [one] => *one,
        many => {
            return Err(format!(
                "holds {} PEM blocks; pass a file with one certificate signing request",
                many.len()
            ));
        }
    };
    if !CSR_LABELS.contains(&label) {
        let shown =
            if label.len() <= 40 && label.chars().all(|c| c.is_ascii_uppercase() || c == ' ') {
                format!("`{label}`")
            } else {
                "of another kind".to_owned()
            };
        return Err(format!(
            "holds a PEM block {shown}, not a certificate signing request (`CERTIFICATE \
             REQUEST`)"
        ));
    }
    let Some(end) = lines
        .iter()
        .skip(start + 1)
        .position(|line| line.starts_with("-----"))
        .map(|offset| start + 1 + offset)
    else {
        return Err(format!(
            "is not valid PEM: the `-----END {label}-----` line is missing"
        ));
    };
    if pem_label(lines[end], "END") != Some(label) {
        return Err(format!(
            "is not valid PEM: the block must end with `-----END {label}-----`"
        ));
    }
    let body: Vec<&str> = lines[start + 1..end]
        .iter()
        .copied()
        .filter(|line| !line.is_empty())
        .collect();
    check_base64(&body)?;
    let csr = format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----",
        body.join("\n")
    );
    let chars = csr.chars().count();
    if chars > MAX_CSR_CHARS {
        return Err(format!(
            "holds a certificate signing request of {chars} characters; AppGallery Connect \
             accepts up to {MAX_CSR_CHARS}"
        ));
    }
    Ok(csr)
}

/// Вид блока из строки `-----BEGIN <вид>-----` или `-----END <вид>-----`.
fn pem_label<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.strip_prefix("-----")?
        .strip_prefix(marker)?
        .strip_prefix(' ')?
        .strip_suffix("-----")
        .map(str::trim)
}

/// Тело блока: base64 без заголовков, данные DER начинаются с `SEQUENCE`.
fn check_base64(lines: &[&str]) -> Result<(), String> {
    if lines.is_empty() {
        return Err("is not valid PEM: the certificate signing request block is empty".into());
    }
    if lines.iter().any(|line| line.contains(':')) {
        return Err(
            "is not valid PEM: encrypted or annotated blocks (header lines) are not supported"
                .into(),
        );
    }
    let data: String = lines.concat();
    let padding = data.len() - data.trim_end_matches('=').len();
    let valid = data
        .trim_end_matches('=')
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/');
    if !valid || padding > 2 || !data.len().is_multiple_of(4) {
        return Err("is not valid PEM: the block is not base64".into());
    }
    // 0x30 (SEQUENCE) в первом байте даёт первый символ base64 `M`.
    if !data.starts_with('M') {
        return Err("is not valid PEM: the block does not hold DER data".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use appgallery_connect_api::token::StaticToken;
    use appgallery_connect_api::{
        ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy, TransferHosts,
    };
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::{MemoryVault, ProfileInfo};
    use crate::files::FilePolicy;

    const CERT: &str = "/api/publish/v3/cert";

    /// Тело CSR: base64 с `SEQUENCE` в начале, не настоящий запрос.
    fn csr_body() -> Vec<String> {
        vec![
            ["MIIBPTCB5AIBADBC", "MQswCQYDVQQGEwJS"].concat(),
            ["VTEPMA0GA1UEChMG", "RXhhbXBsZTEiMCAG"].concat(),
            "A1UEAxMZ".to_owned(),
        ]
    }

    fn csr_pem(label: &str, eol: &str) -> String {
        format!(
            "-----BEGIN {label}-----{eol}{}{eol}-----END {label}-----{eol}",
            csr_body().join(eol)
        )
    }

    fn expected_csr(label: &str) -> String {
        csr_pem(label, "\n").trim_end().to_owned()
    }

    fn signature() -> String {
        ["Cert", "Link", "Sig", "4711"].concat()
    }

    fn context(server: &MockServer, dir: &std::path::Path) -> Context {
        let id = ["2718", "2818", "2845"].concat();
        let secret = ["QW12", "ER34", "TY56"].concat().repeat(4);
        let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
        let mut config = ClientConfig::new(Region::Cn, Some(credentials));
        config.base_url = Some(server.uri());
        config.retry = RetryPolicy {
            max_retries: 2,
            default_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        };
        config.transfer.hosts = TransferHosts::HuaweiAndLoopback;
        let token = ["cert", "-test-", "token"].concat();
        let api = ApiClient::with_token_source(config, Arc::new(StaticToken::new(token))).unwrap();
        Context {
            api: Arc::new(api),
            files: Arc::new(
                FilePolicy::new(&[dir.to_path_buf()])
                    .unwrap()
                    .with_base_dir(dir),
            ),
            vault: Arc::new(MemoryVault::default()),
            profile: ProfileInfo::default(),
            access: AccessLevel::Write,
            http: false,
        }
    }

    async fn mount_create(server: &MockServer, label: &str, link: String) {
        Mock::given(method("POST"))
            .and(path(CERT))
            .and(body_json(json!({
                "csr": expected_csr(label),
                "certName": "debug-cert",
                "certType": 1,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 0, "msg": "success"},
                "certInfo": {
                    "id": "1443604834804035072",
                    "certName": "debug-cert",
                    "certType": 1,
                    "createTime": 1_718_363_106_749_i64,
                    "expireTime": 1_844_593_505_000_i64,
                    "certDownloadUrl": link,
                },
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn requests_the_certificate_and_saves_the_file() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("debug.csr"),
            format!("\u{feff}{}", csr_pem("NEW CERTIFICATE REQUEST", "\r\n")),
        )
        .unwrap();
        let link = format!(
            "{}/files/debug.cer?X-Amz-Signature={}",
            server.uri(),
            signature()
        );
        mount_create(&server, "NEW CERTIFICATE REQUEST", link).await;
        Mock::given(method("GET"))
            .and(path("/files/debug.cer"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"CERT-BYTES".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = context(&server, dir.path());

        let out = certificate_request(
            &ctx,
            json!({
                "csr_file": "debug.csr",
                "certName": " debug-cert ",
                "certType": 1,
                "save_to": "debug.cer",
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("debug.cer")).unwrap(),
            b"CERT-BYTES"
        );
        assert_eq!(out["created"], true);
        assert_eq!(out["certificate"]["id"], "1443604834804035072");
        assert_eq!(out["certificate"]["certType"], 1);
        assert_eq!(out["saved"]["bytes"], 10);
        assert!(
            out["saved"]["path"]
                .as_str()
                .unwrap()
                .ends_with("debug.cer")
        );
        let text = out.to_string();
        assert!(!text.contains(DOWNLOAD_URL), "{text}");
        assert!(!text.contains(&signature()), "{text}");
        assert!(!text.contains("MIIBPTCB"), "{text}");
    }

    #[tokio::test]
    async fn without_save_to_the_link_is_hidden_and_explained() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("debug.csr"),
            format!("comment\n{}", csr_pem("CERTIFICATE REQUEST", "\n")),
        )
        .unwrap();
        let link = format!(
            "https://cert.obs.example.myhuaweicloud.com/a.cer?s={}",
            signature()
        );
        mount_create(&server, "CERTIFICATE REQUEST", link).await;
        let ctx = context(&server, dir.path());
        let out = certificate_request(
            &ctx,
            json!({"csr_file": "debug.csr", "certName": "debug-cert", "certType": 1}),
        )
        .await
        .unwrap();
        assert!(out.get("saved").is_none(), "{out}");
        let notes = out["notes"].to_string();
        assert!(notes.contains("list_certificates"), "{notes}");
        assert!(notes.contains("1443604834804035072"), "{notes}");
        assert!(!out.to_string().contains(&signature()), "{out}");
    }

    #[tokio::test]
    async fn failed_download_after_creation_is_not_an_error() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("debug.csr"),
            csr_pem("CERTIFICATE REQUEST", "\n"),
        )
        .unwrap();
        // Ссылка на чужой хост: сертификат создан, файл не скачивается.
        let link = format!("https://evil.example.com/a.cer?s={}", signature());
        mount_create(&server, "CERTIFICATE REQUEST", link).await;
        let ctx = context(&server, dir.path());
        let out = certificate_request(
            &ctx,
            json!({
                "csr_file": "debug.csr",
                "certName": "debug-cert",
                "certType": 1,
                "save_to": "debug.cer",
            }),
        )
        .await
        .unwrap();
        assert!(out.get("saved").is_none(), "{out}");
        let notes = out["notes"][0].as_str().unwrap();
        assert!(
            notes.contains("was created, but its file was not saved"),
            "{notes}"
        );
        assert!(
            notes.contains("Do not request the certificate again"),
            "{notes}"
        );
        assert!(!out.to_string().contains(&signature()), "{out}");
        assert!(!dir.path().join("debug.cer").exists());
    }

    #[tokio::test]
    async fn bad_files_and_paths_are_refused_before_the_request() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let key = format!(
            "{}\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n{}\n",
            concat!("-----", "BEGIN EC PRIVATE KEY-----"),
            concat!("-----", "END EC PRIVATE KEY-----")
        );
        let files = [
            ("key.pem", key.clone()),
            (
                "both.pem",
                format!("{}{key}", csr_pem("CERTIFICATE REQUEST", "\n")),
            ),
            ("cert.pem", csr_pem("CERTIFICATE", "\n")),
            ("two.csr", csr_pem("CERTIFICATE REQUEST", "\n").repeat(2)),
            ("plain.txt", "hello".to_owned()),
            ("empty.csr", String::new()),
            (
                "noend.csr",
                "-----BEGIN CERTIFICATE REQUEST-----\nMIIB\n".to_owned(),
            ),
            (
                "notb64.csr",
                "-----BEGIN CERTIFICATE REQUEST-----\nMII*\n-----END CERTIFICATE REQUEST-----"
                    .to_owned(),
            ),
            (
                "notder.csr",
                "-----BEGIN CERTIFICATE REQUEST-----\nAAAA\n-----END CERTIFICATE REQUEST-----"
                    .to_owned(),
            ),
            (
                "long.csr",
                format!(
                    "-----BEGIN CERTIFICATE REQUEST-----\n{}\n-----END CERTIFICATE REQUEST-----",
                    "MIIB".repeat(600)
                ),
            ),
        ];
        for (name, content) in &files {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        std::fs::write(dir.path().join("binary.p12"), [0x30_u8, 0x82, 0xff, 0xfe]).unwrap();
        std::fs::write(dir.path().join("huge.csr"), vec![b'M'; 20 * 1024]).unwrap();
        std::fs::write(
            dir.path().join("ok.csr"),
            csr_pem("CERTIFICATE REQUEST", "\n"),
        )
        .unwrap();
        std::fs::write(dir.path().join("taken.cer"), "old").unwrap();
        let ctx = context(&server, dir.path());
        let call = |file: &str, save_to: Option<&str>| {
            let mut args = json!({"csr_file": file, "certName": "debug-cert", "certType": 1});
            if let Some(save_to) = save_to {
                args["save_to"] = json!(save_to);
            }
            certificate_request(&ctx, args)
        };
        for (file, expected) in [
            ("key.pem", "holds a private key"),
            ("both.pem", "holds a private key"),
            ("cert.pem", "PEM block `CERTIFICATE`"),
            ("two.csr", "holds 2 PEM blocks"),
            ("plain.txt", "is not a PEM certificate signing request"),
            ("empty.csr", "is empty"),
            ("noend.csr", "line is missing"),
            ("notb64.csr", "not base64"),
            ("notder.csr", "does not hold DER data"),
            ("long.csr", "accepts up to 2048"),
            ("binary.p12", "is not a text PEM file"),
            ("huge.csr", "too large"),
            ("missing.csr", "missing.csr"),
        ] {
            let err = call(file, None).await.unwrap_err();
            assert!(err.message().contains(expected), "{file}: {err}");
            assert!(!err.message().contains("MIIEvQ"), "{file}: {err}");
        }
        let outside_csr = outside.path().join("ok.csr");
        std::fs::write(&outside_csr, csr_pem("CERTIFICATE REQUEST", "\n")).unwrap();
        let err = call(outside_csr.to_str().unwrap(), None).await.unwrap_err();
        assert!(err.message().contains("outside the allowed"), "{err}");
        let err = call("ok.csr", Some("taken.cer")).await.unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
        let err = call("ok.csr", Some("../x.cer")).await.unwrap_err();
        assert!(err.message().contains("outside the allowed"), "{err}");
        let err = certificate_request(
            &ctx,
            json!({"csr_file": "ok.csr", "certName": "x".repeat(101), "certType": 1}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("certName"), "{err}");
        let err = certificate_request(
            &ctx,
            json!({"csr_file": "ok.csr", "certName": "x", "certType": 5}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("certType"), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn api_errors_are_not_retried_and_get_hints() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ok.csr"),
            csr_pem("CERTIFICATE REQUEST", "\n"),
        )
        .unwrap();
        Mock::given(method("POST"))
            .and(path(CERT))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 204144766, "msg": "invalid csr"},
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(CERT))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let ctx = context(&server, dir.path());
        let args = json!({"csr_file": "ok.csr", "certName": "debug-cert", "certType": 2});
        let err = certificate_request(&ctx, args.clone()).await.unwrap_err();
        assert!(err.message().contains("was not created"), "{err}");
        assert!(err.message().contains("204144766"), "{err}");
        assert!(err.message().contains("Generate Key and CSR"), "{err}");
        // 5xx у POST не повторяется: второй запрос создал бы второй сертификат.
        let err = certificate_request(&ctx, args).await.unwrap_err();
        assert!(err.message().contains("503"), "{err}");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[test]
    fn tool_is_registered_as_write_in_provisioning() {
        let tools = tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, NAME);
        assert_eq!(tools[0].toolset, "provisioning");
        assert_eq!(tools[0].access, AccessLevel::Write);
        let op = excluded(CERT_CREATE).unwrap();
        assert!(!op.callable);
        assert_eq!(op.toolset, "provisioning");
        assert_eq!(op.access, AccessLevel::Write);
        assert!(op.alternative.contains(NAME), "{}", op.alternative);
        // Ссылка на сертификат скрывается по имени в любом тексте: имя есть в `[[secrets]]`
        // у `list_certificates`.
        assert!(appgallery_connect_api::spec::secret_field_names().contains(&DOWNLOAD_URL));
        assert_eq!(
            parse_csr(&csr_pem("CERTIFICATE REQUEST", "\r\n")).unwrap(),
            expected_csr("CERTIFICATE REQUEST")
        );
    }
}
