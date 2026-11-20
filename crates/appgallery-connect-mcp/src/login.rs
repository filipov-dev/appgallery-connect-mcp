//! Приём учётных данных API-клиента: общая часть `auth login` и `connect`.
//!
//! Идентификатор клиента — флагом или вопросом в терминале. Секрет — только из файла, из
//! stdin или скрытым вводом в терминале: не аргументом командной строки и не из
//! переменных окружения или `.env`. Пробелы по краям обоих значений обрезаются здесь
//! (клиент API их не обрезает), форма проверяется [`ClientId::new`] и [`ClientSecret::new`].
//! Значения в ошибки и вывод не попадают; путь к файлу секрета нигде не запоминается.

use std::convert::Infallible;
use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use appgallery_connect_api::project::MAX_CREDENTIAL_FILE_BYTES;
use appgallery_connect_api::{
    ApiClientCredentials, ClientId, ClientSecret, CredentialFile, CredentialStore, KEYRING_SERVICE,
    StoreError,
};
use zeroize::Zeroizing;

use crate::profiles::{validate_app_id, validate_package};
use crate::ui::Ui;

/// Предел размера ввода секрета: секрет — до 2048 символов, остальное — запас на пробелы
/// и перевод строки. Больший ввод — явно не секрет.
const MAX_SECRET_INPUT_BYTES: u64 = 16 * 1024;

/// Откуда взять секрет клиента.
pub enum SecretSource<'a> {
    /// Файл с секретом (`--secret-file`).
    File(&'a Path),
    /// Поток (`--secret-stdin`): stdin процесса или его подмена в тестах.
    Reader(&'a mut dyn Read),
    /// Не задан: спросить скрытым вводом в терминале.
    Prompt,
}

/// Короткая подсказка, где в AppGallery Connect управляют API-клиентами.
pub const WHERE_TO_FIND: &str =
    "AppGallery Connect > Users and permissions > API key > Connect API";

/// Консоль AppGallery Connect.
pub const AGC_CONSOLE_URL: &str =
    "https://developer.huawei.com/consumer/en/service/josp/agc/index.html";

/// Роли и права AppGallery Connect.
pub const ROLES_URL: &str =
    "https://developer.huawei.com/consumer/en/doc/app/agc-help-rolepermission-0000002271930352";

/// Пошаговая подсказка, как создать API-клиент. Пункты сверены с разделом «API Client Mode»
/// страницы Getting Started справочника Connect API: клиент создаёт только владелец
/// аккаунта команды, поле Project остаётся N/A (клиент уровня команды; иначе API отвечает
/// 403), права задают роли.
pub fn api_client_steps() -> String {
    format!(
        "To create an API client in AppGallery Connect (only the team account holder can do \
         this):\n  \
         1. Sign in to AppGallery Connect at {AGC_CONSOLE_URL} and click Users and \
         permissions.\n  \
         2. In the navigation pane, choose API key > Connect API, open the API client tab and \
         click Create.\n  \
         3. Enter any name. Keep Project as N/A: the server needs a team-level API client (a \
         project-level one gets HTTP 403).\n  \
         4. Select the roles: Administrator for every tool, or only the roles your tasks need \
         ({ROLES_URL}).\n  \
         5. Copy the client ID and the key (the client secret) shown for the new client. Keep \
         the key in a file or a password manager; never paste it into a chat.\n\
         This server uses the API client mode (client ID and key), not a Service Account."
    )
}

/// Пошаговая подсказка, как создать API-клиент уровня проекта и скачать его файл. Пункты
/// сверены со страницей Getting Started with the Server справочника AppGallery Connect
/// (Downloading an Authentication Credential > Project-Level Credential).
pub fn project_client_steps() -> String {
    format!(
        "To create a project-level API client in AppGallery Connect:\n  \
         1. Sign in to AppGallery Connect at {AGC_CONSOLE_URL}, open Development and services \
         and click the project card (the project needs a data processing location).\n  \
         2. Go to Project settings > Server SDK and click Create under API client (or create \
         it in Users and permissions > API key > Connect API with this project selected and \
         the roles the services need).\n  \
         3. Click Download credential: the file agc-apiclient-*.json holds the project ID, \
         the client ID and the client secret. Pass it with --credential-file; it can be \
         deleted afterwards. Never paste its contents into a chat."
    )
}

/// Страница Getting Started справочника Connect API (режимы API-клиента и OAuth-клиента).
pub const CONNECT_API_GETTING_STARTED_URL: &str = "https://developer.huawei.com/consumer/en/doc/AppGallery-connect-Guides/agcapi-getstarted-0000001111845114";

/// Пошаговая подсказка, как получить OAuth-клиент для входа пользователя. Пункты сверены с
/// разделом «OAuth Client Mode» страницы Getting Started справочника Connect API: режим для
/// разработчиков платформ, области Connect API выдаются по заявке и проверке Huawei,
/// вход — HUAWEI ID (Account Kit).
pub fn oauth_client_steps() -> String {
    format!(
        "To sign in as a HUAWEI ID user you need an OAuth client: an app with HUAWEI ID \
         (Account Kit) for which Huawei approved the Connect API scopes ({CONNECT_API_GETTING_STARTED_URL}, \
         OAuth Client Mode):\n  \
         1. In AppGallery Connect at {AGC_CONSOLE_URL} open My projects > the project > the app \
         (or create a web app with HUAWEI ID in HUAWEI Developers Console > App services > \
         HUAWEI ID) and enable HUAWEI ID (Account Kit).\n  \
         2. Set the redirect URL (callback URL) of the app to a loopback address with a \
         fixed port, such as http://127.0.0.1:8765/, and pass the same address with \
         --redirect-uri (without it the command listens on a random port, which works only \
         if HUAWEI ID accepts any loopback port).\n  \
         3. Apply for the Connect API scopes (publish, report/read, product, develop, project) \
         by email to agconnect@huawei.com as the Getting Started page describes; Huawei \
         reviews the request.\n  \
         4. Copy the client ID (OAuth 2.0 client ID, the App ID) and the client secret (App \
         secret) from App information. Keep the secret in a file or a password manager; never \
         paste it into a chat.\n\
         The browser then opens the HUAWEI ID sign-in page; the account you sign in with must \
         be a member of the team (Users and permissions) with the roles your tasks need."
    )
}

/// Client ID OAuth-клиента: из флага, иначе вопросом в терминале (после подсказки
/// [`oauth_client_steps`]).
pub fn read_oauth_client_id(ui: &mut dyn Ui, flag: Option<&str>) -> anyhow::Result<ClientId> {
    let value = match flag {
        Some(value) => value.trim().to_owned(),
        None => {
            if !ui.is_interactive() {
                bail!(
                    "no terminal to ask for the client ID of the OAuth client: pass \
                     --oauth-client-id <ID> (the OAuth 2.0 client ID of the app with HUAWEI ID)"
                );
            }
            ui.say(&oauth_client_steps());
            ui.ask("OAuth client ID: ")
                .context("failed to read the client ID")?
                .trim()
                .to_owned()
        }
    };
    ClientId::new(value).map_err(|err| anyhow!("invalid OAuth client ID: {err}"))
}

/// Client secret OAuth-клиента, если он есть: из файла или stdin (тогда обязателен) или
/// скрытым вводом (пустой ответ — без секрета). Без терминала и без флагов — без секрета.
pub fn read_optional_secret(
    ui: &mut dyn Ui,
    source: SecretSource<'_>,
) -> anyhow::Result<Option<ClientSecret>> {
    match source {
        SecretSource::Prompt => {
            if !ui.is_interactive() {
                return Ok(None);
            }
            let text = ui
                .ask_secret(
                    "Client secret of the OAuth client (input is hidden; press Enter if it has \
                     none): ",
                )
                .context("failed to read the client secret")?;
            if text.trim().is_empty() {
                return Ok(None);
            }
            ClientSecret::new(text.trim().to_owned())
                .map(Some)
                .map_err(|err| {
                    anyhow!(
                        "the hidden input does not contain a usable client secret: {err}; \
                         nothing was stored"
                    )
                })
        }
        source => read_client_secret(ui, source).map(Some),
    }
}

/// Значение аргумента, который принимается только затем, чтобы его отклонить: само
/// значение не сохраняется.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Discarded;

/// Парсер `clap` для [`Discarded`]: значение отбрасывается сразу.
pub fn discard(_: &str) -> Result<Discarded, Infallible> {
    Ok(Discarded)
}

/// Отклоняет секрет и лишние значения в командной строке `command`, не повторяя их.
/// `after` — позиционный аргумент команды, после которого лишние значения не принимаются.
pub fn reject_secret_arguments(
    command: &str,
    after: Option<&str>,
    secret_argument: bool,
    stray: usize,
) -> anyhow::Result<()> {
    if secret_argument {
        bail!(
            "the client secret is not accepted as a command-line argument (it would stay in the \
             shell history and the process list): pass --secret-file <PATH> or --secret-stdin, \
             or omit both to type it hidden in the terminal. If you already typed the secret in \
             a command, consider resetting it in AppGallery Connect"
        );
    }
    if stray > 0 {
        let after = after
            .map(|name| format!(" after {name}"))
            .unwrap_or_default();
        bail!(
            "`{command}` takes no positional arguments{after} ({stray} given and not shown). The \
             client secret is never accepted on the command line: pass --secret-file <PATH> or \
             --secret-stdin, or omit both to type it hidden in the terminal"
        );
    }
    Ok(())
}

/// Приложения по умолчанию из флага `--default-app`: без пустых, только числовые `appId`.
pub fn default_apps(values: &[String]) -> anyhow::Result<Vec<String>> {
    values
        .iter()
        .filter(|app| !app.trim().is_empty())
        .map(|app| validate_app_id(app))
        .collect()
}

/// Пакеты по умолчанию из флага `--default-package`: без пустых, только имена пакетов.
pub fn default_packages(values: &[String]) -> anyhow::Result<Vec<String>> {
    values
        .iter()
        .filter(|package| !package.trim().is_empty())
        .map(|package| validate_package(package))
        .collect()
}

/// Идентификатор клиента: из флага, иначе вопросом в терминале (после пошаговой подсказки).
pub fn read_client_id(ui: &mut dyn Ui, flag: Option<&str>) -> anyhow::Result<ClientId> {
    let value = match flag {
        Some(value) => value.trim().to_owned(),
        None => {
            if !ui.is_interactive() {
                bail!(
                    "no terminal to ask for the API client ID: pass --client-id <ID> (create the \
                     API client in {WHERE_TO_FIND})"
                );
            }
            ui.say(&api_client_steps());
            ui.ask("Client ID: ")
                .context("failed to read the client ID")?
                .trim()
                .to_owned()
        }
    };
    ClientId::new(value).map_err(|err| anyhow!("invalid API client ID: {err}"))
}

/// Идентификатор клиента проекта: из флага, иначе вопросом в терминале (после подсказки
/// [`project_client_steps`]).
pub fn read_project_client_id(ui: &mut dyn Ui, flag: Option<&str>) -> anyhow::Result<ClientId> {
    let value = match flag {
        Some(value) => value.trim().to_owned(),
        None => {
            if !ui.is_interactive() {
                bail!(
                    "no terminal to ask for the client ID of the project-level API client: pass \
                     --credential-file <agc-apiclient-*.json> or --client-id <ID>"
                );
            }
            ui.say(&project_client_steps());
            ui.ask("Client ID: ")
                .context("failed to read the client ID")?
                .trim()
                .to_owned()
        }
    };
    ClientId::new(value).map_err(|err| anyhow!("invalid API client ID: {err}"))
}

/// Читает и разбирает файл `agc-apiclient-*.json`. Ни содержимое, ни значения в ошибки не
/// попадают; путь нигде не запоминается.
pub fn read_credential_file(path: &Path) -> anyhow::Result<CredentialFile> {
    let display = path.display();
    let file = fs::File::open(path)
        .with_context(|| format!("cannot open the credential file `{display}`"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot read the credential file `{display}`"))?;
    if !metadata.is_file() {
        bail!("`{display}` is not a regular file");
    }
    let mut text = Zeroizing::new(String::new());
    file.take(MAX_CREDENTIAL_FILE_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("cannot read the credential file `{display}` as UTF-8 text"))?;
    CredentialFile::parse(&text).map_err(|err| {
        anyhow!("the credential file `{display}` cannot be used: {err}; nothing was stored")
    })
}

/// Читает секрет из потока (не больше [`MAX_SECRET_INPUT_BYTES`]). Содержимое в ошибки не
/// попадает.
fn read_limited(reader: &mut dyn Read, origin: &str) -> anyhow::Result<Zeroizing<String>> {
    let mut text = Zeroizing::new(String::new());
    reader
        .take(MAX_SECRET_INPUT_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("cannot read the client secret from {origin} as UTF-8 text"))?;
    if text.len() as u64 > MAX_SECRET_INPUT_BYTES {
        bail!(
            "{origin} is larger than {MAX_SECRET_INPUT_BYTES} bytes; it must contain only the \
             client secret"
        );
    }
    Ok(text)
}

/// Секрет клиента из источника, с обрезанными по краям пробелами и проверенной формой.
pub fn read_client_secret(
    ui: &mut dyn Ui,
    source: SecretSource<'_>,
) -> anyhow::Result<ClientSecret> {
    let (text, origin) = match source {
        SecretSource::File(path) => {
            let display = path.display();
            let mut file = fs::File::open(path)
                .with_context(|| format!("cannot open the secret file `{display}`"))?;
            let metadata = file
                .metadata()
                .with_context(|| format!("cannot read the secret file `{display}`"))?;
            if !metadata.is_file() {
                bail!("`{display}` is not a regular file");
            }
            let origin = format!("the secret file `{display}`");
            (read_limited(&mut file, &origin)?, origin)
        }
        SecretSource::Reader(reader) => {
            let origin = "stdin".to_owned();
            (read_limited(reader, &origin)?, origin)
        }
        SecretSource::Prompt => {
            if !ui.is_interactive() {
                bail!(
                    "no terminal to ask for the client secret: pass --secret-file <PATH> or \
                     --secret-stdin (the secret is never accepted as a command-line argument)"
                );
            }
            let text = ui
                .ask_secret("Client secret (input is hidden): ")
                .context("failed to read the client secret")?;
            (text, "the hidden input".to_owned())
        }
    };
    ClientSecret::new(text.trim().to_owned()).map_err(|err| {
        anyhow!("{origin} does not contain a usable client secret: {err}; nothing was stored")
    })
}

/// Прежняя запись профиля в хранилище, которую заменяет новый вход.
pub enum Previous {
    /// Записи не было.
    None,
    /// Прежние учётные данные: их возвращают на место, если следующий шаг не удался.
    Credentials(ApiClientCredentials),
    /// Запись была, но не разбиралась: вернуть нечего, при откате она удаляется.
    Corrupted,
}

impl Previous {
    fn existed(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Сохраняет учётные данные профиля в системное хранилище, заменяя прежние. Возвращает
/// прежнюю запись для [`restore_stored`].
pub fn store_credentials(
    store: &dyn CredentialStore,
    profile: &str,
    credentials: &ApiClientCredentials,
) -> anyhow::Result<Previous> {
    let previous = match store.get(profile) {
        Ok(previous) => Previous::Credentials(previous),
        Err(StoreError::NotFound { .. }) => Previous::None,
        Err(StoreError::Corrupted { .. }) => Previous::Corrupted,
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context("cannot read the system keychain; nothing was stored"));
        }
    };
    store.set(profile, credentials).map_err(|err| {
        anyhow::Error::new(err)
            .context("cannot save the API client credentials; nothing was stored")
    })?;
    Ok(previous)
}

/// Сообщение о сохранённых учётных данных.
pub fn stored_message(profile: &str, previous: &Previous) -> String {
    format!(
        "API client credentials {} in the system keychain (service `{KEYRING_SERVICE}`, \
         account `{profile}`).",
        if previous.existed() {
            "replaced"
        } else {
            "saved"
        }
    )
}

/// Возвращает хранилище в прежнее состояние: прежние учётные данные на место или удаляет
/// новые, если прежних не было.
pub fn restore_stored(
    store: &dyn CredentialStore,
    profile: &str,
    previous: &Previous,
) -> Result<(), StoreError> {
    match previous {
        Previous::Credentials(previous) => store.set(profile, previous),
        Previous::None | Previous::Corrupted => match store.delete(profile) {
            Ok(()) | Err(StoreError::NotFound { .. }) => Ok(()),
            Err(err) => Err(err),
        },
    }
}
