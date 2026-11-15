//! Общее для инструментов страницы приложения (`listing_export`, `listing_apply`,
//! `image_sync`): формат каталога, чтение каталога, текущее состояние в AppGallery
//! Connect, аргументы и разница текстов.
//!
//! Каталог повторяет формат play-console (fastlane `supply`) с полями AppGallery:
//!
//! ```text
//! <dir>/default_language.txt                язык по умолчанию (defaultLang)
//! <dir>/privacy_policy.txt                  ссылка на политику конфиденциальности (до 255)
//! <dir>/developer_website.txt               сайт разработчика (до 255)
//! <dir>/privacy_rights_url.txt              ссылка на управление личными данными (до 256)
//! <dir>/<lang>/title.txt                    название (appName, до 64 символов)
//! <dir>/<lang>/short_description.txt        краткое описание (briefInfo, до 80)
//! <dir>/<lang>/full_description.txt         полное описание (appDesc, до 8000)
//! <dir>/<lang>/release_notes.txt            что нового (newFeatures, до 500)
//! <dir>/<lang>/images/icon.png              иконка (fileType 0)
//! <dir>/<lang>/images/screenshots/1.png …   скриншоты (fileType 2)
//! <dir>/<lang>/videos/1.mp4 и 1.jpg …       видео и их обложки (fileType 1)
//! <dir>/.appgallery-images.json             манифест `image_sync`
//! ```
//!
//! `<lang>` — код языка AppGallery Connect из приложения справочника «Languages»
//! (`en-US`, `ru-RU`, `fil`, `es-419`, `he_IL`). Длины — по странице обновления
//! локализации (`publishing.languageInfo.update`) и схеме обновления сведений
//! (`publishing.appInfo.update`), в символах Unicode. Текст сравнивается без пробелов и
//! переводов строк по краям, `\r\n` считается `\n` (файлы из редакторов Windows).
//!
//! Каталог читается только через [`super::LocalFiles`]: сам каталог — `resolve_read_dir`,
//! каждый файл — `resolve_read` (символическая ссылка внутри каталога не выводит за
//! разрешённые каталоги). Символические ссылки на каталоги не обходятся.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

pub(crate) use super::reviews_summary::operation;
use super::{Context, reviews_summary};
use crate::error::{ToolError, describe_api_error};

/// Набор инструментов страницы приложения.
pub(crate) const TOOLSET: &str = "listing";

pub(crate) const APP_INFO_GET: &str = "publishing.appInfo.get";
pub(crate) const APP_INFO_UPDATE: &str = "publishing.appInfo.update";
pub(crate) const LANGUAGE_UPDATE: &str = "publishing.languageInfo.update";
pub(crate) const LANGUAGE_DELETE: &str = "publishing.languageInfo.delete";

/// Поля локализации: файл, поле `LanguageInfo`, наибольшая длина в символах.
///
/// Длины — по странице обновления локализации. Страница чтения сведений называет другие
/// (`briefInfo` до 60, `newFeatures` до 1000); проверка идёт по записи.
pub(crate) const LANGUAGE_FIELDS: &[(&str, &str, usize)] = &[
    ("title.txt", "appName", 64),
    ("short_description.txt", "briefInfo", 80),
    ("full_description.txt", "appDesc", 8000),
    ("release_notes.txt", "newFeatures", 500),
];

/// Основные сведения: файл в корне каталога, поле `AppInfo`, наибольшая длина.
pub(crate) const APP_FIELDS: &[(&str, &str, usize)] = &[
    ("default_language.txt", "defaultLang", 64),
    ("privacy_policy.txt", "privacyPolicy", 255),
    ("developer_website.txt", "developerWebsite", 255),
    ("privacy_rights_url.txt", "privacyRightsUrl", 256),
];

/// Коды языков AppGallery Connect (приложение справочника «Languages»). `he_IL` — с
/// подчёркиванием, так в справочнике.
pub(crate) const LANGUAGES: &[&str] = &[
    "ar", "az-AZ", "eu-ES", "be", "bn-BD", "bs", "bg", "my-MM", "ca", "zh-CN", "zh-TW", "zh-HK",
    "hr", "cs-CZ", "da-DK", "nl-NL", "en-US", "en-GB", "et", "fil", "fi-FI", "fr-FR", "gl-ES",
    "ka-GE", "de-DE", "el-GR", "he_IL", "hi-IN", "hu-HU", "id", "it-IT", "ja-JP", "jv", "kk",
    "km-KH", "ko-KR", "lo-LA", "lv", "lt", "mk-MK", "ms", "ne-NP", "no-NO", "fa", "pl-PL", "pt-BR",
    "pt-PT", "ro", "ru-RU", "sr", "si-LK", "sk", "sl", "es-419", "es-ES", "sv-SE", "th", "bo",
    "tr-TR", "uk", "ur", "uz", "vi",
];

/// Похоже ли имя каталога на код языка: буквы в начале, дальше буквы, цифры, `-` и `_`.
/// Каталоги вроде `images` сюда тоже попадают; известность кода проверяется отдельно
/// ([`is_known_language`]).
pub(crate) fn is_language_dir(name: &str) -> bool {
    let mut parts = name.split(['-', '_']);
    let Some(first) = parts.next() else {
        return false;
    };
    (2..=3).contains(&first.len())
        && first.chars().all(|c| c.is_ascii_alphabetic())
        && parts.all(|part| {
            (2..=8).contains(&part.len()) && part.chars().all(|c| c.is_ascii_alphanumeric())
        })
}

/// Код языка из списка справочника (точное написание).
pub(crate) fn is_known_language(code: &str) -> bool {
    LANGUAGES.contains(&code)
}

/// Код справочника с тем же написанием без учёта регистра и разделителя (`ru_ru` →
/// `ru-RU`): подсказка в ошибке.
pub(crate) fn suggest_language(code: &str) -> Option<&'static str> {
    let key = |text: &str| text.to_ascii_lowercase().replace('_', "-");
    let wanted = key(code);
    LANGUAGES.iter().copied().find(|known| key(known) == wanted)
}

/// Ключ естественной сортировки имён файлов: `2.png` раньше `10.png`.
pub(crate) fn natural_key(name: &str) -> (u64, String) {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    let number = digits.parse::<u64>().unwrap_or(u64::MAX);
    (number, name.to_owned())
}

/// Текст для сравнения: `\r\n` как `\n`, без пробелов и переводов строк по краям.
pub(crate) fn normalized(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_owned()
}

/// Локализация из каталога: только поля, файлы которых есть.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalLanguage {
    pub language: String,
    /// Поле `LanguageInfo` → текст.
    pub fields: BTreeMap<&'static str, String>,
}

/// Тексты каталога.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalListing {
    /// Поле `AppInfo` → значение.
    pub app: BTreeMap<&'static str, String>,
    pub languages: Vec<LocalLanguage>,
    /// Что пропущено (не каталог языка, лишние файлы).
    pub skipped: Vec<String>,
}

/// Записи каталога: (имя, путь, это каталог), по естественному порядку имён. Скрытые
/// файлы, символические ссылки на каталоги и особые файлы пропускаются; файлы-ссылки
/// проверяются при чтении.
pub(crate) fn dir_entries(dir: &Path) -> Result<Vec<(String, PathBuf, bool)>, ToolError> {
    let read = std::fs::read_dir(dir)
        .map_err(|e| ToolError::new(format!("cannot read `{}`: {e}", dir.display())))?;
    let mut entries = Vec::new();
    for entry in read {
        let entry =
            entry.map_err(|e| ToolError::new(format!("cannot read `{}`: {e}", dir.display())))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            entries.push((name, path, true));
        } else if kind.is_file() || kind.is_symlink() && path.is_file() {
            entries.push((name, path, false));
        }
    }
    entries.sort_by_key(|entry| natural_key(&entry.0));
    Ok(entries)
}

/// Путь строкой для [`super::LocalFiles`].
pub(crate) fn path_text(path: &Path) -> Result<&str, ToolError> {
    path.to_str()
        .ok_or_else(|| ToolError::new(format!("unsupported file name `{}`", path.display())))
}

/// Читает текстовый файл каталога (проверка пути — [`super::LocalFiles::resolve_read`]).
pub(crate) fn read_text(ctx: &Context, path: &Path) -> Result<String, ToolError> {
    let canonical = ctx.files.resolve_read(path_text(path)?)?;
    let bytes = std::fs::read(&canonical)
        .map_err(|e| ToolError::new(format!("cannot read `{}`: {e}", path.display())))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::new(format!("`{}` is not UTF-8 text", path.display())))?;
    // BOM редакторов Windows в начале файла.
    Ok(text.trim_start_matches('\u{feff}').to_owned())
}

/// Читает тексты каталога и проверяет длины до любых запросов. Все ошибки собираются в
/// одну. Коды языков проверяет вызывающий: неизвестный код может быть у языка, который
/// уже есть в приложении.
pub(crate) fn read_listing(ctx: &Context, dir: &str) -> Result<LocalListing, ToolError> {
    let root = ctx.files.resolve_read_dir(dir)?;
    let mut listing = LocalListing::default();
    let mut errors = Vec::new();
    for (name, path, is_dir) in dir_entries(&root)? {
        if !is_dir {
            if let Some((file, field, limit)) = APP_FIELDS.iter().find(|(file, ..)| *file == name) {
                let value = normalized(&read_text(ctx, &path)?);
                check_length(&mut errors, file, &value, *limit);
                if !value.is_empty() {
                    listing.app.insert(field, value);
                }
            } else {
                listing.skipped.push(name);
            }
            continue;
        }
        if !is_language_dir(&name) {
            listing.skipped.push(format!("{name}/"));
            continue;
        }
        let mut language = LocalLanguage {
            language: name.clone(),
            ..LocalLanguage::default()
        };
        for (file, field, limit) in LANGUAGE_FIELDS {
            let file_path = path.join(file);
            if file_path.symlink_metadata().is_err() {
                continue;
            }
            let text = normalized(&read_text(ctx, &file_path)?);
            check_length(&mut errors, &format!("{name}/{file}"), &text, *limit);
            if *field == "appName" && text.is_empty() {
                errors.push(format!(
                    "{name}/{file}: the app name is empty; AppGallery requires one (delete the \
                     file to keep the current name)"
                ));
            }
            language.fields.insert(field, text);
        }
        listing.languages.push(language);
    }
    if !errors.is_empty() {
        return Err(ToolError::new(format!(
            "the listing directory does not pass the AppGallery Connect checks; nothing was \
             sent:\n- {}",
            errors.join("\n- ")
        )));
    }
    if listing.languages.is_empty() && listing.app.is_empty() {
        return Err(ToolError::new(format!(
            "`{dir}` has no language directories (en-US/, ru-RU/, ...) and no app information \
             files; export one with listing_export first"
        )));
    }
    Ok(listing)
}

fn check_length(errors: &mut Vec<String>, label: &str, text: &str, limit: usize) {
    let count = text.chars().count();
    if count > limit {
        errors.push(format!(
            "{label}: {count} characters, AppGallery Connect allows at most {limit}"
        ));
    }
}

/// Текущее состояние страницы приложения в AppGallery Connect.
#[derive(Debug, Clone, Default)]
pub(crate) struct Remote {
    /// `appInfo` ответа (объект; пустой, если его нет).
    pub app_info: Value,
    /// `languages[]` по коду языка.
    pub languages: BTreeMap<String, Value>,
}

impl Remote {
    /// Текст поля `appInfo`.
    pub(crate) fn app_text(&self, field: &str) -> String {
        text_of(&self.app_info, field)
    }
}

/// Текст поля объекта ответа (для сравнения).
pub(crate) fn text_of(value: &Value, field: &str) -> String {
    match value.get(field) {
        Some(Value::String(text)) => normalized(text),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// Аргументы `get_app_info` и операций записи: `appId` и необязательный `releaseType`.
pub(crate) fn app_args(app_id: &str, release_type: Option<u64>) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("appId".into(), json!(app_id));
    if let Some(release_type) = release_type {
        args.insert("releaseType".into(), json!(release_type));
    }
    args
}

/// Сведения о приложении со всеми языками (`publishing.appInfo.get`).
pub(crate) async fn fetch_remote(
    ctx: &Context,
    app_id: &str,
    release_type: Option<u64>,
) -> Result<Remote, ToolError> {
    let op = operation(APP_INFO_GET)?;
    let response = ctx
        .api
        .call(op, &Value::Object(app_args(app_id, release_type)))
        .await
        .map_err(|err| {
            ToolError::new(format!(
                "reading the app information failed: {}",
                describe_api_error(&err)
            ))
        })?;
    let body = response.body.json().cloned().unwrap_or(Value::Null);
    let app_info = match body.get("appInfo") {
        Some(Value::Object(info)) => Value::Object(info.clone()),
        _ => json!({}),
    };
    let mut languages = BTreeMap::new();
    for language in body
        .get("languages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(code) = language.get("lang").and_then(Value::as_str)
            && !code.trim().is_empty()
        {
            languages.insert(code.trim().to_owned(), language.clone());
        }
    }
    Ok(Remote {
        app_info,
        languages,
    })
}

/// `appId` из аргумента `appId` или единственное приложение по умолчанию из профиля
/// (общий [`reviews_summary::app_id`]).
pub(crate) fn app_id(ctx: &Context, args: &Value) -> Result<String, ToolError> {
    reviews_summary::app_id(ctx, args.get("appId"))
}

/// Схема аргумента `appId`.
pub(crate) fn app_id_schema() -> Value {
    json!({
        "type": ["string", "integer"],
        "description": "Numeric app ID (like 100000001). Optional when the profile has exactly \
            one default app."
    })
}

/// Схема аргумента `releaseType`.
pub(crate) fn release_type_schema() -> Value {
    json!({
        "type": "integer",
        "enum": [1, 3],
        "description": "Which version data to use: 1 (default) the full release, 3 the phased \
            release."
    })
}

/// Строковый аргумент; пустая строка — как отсутствующий.
pub(crate) fn str_arg<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.trim())),
        Some(_) => Err(ToolError::new(format!("`{name}` must be a string"))),
    }
}

/// Обязательный строковый аргумент.
pub(crate) fn required_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    str_arg(args, name)?.ok_or_else(|| ToolError::new(format!("`{name}` is required")))
}

/// Логический аргумент.
pub(crate) fn bool_arg(args: &Value, name: &str) -> Result<Option<bool>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::new(format!("`{name}` must be a boolean"))),
    }
}

/// `releaseType`: 1 или 3.
pub(crate) fn release_type(args: &Value) -> Result<Option<u64>, ToolError> {
    match args.get("releaseType") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(kind @ (1 | 3)) => Ok(Some(kind)),
            _ => Err(ToolError::new(
                "`releaseType` must be 1 (full release) or 3 (phased release)",
            )),
        },
    }
}

/// Список строк аргумента; `None` — аргумента нет.
pub(crate) fn string_list(args: &Value, name: &str) -> Result<Option<Vec<String>>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|text| text.trim().to_owned())
                    .ok_or_else(|| ToolError::new(format!("`{name}` must be a list of strings")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(ToolError::new(format!(
            "`{name}` must be a list of strings"
        ))),
    }
}

/// Разница одного текстового поля: старое и новое значение, у полного описания — число
/// символов и изменённые строки.
pub(crate) fn text_diff(field: &str, old: &str, new: &str) -> Value {
    if field != "appDesc" {
        return json!({"field": field, "old": old, "new": new});
    }
    json!({
        "field": field,
        "old_characters": old.chars().count(),
        "new_characters": new.chars().count(),
        "lines": line_diff(old, new),
    })
}

/// Изменённые строки: `- старая`, `+ новая` (по наибольшей общей подпоследовательности).
pub(crate) fn line_diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // Описания короткие (до 8000 символов), квадратичная таблица допустима.
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            out.push(format!("+ {}", b[j]));
            j += 1;
        } else {
            out.push(format!("- {}", a[i]));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod testing {
    //! Окружение тестов страницы приложения: клиент API против `wiremock` (загрузка — на
    //! loopback, `TransferHosts::HuaweiAndLoopback`), локальные файлы — только во временном
    //! каталоге теста, приложение с двумя языками и картинками.

    use std::sync::Arc;
    use std::time::Duration;

    use appgallery_connect_api::token::StaticToken;
    use appgallery_connect_api::transfer::{TransferConfig, TransferHosts};
    use appgallery_connect_api::{
        AccessLevel, ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy,
    };
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::composite::{Context, MemoryVault, ProfileInfo};
    use crate::files::FilePolicy;

    pub(crate) const APP: &str = "100000001";
    pub(crate) const APP_INFO: &str = "/api/publish/v2/app-info";
    pub(crate) const LANGUAGE_INFO: &str = "/api/publish/v2/app-language-info";
    pub(crate) const FILE_INFO: &str = "/api/publish/v2/app-file-info";
    pub(crate) const UPLOAD_URL: &str = "/api/publish/v2/upload-url/for-obs";

    pub(crate) fn context(
        server: &MockServer,
        dir: &std::path::Path,
        access: AccessLevel,
    ) -> Context {
        let id = ["3141", "5926", "5358"].concat();
        let secret = ["LS12", "TX34", "PQ56"].concat().repeat(4);
        let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
        let mut config = ClientConfig::new(Region::Ru, Some(credentials));
        config.base_url = Some(server.uri());
        config.retry = RetryPolicy {
            max_retries: 0,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
        };
        config.transfer = TransferConfig {
            hosts: TransferHosts::HuaweiAndLoopback,
            timeout: Duration::from_secs(10),
            max_download_bytes: 1024 * 1024,
        };
        let token = ["listing", "-test-", "token"].concat();
        let api = ApiClient::with_token_source(config, Arc::new(StaticToken::new(token))).unwrap();
        // Канонический корень: во временном каталоге macOS `/var` — ссылка на `/private/var`.
        let root = dir.canonicalize().unwrap();
        let files = FilePolicy::new(std::slice::from_ref(&root))
            .unwrap()
            .with_base_dir(root);
        Context {
            api: Arc::new(api),
            files: Arc::new(files),
            vault: Arc::new(MemoryVault::default()),
            profile: ProfileInfo {
                name: "work".into(),
                apps: vec![APP.to_owned()],
                packages: Vec::new(),
            },
            access,
        }
    }

    pub(crate) fn ok(extra: Value) -> ResponseTemplate {
        let mut body = json!({"ret": {"code": 0, "msg": "success"}});
        if let (Value::Object(body), Value::Object(extra)) = (&mut body, extra) {
            body.extend(extra);
        }
        ResponseTemplate::new(200).set_body_json(body)
    }

    /// Картинки приложения в AppGallery для тестов.
    pub(crate) struct AppImages {
        pub icon_en: Vec<u8>,
        pub icon_ru: Vec<u8>,
        pub shots_en: Vec<Vec<u8>>,
    }

    /// Ответ `get_app_info`: два языка (en-US — по умолчанию), основные сведения, иконки
    /// и скриншоты en-US (адреса на `wiremock`).
    pub(crate) fn app_info(server: &MockServer) -> Value {
        let img = |name: &str| format!("{}/img/{name}", server.uri());
        json!({
            "appInfo": {
                "defaultLang": "en-US",
                "privacyPolicy": "https://example.com/privacy",
                "developerWebsite": "https://example.com",
                "publishCountry": "RU,BY",
                "releaseState": 7,
            },
            "languages": [
                {
                    "lang": "en-US",
                    "appName": "Example",
                    "briefInfo": "Short",
                    "appDesc": "Line one\nLine two",
                    "newFeatures": "",
                    "icon": img("icon-en.png"),
                    "showType": 0,
                    "introPic": format!("{},{},{}", img("s1.png"), img("s2.jpg"), img("s3.png")),
                },
                {
                    "lang": "ru-RU",
                    "appName": "Пример",
                    "briefInfo": "Кратко",
                    "appDesc": "Строка",
                    "newFeatures": "Исправления",
                    "icon": img("icon-ru.png"),
                },
            ],
        })
    }

    /// Монтирует приложение: `get_app_info` и картинки. Возвращает содержимое картинок.
    pub(crate) async fn mount_app(server: &MockServer) -> AppImages {
        use super::super::image_sync::fixtures::{jpeg, png};
        Mock::given(method("GET"))
            .and(path(APP_INFO))
            .respond_with(ok(app_info(server)))
            .named("get_app_info")
            .mount(server)
            .await;
        let images = AppImages {
            icon_en: png(216, 216, 1),
            icon_ru: png(216, 216, 2),
            shots_en: vec![png(450, 800, 3), jpeg(450, 800, 4), png(1080, 1920, 5)],
        };
        let files: Vec<(&str, &Vec<u8>)> = vec![
            ("icon-en.png", &images.icon_en),
            ("icon-ru.png", &images.icon_ru),
            ("s1.png", &images.shots_en[0]),
            ("s2.jpg", &images.shots_en[1]),
            ("s3.png", &images.shots_en[2]),
        ];
        for (name, bytes) in files {
            Mock::given(method("GET"))
                .and(path(format!("/img/{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
                .mount(server)
                .await;
        }
        images
    }

    /// Запросы, кроме чтения сведений о приложении и картинок: `METHOD path`.
    pub(crate) async fn writes(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| {
                !(r.method.as_str() == "GET"
                    && (r.url.path() == APP_INFO || r.url.path().starts_with("/img/")))
            })
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_and_sorting() {
        for name in ["en-US", "fil", "es-419", "he_IL", "zh-CN", "ru-RU"] {
            assert!(is_language_dir(name), "{name}");
            assert!(is_known_language(name), "{name}");
        }
        for name in ["e", "en-", "1234", "en US"] {
            assert!(!is_language_dir(name), "{name}");
        }
        assert!(!is_language_dir("images") && !is_language_dir("videos"));
        assert!(is_language_dir("xx-YY") && !is_known_language("xx-YY"));
        assert!(!is_known_language("ru_RU"));
        assert_eq!(suggest_language("ru_ru"), Some("ru-RU"));
        assert_eq!(suggest_language("he-il"), Some("he_IL"));
        assert_eq!(suggest_language("xx-YY"), None);
        let mut names = vec!["10.png", "2.png", "1.png", "a.png"];
        names.sort_by_key(|n| natural_key(n));
        assert_eq!(names, ["1.png", "2.png", "10.png", "a.png"]);
        assert_eq!(normalized(" a\r\nb \n"), "a\nb");
    }

    #[test]
    fn diffs() {
        assert_eq!(
            line_diff("a\nb\nc", "a\nB\nc\nd"),
            vec!["+ B", "- b", "+ d"]
        );
        assert!(line_diff("same", "same").is_empty());
        assert_eq!(
            text_diff("appName", "A", "B"),
            json!({"field": "appName", "old": "A", "new": "B"})
        );
        assert_eq!(text_diff("appDesc", "x", "yz")["new_characters"], 2);
    }
}
