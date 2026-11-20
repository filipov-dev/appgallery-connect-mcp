//! `remote_config_set`: изменение параметров Remote Configuration по имени.
//!
//! `PUT` Remote Configuration (`remoteconfig.config.update`) принимает конфигурацию целиком
//! (судя по справочнику, опущенный параметр удаляется) и номер последней версии
//! (оптимистическая блокировка: если конфигурацию успели изменить, приходит код
//! [`VERSION_CONFLICT`]). Собирать такое тело агенту руками опасно: легко потерять чужие
//! параметры, а значения, которые сервер вырезал из ответа (`***`), записались бы вместо
//! настоящих. Поэтому инструмент сам читает текущую конфигурацию (`remoteconfig.config.get`,
//! ответ агенту не отдаётся), применяет изменения по именам параметров и условий,
//! показывает разницу и только без `dry_run` пишет новую версию с прочитанным номером
//! версии. Неизменённые параметры и условия уходят такими, какими их вернул API (вместе с
//! полями, которых нет в снимке), а не такими, какими их видел агент.
//!
//! Любая запись — уровень удаления: черновика нет, новая версия сразу действует для
//! пользователей приложения. `dry_run` по умолчанию `true`.
//!
//! Защита от записи вырезанных значений: строка с маской [`MASK`] в новых значениях или
//! условиях — отказ до запроса ([`reject_masked`]); то же правило сервер применяет к телу
//! `update_remote_config` и `api_request` к его пути ([`guard_masked_body`]).
//!
//! Запросы подписывает API-клиент проекта из `productId` (`OperationDef::project_param`);
//! без аргумента берётся единственный проект с API-клиентом в профиле.

use std::collections::{BTreeMap, BTreeSet};

use appgallery_connect_api::executor::ApiError;
use appgallery_connect_api::scrub::MASK;
use appgallery_connect_api::{AccessLevel, OperationDef};
use serde_json::{Map, Value, json};

use super::listing::bool_arg;
use super::reviews_summary::{app_id, operation};
use super::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error_for};

/// Имя инструмента.
pub const NAME: &str = "remote_config_set";
/// Набор инструментов.
pub const TOOLSET: &str = "remote-config";
/// `remoteconfig.config.get` (`get_remote_config`).
const CONFIG_GET: &str = "remoteconfig.config.get";
/// `remoteconfig.config.update` (`update_remote_config`).
pub const CONFIG_UPDATE: &str = "remoteconfig.config.update";
/// Код результата «версию успели изменить» (страница Result Codes справочника).
pub const VERSION_CONFLICT: &str = "204083201";
/// Сколько изменений принимает один вызов.
const MAX_CHANGES: usize = 200;

const DESCRIPTION: &str = "Change parameters of the Remote Configuration of an app by name: set \
the default value, set or remove the value for a condition, change the description, add or \
delete a parameter, and add, replace or delete conditions (filters). The server reads the \
current configuration itself, applies the changes, shows the difference (`changes`: every \
parameter and condition with its old and new state) and, only with dry_run false, writes a new \
version with the version number it read. Parameters and conditions that are not named are sent \
back exactly as AppGallery Connect stores them, so values hidden as `***` in responses are never \
overwritten; new values that contain `***` are refused. `dry_run` defaults to true: show the \
difference to the user and apply only after they confirm. Destructive: the new version takes \
effect for the users of the app at once, there is no draft or review. If the configuration was \
changed by someone else after it was read, nothing is written and the error says so: run the \
tool again with dry_run true. Pass `baseVersion` (the `version` of the dry run) to refuse the \
write when the configuration changed after the user reviewed the difference. Requests are \
signed with the project-level API client of `productId`; to undo a change use \
rollback_remote_config with an earlier version from list_remote_config_versions.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: TOOLSET,
        access: AccessLevel::Destructive,
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
            "productId": {
                "type": "string",
                "minLength": 1,
                "description": "Project ID of the app (the project-level API client of this \
                    project signs the requests). Optional when the profile has exactly one \
                    project with an API client."
            },
            "appId": {
                "type": ["string", "integer"],
                "description": "Numeric app ID (like 100000001). Optional when the profile has \
                    exactly one default app."
            },
            "parameters": {
                "type": "array",
                "maxItems": MAX_CHANGES,
                "description": "Changes of parameters, one entry per parameter name. Fields \
                    that are not given keep their current values.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "name": {"type": "string", "minLength": 1, "description": "Parameter name."},
                        "value": {
                            "type": "string",
                            "description": "New default value (a string, as Remote Configuration \
                                stores values). Required for a new parameter unless useSdkValue \
                                is true."
                        },
                        "useSdkValue": {
                            "type": "boolean",
                            "description": "true: use the in-app default value instead of value \
                                by default; false: use value."
                        },
                        "description": {"type": "string", "description": "New parameter description."},
                        "conditions": {
                            "type": "object",
                            "description": "Values of the parameter for conditions, by condition \
                                (filter) name: a string sets the value, {\"useSdkValue\": true} uses \
                                the in-app default, null removes the value for that condition. \
                                Conditions must exist in the configuration or be added in \
                                `filters`."
                        },
                        "delete": {
                            "type": "boolean",
                            "description": "true deletes the parameter; no other fields are \
                                allowed then."
                        }
                    },
                    "required": ["name"]
                }
            },
            "filters": {
                "type": "array",
                "maxItems": MAX_CHANGES,
                "description": "Conditions to add or replace by name: whole Filter objects as \
                    get_remote_config returns them (name, desc, language, country, versionName, \
                    deviceChip, dateTimes, userRatio, osVersion, systemVersion, browsersVersion). \
                    To change the percentage of users of a condition, pass it with a new \
                    userRatio (value: percentage multiplied by 1000000, like 2000000 for 2%).",
                "items": {"type": "object"}
            },
            "removeFilters": {
                "type": "array",
                "maxItems": MAX_CHANGES,
                "items": {"type": "string", "minLength": 1},
                "description": "Names of conditions to delete. A condition that a parameter \
                    still uses cannot be deleted: remove its values in `parameters` too."
            },
            "baseVersion": {
                "type": "integer",
                "minimum": 0,
                "description": "Version the user reviewed (`version` of the dry run): if the \
                    current version is different, nothing is written."
            },
            "dry_run": {
                "type": "boolean",
                "description": "Default true: only show the difference. false writes a new \
                    version of the configuration."
            }
        }
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(remote_config_set(ctx, args))
}

/// Отказ, если в теле записи Remote Configuration есть маска вырезанного значения: агент,
/// скорее всего, отправляет обратно прочитанный ответ, и настоящее значение затёрлось бы
/// звёздочками. Проверяет только `update_remote_config` (инструмент и `api_request` к его
/// пути); у остальных операций ничего не делает.
pub fn guard_masked_body(op: &OperationDef, body: Option<&Value>) -> Result<(), ToolError> {
    if op.operation_id != CONFIG_UPDATE {
        return Ok(());
    }
    let Some(body) = body else {
        return Ok(());
    };
    match masked_path(body, "body") {
        Some(path) => Err(ToolError::new(format!(
            "`{path}` contains `{MASK}`, the mask the server puts in place of hidden values in \
             its responses: writing it would replace the real value stored in AppGallery \
             Connect. Nothing was sent. Use {NAME}: it changes parameters by name and sends the \
             other values back as AppGallery Connect stores them. A value that really contains \
             `{MASK}` can only be set in AppGallery Connect."
        ))),
        None => Ok(()),
    }
}

/// Путь первой строки (или ключа) с маской [`MASK`]; `None` — маски нет.
fn masked_path(value: &Value, path: &str) -> Option<String> {
    match value {
        Value::String(text) if text.contains(MASK) => Some(path.to_owned()),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(index, item)| masked_path(item, &format!("{path}[{index}]"))),
        Value::Object(object) => object.iter().find_map(|(key, item)| {
            let here = format!("{path}.{key}");
            if key.contains(MASK) {
                Some(here)
            } else {
                masked_path(item, &here)
            }
        }),
        _ => None,
    }
}

/// Маска в аргументах инструмента — отказ до запроса.
fn reject_masked(value: &Value, path: &str) -> Result<(), ToolError> {
    match masked_path(value, path) {
        Some(path) => Err(ToolError::new(format!(
            "`{path}` contains `{MASK}`, the mask the server puts in place of hidden values: it \
             is not the real value, and writing it would replace the value stored in AppGallery \
             Connect. Nothing was sent. Pass only the values you change; the others are kept as \
             AppGallery Connect stores them. A value that really contains `{MASK}` can only be \
             set in AppGallery Connect."
        ))),
        None => Ok(()),
    }
}

/// Значение параметра для условия из аргумента `conditions`.
#[derive(Debug, Clone, PartialEq)]
enum ConditionChange {
    /// Строка — новое значение.
    Value(String),
    /// `{"useSdkValue": true}` — значение по умолчанию из приложения.
    UseSdk,
    /// `null` — убрать значение для условия.
    Remove,
}

/// Изменение одного параметра.
#[derive(Debug, Clone, Default)]
struct ParameterChange {
    name: String,
    value: Option<String>,
    use_sdk_value: Option<bool>,
    description: Option<String>,
    conditions: BTreeMap<String, ConditionChange>,
    delete: bool,
}

/// Разобранные аргументы.
#[derive(Debug, Default)]
struct Changes {
    parameters: Vec<ParameterChange>,
    /// Условия на добавление или замену, по имени (объекты как в API).
    filters: Vec<(String, Map<String, Value>)>,
    remove_filters: Vec<String>,
}

fn parse_changes(args: &Value) -> Result<Changes, ToolError> {
    let mut changes = Changes::default();
    let mut names = BTreeSet::new();
    for (index, raw) in array(args, "parameters")?.iter().enumerate() {
        let path = format!("parameters[{index}]");
        reject_masked(raw, &path)?;
        let Some(object) = raw.as_object() else {
            return Err(ToolError::new(format!("`{path}` must be an object")));
        };
        let name = text_field(object, "name", &path)?
            .ok_or_else(|| ToolError::new(format!("`{path}.name` is required")))?;
        if !names.insert(name.clone()) {
            return Err(ToolError::new(format!(
                "parameter `{name}` is given more than once in `parameters`: merge the changes \
                 into one entry"
            )));
        }
        let mut change = ParameterChange {
            name,
            value: text_field(object, "value", &path)?,
            description: text_field(object, "description", &path)?,
            use_sdk_value: match object.get("useSdkValue") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(flag)) => Some(*flag),
                Some(_) => {
                    return Err(ToolError::new(format!(
                        "`{path}.useSdkValue` must be a boolean"
                    )));
                }
            },
            delete: matches!(object.get("delete"), Some(Value::Bool(true))),
            ..ParameterChange::default()
        };
        match object.get("conditions") {
            None | Some(Value::Null) => {}
            Some(Value::Object(conditions)) => {
                for (filter, value) in conditions {
                    let parsed = match value {
                        Value::Null => ConditionChange::Remove,
                        Value::String(text) => ConditionChange::Value(text.clone()),
                        Value::Object(flag)
                            if flag.len() == 1
                                && flag.get("useSdkValue") == Some(&Value::Bool(true)) =>
                        {
                            ConditionChange::UseSdk
                        }
                        _ => {
                            return Err(ToolError::new(format!(
                                "`{path}.conditions.{filter}` must be a string (the value), \
                                 {{\"useSdkValue\": true}} or null (remove the value)"
                            )));
                        }
                    };
                    change.conditions.insert(filter.clone(), parsed);
                }
            }
            Some(_) => {
                return Err(ToolError::new(format!(
                    "`{path}.conditions` must be an object: condition name to value"
                )));
            }
        }
        if change.delete
            && (change.value.is_some()
                || change.use_sdk_value.is_some()
                || change.description.is_some()
                || !change.conditions.is_empty())
        {
            return Err(ToolError::new(format!(
                "`{path}`: a parameter that is deleted takes no other changes"
            )));
        }
        changes.parameters.push(change);
    }

    let mut filter_names = BTreeSet::new();
    for (index, raw) in array(args, "filters")?.iter().enumerate() {
        let path = format!("filters[{index}]");
        reject_masked(raw, &path)?;
        let Some(object) = raw.as_object() else {
            return Err(ToolError::new(format!("`{path}` must be a Filter object")));
        };
        let name = text_field(object, "name", &path)?
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| ToolError::new(format!("`{path}.name` is required")))?;
        if !filter_names.insert(name.clone()) {
            return Err(ToolError::new(format!(
                "condition `{name}` is given more than once in `filters`"
            )));
        }
        changes.filters.push((name, object.clone()));
    }
    for (index, raw) in array(args, "removeFilters")?.iter().enumerate() {
        let Some(name) = raw.as_str() else {
            return Err(ToolError::new(format!(
                "`removeFilters[{index}]` must be a string"
            )));
        };
        if filter_names.contains(name) {
            return Err(ToolError::new(format!(
                "condition `{name}` is both in `filters` and in `removeFilters`"
            )));
        }
        if !changes.remove_filters.iter().any(|known| known == name) {
            changes.remove_filters.push(name.to_owned());
        }
    }
    if changes.parameters.is_empty()
        && changes.filters.is_empty()
        && changes.remove_filters.is_empty()
    {
        return Err(ToolError::new(
            "nothing to change: pass `parameters`, `filters` or `removeFilters`",
        ));
    }
    Ok(changes)
}

fn array<'a>(args: &'a Value, name: &str) -> Result<&'a [Value], ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(ToolError::new(format!("`{name}` must be an array"))),
    }
}

fn text_field(
    object: &Map<String, Value>,
    name: &str,
    path: &str,
) -> Result<Option<String>, ToolError> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ToolError::new(format!("`{path}.{name}` must be a string"))),
    }
}

/// Текущая конфигурация, как её вернул API.
#[derive(Debug, Clone)]
struct Config {
    version: u64,
    items: Vec<Value>,
    filters: Vec<Value>,
}

impl Config {
    fn from_response(body: &Value) -> Result<Self, ToolError> {
        let version = match body.pointer("/versionInfo/version") {
            None | Some(Value::Null) => 0,
            Some(value) => value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
                .ok_or_else(|| {
                    ToolError::new(
                        "the Remote Configuration response has no valid version number \
                         (`versionInfo.version`); nothing was written",
                    )
                })?,
        };
        let list = |name: &str| -> Result<Vec<Value>, ToolError> {
            match body.get(name) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Array(items)) => Ok(items.clone()),
                Some(_) => Err(ToolError::new(format!(
                    "`{name}` in the Remote Configuration response is not an array; nothing \
                     was written"
                ))),
            }
        };
        Ok(Self {
            version,
            items: list("configItems")?,
            filters: list("filters")?,
        })
    }
}

fn name_of(value: &Value) -> Option<&str> {
    value.get("name").and_then(Value::as_str)
}

/// Запись разницы: имя, действие, состояние до и после.
fn diff_entry(name: &str, before: Option<&Value>, after: Option<&Value>) -> Value {
    let action = match (before, after) {
        (None, Some(_)) => "add",
        (Some(_), None) => "delete",
        _ => "update",
    };
    json!({"name": name, "action": action, "before": before, "after": after})
}

/// Новое значение `ConfigItemValue`: `value` и `useSdkValue` поверх текущего.
fn item_value(current: Option<&Value>, value: Option<&str>, use_sdk: Option<bool>) -> Value {
    let mut out = current
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(value) = value {
        out.insert("value".into(), json!(value));
        // Новое значение без явного useSdkValue — значение из конфигурации, а не из приложения.
        if use_sdk.is_none() {
            out.insert("useSdkValue".into(), json!(false));
        }
    }
    if let Some(flag) = use_sdk {
        out.insert("useSdkValue".into(), json!(flag));
    }
    if !out.contains_key("useSdkValue") {
        out.insert("useSdkValue".into(), json!(false));
    }
    Value::Object(out)
}

/// Результат применения изменений к конфигурации.
struct Applied {
    items: Vec<Value>,
    filters: Vec<Value>,
    parameter_diff: Vec<Value>,
    filter_diff: Vec<Value>,
    notes: Vec<String>,
}

fn apply_changes(config: &Config, changes: &Changes) -> Result<Applied, ToolError> {
    let mut notes = Vec::new();

    // Условия: замена и добавление, затем удаление.
    let mut filters = config.filters.clone();
    let mut filter_diff = Vec::new();
    for (name, new) in &changes.filters {
        let new = Value::Object(new.clone());
        match filters.iter().position(|f| name_of(f) == Some(name)) {
            Some(index) => {
                if filters[index] != new {
                    filter_diff.push(diff_entry(name, Some(&filters[index]), Some(&new)));
                    filters[index] = new;
                } else {
                    notes.push(format!(
                        "Condition `{name}` is already as given: no change."
                    ));
                }
            }
            None => {
                filter_diff.push(diff_entry(name, None, Some(&new)));
                filters.push(new);
            }
        }
    }
    for name in &changes.remove_filters {
        match filters.iter().position(|f| name_of(f) == Some(name)) {
            Some(index) => {
                let removed = filters.remove(index);
                filter_diff.push(diff_entry(name, Some(&removed), None));
            }
            None => {
                return Err(ToolError::new(format!(
                    "condition `{name}` in `removeFilters` is not in the configuration; nothing \
                     was written"
                )));
            }
        }
    }
    let known_filters: BTreeSet<&str> = filters.iter().filter_map(name_of).collect();

    // Параметры.
    let mut items = config.items.clone();
    let mut parameter_diff = Vec::new();
    for change in &changes.parameters {
        let name = change.name.as_str();
        let position = items.iter().position(|item| name_of(item) == Some(name));
        if change.delete {
            match position {
                Some(index) => {
                    let removed = items.remove(index);
                    parameter_diff.push(diff_entry(name, Some(&removed), None));
                }
                None => {
                    return Err(ToolError::new(format!(
                        "parameter `{name}` cannot be deleted: it is not in the configuration; \
                         nothing was written"
                    )));
                }
            }
            continue;
        }
        let before = position.map(|index| items[index].clone());
        if before.is_none() && change.value.is_none() && change.use_sdk_value != Some(true) {
            return Err(ToolError::new(format!(
                "parameter `{name}` is not in the configuration: a new parameter needs `value` \
                 (or useSdkValue true); nothing was written"
            )));
        }
        let mut item = before
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| {
                let mut new = Map::new();
                new.insert("name".into(), json!(name));
                new
            });
        if change.value.is_some() || change.use_sdk_value.is_some() || before.is_none() {
            let value = item_value(
                item.get("defaultValue"),
                change.value.as_deref(),
                change.use_sdk_value,
            );
            item.insert("defaultValue".into(), value);
        }
        if let Some(description) = &change.description {
            item.insert("desc".into(), json!(description));
        }
        if !change.conditions.is_empty() {
            let mut values: Vec<Value> = match item.get("filterValues") {
                Some(Value::Array(values)) => values.clone(),
                _ => Vec::new(),
            };
            for (filter, condition) in &change.conditions {
                let index = values.iter().position(|v| name_of(v) == Some(filter));
                match condition {
                    ConditionChange::Remove => match index {
                        Some(index) => {
                            values.remove(index);
                        }
                        None => notes.push(format!(
                            "Parameter `{name}` has no value for condition `{filter}`: nothing \
                             to remove."
                        )),
                    },
                    ConditionChange::Value(_) | ConditionChange::UseSdk => {
                        if !known_filters.contains(filter.as_str()) {
                            return Err(ToolError::new(format!(
                                "condition `{filter}` of parameter `{name}` is not in the \
                                 configuration: add it in `filters` first; nothing was written"
                            )));
                        }
                        let (text, use_sdk) = match condition {
                            ConditionChange::Value(text) => (Some(text.as_str()), None),
                            _ => (None, Some(true)),
                        };
                        let current = index.and_then(|i| values[i].get("value"));
                        let value = item_value(current, text, use_sdk);
                        match index {
                            Some(index) => {
                                if let Some(entry) = values[index].as_object_mut() {
                                    entry.insert("value".into(), value);
                                }
                            }
                            None => values.push(json!({"name": filter, "value": value})),
                        }
                    }
                }
            }
            item.insert("filterValues".into(), Value::Array(values));
        }
        let after = Value::Object(item);
        match (position, &before) {
            (Some(index), Some(before)) => {
                if *before == after {
                    notes.push(format!(
                        "Parameter `{name}` is already as given: no change."
                    ));
                } else {
                    parameter_diff.push(diff_entry(name, Some(before), Some(&after)));
                    items[index] = after;
                }
            }
            _ => {
                parameter_diff.push(diff_entry(name, None, Some(&after)));
                items.push(after);
            }
        }
    }

    // Условие, которое удаляют, не должно оставаться в значениях параметров.
    for name in &changes.remove_filters {
        let users: Vec<&str> = items
            .iter()
            .filter(|item| {
                item.get("filterValues")
                    .and_then(Value::as_array)
                    .is_some_and(|values| values.iter().any(|v| name_of(v) == Some(name)))
            })
            .filter_map(name_of)
            .collect();
        if !users.is_empty() {
            return Err(ToolError::new(format!(
                "condition `{name}` cannot be deleted: parameter(s) {} still have a value for \
                 it; remove it with `conditions: {{\"{name}\": null}}` in `parameters`. Nothing \
                 was written",
                users
                    .iter()
                    .map(|user| format!("`{user}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(Applied {
        items,
        filters,
        parameter_diff,
        filter_diff,
        notes,
    })
}

/// Project ID: аргумент или единственный проект с API-клиентом в профиле.
fn product_id(ctx: &Context, args: &Value) -> Result<String, ToolError> {
    if let Some(value) = args.get("productId").and_then(Value::as_str) {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(value.to_owned());
        }
    }
    match ctx.api.project_ids().as_slice() {
        [only] => Ok((*only).to_owned()),
        [] => Err(ToolError::new(
            "`productId` is required: pass the project ID of the app (it is shown in \
             AppGallery Connect > the project > Project settings)",
        )),
        several => Err(ToolError::new(format!(
            "`productId` is required: the server has project-level API clients of several \
             projects ({}); pass the project of the app",
            several
                .iter()
                .map(|id| format!("`{id}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn read_error(err: &ApiError, http: bool) -> ToolError {
    ToolError::new(format!(
        "could not read the Remote Configuration, nothing was written: {}",
        describe_api_error_for(err, http)
    ))
}

async fn remote_config_set(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let dry_run = bool_arg(&args, "dry_run")?.unwrap_or(true);
    let changes = parse_changes(&args)?;
    let product = product_id(ctx, &args)?;
    let app = app_id(ctx, args.get("appId"))?;
    let base_version = args.get("baseVersion").and_then(Value::as_u64);
    let get = operation(CONFIG_GET)?;
    let update = operation(CONFIG_UPDATE)?;
    let headers = json!({"productId": product, "appId": app});

    tracing::debug!(tool = NAME, dry_run, "чтение Remote Configuration");
    let response = ctx
        .api
        .call(get, &headers)
        .await
        .map_err(|err| read_error(&err, ctx.http))?;
    let Some(body) = response.body.json() else {
        return Err(ToolError::new(
            "the Remote Configuration response is not JSON; nothing was written",
        ));
    };
    let config = Config::from_response(body)?;
    if let Some(expected) = base_version
        && expected != config.version
    {
        return Err(ToolError::new(format!(
            "the Remote Configuration is now at version {}, not at baseVersion {expected}: it \
             was changed after the difference was reviewed. Nothing was written. Run {NAME} \
             again with dry_run true and show the new difference to the user",
            config.version
        )));
    }
    let applied = apply_changes(&config, &changes)?;

    let mut result = Map::new();
    result.insert("productId".into(), json!(product));
    result.insert("appId".into(), json!(app));
    result.insert("version".into(), json!(config.version));
    result.insert("dry_run".into(), json!(dry_run));
    result.insert(
        "changes".into(),
        json!({"parameters": applied.parameter_diff, "filters": applied.filter_diff}),
    );
    let changed = !applied.parameter_diff.is_empty() || !applied.filter_diff.is_empty();
    let mut notes = applied.notes;
    result.insert(
        "request".into(),
        json!({
            "tool": update.tool_name,
            "method": update.method.as_str(),
            "version": config.version,
            "configItems": applied.items.len(),
            "filters": applied.filters.len(),
        }),
    );
    if !changed {
        notes.push("Nothing differs from the current configuration: nothing was written.".into());
        result.insert("applied".into(), json!(false));
        result.insert("notes".into(), json!(notes));
        return Ok(Value::Object(result));
    }
    if dry_run {
        notes.push(format!(
            "Dry run: nothing was written. Show the changes to the user; after they confirm, \
             call {NAME} again with dry_run false and baseVersion {}. The new version takes \
             effect for the users of the app at once.",
            config.version
        ));
        result.insert("applied".into(), json!(false));
        result.insert("notes".into(), json!(notes));
        return Ok(Value::Object(result));
    }
    if !ctx.access.allows(AccessLevel::Destructive) {
        return Err(ToolError::new(format!(
            "writing the Remote Configuration is destructive: {}",
            crate::registry::access_hint(AccessLevel::Destructive, ctx.http)
        )));
    }

    let mut put = headers.clone();
    put["body"] = json!({
        "version": config.version,
        "configItems": applied.items,
        "filters": applied.filters,
    });
    tracing::debug!(
        tool = NAME,
        version = config.version,
        "запись Remote Configuration"
    );
    let written = ctx.api.call(update, &put).await.map_err(|err| {
        if err.result_code() == Some(VERSION_CONFLICT) {
            ToolError::new(format!(
                "the Remote Configuration was changed by someone else after it was read at \
                 version {} (result code {VERSION_CONFLICT}, \"the version has been \
                 modified\"). Nothing was written. Run {NAME} again with dry_run true to see \
                 the difference against the new version",
                config.version
            ))
        } else {
            ToolError::new(format!(
                "the Remote Configuration was not written: {}. Check with get_remote_config \
                 whether a new version appeared before repeating the call",
                describe_api_error_for(&err, ctx.http)
            ))
        }
    })?;
    let new_version = written
        .body
        .json()
        .and_then(|body| body.get("version"))
        .cloned()
        .unwrap_or(Value::Null);
    result.insert("applied".into(), json!(true));
    result.insert("newVersion".into(), new_version);
    notes.push(format!(
        "The new version takes effect for the users of the app when their devices fetch the \
         configuration. To undo, call rollback_remote_config with version {}.",
        config.version
    ));
    result.insert("notes".into(), json!(notes));
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests;
