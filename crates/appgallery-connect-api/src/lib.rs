//! Клиент AppGallery Connect API (Huawei).
//!
//! Операции описаны в снимке спецификации (`spec/openapi.json`, собран из справочника
//! Huawei) и файле-дополнении с именами инструментов, наборами и уровнями доступа.
//!
//! - [`spec`] — группы и операции API, сгенерированные из снимка при сборке.
//! - [`region`] — регионы и хосты API.
//! - [`credentials`] — учётные данные API-клиента (идентификатор и секрет).
//! - [`store`] — хранение учётных данных по профилям в системном хранилище.
//! - [`token`] — access token API-клиента: получение, кэш в памяти, обновление.
//! - [`executor`] — исполнитель запросов: операции из таблицы и произвольные пути API,
//!   повторы, конверт результата, ошибки.
//! - [`envelope`] — конверт результата в теле ответа.
//! - [`path`] — проверка и сборка относительных путей API.
//! - [`transfer`] — загрузка и скачивание файлов по адресам из ответов API.
//! - [`scrub`] — вырезание секретов из текстов.

pub mod credentials;
pub mod envelope;
pub mod executor;
mod http;
pub mod path;
pub mod region;
pub mod scrub;
pub mod spec;
pub mod store;
pub mod token;
pub mod transfer;

pub use credentials::{ApiClientCredentials, ClientId, ClientSecret, CredentialsError};
pub use envelope::{ResultStatus, detect_result, read_result, result_of};
pub use executor::{
    ApiClient, ApiError, ApiRequest, ApiResponse, ClientConfig, HttpError, ResponseBody,
    ResultCheck, ResultError, RetryPolicy,
};
pub use region::Region;
pub use spec::{
    AccessLevel, BodyKind, ExcludedOperationDef, ExclusionReason, GroupDef, HttpMethod,
    OperationDef, ParamDef, ParamLocation, ParamType, ResponseKind, ResultDef, SecretFieldDef,
    SecretKind, ToolsetDef,
};
pub use store::{
    CredentialStore, KEYRING_SERVICE, KeyringStore, KeyringVault, MemoryStore,
    SECRETS_KEYRING_SERVICE, StoreError,
};
pub use token::{AccessToken, ApiClientTokens, StaticToken, TokenError, TokenSource};
pub use transfer::{
    DownloadedFile, TransferClient, TransferConfig, TransferHosts, UploadRequest, UploadResponse,
};
