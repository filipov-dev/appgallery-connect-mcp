//! Приведение ответа операции к виду для агента до вырезания секретов.
//!
//! HMS IAP отдаёт данные покупки JSON-строкой внутри JSON (`purchaseTokenData`,
//! `inappPurchaseData`, `cancelledPurchaseList`): агенту пришлось бы разбирать её сам.
//! Пути таких полей — [`OperationDef::json_strings`] (расширение `x-agc-json` снимка):
//! сервер заменяет строку разобранным значением. Строку, которая не разбирается как JSON,
//! он оставляет как есть и говорит об этом в пометке.
//!
//! Поля [`OperationDef::omitted`] (`[[omit]]` дополнения) сервер убирает: подпись данных
//! покупки (`dataSignature`) покрывает исходную строку, после разбора её не проверить, а
//! место в ответе она занимает.
//!
//! Разбор идёт до вырезания секретов ([`crate::redact`]): так правила вырезания видят и
//! содержимое разобранных строк.

use appgallery_connect_api::spec::OperationDef;
use serde_json::Value;

/// Что сервер изменил в ответе.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResponseChanges {
    /// Поля-строки, которые не разобрались как JSON (путь).
    pub unparsed: Vec<&'static str>,
    /// Убранные поля: путь и причина; по одному разу на путь.
    pub removed: Vec<(&'static str, &'static str)>,
}

impl ResponseChanges {
    /// Пометка для агента; `None` — сказать нечего (разобранные строки описаны в описании
    /// инструмента и пометки не требуют).
    pub fn note(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.unparsed.is_empty() {
            parts.push(format!(
                "These fields should hold JSON strings but could not be parsed, so they are \
                 returned as text: {}.",
                quote(&self.unparsed)
            ));
        }
        let mut reasons: Vec<&str> = self.removed.iter().map(|(_, reason)| *reason).collect();
        reasons.sort_unstable();
        reasons.dedup();
        for reason in reasons {
            let fields: Vec<&str> = self
                .removed
                .iter()
                .filter(|(_, why)| *why == reason)
                .map(|(path, _)| *path)
                .collect();
            parts.push(format!("Removed {}: {reason}", quote(&fields)));
        }
        (!parts.is_empty()).then(|| format!("[{}]", parts.join(" ")))
    }
}

/// Разбирает JSON-строки и убирает поля ответа операции `op` по таблице спецификации.
pub fn reshape_operation_json(op: &'static OperationDef, value: &mut Value) -> ResponseChanges {
    let mut changes = ResponseChanges::default();
    for path in op.json_strings {
        let segments = segments(path);
        let mut failed = false;
        visit(value, &segments, &mut |parent, name| {
            let Some(Value::String(text)) = parent.get(name) else {
                return;
            };
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) => {
                    parent.insert(name.to_owned(), parsed);
                }
                Err(_) => failed = true,
            }
        });
        if failed {
            changes.unparsed.push(path);
        }
    }
    for field in op.omitted {
        let segments = segments(field.path);
        let mut removed = false;
        visit(value, &segments, &mut |parent, name| {
            removed |= parent.remove(name).is_some();
        });
        if removed {
            changes.removed.push((field.path, field.reason));
        }
    }
    changes
}

/// Объединяет пометки: сначала об изменениях ответа, затем о вырезанных секретах.
pub fn join_notes(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first} {second}")),
        (first, second) => first.or(second),
    }
}

/// Сегмент пути: имя поля и признак «каждый элемент массива».
fn segments(path: &str) -> Vec<(&str, bool)> {
    path.split('.')
        .map(|segment| match segment.strip_suffix("[]") {
            Some(name) => (name, true),
            None => (segment, false),
        })
        .collect()
}

/// Вызывает `action(родитель, имя)` для каждого поля по пути; отсутствующие части пути
/// пропускаются.
fn visit(
    value: &mut Value,
    segments: &[(&str, bool)],
    action: &mut dyn FnMut(&mut serde_json::Map<String, Value>, &str),
) {
    let Some(((name, array), rest)) = segments.split_first() else {
        return;
    };
    let Value::Object(object) = value else {
        return;
    };
    if rest.is_empty() && !array {
        action(object, name);
        return;
    }
    let Some(child) = object.get_mut(*name) else {
        return;
    };
    if *array {
        if let Value::Array(items) = child {
            for item in items {
                if rest.is_empty() {
                    // Путь `a[]` указывает на элементы массива, а не на поле: менять нечего.
                    continue;
                }
                visit(item, rest, action);
            }
        }
    } else {
        visit(child, rest, action);
    }
}

fn quote(paths: &[&str]) -> String {
    paths
        .iter()
        .map(|path| format!("`{path}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use appgallery_connect_api::spec;
    use serde_json::json;

    fn op(name: &str) -> &'static OperationDef {
        spec::find(name).unwrap_or_else(|| panic!("нет операции {name}"))
    }

    #[test]
    fn parses_purchase_data_and_drops_the_signature() {
        let mut value = json!({
            "responseCode": "0",
            "purchaseTokenData": "{\"productId\":\"prd1\",\"purchaseState\":0,\"developerPayload\":\"p\"}",
            "dataSignature": "c2lnbmF0dXJl",
            "signatureAlgorithm": "SHA256WithRSA",
        });
        let changes = reshape_operation_json(op("verify_product_purchase"), &mut value);
        assert_eq!(
            value,
            json!({
                "responseCode": "0",
                "purchaseTokenData": {"productId": "prd1", "purchaseState": 0, "developerPayload": "p"},
            })
        );
        let note = changes.note().unwrap_or_default();
        assert!(
            note.contains("`dataSignature`, `signatureAlgorithm`"),
            "{note}"
        );
        assert!(note.starts_with('[') && note.ends_with(']'), "{note}");
    }

    #[test]
    fn parses_an_array_string_and_keeps_broken_json_as_text() {
        let mut value = json!({
            "responseCode": "0",
            "cancelledPurchaseList": "[{\"orderId\":\"o1\",\"cancelledReason\":1}]",
            "continuationToken": "next",
        });
        let changes = reshape_operation_json(op("list_cancelled_purchases"), &mut value);
        assert_eq!(value["cancelledPurchaseList"][0]["orderId"], "o1");
        assert_eq!(value["continuationToken"], "next");
        assert_eq!(changes.note(), None);

        let mut value = json!({"cancelledPurchaseList": "not json"});
        let changes = reshape_operation_json(op("list_cancelled_purchases"), &mut value);
        assert_eq!(value["cancelledPurchaseList"], "not json");
        assert!(
            changes
                .note()
                .is_some_and(|note| note.contains("`cancelledPurchaseList`"))
        );
    }

    #[test]
    fn other_operations_and_missing_fields_stay_as_they_are() {
        let mut value = json!({"ret": {"code": 0}, "dataSignature": "x"});
        let before = value.clone();
        let changes = reshape_operation_json(op("get_app_info"), &mut value);
        assert_eq!(value, before);
        assert_eq!(changes, ResponseChanges::default());

        let mut value = json!({"responseCode": "5", "responseMessage": "not enabled"});
        let changes = reshape_operation_json(op("get_subscription_purchase"), &mut value);
        assert_eq!(value["responseCode"], "5");
        assert_eq!(changes.note(), None);
    }

    #[test]
    fn notes_are_joined() {
        assert_eq!(join_notes(None, None), None);
        assert_eq!(join_notes(Some("[a]".into()), None).as_deref(), Some("[a]"));
        assert_eq!(join_notes(None, Some("[b]".into())).as_deref(), Some("[b]"));
        assert_eq!(
            join_notes(Some("[a]".into()), Some("[b]".into())).as_deref(),
            Some("[a] [b]")
        );
    }
}
