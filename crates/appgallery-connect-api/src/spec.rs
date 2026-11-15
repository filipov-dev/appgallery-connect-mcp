//! Описание групп и операций AppGallery Connect API.
//!
//! Таблицы [`groups`], [`operations`], [`excluded_operations`] и [`toolsets`] строит
//! генератор из снимка спецификации `spec/openapi.json` и файла-дополнения
//! `spec/overlay.toml`. Типы этого модуля — контракт между генератором, исполнителем
//! запросов и MCP-сервером.
//!
//! Как MCP-сервер строит запрос из аргументов инструмента:
//!
//! 1. Проверить аргументы по [`OperationDef::input_schema`] (неизвестные, недостающие
//!    и аргументы неверного типа отклоняются).
//! 2. Подставить [`ParamLocation::Path`] в шаблон [`OperationDef::path`]: `{name}`
//!    кодируется как один сегмент пути.
//! 3. [`ParamLocation::Query`] передать в строке запроса под именем
//!    [`ParamDef::api_name`]; массивы ([`ParamDef::repeated`]) — повтором `?a=1&a=2`.
//! 4. [`ParamLocation::Header`] передать заголовком с именем [`ParamDef::api_name`]
//!    (например `appId` у операций PMS API).
//! 5. Тело — аргумент `body`, отправляется JSON-телом ([`BodyKind::Json`]).
//! 6. Отправить запрос на хост региона профиля ([`crate::region::Region::host`]).
//! 7. Разобрать ответ: HTTP 200 с ненулевым кодом в конверте результата
//!    ([`OperationDef::result`]) — ошибка, а не успех.
//!
//! Заголовки `Authorization` и `client_id` в параметры не входят: их добавляет
//! исполнитель.

/// HTTP-метод операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// Уровень доступа операции (из файла-дополнения или по HTTP-методу).
/// Уровни упорядочены: `Read < Write < Destructive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccessLevel {
    Read,
    Write,
    Destructive,
}

impl AccessLevel {
    /// Разрешает ли текущий уровень операцию с уровнем `required`.
    pub fn allows(self, required: AccessLevel) -> bool {
        required <= self
    }

    /// Имя уровня: `read`, `write` или `destructive`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Destructive => "destructive",
        }
    }
}

/// Группа AppGallery Connect API (раздел справочника Huawei).
#[derive(Debug, Clone, Copy)]
pub struct GroupDef {
    /// Короткое имя группы (`x-agc-group` снимка): `auth` (получение токена),
    /// `publishing`, `upload`, `reports`, `comments`, `pms`, `project`, `provisioning`.
    pub slug: &'static str,
    /// Название раздела справочника, например `Publishing API`.
    pub title: &'static str,
    /// Адрес раздела в справочнике Huawei; пустая строка — адреса нет.
    pub reference: &'static str,
}

/// Где передаётся параметр операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamLocation {
    /// Подставляется в шаблон пути (`{name}`).
    Path,
    /// Передаётся в строке запроса.
    Query,
    /// Передаётся заголовком запроса (например `appId` у PMS API).
    Header,
    /// JSON-тело запроса; у операции не больше одного такого параметра, его имя — `body`.
    Body,
}

/// Тип значения параметра в аргументах инструмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    Integer,
    Number,
    Boolean,
    /// Массив значений повторяющегося параметра запроса.
    Array,
    /// JSON-объект (тело запроса).
    Object,
}

/// Параметр операции.
#[derive(Debug, Clone, Copy)]
pub struct ParamDef {
    /// Ключ в аргументах инструмента (`appId`, `lang`, `releaseType`); у тела — `body`.
    pub name: &'static str,
    /// Имя в пути, строке запроса или заголовке, как в справочнике.
    pub api_name: &'static str,
    pub location: ParamLocation,
    /// Обязателен ли параметр. Параметры пути обязательны всегда.
    pub required: bool,
    pub description: &'static str,
    pub value_type: ParamType,
    /// Параметр запроса принимает массив: значения передаются повтором имени.
    pub repeated: bool,
}

/// Вид тела запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// `application/json`: аргумент `body` ([`ParamLocation::Body`]).
    Json,
}

/// Вид успешного ответа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    /// JSON.
    Json,
    /// Пустой ответ.
    Empty,
}

/// Конверт результата в теле ответа.
///
/// AppGallery Connect API сообщает об ошибке не только кодом HTTP: ответ с HTTP 200
/// может нести ненулевой код результата. У разных групп API конверт разный:
/// `{"ret": {"code": 0, "msg": "success"}}` (Publishing),
/// `{"ret": {"rtnCode": 0, "rtnDesc": "success"}}` (Comments),
/// `{"error": {"errorCode": 0, "errorMsg": null}}` (PMS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultDef {
    /// Поле верхнего уровня с объектом результата: `ret` или `error`.
    pub field: &'static str,
    /// Поле кода внутри объекта результата: `code`, `rtnCode`, `errorCode`.
    /// Успех — число `0` (или строка `"0"`).
    pub code_field: &'static str,
    /// Поле текста внутри объекта результата: `msg`, `rtnDesc`, `errorMsg`.
    pub message_field: &'static str,
}

/// Набор инструментов: несколько связанных операций под одним slug.
#[derive(Debug, Clone, Copy)]
pub struct ToolsetDef {
    /// Короткое английское имя для `--toolsets`, например `releases`.
    pub slug: &'static str,
    /// Описание набора.
    pub description: &'static str,
    /// Включён ли набор по умолчанию.
    pub default_enabled: bool,
}

/// Операция AppGallery Connect API, доступная как MCP-инструмент.
#[derive(Debug, Clone, Copy)]
pub struct OperationDef {
    /// Имя MCP-инструмента («глагол_ресурс», не длиннее 40 символов), уникально.
    pub tool_name: &'static str,
    /// `operationId` из снимка спецификации, например `publishing.appInfo.get`.
    pub operation_id: &'static str,
    /// Группа API ([`GroupDef::slug`]).
    pub group: &'static str,
    /// Slug набора инструментов ([`ToolsetDef::slug`]).
    pub toolset: &'static str,
    /// Первое предложение описания операции.
    pub summary: &'static str,
    pub description: &'static str,
    pub method: HttpMethod,
    /// Шаблон пути от корня хоста, например `/api/publish/v2/app-info`.
    pub path: &'static str,
    pub access: AccessLevel,
    pub params: &'static [ParamDef],
    /// Вид тела запроса; `None` — тела нет.
    pub body: Option<BodyKind>,
    pub response: ResponseKind,
    /// Конверт результата в теле ответа; `None` — у операции его нет.
    pub result: Option<ResultDef>,
    /// Страница операции в справочнике Huawei; пустая строка — адреса нет.
    pub reference: &'static str,
    /// JSON Schema аргументов инструмента в виде JSON-текста.
    ///
    /// Объект с `type: object` и `additionalProperties: false`: параметры пути, запроса
    /// и заголовков и `body` (JSON-тело с раскрытыми `$ref`). Заголовков авторизации
    /// в схеме нет.
    pub input_schema: &'static str,
    /// Поля ответа с секретами и временными ссылками (`[[secrets]]` дополнения), по пути.
    /// MCP-сервер скрывает их значения в ответе агенту, а значения видов
    /// [`SecretKind::DownloadUrl`] и [`SecretKind::FileContent`] умеет сохранить в файл.
    pub secrets: &'static [SecretFieldDef],
}

/// Вид секретного поля ответа.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    /// Учётные данные (секрет приложения): агенту не отдаются ни в каком виде.
    Credential,
    /// Временная ссылка на файл (отчёт, сертификат, профиль): сама ссылка даёт доступ к
    /// файлу. По запросу (`save_to`) сервер скачивает файл и отдаёт агенту путь и размер.
    DownloadUrl,
    /// Содержимое файла с учётными данными (файл конфигурации приложения). По запросу
    /// (`save_to`) сервер сохраняет его в файл.
    FileContent,
}

impl SecretKind {
    /// Можно ли сохранить значение в файл.
    pub fn savable(self) -> bool {
        matches!(self, Self::DownloadUrl | Self::FileContent)
    }
}

/// Секретное поле ответа операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretFieldDef {
    /// Путь к полю в JSON ответа: имена через `.`, `[]` — каждый элемент массива
    /// (`certInfo[].certDownloadUrl`). Генератор проверяет его по схеме ответа снимка.
    pub path: &'static str,
    pub kind: SecretKind,
    /// Что это за значение (английский текст для агента).
    pub description: &'static str,
    /// Где ещё его взять или что с ним делать (английский); пустая строка — подсказки нет.
    pub hint: &'static str,
}

impl SecretFieldDef {
    /// Имя поля (последний сегмент пути без `[]`).
    pub fn name(&self) -> &'static str {
        let last = self.path.rsplit('.').next().unwrap_or(self.path);
        last.strip_suffix("[]").unwrap_or(last)
    }
}

impl OperationDef {
    /// Можно ли сохранить файл из ответа (`save_to`): в ответе есть временная ссылка на
    /// файл или содержимое файла.
    pub fn saves_file(&self) -> bool {
        self.secrets.iter().any(|secret| secret.kind.savable())
    }

    /// Параметры в расположении `location`.
    pub fn params_in(&self, location: ParamLocation) -> impl Iterator<Item = &'static ParamDef> {
        self.params
            .iter()
            .filter(move |param| param.location == location)
    }

    /// Параметр по имени.
    pub fn param(&self, name: &str) -> Option<&'static ParamDef> {
        self.params.iter().find(|param| param.name == name)
    }
}

/// Почему операция не зарегистрирована как MCP-инструмент.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExclusionReason {
    /// Операция устарела (пометка в снимке или в дополнении).
    Deprecated,
    /// Операция исключена дополнением: ею пользуется составной инструмент, она выдаёт
    /// учётные данные или не нужна агенту.
    Overlay,
}

impl ExclusionReason {
    /// Код причины: `deprecated`, `overlay`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deprecated => "deprecated",
            Self::Overlay => "overlay",
        }
    }

    /// Объяснение для агента (английский).
    pub fn explanation(self) -> &'static str {
        match self {
            Self::Deprecated => "The operation is deprecated in the AppGallery Connect API.",
            Self::Overlay => "The operation is not exposed as a separate tool.",
        }
    }
}

/// Операция, которая не зарегистрирована как MCP-инструмент. Её находит поиск по
/// операциям; вызвать её через `api_request` можно, если [`Self::callable`].
///
/// Внешних операций (`x-agc-external-operations` снимка: отправка файла на адрес из
/// ответа API) в таблице нет: пути на хосте API у них нет, их выполняет составной
/// инструмент загрузки.
#[derive(Debug, Clone, Copy)]
pub struct ExcludedOperationDef {
    pub operation_id: &'static str,
    pub group: &'static str,
    /// Slug набора инструментов, к которому относится операция. Пустая строка — набора
    /// нет; так бывает только при `callable = false` (получение токена).
    pub toolset: &'static str,
    pub summary: &'static str,
    pub method: HttpMethod,
    /// Шаблон пути от корня хоста.
    pub path: &'static str,
    /// Уровень доступа, нужный для вызова через `api_request`.
    pub access: AccessLevel,
    pub reason: ExclusionReason,
    /// Можно ли вызвать операцию через `api_request`. `false` у операций, которые
    /// выдают учётные данные (получение токена).
    pub callable: bool,
    /// Почему операция исключена (английский текст для агента): `reason` из `[[exclude]]`
    /// или `note` из `[[deprecated]]` дополнения; пустая строка — пояснения нет.
    pub note: &'static str,
    /// Что вызвать вместо операции (английский текст для агента); пустая строка — замены нет.
    pub alternative: &'static str,
}

// Таблицы `GROUPS`, `TOOLSETS`, `OPERATIONS` и `EXCLUDED` строит `build.rs` из снимка
// спецификации и файла-дополнения.
include!(concat!(env!("OUT_DIR"), "/operations.rs"));

/// Группы API в порядке дополнения.
pub fn groups() -> &'static [GroupDef] {
    GROUPS
}

/// Группа по slug.
pub fn find_group(slug: &str) -> Option<&'static GroupDef> {
    GROUPS.iter().find(|group| group.slug == slug)
}

/// Все операции API, отсортированные по имени инструмента.
pub fn operations() -> &'static [OperationDef] {
    OPERATIONS
}

/// Операция по имени MCP-инструмента.
pub fn find(tool_name: &str) -> Option<&'static OperationDef> {
    OPERATIONS
        .binary_search_by(|op| op.tool_name.cmp(tool_name))
        .ok()
        .and_then(|index| OPERATIONS.get(index))
}

/// Операции, не зарегистрированные как инструменты, отсортированные по `operationId`.
pub fn excluded_operations() -> &'static [ExcludedOperationDef] {
    EXCLUDED
}

/// Исключённая операция по `operationId`.
pub fn find_excluded(operation_id: &str) -> Option<&'static ExcludedOperationDef> {
    EXCLUDED
        .binary_search_by(|op| op.operation_id.cmp(operation_id))
        .ok()
        .and_then(|index| EXCLUDED.get(index))
}

/// Имена секретных полей ответов всех операций, которые скрываются по имени в любом
/// ответе (учётные данные и временные ссылки, [`SecretKind::Credential`] и
/// [`SecretKind::DownloadUrl`]), по алфавиту и без повторов. Содержимое файлов сюда не
/// входит: поле `content` у других операций — обычные данные (текст отзыва).
pub fn secret_field_names() -> &'static [&'static str] {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: Vec<&'static str> = OPERATIONS
            .iter()
            .flat_map(|op| op.secrets.iter())
            .filter(|secret| secret.kind != SecretKind::FileContent)
            .map(SecretFieldDef::name)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    })
}

/// Наборы инструментов в порядке таблицы дополнения.
pub fn toolsets() -> &'static [ToolsetDef] {
    TOOLSETS
}

/// Набор инструментов по slug.
pub fn find_toolset(slug: &str) -> Option<&'static ToolsetDef> {
    TOOLSETS.iter().find(|toolset| toolset.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_are_ordered() {
        assert!(AccessLevel::Destructive.allows(AccessLevel::Write));
        assert!(AccessLevel::Write.allows(AccessLevel::Read));
        assert!(!AccessLevel::Read.allows(AccessLevel::Write));
        assert!(!AccessLevel::Write.allows(AccessLevel::Destructive));
    }
}
