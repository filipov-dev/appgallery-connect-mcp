//! Инварианты таблиц операций, сгенерированных `build.rs`, и снапшоты характерных
//! операций каждой группы.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use appgallery_connect_api::executor::is_forbidden_request_header;
use appgallery_connect_api::spec::{
    AccessLevel, BodyKind, ExcludedOperationDef, ExclusionReason, OperationDef, ParamLocation,
    ParamType, ResponseKind, SecretKind, excluded_operations, find, find_excluded, find_group,
    find_toolset, groups, operations, secret_field_names, toolsets,
};
use serde_json::{Value, json};

const TOOLSETS: [&str; 8] = [
    "apps",
    "releases",
    "listing",
    "reviews",
    "reports",
    "products",
    "projects",
    "provisioning",
];

const GROUPS: [&str; 8] = [
    "auth",
    "publishing",
    "upload",
    "reports",
    "comments",
    "pms",
    "project",
    "provisioning",
];

/// Операции снимка по `operationId`: (метод, путь, операция).
fn spec_operations() -> &'static BTreeMap<String, (String, String, Value)> {
    static OPERATIONS: OnceLock<BTreeMap<String, (String, String, Value)>> = OnceLock::new();
    OPERATIONS.get_or_init(|| {
        let spec: Value =
            serde_json::from_str(include_str!("../spec/openapi.json")).expect("снимок — JSON");
        let mut out = BTreeMap::new();
        for (path, item) in spec["paths"].as_object().expect("paths") {
            for (method, op) in item.as_object().expect("path item") {
                out.insert(
                    op["operationId"].as_str().expect("operationId").to_owned(),
                    (method.to_ascii_uppercase(), path.clone(), op.clone()),
                );
            }
        }
        out
    })
}

fn schema(op: &OperationDef) -> Value {
    serde_json::from_str(op.input_schema)
        .unwrap_or_else(|err| panic!("{}: input_schema не JSON: {err}", op.tool_name))
}

fn op(name: &str) -> &'static OperationDef {
    find(name).unwrap_or_else(|| panic!("нет операции {name}"))
}

fn by_id(id: &str) -> &'static OperationDef {
    operations()
        .iter()
        .find(|op| op.operation_id == id)
        .unwrap_or_else(|| panic!("операция {id} не зарегистрирована"))
}

fn default_access(method: &str) -> AccessLevel {
    match method {
        "GET" => AccessLevel::Read,
        "DELETE" => AccessLevel::Destructive,
        _ => AccessLevel::Write,
    }
}

#[test]
fn totals() {
    assert_eq!(spec_operations().len(), 61, "операций в paths снимка");
    assert_eq!(operations().len(), 56, "инструментов");
    // 7 исключённых в дополнении, из них 2 внешние (без пути) в таблицу не попадают.
    assert_eq!(excluded_operations().len(), 5, "исключённых");
    assert_eq!(
        operations().len() + excluded_operations().len(),
        spec_operations().len()
    );

    let mut by_access = BTreeMap::new();
    let mut by_toolset = BTreeMap::new();
    for op in operations() {
        *by_access.entry(op.access.as_str()).or_insert(0) += 1;
        *by_toolset.entry(op.toolset).or_insert(0) += 1;
    }
    assert_eq!(
        by_access,
        BTreeMap::from([("read", 28), ("write", 11), ("destructive", 17)])
    );
    assert_eq!(
        by_toolset,
        BTreeMap::from([
            ("apps", 2),
            ("releases", 9),
            ("listing", 3),
            ("reviews", 4),
            ("reports", 5),
            ("products", 15),
            ("projects", 8),
            ("provisioning", 10),
        ])
    );
}

#[test]
fn groups_and_toolsets_match_overlay() {
    let slugs: Vec<&str> = groups().iter().map(|g| g.slug).collect();
    assert_eq!(slugs, GROUPS);
    for group in groups() {
        assert!(!group.title.is_empty(), "{}", group.slug);
        assert!(group.reference.starts_with("https://developer.huawei.com/"));
    }
    assert_eq!(find_group("auth").map(|g| g.title), Some("Authentication"));

    let slugs: Vec<&str> = toolsets().iter().map(|t| t.slug).collect();
    assert_eq!(slugs, TOOLSETS);
    let defaults: Vec<&str> = toolsets()
        .iter()
        .filter(|t| t.default_enabled)
        .map(|t| t.slug)
        .collect();
    assert_eq!(defaults, ["apps", "releases", "reviews"]);
    assert!(!find_toolset("provisioning").expect("набор").default_enabled);
    for toolset in toolsets() {
        assert!(!toolset.description.is_empty(), "{}", toolset.slug);
        assert!(
            operations().iter().any(|op| op.toolset == toolset.slug),
            "{}: нет инструментов",
            toolset.slug
        );
    }
}

#[test]
fn every_spec_operation_is_kept_or_excluded() {
    let spec = spec_operations();
    let kept: BTreeSet<&str> = operations().iter().map(|op| op.operation_id).collect();
    let excluded: BTreeSet<&str> = excluded_operations()
        .iter()
        .map(|op| op.operation_id)
        .collect();
    assert!(kept.is_disjoint(&excluded));
    let all: BTreeSet<&str> = kept.union(&excluded).copied().collect();
    let in_spec: BTreeSet<&str> = spec.keys().map(String::as_str).collect();
    assert_eq!(all, in_spec);

    for op in operations() {
        let (method, path, raw) = &spec[op.operation_id];
        assert_eq!(op.method.as_str(), method, "{}", op.tool_name);
        assert_eq!(op.path, path, "{}", op.tool_name);
        assert!(op.path.starts_with("/api/"), "{}", op.tool_name);
        assert_eq!(op.summary, raw["summary"], "{}", op.tool_name);
        assert_eq!(op.reference, raw["x-agc-reference"], "{}", op.tool_name);
        assert_eq!(op.group, raw["x-agc-group"], "{}", op.tool_name);
        assert!(find_group(op.group).is_some(), "{}", op.tool_name);
        assert!(find_toolset(op.toolset).is_some(), "{}", op.tool_name);
        assert_eq!(op.response, ResponseKind::Json, "{}", op.tool_name);
        let result = op.result.expect("конверт результата");
        assert_eq!(
            result.field, raw["x-agc-result"]["field"],
            "{}",
            op.tool_name
        );
        assert_eq!(result.code_field, raw["x-agc-result"]["codeField"]);
        assert_eq!(result.message_field, raw["x-agc-result"]["messageField"]);
        // Описание — остаток описания снимка после summary: без повтора summary.
        assert!(!op.description.starts_with(op.summary), "{}", op.tool_name);
    }
    for ex in excluded_operations() {
        let (method, path, raw) = &spec[ex.operation_id];
        assert_eq!(ex.method.as_str(), method);
        assert_eq!(ex.path, path);
        assert_eq!(ex.summary, raw["summary"]);
        assert_eq!(ex.reason, ExclusionReason::Overlay);
        assert!(!ex.note.is_empty(), "{}", ex.operation_id);
        assert!(!ex.alternative.is_empty(), "{}", ex.operation_id);
        // Пустой набор — только у операций, которые нельзя вызвать.
        assert!(
            !ex.callable || !ex.toolset.is_empty(),
            "{}: набор {:?}, callable {}",
            ex.operation_id,
            ex.toolset,
            ex.callable
        );
        assert_eq!(
            find_excluded(ex.operation_id).map(|e| e.operation_id),
            Some(ex.operation_id)
        );
    }
    // Внешние операции (отправка файла на адрес из ответа) в таблицах не появляются.
    for id in ["upload.file.put", "upload.chunk.put"] {
        assert!(find_excluded(id).is_none());
        assert!(operations().iter().all(|op| op.operation_id != id));
    }
    assert!(find_excluded("publishing.appInfo.get").is_none());
}

#[test]
fn token_operation_is_not_callable() {
    let token = find_excluded("auth.token.create").expect("получение токена");
    assert!(!token.callable);
    assert_eq!(token.toolset, "");
    assert_eq!(token.group, "auth");
    assert_eq!(token.path, "/api/oauth2/v1/token");
    // Шаги загрузки выдают одноразовые адреса и заголовки загрузки: их нельзя вызвать и
    // через `api_request`, заменой служит составной инструмент загрузки.
    let upload: Vec<_> = excluded_operations()
        .iter()
        .filter(|ex| ex.group == "upload")
        .collect();
    assert_eq!(upload.len(), 4);
    for ex in upload {
        assert!(!ex.callable, "{}", ex.operation_id);
        assert_eq!(ex.toolset, "releases", "{}", ex.operation_id);
        assert!(ex.alternative.contains("package_upload"));
        assert!(ex.alternative.contains("image_sync"));
    }
    // GET адреса загрузки выдаёт право записи: уровень из [[access]].
    assert_eq!(
        find_excluded("upload.uploadUrl.get").map(|ex| ex.access),
        Some(AccessLevel::Write)
    );
}

#[test]
fn tool_names_are_unique_sorted_and_valid() {
    let names: Vec<&str> = operations().iter().map(|op| op.tool_name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "OPERATIONS отсортированы по имени");
    let unique: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(unique.len(), names.len(), "имена уникальны");
    for name in names {
        assert!(name.len() <= 40, "{name}: длиннее 40 символов");
        assert!(
            name.starts_with(|c: char| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{name}"
        );
        assert_eq!(find(name).map(|op| op.tool_name), Some(name));
    }
    assert!(find("missing_tool").is_none());
    let ids: Vec<&str> = excluded_operations()
        .iter()
        .map(|op| op.operation_id)
        .collect();
    let mut sorted_ids = ids.clone();
    sorted_ids.sort_unstable();
    assert_eq!(ids, sorted_ids, "EXCLUDED отсортированы по operationId");
}

#[test]
fn access_levels() {
    for (id, level) in [
        ("publishing.appInfo.get", AccessLevel::Read),
        ("publishing.appInfo.update", AccessLevel::Write),
        ("publishing.languageInfo.delete", AccessLevel::Destructive),
        ("publishing.app.submit", AccessLevel::Destructive),
        ("publishing.app.submitWithFile", AccessLevel::Destructive),
        ("publishing.phasedRelease.update", AccessLevel::Destructive),
        ("publishing.releaseTime.update", AccessLevel::Destructive),
        ("publishing.fileInfo.update", AccessLevel::Write),
        ("comments.review.reply", AccessLevel::Destructive),
        ("comments.review.list", AccessLevel::Read),
        ("pms.product.search", AccessLevel::Read),
        ("pms.product.update", AccessLevel::Destructive),
        ("pms.productGroup.update", AccessLevel::Write),
        ("pms.promotion.batchDeactivate", AccessLevel::Destructive),
        ("project.certFingerprint.add", AccessLevel::Write),
        ("provisioning.cert.list", AccessLevel::Read),
        ("provisioning.cert.delete", AccessLevel::Destructive),
        ("provisioning.profile.delete", AccessLevel::Destructive),
        ("provisioning.profile.create", AccessLevel::Write),
    ] {
        assert_eq!(by_id(id).access, level, "{id}");
    }
    // Исключения из правила по HTTP-методу объясняются агенту в описании.
    for op in operations() {
        let marker = match op.access {
            AccessLevel::Read => "Read-only: ",
            AccessLevel::Write => "Write: ",
            AccessLevel::Destructive => "Destructive: ",
        };
        assert_eq!(
            op.access != default_access(op.method.as_str()),
            op.description.contains(marker),
            "{}: причина уровня в описании",
            op.tool_name
        );
    }
    assert!(
        op("reply_review")
            .description
            .ends_with("Destructive: publishes a reply that all AppGallery users see.")
    );
}

fn json_contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|v| json_contains_key(v, key))
        }
        Value::Array(items) => items.iter().any(|v| json_contains_key(v, key)),
        _ => false,
    }
}

#[test]
fn input_schemas_are_strict_objects_with_all_params() {
    for op in operations() {
        let schema = schema(op);
        assert_eq!(schema["type"], "object", "{}", op.tool_name);
        assert_eq!(schema["additionalProperties"], false, "{}", op.tool_name);
        let properties = schema["properties"].as_object().expect("properties");
        let names: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
        let params: BTreeSet<&str> = op.params.iter().map(|p| p.name).collect();
        assert_eq!(names, params, "{}", op.tool_name);
        assert_eq!(
            params.len(),
            op.params.len(),
            "{}: повтор аргумента",
            op.tool_name
        );
        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        for param in op.params {
            let label = format!("{}.{}", op.tool_name, param.name);
            assert_eq!(required.contains(param.name), param.required, "{label}");
            assert_eq!(param.name, param.api_name, "{label}");
            if param.location == ParamLocation::Path {
                assert!(param.required, "{label}");
                assert!(
                    op.path.contains(&format!("{{{}}}", param.api_name)),
                    "{label}"
                );
            }
            if param.location == ParamLocation::Header {
                assert!(!is_forbidden_request_header(param.api_name), "{label}");
            }
            assert_eq!(
                param.repeated,
                param.value_type == ParamType::Array,
                "{label}"
            );
            if param.repeated {
                assert_eq!(param.location, ParamLocation::Query, "{label}");
            }
            let property = &properties[param.name];
            assert!(
                property["description"]
                    .as_str()
                    .is_some_and(|d| !d.is_empty()),
                "{label}"
            );
            assert_eq!(property["description"], param.description, "{label}");
            assert!(!json_contains_key(property, "$ref"), "{label}");
            assert!(!json_contains_key(property, "nullable"), "{label}");
        }
        let body: Vec<_> = op.params_in(ParamLocation::Body).collect();
        assert_eq!(
            body.len(),
            usize::from(op.body.is_some()),
            "{}",
            op.tool_name
        );
        if let Some(body) = body.first() {
            assert_eq!(body.name, "body");
            assert_eq!(body.value_type, ParamType::Object);
            assert_eq!(op.body, Some(BodyKind::Json));
            assert_eq!(properties["body"]["type"], "object");
        }
    }
    // Описание рядом с `$ref` сохраняется вместо описания модели.
    let update = schema(op("update_product"));
    let resource = &update["properties"]["body"]["properties"]["resource"];
    assert!(resource["properties"].is_object(), "модель раскрыта");
    let raw = &spec_operations()["pms.product.update"].2;
    assert_eq!(
        resource["description"],
        raw.pointer(
            "/requestBody/content/application~1json/schema/properties/resource/description"
        )
        .cloned()
        .expect("описание поля")
    );
    // Тело отправки на проверку необязательно.
    assert!(!op("submit_app").param("body").expect("body").required);
    assert!(op("update_app_info").param("body").expect("body").required);
}

#[test]
fn headers_arrays_and_deprecated_params() {
    // `appId` у PMS API — заголовок.
    for op in operations().iter().filter(|op| op.group == "pms") {
        let app_id = op.param("appId").expect("appId");
        assert_eq!(app_id.location, ParamLocation::Header, "{}", op.tool_name);
        assert!(app_id.required, "{}", op.tool_name);
    }
    let brief = op("list_app_brief_info").param("userID").expect("userID");
    assert_eq!(brief.location, ParamLocation::Header);
    assert_eq!(brief.value_type, ParamType::Integer);
    assert_eq!(
        op("reply_review").param("requestId").map(|p| p.location),
        Some(ParamLocation::Header)
    );

    // Массивы запроса — только у Reports API.
    let repeated: BTreeSet<(&str, &str)> = operations()
        .iter()
        .flat_map(|op| {
            op.params
                .iter()
                .filter(|p| p.repeated)
                .map(move |p| (op.tool_name, p.name))
        })
        .collect();
    assert_eq!(repeated.len(), 11, "{repeated:?}");
    assert!(
        repeated
            .iter()
            .all(|(tool, _)| tool.ends_with("_report_url"))
    );

    // Устаревший параметр остаётся аргументом, пометка — в описании.
    let audit = op("list_reviews")
        .param("auditStates")
        .expect("auditStates");
    assert!(audit.description.starts_with("Deprecated."));
}

#[test]
fn secret_fields_of_responses() {
    let kinds = |id: &str| -> Vec<(&str, SecretKind)> {
        by_id(id).secrets.iter().map(|s| (s.path, s.kind)).collect()
    };
    assert_eq!(
        kinds("project.appExtraInfo.get"),
        [("secret", SecretKind::Credential)]
    );
    assert_eq!(
        kinds("project.configFile.get"),
        [("content", SecretKind::FileContent)]
    );
    for id in [
        "reports.downloadInstall.export",
        "reports.iap.export",
        "reports.paidApp.export",
        "reports.paidAppDetails.export",
        "reports.installFailure.export",
    ] {
        assert_eq!(kinds(id), [("fileURL", SecretKind::DownloadUrl)], "{id}");
    }
    assert_eq!(
        kinds("provisioning.cert.list"),
        [
            ("certInfo[].certDownloadUrl", SecretKind::DownloadUrl),
            ("certList[].certDownloadUrl", SecretKind::DownloadUrl),
        ]
    );
    assert_eq!(
        kinds("provisioning.profile.create"),
        [(
            "provisionInfo.provisionDownloadUrl",
            SecretKind::DownloadUrl
        )]
    );
    // `save_to` — только там, где в ответе ссылка или содержимое файла.
    let savable: BTreeSet<&str> = operations()
        .iter()
        .filter(|op| op.saves_file())
        .map(|op| op.operation_id)
        .collect();
    assert_eq!(savable.len(), 11, "{savable:?}");
    assert!(!savable.contains("project.appExtraInfo.get"));
    assert!(savable.contains("project.configFile.get"));
    assert_eq!(
        secret_field_names(),
        [
            "certDownloadUrl",
            "fileURL",
            "provisionDownloadUrl",
            "secret"
        ]
    );
    for op in operations() {
        for secret in op.secrets {
            assert!(!secret.description.is_empty(), "{}", op.tool_name);
            assert!(secret.path.ends_with(secret.name()), "{}", op.tool_name);
        }
    }
}

/// Представление операции для снапшота: все поля плюс разобранная JSON Schema.
fn snapshot_view(op: &OperationDef) -> Value {
    let params: Vec<Value> = op
        .params
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "api_name": p.api_name,
                "location": format!("{:?}", p.location),
                "required": p.required,
                "value_type": format!("{:?}", p.value_type),
                "repeated": p.repeated,
                "description": p.description,
            })
        })
        .collect();
    let mut view = json!({
        "tool_name": op.tool_name,
        "operation_id": op.operation_id,
        "group": op.group,
        "toolset": op.toolset,
        "summary": op.summary,
        "description": op.description,
        "method": op.method.as_str(),
        "path": op.path,
        "access": op.access.as_str(),
        "params": params,
        "body": op.body.map(|kind| format!("{kind:?}")),
        "response": format!("{:?}", op.response),
        "result": op.result.map(|r| json!({
            "field": r.field,
            "code_field": r.code_field,
            "message_field": r.message_field,
        })),
        "reference": op.reference,
        "input_schema": schema(op),
    });
    // Секретные поля — только у операций, где они есть: остальные снапшоты не меняются.
    if !op.secrets.is_empty() {
        view["secrets"] = op
            .secrets
            .iter()
            .map(|s| {
                json!({
                    "path": s.path,
                    "kind": format!("{:?}", s.kind),
                    "description": s.description,
                    "hint": s.hint,
                })
            })
            .collect();
    }
    view
}

fn excluded_view(ex: &ExcludedOperationDef) -> Value {
    json!({
        "operation_id": ex.operation_id,
        "group": ex.group,
        "toolset": ex.toolset,
        "summary": ex.summary,
        "method": ex.method.as_str(),
        "path": ex.path,
        "access": ex.access.as_str(),
        "reason": ex.reason.as_str(),
        "callable": ex.callable,
        "note": ex.note,
        "alternative": ex.alternative,
    })
}

macro_rules! snapshot {
    ($name:literal) => {
        insta::assert_json_snapshot!($name, snapshot_view(op($name)))
    };
}

#[test]
fn snapshot_get_app_info() {
    // Publishing API: GET с параметрами запроса, конверт `ret.code`.
    snapshot!("get_app_info");
}

#[test]
fn snapshot_submit_app() {
    // Publishing API: необязательное тело, уровень удаления с причиной.
    snapshot!("submit_app");
}

#[test]
fn snapshot_get_download_report_url() {
    // Reports API: параметр пути и массивы запроса повтором имени.
    snapshot!("get_download_report_url");
}

#[test]
fn snapshot_list_reviews() {
    // Comments API: конверт `ret.rtnCode`, устаревший параметр.
    snapshot!("list_reviews");
}

#[test]
fn snapshot_reply_review() {
    // Comments API: заголовок `requestId` и тело.
    snapshot!("reply_review");
}

#[test]
fn snapshot_update_product() {
    // PMS API: заголовок `appId`, конверт `error.errorCode`, тело с раскрытыми `$ref`.
    snapshot!("update_product");
}

#[test]
fn snapshot_list_app_brief_info() {
    // Project Management API: целочисленный заголовок `userID`.
    snapshot!("list_app_brief_info");
}

#[test]
fn snapshot_list_profiles() {
    // Provisioning API: `appId` и `provisionId` заголовками у GET.
    snapshot!("list_profiles");
}

#[test]
fn snapshot_excluded_operations() {
    // Получение токена (группа `auth`) и шаги загрузки (группа `upload`).
    let excluded: Vec<Value> = excluded_operations().iter().map(excluded_view).collect();
    insta::assert_json_snapshot!("excluded_operations", excluded);
}
