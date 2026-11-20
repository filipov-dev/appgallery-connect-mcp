//! Разбор изменений и их применение к конфигурации без обращения к API; запросы через
//! сервер и `wiremock` — в `tests/remote_config.rs`.

use serde_json::json;

use super::*;
use crate::composite::reviews_summary::operation;

fn config() -> Config {
    Config::from_response(&json!({
        "ret": {"code": 0, "msg": "OK"},
        "versionInfo": {"version": 7, "updateTime": 1_700_000_000_000_i64},
        "configItems": [
            {
                "name": "welcome_text",
                "desc": "",
                "defaultValue": {"value": "Hello", "useSdkValue": false},
                "filterValues": [{"name": "russian", "value": {"value": "Привет", "useSdkValue": false}}]
            },
            {"name": "new_checkout", "defaultValue": {"value": "false", "useSdkValue": false}}
        ],
        "filters": [
            {"name": "russian", "desc": "", "language": {"values": ["ru"]}, "audiences": []},
            {"name": "beta", "userRatio": [{"operator": "LESS_EQUAL", "value": 1_000_000}]}
        ]
    }))
    .unwrap()
}

fn changes(args: Value) -> Changes {
    parse_changes(&args).unwrap()
}

fn error(args: Value) -> String {
    match parse_changes(&args) {
        Ok(_) => panic!("ожидалась ошибка: {args}"),
        Err(err) => err.message,
    }
}

#[test]
fn version_and_lists_come_from_the_response() {
    let config = config();
    assert_eq!(config.version, 7);
    assert_eq!((config.items.len(), config.filters.len()), (2, 2));
    // Конфигурации ещё нет: версия 0, пустые списки.
    let empty = Config::from_response(&json!({"ret": {"code": 0}})).unwrap();
    assert_eq!((empty.version, empty.items.len()), (0, 0));
    let text = Config::from_response(&json!({"versionInfo": {"version": "12"}})).unwrap();
    assert_eq!(text.version, 12);
    assert!(Config::from_response(&json!({"versionInfo": {"version": "x"}})).is_err());
    assert!(Config::from_response(&json!({"configItems": {}})).is_err());
}

#[test]
fn changes_are_applied_by_name_and_the_rest_is_kept() {
    let config = config();
    let applied = apply_changes(
        &config,
        &changes(json!({"parameters": [
            {"name": "new_checkout", "value": "true", "conditions": {"beta": "true"}},
            {"name": "welcome_text", "conditions": {"russian": null}, "description": "Greeting"},
            {"name": "timeout_ms", "value": "3000"},
        ]})),
    )
    .unwrap();
    assert!(applied.notes.is_empty(), "{:?}", applied.notes);
    assert_eq!(applied.items.len(), 3);
    assert_eq!(
        applied.items[1],
        json!({
            "name": "new_checkout",
            "defaultValue": {"value": "true", "useSdkValue": false},
            "filterValues": [{"name": "beta", "value": {"value": "true", "useSdkValue": false}}]
        })
    );
    assert_eq!(applied.items[0]["desc"], "Greeting");
    assert_eq!(applied.items[0]["filterValues"], json!([]));
    assert_eq!(applied.items[0]["defaultValue"]["value"], "Hello");
    assert_eq!(
        applied.items[2],
        json!({"name": "timeout_ms", "defaultValue": {"value": "3000", "useSdkValue": false}})
    );
    // Условия не менялись и уходят такими, какими их вернул API (с полями вне снимка).
    assert_eq!(applied.filters, config.filters);
    let actions: Vec<(&str, &str)> = applied
        .parameter_diff
        .iter()
        .map(|d| (d["name"].as_str().unwrap(), d["action"].as_str().unwrap()))
        .collect();
    assert_eq!(
        actions,
        [
            ("new_checkout", "update"),
            ("welcome_text", "update"),
            ("timeout_ms", "add")
        ]
    );
    assert_eq!(applied.parameter_diff[0]["before"], config.items[1]);
    assert!(applied.parameter_diff[2]["before"].is_null());
}

#[test]
fn filters_are_replaced_added_and_deleted() {
    let config = config();
    let applied = apply_changes(
        &config,
        &changes(json!({
            "filters": [
                {"name": "beta", "userRatio": [{"operator": "LESS_EQUAL", "value": 5_000_000}]},
                {"name": "germany", "country": {"values": ["DE"]}},
            ],
            "parameters": [{"name": "welcome_text", "conditions": {"russian": null, "germany": "Hallo"}}],
            "removeFilters": ["russian"],
        })),
    )
    .unwrap();
    let names: Vec<&str> = applied.filters.iter().filter_map(name_of).collect();
    assert_eq!(names, ["beta", "germany"]);
    assert_eq!(applied.filters[0]["userRatio"][0]["value"], 5_000_000);
    let actions: Vec<(&str, &str)> = applied
        .filter_diff
        .iter()
        .map(|d| (d["name"].as_str().unwrap(), d["action"].as_str().unwrap()))
        .collect();
    assert_eq!(
        actions,
        [
            ("beta", "update"),
            ("germany", "add"),
            ("russian", "delete")
        ]
    );
    assert_eq!(
        applied.items[0]["filterValues"],
        json!([{"name": "germany", "value": {"value": "Hallo", "useSdkValue": false}}])
    );
}

#[test]
fn unchanged_values_are_noted_and_use_sdk_value_is_kept_explicit() {
    let config = config();
    let applied = apply_changes(
        &config,
        &changes(json!({"parameters": [
            {"name": "welcome_text", "value": "Hello"},
            {"name": "new_checkout", "useSdkValue": true, "conditions": {"russian": {"useSdkValue": true}}},
        ]})),
    )
    .unwrap();
    assert_eq!(applied.parameter_diff.len(), 1);
    assert!(applied.notes[0].contains("`welcome_text` is already as given"));
    assert_eq!(
        applied.items[1]["defaultValue"],
        json!({"value": "false", "useSdkValue": true})
    );
    assert_eq!(
        applied.items[1]["filterValues"],
        json!([{"name": "russian", "value": {"useSdkValue": true}}])
    );
}

#[test]
fn inconsistent_changes_are_refused_before_writing() {
    let config = config();
    let refused = |args: Value| match apply_changes(&config, &changes(args)) {
        Ok(_) => panic!("ожидалась ошибка"),
        Err(err) => err.message,
    };
    // Условие, которое ещё используется, неизвестное условие, неизвестный параметр.
    let text = refused(json!({"removeFilters": ["russian"]}));
    assert!(
        text.contains("`welcome_text`") && text.contains("null"),
        "{text}"
    );
    let text =
        refused(json!({"parameters": [{"name": "welcome_text", "conditions": {"nope": "x"}}]}));
    assert!(text.contains("condition `nope`"), "{text}");
    let text = refused(json!({"removeFilters": ["nope"]}));
    assert!(text.contains("`nope`"), "{text}");
    let text = refused(json!({"parameters": [{"name": "missing", "delete": true}]}));
    assert!(text.contains("not in the configuration"), "{text}");
    let text = refused(json!({"parameters": [{"name": "missing", "description": "x"}]}));
    assert!(text.contains("needs `value`"), "{text}");
    // Удаление параметра вместе с условием.
    let applied = apply_changes(
        &config,
        &changes(json!({"parameters": [{"name": "welcome_text", "delete": true}], "removeFilters": ["russian"]})),
    )
    .unwrap();
    assert_eq!(applied.items.len(), 1);
    assert_eq!(applied.parameter_diff[0]["action"], "delete");
}

#[test]
fn arguments_are_checked() {
    assert!(error(json!({})).contains("nothing to change"));
    assert!(
        error(json!({"parameters": [{"name": "a", "value": "1"}, {"name": "a", "value": "2"}]}))
            .contains("more than once")
    );
    assert!(
        error(json!({"parameters": [{"name": "a", "delete": true, "value": "1"}]}))
            .contains("no other changes")
    );
    assert!(
        error(json!({"parameters": [{"name": "a", "conditions": {"b": 1}}]}))
            .contains("conditions.b")
    );
    assert!(error(json!({"filters": [{"desc": "x"}]})).contains("filters[0].name"));
    assert!(error(json!({"filters": [{"name": "a"}], "removeFilters": ["a"]})).contains("both in"));
}

#[test]
fn masked_values_are_refused() {
    // Значение, прочитанное с маской, и маска в условии.
    let text = error(json!({"parameters": [{"name": "api_key", "value": "***"}]}));
    assert!(
        text.contains("`parameters[0].value` contains `***`"),
        "{text}"
    );
    let text = error(json!({"filters": [{"name": "a", "language": {"values": ["Bearer ***"]}}]}));
    assert!(text.contains("filters[0].language.values[0]"), "{text}");
    let text = error(json!({"parameters": [{"name": "a", "conditions": {"***": "x"}}]}));
    assert!(text.contains("parameters[0].conditions.***"), "{text}");

    // Тело update_remote_config; у других операций правило не действует.
    let update = operation(CONFIG_UPDATE).unwrap();
    let body =
        json!({"version": 3, "configItems": [{"name": "k", "defaultValue": {"value": "x***y"}}]});
    let err = guard_masked_body(update, Some(&body)).unwrap_err();
    assert!(
        err.message
            .contains("`body.configItems[0].defaultValue.value`"),
        "{}",
        err.message
    );
    assert!(err.message.contains(NAME), "{}", err.message);
    assert!(guard_masked_body(update, Some(&json!({"version": 3}))).is_ok());
    assert!(guard_masked_body(update, None).is_ok());
    let other = operation("pms.product.update").unwrap();
    assert!(guard_masked_body(other, Some(&body)).is_ok());
}
