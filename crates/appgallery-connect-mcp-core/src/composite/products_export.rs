//! `products_export`: товары, группы подписок и акции приложения в новый JSON-файл (формат
//! — [`super::products`]).
//!
//! Путь `save_to` проверяется до запросов ([`super::LocalFiles::resolve_write`]), файл
//! создаётся только новым. Ответ агенту — путь, размер и счётчики: названия, цены и тексты
//! акций остаются в файле.

use std::collections::BTreeSet;

use appgallery_connect_api::AccessLevel;
use serde_json::{Value, json};

use super::listing::{app_id_schema, bool_arg, required_str};
use super::products::{self, Scope, TOOLSET, country_arg, country_schema};
use super::{CompositeTool, Context, ToolFuture};
use crate::error::ToolError;
use crate::files::{display_path, write_new_file};

/// Имя инструмента.
pub const NAME: &str = "products_export";

const DESCRIPTION: &str = "Export the in-app products of an app to a new local JSON file \
(`save_to`): subscription groups, products (names and descriptions per language, the default \
price and the prices per country, status, the group and period of subscriptions) and the \
promotions of subscriptions (free trials and introductory prices with their periods, prices and \
texts). Prices are amounts in minor units of the currency (cents), as in the PMS API. Edit the \
file and apply it with products_apply, which compares it with AppGallery Connect and shows the \
differences first. The result has only the path, the size and counts. Deleted products and \
promotions are not exported, finished promotions only with include_finished_promotions. Local \
paths are allowed only inside the server's allowed directories; existing files are never \
overwritten.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: TOOLSET,
        access: AccessLevel::Read,
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
            "appId": app_id_schema(),
            "save_to": {
                "type": "string",
                "minLength": 1,
                "description": "Path of a new local JSON file (absolute, or relative to the \
                    server's working directory; only inside the allowed directories). The \
                    parent directory must exist; existing files are never overwritten."
            },
            "country": country_schema(),
            "include_finished_promotions": {
                "type": "boolean",
                "description": "Also export promotions that have ended. Default: false."
            }
        },
        "required": ["save_to"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(export(ctx, args))
}

async fn export(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let app_id = super::listing::app_id(ctx, &args)?;
    let save_to = required_str(&args, "save_to")?;
    let country = country_arg(&args)?;
    let include_finished = bool_arg(&args, "include_finished_promotions")?.unwrap_or(false);
    // Путь проверяется до запросов: ошибка в пути не должна стоить обхода товаров.
    let target = ctx.files.resolve_write(save_to)?;

    let remote = products::fetch_remote(
        ctx,
        &app_id,
        &country,
        Scope {
            include_finished,
            ..Scope::default()
        },
    )
    .await?;
    let file = products::file_from_remote(&app_id, &remote);
    let mut text = serde_json::to_string_pretty(&file)
        .map_err(|err| ToolError::new(format!("cannot write the products file: {err}")))?;
    text.push('\n');
    let bytes = write_new_file(&target, text.as_bytes())?;

    let subscriptions = file.products.iter().filter(|p| p.is_subscription()).count();
    let countries: BTreeSet<&str> = file
        .products
        .iter()
        .flat_map(|p| p.prices.iter().flatten())
        .map(|price| price.country.as_str())
        .collect();
    let locales: BTreeSet<&str> = file
        .products
        .iter()
        .flat_map(|p| p.languages.iter().flatten())
        .map(|lang| lang.locale.as_str())
        .collect();
    let mut result = json!({
        "appId": app_id,
        "saved": {"path": display_path(&target), "bytes": bytes},
        "subscriptionGroups": file.subscription_groups.len(),
        "products": file.products.len(),
        "subscriptions": subscriptions,
        "promotions": file.promotions.len(),
        "priceCountries": countries.len(),
        "languages": locales,
        "next_step": format!(
            "Edit the file, then call products_apply with file `{save_to}` (dry_run first) to \
             see and apply the differences."
        ),
    });
    if !remote.warnings.is_empty() {
        result["warnings"] = json!(remote.warnings);
    }
    Ok(result)
}
