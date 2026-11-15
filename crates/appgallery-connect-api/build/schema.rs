//! Перевод схем OpenAPI 3.0 снимка в самодостаточную JSON Schema аргументов.
//!
//! `$ref` (`#/components/schemas/<Имя>`) раскрываются на месте: MCP-клиенты не получают
//! `components`, поэтому ссылки в схеме инструмента были бы неразрешимы. Описание рядом
//! с `$ref` относится к конкретному полю и заменяет описание типа. Рекурсивные и слишком
//! глубокие ссылки заменяются объектом без структуры.
//!
//! Конструкции OpenAPI, которых нет в JSON Schema, переводятся: `nullable: true` —
//! в `type: [<тип>, "null"]`, `deprecated: true` — в пометку `Deprecated.` в начале
//! описания (если описание само этого не говорит). Неизвестный ключ схемы — ошибка
//! сборки: новая конструкция должна быть обработана осознанно, а не потеряна молча.

use serde_json::{Map, Value, json};

/// Префикс ссылок на общие модели.
pub const REF_PREFIX: &str = "#/components/schemas/";

/// Максимальная вложенность раскрытия `$ref`.
const MAX_REF_DEPTH: usize = 8;

/// Ключи, которые переносятся как есть.
const PLAIN_KEYS: &[&str] = &[
    "type",
    "format",
    "enum",
    "default",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "required",
];

/// Ключи, которые обрабатываются отдельно.
const SPECIAL_KEYS: &[&str] = &[
    "$ref",
    "description",
    "items",
    "properties",
    "additionalProperties",
    "nullable",
    "deprecated",
    "example",
];

pub struct Resolver<'a> {
    schemas: &'a Map<String, Value>,
    /// Где находится схема (для текстов ошибок).
    context: String,
    /// Цепочка раскрываемых ссылок — защита от рекурсивных схем.
    stack: Vec<String>,
    pub errors: Vec<String>,
}

impl<'a> Resolver<'a> {
    pub fn new(schemas: &'a Map<String, Value>, context: impl Into<String>) -> Self {
        Self {
            schemas,
            context: context.into(),
            stack: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn error(&mut self, text: String) {
        let text = format!("{}: {text}", self.context);
        self.errors.push(text);
    }

    /// Имя модели по ссылке `#/components/schemas/<Имя>`.
    pub fn ref_name(reference: &str) -> Option<&str> {
        reference
            .strip_prefix(REF_PREFIX)
            .filter(|name| !name.is_empty() && !name.contains('/'))
    }

    /// Раскрывает схему в JSON Schema без `$ref`.
    pub fn resolve(&mut self, schema: &Value) -> Value {
        let Some(object) = schema.as_object() else {
            self.error("схема не является объектом".to_owned());
            return json!({});
        };
        for key in object.keys() {
            if !PLAIN_KEYS.contains(&key.as_str()) && !SPECIAL_KEYS.contains(&key.as_str()) {
                self.error(format!("неподдерживаемый ключ схемы {key}"));
            }
        }

        if let Some(reference) = object.get("$ref") {
            if let Some(extra) = object.keys().find(|k| *k != "$ref" && *k != "description") {
                self.error(format!(
                    "рядом с $ref допустим только description, а не {extra}"
                ));
            }
            let local = description_of(object);
            let Some(name) = reference.as_str().and_then(Self::ref_name) else {
                self.error(format!("неподдерживаемая ссылка {reference}"));
                return json!({});
            };
            let mut resolved = self.resolve_ref(name);
            if let (Some(text), Some(target)) = (local, resolved.as_object_mut()) {
                target.insert("description".into(), Value::String(text));
            }
            return resolved;
        }

        let mut out = Map::new();
        for key in PLAIN_KEYS {
            if let Some(value) = object.get(*key) {
                out.insert((*key).into(), value.clone());
            }
        }
        if let Some(required) = object.get("required") {
            let names: Option<Vec<&str>> = required
                .as_array()
                .map(|items| items.iter().filter_map(Value::as_str).collect());
            let properties = object.get("properties").and_then(Value::as_object);
            match (names, properties) {
                (Some(names), Some(properties)) => {
                    for name in names {
                        if !properties.contains_key(name) {
                            self.error(format!("обязательного поля {name} нет в properties"));
                        }
                    }
                }
                _ => self.error("required без properties или не массив строк".to_owned()),
            }
        }
        if object.get("nullable").and_then(Value::as_bool) == Some(true)
            && let Some(kind) = out.get("type").cloned()
        {
            out.insert("type".into(), json!([kind, "null"]));
        }
        let deprecated = object.get("deprecated").and_then(Value::as_bool) == Some(true);
        if let Some(text) = mark_deprecated(description_of(object), deprecated) {
            out.insert("description".into(), Value::String(text));
        }
        if let Some(items) = object.get("items") {
            let items = self.resolve(items);
            out.insert("items".into(), items);
        } else if object.get("type").and_then(Value::as_str) == Some("array") {
            // Часть клиентов отклоняет массив без `items`: разрешаем любые элементы.
            out.insert("items".into(), json!({}));
        }
        if let Some(properties) = object.get("properties") {
            match properties.as_object() {
                Some(properties) => {
                    let mut resolved = Map::new();
                    let mut names: Vec<&String> = properties.keys().collect();
                    names.sort();
                    for name in names {
                        let value = self.resolve(&properties[name]);
                        resolved.insert(name.clone(), value);
                    }
                    out.insert("properties".into(), Value::Object(resolved));
                }
                None => self.error("properties не является объектом".to_owned()),
            }
        }
        if let Some(additional) = object.get("additionalProperties") {
            let value = if additional.is_object() {
                self.resolve(additional)
            } else {
                additional.clone()
            };
            out.insert("additionalProperties".into(), value);
        }
        Value::Object(out)
    }

    /// Раскрывает модель `name` из `components/schemas`.
    pub fn resolve_ref(&mut self, name: &str) -> Value {
        if self.stack.iter().any(|seen| seen == name) || self.stack.len() >= MAX_REF_DEPTH {
            // Рекурсия или слишком глубокая вложенность: структура не раскрывается,
            // агент может передать объект произвольной формы.
            return json!({
                "type": "object",
                "description": format!("`{name}` object (nested structure is not expanded)."),
            });
        }
        let Some(target) = self.schemas.get(name) else {
            self.error(format!("модели {name} нет в components/schemas"));
            return json!({});
        };
        self.stack.push(name.to_owned());
        let resolved = self.resolve(target);
        self.stack.pop();
        resolved
    }
}

/// Описание схемы без лишних пробелов.
pub fn description_of(object: &Map<String, Value>) -> Option<String> {
    object
        .get("description")
        .and_then(Value::as_str)
        .map(normalize_space)
        .filter(|s| !s.is_empty())
}

/// Пробелы и переводы строк схлопываются в один пробел.
pub fn normalize_space(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Пометка устаревшего параметра или поля: справочник обычно пишет это в описании,
/// иначе описание получает `Deprecated.` в начале.
pub fn mark_deprecated(description: Option<String>, deprecated: bool) -> Option<String> {
    if !deprecated {
        return description;
    }
    match description {
        Some(text) if text.to_ascii_lowercase().contains("deprecated") => Some(text),
        Some(text) => Some(format!("Deprecated. {text}")),
        None => Some("Deprecated.".to_owned()),
    }
}
