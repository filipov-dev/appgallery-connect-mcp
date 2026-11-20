//! Раздел инструкций для агента о покупках в приложении (набор `purchases`, HMS IAP).
//!
//! Операции HMS IAP идут не в AppGallery Connect API, а в сервисы заказов и подписок и
//! подписываются секретом приложения (`auth app-secret set`). Агенту нужно знать, откуда
//! берётся `purchaseToken`, какие ограничения у запросов, что возвраты и отмены касаются
//! денег настоящих пользователей и что данные покупок — персональные данные покупателей.
//! Раздел добавляется, только если инструменты набора есть или могут появиться
//! (динамические наборы).

use appgallery_connect_api::spec;

use crate::registry::{ToolKind, ToolRegistry};
use crate::{DEFAULT_PROFILE_NAME, ServerOptions};

/// Slug набора инструментов HMS IAP.
pub const TOOLSET: &str = "purchases";

/// Раздел инструкций о покупках; пустая строка — инструментов набора нет. `app_ids` —
/// приложения, для которых в профиле сохранён секрет приложения.
pub fn purchases_instructions(
    options: &ServerOptions,
    registry: &ToolRegistry,
    app_ids: &[&str],
) -> String {
    let present = registry
        .iter()
        .any(|tool| matches!(tool.kind, ToolKind::Operation(op) if op.toolset == TOOLSET))
        || (registry.is_dynamic()
            && spec::operations()
                .iter()
                .any(|op| op.toolset == TOOLSET && options.max_access.allows(op.access)));
    if !present {
        return String::new();
    }
    let profile = options.profile.as_deref().unwrap_or(DEFAULT_PROFILE_NAME);
    let stored = stored_text(options.http, profile, app_ids);
    format!(
        "\n- In-app purchases (toolset `{TOOLSET}`): these tools call the HMS In-App Purchases \
         Order and Subscription services, not the AppGallery Connect API, and `api_request` \
         cannot call them. Each takes `appId`: the server signs the request with the stored \
         app secret of that app. {stored} A `purchaseToken` (and `subscriptionId` of a \
         subscription) comes from the purchase data (InAppPurchaseData) that the IAP client \
         returns to the app after a purchase or a purchase query, from the app's backend, or \
         from `list_cancelled_purchases`: ask the user for it, never guess it. \
         `verify_product_purchase` checks consumables and non-consumables: only the latest \
         token of a product can be verified, an older one returns result code 12. \
         `get_subscription_purchase` checks a subscription. `list_cancelled_purchases` covers \
         only the last 30 days and `list_iap_orders` a period of at most 48 hours (UTC \
         timestamps in milliseconds); when a response has `continuationToken`, pass it in \
         `body` of the next call for the next page. The purchase data strings come parsed \
         into objects, without the signature. `purchaseType` 0 in purchase data means a \
         sandbox purchase. Result codes: 5 IAP is not enabled for the app, 8 the user does \
         not own the product, 9 already consumed or confirmed, 11 a problem with the user's \
         account, 12 no such order or not the latest token.\n\
         - Purchases change money and access of real users: confirming delivery, cancelling, \
         deferring, refunding and revoking subscriptions are destructive tools. Call them \
         only when the user explicitly asks for that purchase or subscription, and confirm \
         the purchase with the user first. Only subscriptions can be refunded here \
         (`refund_subscription` refunds the latest payment, `revoke_subscription` also ends \
         access at once); consumables and non-consumables cannot be refunded through these \
         APIs. Purchase data and orders are personal data of buyers (order IDs, countries, \
         amounts, `developerPayload` set by the app): use them only for the task at hand and \
         do not copy them into files, commits or messages."
    )
}

/// Какие секреты приложений сохранены и как добавить недостающий.
fn stored_text(http: bool, profile: &str, app_ids: &[&str]) -> String {
    let list = app_ids
        .iter()
        .map(|id| format!("`{id}`"))
        .collect::<Vec<_>>()
        .join(", ");
    match (http, app_ids.is_empty()) {
        (false, true) => format!(
            "No app secret is stored in this profile yet: the user adds one with \
             `appgallery-connect-mcp auth app-secret set --profile {profile} --app <app ID>` \
             and restarts the server; never ask them to paste the secret into the conversation."
        ),
        (false, false) => format!(
            "App secrets are stored for app IDs {list}; for another app the user runs \
             `appgallery-connect-mcp auth app-secret set --profile {profile} --app <app ID>` \
             and restarts the server. Never ask them to paste the secret into the conversation."
        ),
        (true, true) => "No app secret is stored on this server: the server operator stores \
                         them; never ask for app secrets in the conversation."
            .to_owned(),
        (true, false) => format!(
            "App secrets are stored on this server for app IDs {list}; for another app ask \
             the server operator. Never ask for app secrets in the conversation."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use appgallery_connect_api::AccessLevel;

    fn text(options: &ServerOptions, app_ids: &[&str]) -> String {
        let registry = ToolRegistry::new(options).unwrap();
        purchases_instructions(options, &registry, app_ids)
    }

    #[test]
    fn only_with_the_toolset() {
        assert_eq!(text(&ServerOptions::default(), &[]), "");
        let options = ServerOptions {
            toolsets: Some(vec![TOOLSET.to_owned()]),
            ..ServerOptions::default()
        };
        let read = text(&options, &[]);
        assert!(read.contains("toolset `purchases`"), "{read}");
        assert!(
            read.contains("auth app-secret set --profile default"),
            "{read}"
        );
        assert!(
            read.contains("30 days") && read.contains("48 hours"),
            "{read}"
        );
        assert!(read.contains("continuationToken") && read.contains("developerPayload"));
        let dynamic = ServerOptions {
            dynamic_toolsets: true,
            ..ServerOptions::default()
        };
        assert!(!text(&dynamic, &[]).is_empty());
    }

    #[test]
    fn names_stored_apps_and_the_operator_over_http() {
        let options = ServerOptions {
            toolsets: Some(vec![TOOLSET.to_owned()]),
            max_access: AccessLevel::Destructive,
            profile: Some("work".to_owned()),
            ..ServerOptions::default()
        };
        let local = text(&options, &["101", "202"]);
        assert!(local.contains("app IDs `101`, `202`"), "{local}");
        assert!(local.contains("--profile work"), "{local}");
        let http = ServerOptions {
            http: true,
            ..options
        };
        let remote = text(&http, &[]);
        assert!(remote.contains("server operator"), "{remote}");
        assert!(!remote.contains("auth app-secret set"), "{remote}");
    }
}
