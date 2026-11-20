//! Файл-дополнение `spec/overlay.toml`: всё, чего нет в справочнике Huawei.
//!
//! Операции в дополнении называются по `operationId` снимка (`publishing.appInfo.get`),
//! внешние операции — по ключу в `x-agc-external-operations` (`upload.file.put`). Ссылка
//! на операцию или группу, которых нет в снимке, — ошибка сборки (её проверяет
//! генератор), неизвестный ключ — ошибка разбора: дополнение не должно устаревать молча.
//! Пути полей `[[secrets]]` генератор сверяет со схемой ответа операции в снимке.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overlay {
    /// Группы API в порядке вывода.
    pub groups: Vec<Group>,
    /// Наборы инструментов в порядке вывода.
    pub toolsets: Vec<Toolset>,
    /// `operationId` → имя инструмента.
    pub names: BTreeMap<String, String>,
    /// Исключения из уровня доступа по HTTP-методу.
    #[serde(default)]
    pub access: Vec<AccessRule>,
    /// Устаревшие операции и их замены.
    #[serde(default)]
    pub deprecated: Vec<Deprecated>,
    /// Операции, которые не становятся инструментами.
    #[serde(default)]
    pub exclude: Vec<Exclude>,
    /// Поля ответов с секретами и временными ссылками.
    #[serde(default)]
    pub secrets: Vec<SecretRule>,
    /// Операции, которым нужен API-клиент уровня проекта.
    #[serde(default)]
    pub project_credentials: Vec<ProjectCredentialsRule>,
    /// Поля ответов, которые сервер убирает из ответа агенту.
    #[serde(default)]
    pub omit: Vec<OmitRule>,
}

/// Операции и группы, которые вызываются от имени API-клиента уровня проекта
/// (`[[project_credentials]]`): серверные API сервисов проекта отказывают клиенту команды.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCredentialsRule {
    /// Группы API (`x-agc-group`): все их операции-инструменты.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Отдельные операции (`operationId`).
    #[serde(default)]
    pub operations: Vec<String>,
    /// Параметр с идентификатором проекта. Без ключа — тот из `productId` и `projectId`,
    /// который есть у операции.
    #[serde(default)]
    pub param: Option<String>,
}

/// Вид секретного поля ответа (`kind` в `[[secrets]]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretKind {
    /// Учётные данные (секрет приложения): агенту не отдаются никак.
    Credential,
    /// Временная ссылка на файл: с `save_to` сервер скачивает файл сам.
    DownloadUrl,
    /// Содержимое файла с учётными данными: с `save_to` сервер сохраняет его в файл.
    FileContent,
}

impl SecretKind {
    pub fn rust_name(self) -> &'static str {
        match self {
            Self::Credential => "SecretKind::Credential",
            Self::DownloadUrl => "SecretKind::DownloadUrl",
            Self::FileContent => "SecretKind::FileContent",
        }
    }

    /// Можно ли сохранить значение в файл (`save_to`).
    pub fn savable(self) -> bool {
        matches!(self, Self::DownloadUrl | Self::FileContent)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRule {
    pub kind: SecretKind,
    /// Что это за значение (английский, для пометки `note` агенту).
    pub description: String,
    /// Где ещё его взять или что с ним делать (английский, для агента); необязательно.
    #[serde(default)]
    pub hint: String,
    pub operations: Vec<String>,
    /// Пути к полям в JSON ответа: имена через `.`, `[]` — каждый элемент массива.
    pub fields: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// Значение `x-agc-group` снимка.
    pub slug: String,
    pub title: String,
    pub reference: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Toolset {
    pub slug: String,
    pub description: String,
    /// Включён ли набор по умолчанию.
    #[serde(default)]
    pub default: bool,
    /// Операции набора (`operationId` или ключ внешней операции).
    pub operations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Read,
    Write,
    Destructive,
}

impl Level {
    pub fn rust_name(self) -> &'static str {
        match self {
            Self::Read => "AccessLevel::Read",
            Self::Write => "AccessLevel::Write",
            Self::Destructive => "AccessLevel::Destructive",
        }
    }

    /// Уровень по HTTP-методу: `GET` — чтение, `DELETE` — удаление, остальное — запись.
    pub fn by_method(method: &str) -> Self {
        match method {
            "GET" => Self::Read,
            "DELETE" => Self::Destructive,
            _ => Self::Write,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRule {
    pub level: Level,
    /// Короткое английское объяснение для агента: попадает в описание инструмента.
    pub reason: String,
    pub operations: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deprecated {
    pub operation: String,
    /// Операция, которую вызвать вместо устаревшей (должна быть инструментом).
    #[serde(default)]
    pub replacement: Option<String>,
    /// Пояснение для агента.
    #[serde(default)]
    pub note: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclude {
    pub operations: Vec<String>,
    /// Почему операции нет среди инструментов.
    pub reason: String,
    /// Что вызвать вместо неё.
    #[serde(default)]
    pub alternative: String,
    /// `false` запрещает вызов и через `api_request`.
    #[serde(default = "yes")]
    pub callable: bool,
}

/// Разбирает дополнение; синтаксическая ошибка или неизвестный ключ — `Err` с текстом.
pub fn parse(raw: &str) -> Result<Overlay, String> {
    toml::from_str(raw).map_err(|err| format!("spec/overlay.toml: {err}"))
}

/// Поля ответа, которые сервер убирает из ответа агенту (`[[omit]]`): подписи данных,
/// которые нельзя проверить после разбора JSON-строк, и другие бесполезные агенту поля.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OmitRule {
    /// Почему полей нет (английский текст для агента).
    pub reason: String,
    pub operations: Vec<String>,
    /// Пути полей, как в `[[secrets]]`.
    pub fields: Vec<String>,
}
