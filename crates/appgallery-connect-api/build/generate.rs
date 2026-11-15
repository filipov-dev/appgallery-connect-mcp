//! Построение таблиц операций из снимка OpenAPI и файла-дополнения и вывод Rust-кода.
//!
//! Этапы: группы → внешние операции → разбор операций снимка (параметры, тело, ответ,
//! конверт результата) → наборы → уровни доступа → исключения → секретные поля ответов →
//! имена инструментов → описания и схемы аргументов → вывод `GROUPS`, `TOOLSETS`,
//! `OPERATIONS` и `EXCLUDED`.
//!
//! Секретные поля (`[[secrets]]` дополнения) сверяются со схемой ответа `200` операции:
//! путь должен вести к существующему полю-строке, иначе это ошибка сборки. Так вырезание
//! секретов в ядре опирается на имена полей снимка, а переименование поля в снимке не
//! оставит секрет незамеченным.
//!
//! Все ошибки — и формы снимка, и согласования дополнения со снимком — собираются в
//! список и выводятся разом: так одна правка снимка показывает все проблемы за один
//! прогон, а тесты проверяют ошибки на испорченном дополнении без паники.
//!
//! Внешние операции (`x-agc-external-operations`: отправка файла на адрес из ответа API)
//! в таблицы не попадают: пути на хосте API у них нет, их выполняет составной инструмент
//! загрузки. Генератор только проверяет их описание и то, что они исключены дополнением
//! с `callable = false` и не получили имени инструмента.
//!
//! Обходы объектов идут по отсортированным ключам, JSON Schema сериализуется с ключами
//! по алфавиту: результат не зависит от фичи `preserve_order` у `serde_json`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use crate::overlay::{Level, Overlay, SecretKind};
use crate::schema::{Resolver, description_of, mark_deprecated, normalize_space};

/// Предел длины имени инструмента: `mcp__<server>__<tool>` не длиннее 64 символов.
pub const MAX_TOOL_NAME: usize = 40;

const HTTP_METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// Ключи операции снимка, которые генератор понимает.
const OPERATION_KEYS: &[&str] = &[
    "operationId",
    "tags",
    "summary",
    "description",
    "parameters",
    "requestBody",
    "responses",
    "deprecated",
    "x-agc-group",
    "x-agc-result",
    "x-agc-auth-modes",
    "x-agc-reference",
];

/// Ключи параметра снимка.
const PARAM_KEYS: &[&str] = &[
    "name",
    "in",
    "required",
    "description",
    "deprecated",
    "schema",
    "style",
    "explode",
];

/// Ключи внешней операции (`x-agc-external-operations`).
const EXTERNAL_KEYS: &[&str] = &[
    "method",
    "summary",
    "description",
    "x-agc-group",
    "x-agc-reference",
    "x-agc-url-source",
];

/// Режимы авторизации `x-agc-auth-modes`. В таблицы не попадают: справочник говорит
/// о них в описаниях операций.
const AUTH_MODES: &[&str] = &["api-client", "oauth-client", "service-account"];

/// Заголовки авторизации добавляет исполнитель: параметром операции они быть не могут.
const AUTH_HEADERS: &[&str] = &["authorization", "client_id", "teamid", "oauth2token"];

/// Страницы справочника Huawei.
const REFERENCE_PREFIX: &str = "https://developer.huawei.com/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Path,
    Query,
    Header,
    Body,
}

impl Location {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Path => "ParamLocation::Path",
            Self::Query => "ParamLocation::Query",
            Self::Header => "ParamLocation::Header",
            Self::Body => "ParamLocation::Body",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exclusion {
    Deprecated,
    Overlay,
}

impl Exclusion {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Deprecated => "ExclusionReason::Deprecated",
            Self::Overlay => "ExclusionReason::Overlay",
        }
    }
}

#[derive(Debug, Clone)]
struct Param {
    /// Ключ в аргументах инструмента (совпадает с именем в справочнике, у тела — `body`).
    name: String,
    location: Location,
    required: bool,
    description: String,
    /// Имя варианта `ParamType` в `spec.rs`.
    value_type: &'static str,
    repeated: bool,
    /// JSON Schema значения без описания.
    schema: Value,
}

#[derive(Debug, Clone)]
struct ResultEnvelope {
    field: String,
    code_field: String,
    message_field: String,
}

/// Секретное поле ответа операции (`[[secrets]]`).
#[derive(Debug, Clone)]
struct Secret {
    path: String,
    kind: SecretKind,
    description: String,
    hint: String,
}

/// Предел вложенности `$ref` при проверке путей секретных полей.
const MAX_SECRET_REF_DEPTH: usize = 16;

#[derive(Debug)]
struct Operation {
    operation_id: String,
    group: String,
    toolset: String,
    tool_name: String,
    summary: String,
    /// Описание снимка без первого предложения (`summary`).
    details: String,
    description: String,
    /// `GET`, `POST`, ...
    http_method: String,
    path: String,
    access: Level,
    access_reason: Option<String>,
    params: Vec<Param>,
    has_body: bool,
    json_response: bool,
    result: Option<ResultEnvelope>,
    reference: String,
    deprecated: bool,
    exclusion: Option<Exclusion>,
    callable: bool,
    note: String,
    alternative: String,
    input_schema: String,
    /// Схема ответа `200` (`application/json`) как в снимке, с `$ref`.
    response_schema: Option<Value>,
    /// Секретные поля ответа, по пути.
    secrets: Vec<Secret>,
}

pub struct Output {
    /// Код `operations.rs`; пустой, если есть ошибки.
    pub code: String,
    /// Ошибки снимка и согласования с дополнением.
    pub errors: Vec<String>,
}

/// Генерирует исходный код `operations.rs` из снимка `spec` и дополнения `overlay`.
pub fn generate(spec: &Value, overlay: &Overlay) -> Output {
    let mut errors = Vec::new();
    let empty = Map::new();
    let schemas = match spec.pointer("/components/schemas") {
        Some(Value::Object(schemas)) => schemas,
        Some(_) => {
            errors.push("components/schemas не является объектом".to_owned());
            &empty
        }
        None => &empty,
    };
    if !spec
        .get("openapi")
        .and_then(Value::as_str)
        .is_some_and(|v| v.starts_with("3.0."))
    {
        errors.push("снимок должен быть в формате OpenAPI 3.0".to_owned());
    }

    let mut operations = collect_operations(spec, schemas, &mut errors);
    let external = collect_external(spec, &operations, &mut errors);
    check_groups(spec, overlay, &operations, &external, &mut errors);

    let path_ids: BTreeSet<String> = operations
        .iter()
        .map(|op| op.operation_id.clone())
        .collect();
    let external_ids: BTreeSet<String> = external.keys().cloned().collect();
    let known: BTreeSet<String> = path_ids.union(&external_ids).cloned().collect();

    let toolset_of = check_toolsets(overlay, &known, &mut errors);
    apply_access(overlay, &path_ids, &mut operations, &mut errors);
    apply_exclusions(overlay, &known, &external_ids, &mut operations, &mut errors);
    apply_secrets(overlay, schemas, &mut operations, &mut errors);
    for op in &mut operations {
        match toolset_of.get(&op.operation_id) {
            Some(toolset) => op.toolset = toolset.clone(),
            // Вне наборов — только операции, которые нельзя вызвать даже через
            // `api_request` (получение токена).
            None if op.exclusion.is_some() && !op.callable => {}
            None => errors.push(format!(
                "{}: операция не входит ни в один набор: добавь её в operations [[toolsets]] spec/overlay.toml",
                op.operation_id
            )),
        }
    }
    assign_names(
        overlay,
        &path_ids,
        &external_ids,
        &mut operations,
        &mut errors,
    );
    resolve_replacements(overlay, &mut operations, &mut errors);
    for op in operations.iter_mut().filter(|op| op.exclusion.is_none()) {
        op.description = describe(op);
        op.input_schema = input_schema(&op.params);
    }

    let (mut kept, mut excluded): (Vec<Operation>, Vec<Operation>) = operations
        .into_iter()
        .partition(|op| op.exclusion.is_none());
    kept.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
    excluded.sort_by(|a, b| a.operation_id.cmp(&b.operation_id));
    let code = if errors.is_empty() {
        emit(overlay, &kept, &excluded)
    } else {
        String::new()
    };
    Output { code, errors }
}

fn sorted_entries(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

fn string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(normalize_space)
        .filter(|s| !s.is_empty())
}

/// Операции `paths` снимка по отсортированным путям и методам.
fn collect_operations(
    spec: &Value,
    schemas: &Map<String, Value>,
    errors: &mut Vec<String>,
) -> Vec<Operation> {
    let Some(paths) = spec.get("paths").and_then(Value::as_object) else {
        errors.push("в снимке нет paths".to_owned());
        return Vec::new();
    };
    let mut operations = Vec::new();
    let mut ids = BTreeSet::new();
    for (path, item) in sorted_entries(paths) {
        if !path.starts_with("/api/") || path.contains('?') || path.contains("//") {
            errors.push(format!(
                "{path}: путь должен начинаться с /api/ и не содержать query"
            ));
        }
        let Some(item) = item.as_object() else {
            errors.push(format!("{path}: описание пути не является объектом"));
            continue;
        };
        for (method, raw) in sorted_entries(item) {
            if !HTTP_METHODS.contains(&method.as_str()) {
                errors.push(format!("{path}: неподдерживаемый ключ {method}"));
                continue;
            }
            if let Some(op) = parse_operation(schemas, path, method, raw, errors) {
                if !ids.insert(op.operation_id.clone()) {
                    errors.push(format!("operationId {} повторяется", op.operation_id));
                }
                operations.push(op);
            }
        }
    }
    operations
}

fn parse_operation(
    schemas: &Map<String, Value>,
    path: &str,
    method: &str,
    raw: &Value,
    errors: &mut Vec<String>,
) -> Option<Operation> {
    let http_method = method.to_ascii_uppercase();
    let Some(object) = raw.as_object() else {
        errors.push(format!(
            "{http_method} {path}: операция не является объектом"
        ));
        return None;
    };
    let Some(operation_id) = string_field(object, "operationId") else {
        errors.push(format!("{http_method} {path}: нет operationId"));
        return None;
    };
    let id = operation_id.as_str();
    let mut error = |text: String| errors.push(format!("{id}: {text}"));
    for key in object.keys() {
        if !OPERATION_KEYS.contains(&key.as_str()) {
            error(format!("неподдерживаемый ключ операции {key}"));
        }
    }

    let group = string_field(object, "x-agc-group").unwrap_or_default();
    if group.is_empty() {
        error("нет x-agc-group".to_owned());
    } else if id.split('.').next() != Some(group.as_str()) {
        error(format!("operationId не начинается с группы {group}"));
    }
    let summary = string_field(object, "summary").unwrap_or_default();
    if summary.is_empty() {
        error("нет summary".to_owned());
    }
    let full = string_field(object, "description").unwrap_or_else(|| summary.clone());
    let details = match full.strip_prefix(summary.as_str()) {
        Some(rest) => rest.trim().to_owned(),
        None => {
            error("description не начинается с summary".to_owned());
            String::new()
        }
    };
    let reference = string_field(object, "x-agc-reference").unwrap_or_default();
    if !reference.starts_with(REFERENCE_PREFIX) {
        error(format!(
            "x-agc-reference должен вести на {REFERENCE_PREFIX}"
        ));
    }
    let result = parse_result(object.get("x-agc-result"), &mut error);
    check_auth_modes(object.get("x-agc-auth-modes"), &mut error);
    let deprecated = match object.get("deprecated") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            error("deprecated должен быть логическим".to_owned());
            false
        }
    };

    let mut resolver = Resolver::new(schemas, id);
    let mut params = parse_params(object.get("parameters"), &mut resolver, &mut error);
    let placeholders: BTreeSet<String> = path
        .split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name.to_owned()))
        .collect();
    let declared: BTreeSet<String> = params
        .iter()
        .filter(|p| p.location == Location::Path)
        .map(|p| p.name.clone())
        .collect();
    if placeholders != declared {
        error(format!(
            "плейсхолдеры пути {path} не совпадают с параметрами пути {declared:?}"
        ));
    }

    let has_body = match object.get("requestBody") {
        Some(body) => {
            if http_method == "GET" {
                error("у GET не бывает тела".to_owned());
            }
            if let Some(param) = parse_body(body, &mut resolver, &mut error) {
                params.push(param);
            }
            true
        }
        None => false,
    };
    let json_response = match raw.pointer("/responses/200") {
        Some(response) => response.pointer("/content/application~1json").is_some(),
        None => {
            error("нет ответа 200".to_owned());
            false
        }
    };
    let response_schema = raw
        .pointer("/responses/200/content/application~1json/schema")
        .cloned();
    let resolver_errors = std::mem::take(&mut resolver.errors);
    errors.extend(resolver_errors);

    Some(Operation {
        group,
        toolset: String::new(),
        tool_name: String::new(),
        summary,
        details,
        description: String::new(),
        access: Level::by_method(&http_method),
        access_reason: None,
        http_method,
        path: path.to_owned(),
        params,
        has_body,
        json_response,
        result,
        reference,
        deprecated,
        exclusion: None,
        callable: true,
        note: String::new(),
        alternative: String::new(),
        input_schema: String::new(),
        response_schema,
        secrets: Vec::new(),
        operation_id,
    })
}

/// Конверт результата `x-agc-result`: поле объекта результата, поле кода и поле текста.
fn parse_result(raw: Option<&Value>, error: &mut impl FnMut(String)) -> Option<ResultEnvelope> {
    let Some(raw) = raw else {
        error("нет x-agc-result (конверта результата)".to_owned());
        return None;
    };
    let Some(object) = raw.as_object() else {
        error("x-agc-result не является объектом".to_owned());
        return None;
    };
    let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    if keys != BTreeSet::from(["codeField", "field", "messageField"]) {
        error(format!(
            "x-agc-result должен содержать ровно field, codeField и messageField, а не {keys:?}"
        ));
        return None;
    }
    let field = |key: &str| string_field(object, key);
    match (field("field"), field("codeField"), field("messageField")) {
        (Some(field), Some(code_field), Some(message_field)) => Some(ResultEnvelope {
            field,
            code_field,
            message_field,
        }),
        _ => {
            error("поля x-agc-result должны быть непустыми строками".to_owned());
            None
        }
    }
}

fn check_auth_modes(raw: Option<&Value>, error: &mut impl FnMut(String)) {
    let Some(raw) = raw else {
        return;
    };
    let Some(modes) = raw.as_array() else {
        error("x-agc-auth-modes не является массивом".to_owned());
        return;
    };
    for mode in modes {
        if !mode.as_str().is_some_and(|m| AUTH_MODES.contains(&m)) {
            error(format!("неизвестный режим авторизации {mode}"));
        }
    }
}

/// Параметры пути, запроса и заголовков в порядке снимка.
fn parse_params(
    raw: Option<&Value>,
    resolver: &mut Resolver<'_>,
    error: &mut impl FnMut(String),
) -> Vec<Param> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let Some(items) = raw.as_array() else {
        error("parameters не является массивом".to_owned());
        return Vec::new();
    };
    let mut params = Vec::new();
    let mut names = BTreeSet::new();
    for item in items {
        let Some(object) = item.as_object() else {
            error("параметр не является объектом".to_owned());
            continue;
        };
        let Some(name) = string_field(object, "name") else {
            error("у параметра нет name".to_owned());
            continue;
        };
        let mut perror = |text: String| error(format!("параметр {name}: {text}"));
        for key in object.keys() {
            if !PARAM_KEYS.contains(&key.as_str()) {
                perror(format!("неподдерживаемый ключ {key}"));
            }
        }
        if !names.insert(name.clone()) {
            perror("имя повторяется".to_owned());
        }
        if name == "body" {
            perror("имя body занято телом запроса".to_owned());
        }
        if !is_valid_property_name(&name) {
            perror("недопустимое имя аргумента".to_owned());
        }
        let location = match object.get("in").and_then(Value::as_str) {
            Some("path") => Location::Path,
            Some("query") => Location::Query,
            Some("header") => Location::Header,
            other => {
                perror(format!("неподдерживаемое расположение {other:?}"));
                continue;
            }
        };
        if location == Location::Header
            && AUTH_HEADERS.contains(&name.to_ascii_lowercase().as_str())
        {
            perror("заголовки авторизации добавляет исполнитель".to_owned());
        }
        let required = match object.get("required") {
            None => false,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => {
                perror("required должен быть логическим".to_owned());
                false
            }
        };
        if location == Location::Path && !required {
            perror("параметр пути должен быть обязательным".to_owned());
        }
        let Some(raw_schema) = object.get("schema") else {
            perror("нет schema".to_owned());
            continue;
        };
        let mut schema = resolver.resolve(raw_schema);
        let schema_description = schema
            .as_object_mut()
            .and_then(|s| s.remove("description"))
            .and_then(|d| d.as_str().map(str::to_owned));
        let kind = schema
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (value_type, repeated) = match kind {
            "string" => ("String", false),
            "integer" => ("Integer", false),
            "number" => ("Number", false),
            "boolean" => ("Boolean", false),
            "array" => ("Array", true),
            other => {
                perror(format!("неподдерживаемый тип {other:?}"));
                continue;
            }
        };
        let style = object.get("style").and_then(Value::as_str);
        let explode = object.get("explode").and_then(Value::as_bool);
        if repeated {
            // Массив передаётся только повтором имени в строке запроса: `?a=1&a=2`
            // (`style: form`, `explode: true` — значения OpenAPI по умолчанию для query).
            if location != Location::Query {
                perror("массив допустим только в строке запроса".to_owned());
            }
            if style.is_some_and(|s| s != "form") || explode == Some(false) {
                perror("массив передаётся только как style: form, explode: true".to_owned());
            }
            let item_kind = schema
                .pointer("/items/type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(item_kind, "string" | "integer" | "number" | "boolean") {
                perror(format!(
                    "неподдерживаемый тип элементов массива {item_kind:?}"
                ));
            }
        } else if style.is_some() || explode.is_some() {
            perror("style и explode допустимы только у массива".to_owned());
        }
        let deprecated = object.get("deprecated").and_then(Value::as_bool) == Some(true);
        let description = mark_deprecated(
            description_of(object).or(schema_description.map(|d| normalize_space(&d))),
            deprecated,
        )
        .unwrap_or_else(|| name.clone());
        params.push(Param {
            name,
            location,
            required,
            description,
            value_type,
            repeated,
            schema,
        });
    }
    params
}

/// Тело запроса: JSON-объект в аргументе `body`.
fn parse_body(
    raw: &Value,
    resolver: &mut Resolver<'_>,
    error: &mut impl FnMut(String),
) -> Option<Param> {
    let Some(object) = raw.as_object() else {
        error("requestBody не является объектом".to_owned());
        return None;
    };
    for key in object.keys() {
        if !matches!(key.as_str(), "content" | "required" | "description") {
            error(format!("неподдерживаемый ключ requestBody {key}"));
        }
    }
    let content = object.get("content").and_then(Value::as_object);
    let Some(media) = content
        .filter(|c| c.len() == 1)
        .and_then(|c| c.get("application/json"))
    else {
        error("тело запроса должно быть только application/json".to_owned());
        return None;
    };
    let Some(raw_schema) = media.get("schema") else {
        error("у тела запроса нет schema".to_owned());
        return None;
    };
    let model = raw_schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(Resolver::ref_name)
        .map(str::to_owned);
    let mut schema = resolver.resolve(raw_schema);
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        error("тело запроса должно быть объектом".to_owned());
    }
    let own = schema
        .as_object_mut()
        .and_then(|s| s.remove("description"))
        .and_then(|d| d.as_str().map(str::to_owned))
        .or_else(|| description_of(object));
    let mut description = match &model {
        Some(model) => format!("Request body: `{model}` object."),
        None => "Request body: JSON object.".to_owned(),
    };
    if let Some(own) = own {
        description.push(' ');
        description.push_str(&own);
    }
    Some(Param {
        name: "body".to_owned(),
        location: Location::Body,
        required: object.get("required").and_then(Value::as_bool) == Some(true),
        description,
        value_type: "Object",
        repeated: false,
        schema,
    })
}

/// Внешние операции: проверка описания. Возвращает ключ → группа.
fn collect_external(
    spec: &Value,
    operations: &[Operation],
    errors: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let mut ids = BTreeMap::new();
    let Some(raw) = spec.get("x-agc-external-operations") else {
        return ids;
    };
    let Some(external) = raw.as_object() else {
        errors.push("x-agc-external-operations не является объектом".to_owned());
        return ids;
    };
    let path_ids: BTreeSet<&str> = operations
        .iter()
        .map(|op| op.operation_id.as_str())
        .collect();
    for (id, raw) in sorted_entries(external) {
        let mut error = |text: String| errors.push(format!("внешняя операция {id}: {text}"));
        if path_ids.contains(id.as_str()) {
            error("совпадает с operationId операции снимка".to_owned());
        }
        let Some(object) = raw.as_object() else {
            error("описание не является объектом".to_owned());
            continue;
        };
        for key in object.keys() {
            if !EXTERNAL_KEYS.contains(&key.as_str()) {
                error(format!("неподдерживаемый ключ {key}"));
            }
        }
        let method = string_field(object, "method").unwrap_or_default();
        if !HTTP_METHODS.contains(&method.to_ascii_lowercase().as_str())
            || method != method.to_ascii_uppercase()
        {
            error(format!("неподдерживаемый method {method:?}"));
        }
        let summary = string_field(object, "summary").unwrap_or_default();
        let description = string_field(object, "description").unwrap_or_default();
        if summary.is_empty() || !description.starts_with(&summary) {
            error("нужны summary и description, которое начинается с summary".to_owned());
        }
        let group = string_field(object, "x-agc-group").unwrap_or_default();
        if id.split('.').next() != Some(group.as_str()) {
            error(format!("ключ не начинается с группы {group:?}"));
        }
        ids.insert(id.clone(), group);
        if !string_field(object, "x-agc-reference").is_some_and(|r| r.starts_with(REFERENCE_PREFIX))
        {
            error(format!(
                "x-agc-reference должен вести на {REFERENCE_PREFIX}"
            ));
        }
        let source = string_field(object, "x-agc-url-source").unwrap_or_default();
        if !path_ids.contains(source.as_str()) {
            error(format!(
                "x-agc-url-source {source:?}: такой операции нет в снимке"
            ));
        }
    }
    ids
}

/// Группы дополнения совпадают с группами снимка (операции, внешние операции, теги).
fn check_groups(
    spec: &Value,
    overlay: &Overlay,
    operations: &[Operation],
    external: &BTreeMap<String, String>,
    errors: &mut Vec<String>,
) {
    let mut slugs = BTreeSet::new();
    for group in &overlay.groups {
        if !slugs.insert(group.slug.as_str()) {
            errors.push(format!("[[groups]]: группа {} повторяется", group.slug));
        }
        if group.title.trim().is_empty() || !group.reference.starts_with(REFERENCE_PREFIX) {
            errors.push(format!(
                "[[groups]]: у группы {} нужны title и reference на {REFERENCE_PREFIX}",
                group.slug
            ));
        }
    }
    let used: BTreeSet<String> = operations
        .iter()
        .map(|op| op.group.clone())
        .chain(external.values().cloned())
        .filter(|group| !group.is_empty())
        .collect();
    for group in &used {
        if !slugs.contains(group.as_str()) {
            errors.push(format!(
                "группы {group} из снимка нет в [[groups]] spec/overlay.toml"
            ));
        }
    }
    for slug in &slugs {
        if !used.contains(*slug) {
            errors.push(format!(
                "[[groups]]: группы {slug} нет в снимке; убери её из spec/overlay.toml"
            ));
        }
    }
    // Теги снимка — те же группы с названиями разделов справочника.
    let tags: BTreeMap<&str, &str> = spec
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tag| {
            Some((
                tag.get("name")?.as_str()?,
                tag.get("x-agc-title")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ))
        })
        .collect();
    for group in &overlay.groups {
        match tags.get(group.slug.as_str()) {
            Some(title) if *title == group.title => {}
            Some(title) => errors.push(format!(
                "[[groups]]: название группы {} ({}) не совпадает с x-agc-title тега ({title})",
                group.slug, group.title
            )),
            None => errors.push(format!("в tags снимка нет группы {}", group.slug)),
        }
    }
}

fn is_valid_slug(slug: &str) -> bool {
    slug.starts_with(|c: char| c.is_ascii_lowercase())
        && !slug.ends_with('-')
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Наборы: уникальные корректные slug, каждая операция не больше чем в одном наборе,
/// ссылки только на операции снимка. Возвращает `operationId` → slug набора.
fn check_toolsets(
    overlay: &Overlay,
    known: &BTreeSet<String>,
    errors: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let mut slugs = BTreeSet::new();
    let mut map = BTreeMap::new();
    for toolset in &overlay.toolsets {
        if !is_valid_slug(&toolset.slug) {
            errors.push(format!("[[toolsets]]: недопустимый slug {}", toolset.slug));
        }
        if !slugs.insert(toolset.slug.as_str()) {
            errors.push(format!("[[toolsets]]: slug {} повторяется", toolset.slug));
        }
        if toolset.description.trim().is_empty() {
            errors.push(format!(
                "[[toolsets]]: у набора {} нет описания",
                toolset.slug
            ));
        }
        for id in &toolset.operations {
            if !known.contains(id) {
                errors.push(format!(
                    "[[toolsets]] {}: операции {id} нет в снимке; убери её из spec/overlay.toml",
                    toolset.slug
                ));
            }
            if let Some(other) = map.insert(id.clone(), toolset.slug.clone()) {
                errors.push(format!(
                    "[[toolsets]]: операция {id} указана в наборах {other} и {}",
                    toolset.slug
                ));
            }
        }
    }
    map
}

/// Исключения из уровня по HTTP-методу (`[[access]]`).
fn apply_access(
    overlay: &Overlay,
    path_ids: &BTreeSet<String>,
    operations: &mut [Operation],
    errors: &mut Vec<String>,
) {
    let mut rules: BTreeMap<&str, (Level, &str)> = BTreeMap::new();
    for rule in &overlay.access {
        if rule.reason.trim().is_empty() {
            errors.push("[[access]]: у правила нет reason".to_owned());
        }
        for id in &rule.operations {
            if !path_ids.contains(id) {
                errors.push(format!(
                    "[[access]]: операции {id} нет в снимке; убери её из spec/overlay.toml"
                ));
            }
            if rules
                .insert(id.as_str(), (rule.level, rule.reason.trim()))
                .is_some()
            {
                errors.push(format!("[[access]]: операция {id} указана дважды"));
            }
        }
    }
    for op in operations {
        let Some((level, reason)) = rules.get(op.operation_id.as_str()) else {
            continue;
        };
        if *level == op.access {
            errors.push(format!(
                "[[access]]: исключение для {} совпадает с уровнем по умолчанию для {}",
                op.operation_id, op.http_method
            ));
        }
        op.access = *level;
        op.access_reason = Some(normalize_space(reason));
    }
}

/// Исключения: `deprecated: true` снимка, `[[deprecated]]` и `[[exclude]]` дополнения.
fn apply_exclusions(
    overlay: &Overlay,
    known: &BTreeSet<String>,
    external: &BTreeSet<String>,
    operations: &mut [Operation],
    errors: &mut Vec<String>,
) {
    let mut deprecated = BTreeMap::new();
    for rule in &overlay.deprecated {
        if !operations
            .iter()
            .any(|op| op.operation_id == rule.operation)
        {
            errors.push(format!(
                "[[deprecated]]: операции {} нет в снимке; убери её из spec/overlay.toml",
                rule.operation
            ));
        }
        if deprecated.insert(rule.operation.as_str(), rule).is_some() {
            errors.push(format!(
                "[[deprecated]]: операция {} указана дважды",
                rule.operation
            ));
        }
    }
    let mut excluded = BTreeMap::new();
    for rule in &overlay.exclude {
        if rule.reason.trim().is_empty() {
            errors.push("[[exclude]]: у правила нет reason".to_owned());
        }
        for id in &rule.operations {
            if !known.contains(id) {
                errors.push(format!(
                    "[[exclude]]: операции {id} нет в снимке; убери её из spec/overlay.toml"
                ));
            }
            if excluded.insert(id.as_str(), rule).is_some() {
                errors.push(format!("[[exclude]]: операция {id} указана дважды"));
            }
            if deprecated.contains_key(id.as_str()) {
                errors.push(format!(
                    "[[exclude]]: операция {id} уже указана в [[deprecated]]"
                ));
            }
        }
    }
    for id in external {
        if !excluded.get(id.as_str()).is_some_and(|rule| !rule.callable) {
            errors.push(format!(
                "внешняя операция {id} должна быть в [[exclude]] с callable = false: это не вызов Connect API"
            ));
        }
    }
    for op in operations {
        if let Some(rule) = deprecated.get(op.operation_id.as_str()) {
            op.exclusion = Some(Exclusion::Deprecated);
            op.note = rule
                .note
                .as_deref()
                .map(normalize_space)
                .unwrap_or_default();
        } else if let Some(rule) = excluded.get(op.operation_id.as_str()) {
            if op.deprecated {
                errors.push(format!(
                    "[[exclude]]: операция {} устарела в снимке; перенеси её в [[deprecated]]",
                    op.operation_id
                ));
            }
            op.exclusion = Some(Exclusion::Overlay);
            op.callable = rule.callable;
            op.note = normalize_space(&rule.reason);
            op.alternative = normalize_space(&rule.alternative);
        } else if op.deprecated {
            op.exclusion = Some(Exclusion::Deprecated);
        }
    }
}

/// Секретные поля ответов (`[[secrets]]`): операция — инструмент с JSON-ответом, путь
/// ведёт к полю-строке схемы ответа, поле указано один раз, у операции не больше одного
/// вида сохраняемых в файл значений (ссылка или содержимое).
fn apply_secrets(
    overlay: &Overlay,
    schemas: &Map<String, Value>,
    operations: &mut [Operation],
    errors: &mut Vec<String>,
) {
    for rule in &overlay.secrets {
        if rule.description.trim().is_empty() {
            errors.push("[[secrets]]: у правила нет description".to_owned());
        }
        if rule.operations.is_empty() || rule.fields.is_empty() {
            errors.push("[[secrets]]: нужны operations и fields".to_owned());
        }
        for id in &rule.operations {
            let Some(op) = operations.iter_mut().find(|op| &op.operation_id == id) else {
                errors.push(format!(
                    "[[secrets]]: операции {id} нет в снимке; убери её из spec/overlay.toml"
                ));
                continue;
            };
            if op.exclusion.is_some() {
                errors.push(format!(
                    "[[secrets]]: операция {id} исключена; секретные поля задаются только инструментам"
                ));
                continue;
            }
            let Some(root) = op.response_schema.as_ref().filter(|_| op.json_response) else {
                errors.push(format!("[[secrets]]: у операции {id} нет JSON-ответа"));
                continue;
            };
            for field in &rule.fields {
                if let Err(text) = check_secret_path(schemas, root, field) {
                    errors.push(format!("[[secrets]] {id}: поле {field}: {text}"));
                    continue;
                }
                if op.secrets.iter().any(|secret| &secret.path == field) {
                    errors.push(format!("[[secrets]] {id}: поле {field} указано дважды"));
                    continue;
                }
                op.secrets.push(Secret {
                    path: field.clone(),
                    kind: rule.kind,
                    description: normalize_space(&rule.description),
                    hint: normalize_space(&rule.hint),
                });
            }
        }
    }
    for op in operations.iter_mut() {
        op.secrets.sort_by(|a, b| a.path.cmp(&b.path));
        let savable: BTreeSet<&str> = op
            .secrets
            .iter()
            .filter(|secret| secret.kind.savable())
            .map(|secret| secret.kind.rust_name())
            .collect();
        if savable.len() > 1 {
            errors.push(format!(
                "[[secrets]]: у операции {} и ссылки, и содержимое файла: save_to сохраняет что-то одно",
                op.operation_id
            ));
        }
    }
}

/// Путь `a.b[].c` в схеме ответа: каждый сегмент — свойство объекта, `[]` — элемент
/// массива, последнее поле — строка.
fn check_secret_path(schemas: &Map<String, Value>, root: &Value, path: &str) -> Result<(), String> {
    let mut node = deref_schema(schemas, root)?;
    for segment in path.split('.') {
        let (name, array) = match segment.strip_suffix("[]") {
            Some(name) => (name, true),
            None => (segment, false),
        };
        if !is_valid_property_name(name) || name.contains(['.', '[', ']']) {
            return Err(format!("недопустимый сегмент пути {segment:?}"));
        }
        let property = node
            .get("properties")
            .and_then(|properties| properties.get(name))
            .ok_or_else(|| format!("в схеме ответа нет поля {name}"))?;
        node = deref_schema(schemas, property)?;
        if array {
            if node.get("type").and_then(Value::as_str) != Some("array") {
                return Err(format!("{name} не массив, а путь говорит {name}[]"));
            }
            let items = node
                .get("items")
                .ok_or_else(|| format!("у массива {name} нет items"))?;
            node = deref_schema(schemas, items)?;
        }
    }
    match node.get("type").and_then(Value::as_str) {
        Some("string") => Ok(()),
        other => Err(format!("поле должно быть строкой, а не {other:?}")),
    }
}

/// Раскрывает `$ref` схемы (только на верхнем уровне узла).
fn deref_schema<'a>(
    schemas: &'a Map<String, Value>,
    schema: &'a Value,
) -> Result<&'a Value, String> {
    let mut node = schema;
    for _ in 0..MAX_SECRET_REF_DEPTH {
        let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
            return Ok(node);
        };
        let name = Resolver::ref_name(reference)
            .ok_or_else(|| format!("неподдерживаемая ссылка {reference}"))?;
        node = schemas
            .get(name)
            .ok_or_else(|| format!("модели {name} нет в components/schemas"))?;
    }
    Err("слишком глубокие ссылки $ref".to_owned())
}

/// Имя инструмента: `[a-z][a-z0-9_]*`, без `__` и `_` на конце, не длиннее предела.
pub fn is_valid_tool_name(name: &str) -> bool {
    name.len() <= MAX_TOOL_NAME
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('_')
        && !name.contains("__")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Имя свойства схемы, которое принимают клиенты: `^[a-zA-Z0-9_.-]{1,64}$`.
fn is_valid_property_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Имена из `[names]`: у каждой неисключённой операции, уникальные, допустимые.
fn assign_names(
    overlay: &Overlay,
    path_ids: &BTreeSet<String>,
    external: &BTreeSet<String>,
    operations: &mut [Operation],
    errors: &mut Vec<String>,
) {
    for (id, name) in &overlay.names {
        if external.contains(id) {
            errors.push(format!(
                "[names]: {id} — внешняя операция, инструментом она не бывает; убери имя"
            ));
        } else if !path_ids.contains(id) {
            errors.push(format!(
                "[names]: операции {id} нет в снимке; убери её из spec/overlay.toml"
            ));
        }
        if !is_valid_tool_name(name) {
            errors.push(format!(
                "[names]: имя инструмента {name} у {id} ({} символов) недопустимо или длиннее {MAX_TOOL_NAME}",
                name.len()
            ));
        }
    }
    let mut owners: BTreeMap<&str, &str> = BTreeMap::new();
    for op in operations.iter_mut() {
        let name = overlay.names.get(&op.operation_id);
        match (name, op.exclusion) {
            (Some(name), None) => op.tool_name = name.clone(),
            (None, None) => errors.push(format!(
                "{}: нет имени инструмента: добавь его в [names] spec/overlay.toml",
                op.operation_id
            )),
            (Some(_), Some(_)) => errors.push(format!(
                "[names]: операция {} исключена, имя ей не нужно",
                op.operation_id
            )),
            (None, Some(_)) => {}
        }
    }
    for op in operations.iter().filter(|op| op.exclusion.is_none()) {
        if op.tool_name.is_empty() {
            continue;
        }
        if let Some(other) = owners.insert(op.tool_name.as_str(), op.operation_id.as_str()) {
            errors.push(format!(
                "[names]: имя инструмента {} повторяется у {other} и {}",
                op.tool_name, op.operation_id
            ));
        }
    }
}

/// Замены устаревших операций (`[[deprecated]] replacement`) — текст для агента.
fn resolve_replacements(overlay: &Overlay, operations: &mut [Operation], errors: &mut Vec<String>) {
    let kept: BTreeMap<String, String> = operations
        .iter()
        .filter(|op| op.exclusion.is_none())
        .map(|op| (op.operation_id.clone(), op.tool_name.clone()))
        .collect();
    for rule in &overlay.deprecated {
        let Some(replacement) = &rule.replacement else {
            continue;
        };
        let Some(tool) = kept.get(replacement) else {
            errors.push(format!(
                "[[deprecated]]: замена {replacement} для {} не является инструментом",
                rule.operation
            ));
            continue;
        };
        if let Some(op) = operations
            .iter_mut()
            .find(|op| op.operation_id == rule.operation)
        {
            op.alternative = format!("Use `{tool}` instead.");
        }
    }
}

/// Описание операции для агента: остаток описания снимка после `summary` и причина
/// уровня доступа, если он отличается от уровня по HTTP-методу.
fn describe(op: &Operation) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !op.details.is_empty() {
        parts.push(op.details.clone());
    }
    if let Some(reason) = &op.access_reason {
        let level = match op.access {
            Level::Read => "Read-only",
            Level::Write => "Write",
            Level::Destructive => "Destructive",
        };
        parts.push(format!("{level}: {reason}"));
    }
    parts.join("\n\n")
}

/// JSON Schema аргументов инструмента: плоские параметры и `body`, без лишних аргументов.
fn input_schema(params: &[Param]) -> String {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in params {
        let mut schema = param.schema.clone();
        if let Some(object) = schema.as_object_mut() {
            object.insert(
                "description".into(),
                Value::String(param.description.clone()),
            );
        }
        properties.insert(param.name.clone(), schema);
        if param.required {
            required.push(Value::String(param.name.clone()));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    serde_json::to_string(&sorted(Value::Object(schema))).unwrap_or_default()
}

/// Ключи объектов по алфавиту на всех уровнях.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let entries: BTreeMap<String, Value> = object
                .into_iter()
                .map(|(key, value)| (key, sorted(value)))
                .collect();
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

fn rust_method(method: &str) -> &'static str {
    match method {
        "GET" => "HttpMethod::Get",
        "POST" => "HttpMethod::Post",
        "PUT" => "HttpMethod::Put",
        "PATCH" => "HttpMethod::Patch",
        _ => "HttpMethod::Delete",
    }
}

/// Строковые литералы выводятся через `{:?}`: это корректное экранирование для Rust.
fn emit(overlay: &Overlay, operations: &[Operation], excluded: &[Operation]) -> String {
    let mut out = String::new();
    out.push_str(
        "// Сгенерировано build.rs из spec/openapi.json и spec/overlay.toml. Не редактировать.\n",
    );
    out.push_str("static GROUPS: &[GroupDef] = &[\n");
    for group in &overlay.groups {
        let _ = writeln!(
            out,
            "    GroupDef {{ slug: {:?}, title: {:?}, reference: {:?} }},",
            group.slug,
            group.title.trim(),
            group.reference.trim(),
        );
    }
    out.push_str("];\n\n");
    out.push_str("static TOOLSETS: &[ToolsetDef] = &[\n");
    for toolset in &overlay.toolsets {
        let _ = writeln!(
            out,
            "    ToolsetDef {{ slug: {:?}, description: {:?}, default_enabled: {} }},",
            toolset.slug,
            normalize_space(&toolset.description),
            toolset.default,
        );
    }
    out.push_str("];\n\n");
    out.push_str("static OPERATIONS: &[OperationDef] = &[\n");
    for op in operations {
        let _ = writeln!(out, "    OperationDef {{");
        let _ = writeln!(out, "        tool_name: {:?},", op.tool_name);
        let _ = writeln!(out, "        operation_id: {:?},", op.operation_id);
        let _ = writeln!(out, "        group: {:?},", op.group);
        let _ = writeln!(out, "        toolset: {:?},", op.toolset);
        let _ = writeln!(out, "        summary: {:?},", op.summary);
        let _ = writeln!(out, "        description: {:?},", op.description);
        let _ = writeln!(out, "        method: {},", rust_method(&op.http_method));
        let _ = writeln!(out, "        path: {:?},", op.path);
        let _ = writeln!(out, "        access: {},", op.access.rust_name());
        let _ = writeln!(out, "        params: &[");
        for param in &op.params {
            let _ = writeln!(
                out,
                "            ParamDef {{ name: {:?}, api_name: {:?}, location: {}, required: {}, description: {:?}, value_type: ParamType::{}, repeated: {} }},",
                param.name,
                param.name,
                param.location.rust_name(),
                param.required,
                param.description,
                param.value_type,
                param.repeated,
            );
        }
        let _ = writeln!(out, "        ],");
        let body = if op.has_body {
            "Some(BodyKind::Json)"
        } else {
            "None"
        };
        let _ = writeln!(out, "        body: {body},");
        let response = if op.json_response {
            "ResponseKind::Json"
        } else {
            "ResponseKind::Empty"
        };
        let _ = writeln!(out, "        response: {response},");
        match &op.result {
            Some(result) => {
                let _ = writeln!(
                    out,
                    "        result: Some(ResultDef {{ field: {:?}, code_field: {:?}, message_field: {:?} }}),",
                    result.field, result.code_field, result.message_field
                );
            }
            None => {
                let _ = writeln!(out, "        result: None,");
            }
        }
        let _ = writeln!(out, "        reference: {:?},", op.reference);
        let _ = writeln!(out, "        input_schema: {:?},", op.input_schema);
        let _ = writeln!(out, "        secrets: &[");
        for secret in &op.secrets {
            let _ = writeln!(
                out,
                "            SecretFieldDef {{ path: {:?}, kind: {}, description: {:?}, hint: {:?} }},",
                secret.path,
                secret.kind.rust_name(),
                secret.description,
                secret.hint,
            );
        }
        let _ = writeln!(out, "        ],");
        let _ = writeln!(out, "    }},");
    }
    out.push_str("];\n\n");
    out.push_str("static EXCLUDED: &[ExcludedOperationDef] = &[\n");
    for op in excluded {
        let reason = op.exclusion.map(Exclusion::rust_name).unwrap_or_default();
        let _ = writeln!(
            out,
            "    ExcludedOperationDef {{ operation_id: {:?}, group: {:?}, toolset: {:?}, summary: {:?}, method: {}, path: {:?}, access: {}, reason: {reason}, callable: {}, note: {:?}, alternative: {:?} }},",
            op.operation_id,
            op.group,
            op.toolset,
            op.summary,
            rust_method(&op.http_method),
            op.path,
            op.access.rust_name(),
            op.callable,
            op.note,
            op.alternative,
        );
    }
    out.push_str("];\n");
    out
}
