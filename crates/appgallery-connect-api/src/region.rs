//! Регионы AppGallery Connect API.
//!
//! У API несколько хостов: основной и региональные. Хост задаётся профилем и берётся
//! только из этого списка: произвольный адрес не принимается, чтобы исполнитель не стал
//! HTTP-прокси.

/// Регион (сайт) AppGallery Connect API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Region {
    /// Основной хост (Китай). Командные API (Publishing, Reports, Comments, PMS)
    /// в справочнике описаны на нём.
    #[default]
    Cn,
    /// Германия.
    De,
    /// Сингапур.
    Sg,
    /// Россия.
    Ru,
}

impl Region {
    /// Все регионы в порядке показа пользователю.
    pub const ALL: [Region; 4] = [Region::Cn, Region::De, Region::Sg, Region::Ru];

    /// Короткое имя региона для CLI и профиля: `cn`, `de`, `sg`, `ru`.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Cn => "cn",
            Self::De => "de",
            Self::Sg => "sg",
            Self::Ru => "ru",
        }
    }

    /// Регион по короткому имени (без учёта регистра).
    pub fn from_slug(slug: &str) -> Option<Region> {
        Region::ALL
            .into_iter()
            .find(|region| region.slug().eq_ignore_ascii_case(slug))
    }

    /// Имя хоста API без схемы.
    pub fn host(self) -> &'static str {
        match self {
            Self::Cn => "connect-api.cloud.huawei.com",
            Self::De => "connect-api-dre.cloud.huawei.com",
            Self::Sg => "connect-api-dra.cloud.huawei.com",
            Self::Ru => "connect-api-drru.cloud.huawei.com",
        }
    }

    /// Корень API: `https://<host>`, без слеша на конце.
    pub fn base_url(self) -> String {
        format!("https://{}", self.host())
    }

    /// Название региона для сообщений (английский).
    pub fn title(self) -> &'static str {
        match self {
            Self::Cn => "China (default)",
            Self::De => "Germany",
            Self::Sg => "Singapore",
            Self::Ru => "Russia",
        }
    }

    /// Регион по `siteId` команды из списка команд пользователя (Project Management API):
    /// 1 — Китай, 5 — Сингапур, 7 — Германия, 8 — Россия.
    pub fn from_site_id(site_id: i64) -> Option<Region> {
        match site_id {
            1 => Some(Self::Cn),
            5 => Some(Self::Sg),
            7 => Some(Self::De),
            8 => Some(Self::Ru),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_round_trip() {
        for region in Region::ALL {
            assert_eq!(Region::from_slug(region.slug()), Some(region));
        }
        assert_eq!(Region::from_slug("RU"), Some(Region::Ru));
        assert_eq!(Region::from_slug("us"), None);
    }

    #[test]
    fn site_ids() {
        assert_eq!(Region::from_site_id(1), Some(Region::Cn));
        assert_eq!(Region::from_site_id(5), Some(Region::Sg));
        assert_eq!(Region::from_site_id(7), Some(Region::De));
        assert_eq!(Region::from_site_id(8), Some(Region::Ru));
        assert_eq!(Region::from_site_id(2), None);
    }

    #[test]
    fn hosts_are_huawei_connect_api() {
        for region in Region::ALL {
            assert!(region.host().starts_with("connect-api"));
            assert!(region.host().ends_with(".cloud.huawei.com"));
            assert!(region.base_url().starts_with("https://"));
        }
    }
}
