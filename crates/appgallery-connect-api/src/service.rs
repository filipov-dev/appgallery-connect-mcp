//! Сервисы, на хосты которых уходят запросы исполнителя.
//!
//! Почти все операции идут в AppGallery Connect API ([`ApiService::Connect`]) на хост
//! региона профиля. Серверные API покупок в приложении (HMS IAP: Order service и
//! Subscription service) живут на своих хостах и подписываются токеном приложения, а не
//! API-клиента ([`crate::app`]). Хост сервиса выбирается по региону профиля и берётся
//! только из таблиц этого модуля: произвольный адрес не принимается, чтобы исполнитель не
//! стал HTTP-прокси.
//!
//! Хосты IAP — из раздела «API Overview» справочника HMS Core (In-App Purchases > Server).
//! Справочник говорит, что IAP сам направляет запрос на площадку, где лежат данные; мы
//! всё равно идём на площадку региона профиля.
//!
//! REST API IAP Kit для HarmonyOS 5 и новее ([`ApiService::IapKit`]) в таблице есть только
//! ради поиска: подписи JWT ES256 ключом IAP Kit и проверки ответов JWS в сервере нет, и
//! генератор требует, чтобы такие операции были исключены с `callable = false`.

use crate::region::Region;

/// Сервис операции (`x-agc-service` снимка; нет расширения — [`ApiService::Connect`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ApiService {
    /// AppGallery Connect API: хост региона, токен API-клиента, путь `/api/…`.
    #[default]
    Connect,
    /// HMS IAP Order service (`orders-*`): проверка и подтверждение покупок, отменённые
    /// покупки, заказы.
    IapOrders,
    /// HMS IAP Subscription service (`subscr-*`): подписки.
    IapSubscriptions,
    /// REST API IAP Kit (HarmonyOS 5 и новее): не вызывается, см. документацию модуля.
    IapKit,
}

impl ApiService {
    /// Все сервисы.
    pub const ALL: [ApiService; 4] = [
        Self::Connect,
        Self::IapOrders,
        Self::IapSubscriptions,
        Self::IapKit,
    ];

    /// Значение `x-agc-service` снимка: `connect`, `iap-orders`, `iap-subscriptions`,
    /// `iap-kit`.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::IapOrders => "iap-orders",
            Self::IapSubscriptions => "iap-subscriptions",
            Self::IapKit => "iap-kit",
        }
    }

    /// Сервис по значению `x-agc-service`.
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|service| service.slug() == slug)
    }

    /// Сервис покупок вне AppGallery Connect API: свой хост, `api_request` его не вызывает.
    /// Запросы HMS IAP подписываются токеном приложения (`Authorization: Basic …`), а не
    /// токеном API-клиента; операции IAP Kit не вызываются вовсе.
    pub fn is_iap(self) -> bool {
        !matches!(self, Self::Connect)
    }

    /// Название сервиса для сообщений (английский).
    pub fn title(self) -> &'static str {
        match self {
            Self::Connect => "AppGallery Connect API",
            Self::IapOrders => "HMS IAP Order service",
            Self::IapSubscriptions => "HMS IAP Subscription service",
            Self::IapKit => "HarmonyOS IAP Kit server API",
        }
    }

    /// Имя хоста сервиса в регионе, без схемы.
    pub fn host(self, region: Region) -> &'static str {
        match (self, region) {
            (Self::Connect, region) => region.host(),
            (Self::IapOrders, Region::Cn) => "orders-drcn.iap.cloud.huawei.com.cn",
            (Self::IapOrders, Region::De) => "orders-dre.iap.cloud.huawei.eu",
            (Self::IapOrders, Region::Sg) => "orders-dra.iap.cloud.huawei.asia",
            (Self::IapOrders, Region::Ru) => "orders-drru.iap.cloud.huawei.ru",
            (Self::IapSubscriptions, Region::Cn) => "subscr-drcn.iap.cloud.huawei.com.cn",
            (Self::IapSubscriptions, Region::De) => "subscr-dre.iap.cloud.huawei.eu",
            (Self::IapSubscriptions, Region::Sg) => "subscr-dra.iap.cloud.huawei.asia",
            (Self::IapSubscriptions, Region::Ru) => "subscr-drru.iap.cloud.huawei.ru",
            // Справочник IAP Kit называет только площадку Китая.
            (Self::IapKit, _) => "iap.cloud.huawei.com",
        }
    }

    /// Корень сервиса: `https://<host>`, без слеша на конце.
    pub fn base_url(self, region: Region) -> String {
        format!("https://{}", self.host(region))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_round_trip() {
        for service in ApiService::ALL {
            assert_eq!(ApiService::from_slug(service.slug()), Some(service));
        }
        assert_eq!(ApiService::from_slug("iap"), None);
        assert!(!ApiService::Connect.is_iap());
        assert!(ApiService::IapOrders.is_iap());
    }

    #[test]
    fn iap_hosts_follow_the_region() {
        for region in Region::ALL {
            assert_eq!(ApiService::Connect.host(region), region.host());
            let orders = ApiService::IapOrders.host(region);
            let subscriptions = ApiService::IapSubscriptions.host(region);
            assert!(orders.starts_with("orders-dr"), "{orders}");
            assert!(subscriptions.starts_with("subscr-dr"), "{subscriptions}");
            assert!(orders.contains(".iap.cloud.huawei."), "{orders}");
            assert_eq!(
                orders.trim_start_matches("orders-"),
                subscriptions.trim_start_matches("subscr-")
            );
        }
        assert_eq!(
            ApiService::IapOrders.base_url(Region::Ru),
            "https://orders-drru.iap.cloud.huawei.ru"
        );
    }
}
