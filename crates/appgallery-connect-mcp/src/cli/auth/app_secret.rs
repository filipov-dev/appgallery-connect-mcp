//! `appgallery-connect-mcp auth app-secret set|remove` — секреты приложений профиля.
//!
//! Серверные API покупок в приложении (HMS IAP) подписываются токеном уровня приложения,
//! который Huawei OAuth выдаёт по client ID и client secret приложения (AppGallery Connect >
//! My apps > приложение > App information, или All services > In-App Purchases). Это третьи
//! учётные данные профиля, по одной записи на приложение: `<профиль>/app/<appId>`
//! ([`app_account`]); App ID — в списке `app_secrets` файла профилей.
//!
//! Секрет — только из файла, stdin или скрытым вводом, как у `auth login`. Перед записью
//! он проверяется получением токена (кроме `--no-verify`); при любой ошибке ничего не
//! сохраняется, а если не удалось записать файл профилей, прежняя запись хранилища
//! возвращается на место. Команды, которая выводит секрет или токен, нет.

use std::path::PathBuf;

use anyhow::{Context, bail};
use appgallery_connect_api::{
    AppCredentials, CredentialStore, KEYRING_SERVICE, StoreError, app_account, validate_app_id,
};
use clap::{Args, Subcommand};

use super::ProfileArg;
use crate::login::{Discarded, SecretSource, discard, read_client_secret, reject_secret_arguments};
use crate::profiles::{ProfileRepo, ProfileSettings, validate_profile_name};
use crate::ui::Ui;
use crate::verify::Verifier;

/// Где в AppGallery Connect взять client ID и client secret приложения.
pub const WHERE_TO_FIND_APP_SECRET: &str = "AppGallery Connect > My apps > the app > App \
information (App ID and client secret), or All services > In-App Purchases";

#[derive(Debug, Subcommand)]
pub enum AppSecretCommand {
    /// Store the app secret (OAuth client ID and client secret) of an app in the system
    /// keychain.
    ///
    /// The HMS In-App Purchases server APIs (the `purchases` toolset) do not accept the API
    /// client of the profile: their requests are signed with an app-level access token that
    /// Huawei OAuth issues for the client ID and client secret of the app (in AppGallery
    /// Connect: My apps > the app > App information, or All services > In-App Purchases). The
    /// client ID is the App ID unless --client-id says otherwise. The secret is read from
    /// --secret-file, from stdin with --secret-stdin, or typed hidden in the terminal; never
    /// from a command-line argument or environment variables. Unless --no-verify, the secret
    /// is checked by getting an app-level access token from Huawei OAuth before it is saved.
    /// It is saved only in the system keychain and the App ID in profiles.toml. Replaces the
    /// stored secret of the app. If anything fails, nothing is stored. Restart the MCP server
    /// to use it.
    Set(AppSecretSetArgs),
    /// Remove the app secret of an app from the system keychain and the app from the
    /// profile.
    Remove(AppSecretRemoveArgs),
}

#[derive(Debug, Clone, Args)]
pub struct AppSecretSetArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// App ID of the app (My apps > the app > App information).
    #[arg(long = "app", value_name = "APP_ID")]
    pub app: String,
    /// OAuth client ID of the app if it differs from the App ID [default: the App ID].
    #[arg(long, value_name = "ID")]
    pub client_id: Option<String>,
    /// Path to a file that contains only the client secret of the app (read once, its path
    /// is not saved).
    #[arg(long, value_name = "PATH", conflicts_with = "secret_stdin")]
    pub secret_file: Option<PathBuf>,
    /// Read the client secret of the app from stdin.
    #[arg(long)]
    pub secret_stdin: bool,
    /// Store the secret without getting an app-level access token.
    #[arg(long)]
    pub no_verify: bool,
    /// Секрет аргументом: принимается только затем, чтобы отклонить его.
    #[arg(
        long = "secret",
        aliases = ["client-secret", "app-secret", "key"],
        hide = true,
        num_args = 0..=1,
        default_missing_value = "",
        allow_hyphen_values = true,
        value_parser = discard
    )]
    pub secret_argument: Option<Discarded>,
    /// Лишние позиционные аргументы (например, секрет): отклоняются без повтора значения.
    #[arg(hide = true, num_args = 0.., value_parser = discard)]
    pub stray: Vec<Discarded>,
}

#[derive(Debug, Clone, Args)]
pub struct AppSecretRemoveArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// App ID of the app whose secret to remove.
    #[arg(long = "app", value_name = "APP_ID")]
    pub app: String,
}

/// Проверяет App ID из флага (пробелы по краям обрезаются).
fn app_id_arg(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    validate_app_id(value).map_err(|err| anyhow::anyhow!("invalid --app: {err}"))?;
    Ok(value.to_owned())
}

pub async fn set(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    args: &AppSecretSetArgs,
    source: SecretSource<'_>,
) -> anyhow::Result<()> {
    reject_secret_arguments(
        "auth app-secret set",
        None,
        args.secret_argument.is_some(),
        args.stray.len(),
    )?;
    let profile = args.profile.profile.as_str();
    validate_profile_name(profile)?;
    let app_id = app_id_arg(&args.app)?;
    let client_id = args
        .client_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    // Секрет приложения — учётные данные профиля, как клиенты проектов: профиль уже должен
    // быть.
    let Some(mut settings) = repo.get(profile)? else {
        bail!(
            "profile `{profile}` does not exist: store its API client first with \
             `appgallery-connect-mcp auth login --profile {profile}`, then add the app secret \
             to it"
        );
    };
    if matches!(source, SecretSource::Prompt) {
        ui.say(&format!(
            "The client secret of the app is in {WHERE_TO_FIND_APP_SECRET}. Never paste it into \
             a chat."
        ));
    }
    let secret = read_client_secret(ui, source)?;
    let app = AppCredentials::new(app_id.clone(), client_id, secret.expose())
        .map_err(|err| anyhow::anyhow!("invalid app secret: {err}; nothing was stored"))?;
    ui.say(&format!(
        "App: {app_id}. Client ID: {}.",
        if app.client_id_is_app_id() {
            "the App ID"
        } else {
            "given with --client-id"
        }
    ));

    match verifier {
        Some(verifier) => {
            verifier.check_app(&app).await.map_err(|err| {
                anyhow::anyhow!(
                    "{err:#} Nothing was stored. Re-run with --no-verify to store the secret \
                     without verification."
                )
            })?;
            ui.say("The app secret works: Huawei OAuth issued an app-level access token.");
        }
        None => ui.say("Skipping the check (--no-verify)."),
    }

    let account = app_account(profile, &app_id);
    let previous = match store.get_app_secret(profile, &app_id) {
        Ok(previous) => Previous::Secret(previous),
        Err(StoreError::AppSecretNotFound { .. }) => Previous::None,
        Err(StoreError::AppSecretCorrupted { .. }) => Previous::Corrupted,
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context("cannot read the system keychain; nothing was stored"));
        }
    };
    store.set_app_secret(profile, &app).map_err(|err| {
        anyhow::Error::new(err).context("cannot save the app secret; nothing was stored")
    })?;
    if !settings.app_secrets.contains(&app_id) {
        settings.app_secrets.push(app_id.clone());
    }
    if let Err(err) = repo.upsert(profile, settings) {
        if let Err(restore) = previous.restore(store, profile, &app_id) {
            return Err(err.context(format!(
                "the profile was not saved, and the keychain could not be restored ({restore})"
            )));
        }
        return Err(err.context("the profile was not saved; the keychain was left unchanged"));
    }
    ui.say(&format!(
        "App secret of app `{app_id}` {} in the system keychain (service `{KEYRING_SERVICE}`, \
         account `{account}`); app added to profile `{profile}` in {}.",
        if matches!(previous, Previous::None) {
            "saved"
        } else {
            "replaced"
        },
        repo.path().display()
    ));
    ui.say("Restart the MCP server of the profile to use the secret.");
    Ok(())
}

/// Прежняя запись секрета приложения, которую заменяет `set`.
enum Previous {
    None,
    /// Прежний секрет: его возвращают на место, если следующий шаг не удался.
    Secret(AppCredentials),
    /// Запись была, но не разбиралась: при откате она удаляется.
    Corrupted,
}

impl Previous {
    /// Возвращает хранилище в прежнее состояние.
    fn restore(
        &self,
        store: &dyn CredentialStore,
        profile: &str,
        app_id: &str,
    ) -> Result<(), StoreError> {
        match self {
            Self::Secret(previous) => store.set_app_secret(profile, previous),
            Self::None | Self::Corrupted => match store.delete_app_secret(profile, app_id) {
                Ok(()) | Err(StoreError::AppSecretNotFound { .. }) => Ok(()),
                Err(err) => Err(err),
            },
        }
    }
}

pub fn remove(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
    app: &str,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let app_id = app_id_arg(app)?;
    match store.delete_app_secret(profile, &app_id) {
        Ok(()) => ui.say(&format!(
            "App secret of app `{app_id}` removed from the system keychain. Reset the secret in \
             AppGallery Connect if it may have leaked."
        )),
        Err(StoreError::AppSecretNotFound { .. }) => ui.say(&format!(
            "No app secret of app `{app_id}` is stored for profile `{profile}`."
        )),
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "cannot remove the app secret of app `{app_id}`; the profile was kept"
            )));
        }
    }
    if let Some(mut settings) = repo.get(profile)?
        && settings.app_secrets.contains(&app_id)
    {
        settings.app_secrets.retain(|id| id != &app_id);
        repo.upsert(profile, settings)
            .with_context(|| format!("cannot update profile `{profile}`"))?;
        ui.say(&format!("App `{app_id}` removed from profile `{profile}`."));
    }
    Ok(())
}

/// Удаляет секреты всех приложений профиля (`auth logout`). Нет записи — не ошибка.
pub fn remove_all(
    store: &dyn CredentialStore,
    ui: &mut dyn Ui,
    profile: &str,
    app_ids: &[String],
) -> anyhow::Result<()> {
    for app_id in app_ids {
        if validate_app_id(app_id).is_err() {
            continue;
        }
        match store.delete_app_secret(profile, app_id) {
            Ok(()) => ui.say(&format!(
                "App secret of app `{app_id}` removed from the system keychain."
            )),
            Err(StoreError::AppSecretNotFound { .. }) => {}
            Err(err) => {
                return Err(anyhow::Error::new(err).context(format!(
                    "cannot remove the app secret of app `{app_id}` of profile `{profile}`; \
                     the profile was kept"
                )));
            }
        }
    }
    Ok(())
}

/// Секреты приложений профиля в `auth status`: есть ли запись и, с `--check`, получение
/// токена. Возвращает проблемы с командой `auth app-secret set`.
pub async fn status(
    store: &dyn CredentialStore,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    name: &str,
    settings: &ProfileSettings,
) -> Vec<String> {
    if settings.app_secrets.is_empty() {
        ui.say("App secrets:  -");
        return Vec::new();
    }
    let mut problems = Vec::new();
    for (index, app_id) in settings.app_secrets.iter().enumerate() {
        let fix =
            format!("`appgallery-connect-mcp auth app-secret set --profile {name} --app {app_id}`");
        let line = if validate_app_id(app_id).is_err() {
            problems.push(format!(
                "app `{}` in profiles.toml is not a valid App ID",
                app_id.escape_debug()
            ));
            format!("{}: not a valid App ID", app_id.escape_debug())
        } else {
            match store.get_app_secret(name, app_id) {
                Ok(app) => {
                    let check = match verifier {
                        None => String::new(),
                        Some(verifier) => match verifier.check_app(&app).await {
                            Ok(()) => ", token ok".to_owned(),
                            Err(err) => {
                                problems.push(format!(
                                    "getting an app-level access token for app `{app_id}` \
                                     failed (check its secret, then {fix})"
                                ));
                                format!(", token failed: {err:#}")
                            }
                        },
                    };
                    format!(
                        "{app_id}: stored (account `{}`){check}",
                        app_account(name, app_id)
                    )
                }
                Err(StoreError::AppSecretNotFound { .. }) => {
                    problems.push(format!(
                        "no app secret of app `{app_id}` in the keychain ({fix})"
                    ));
                    format!("{app_id}: not found in the system keychain")
                }
                Err(StoreError::AppSecretCorrupted { .. }) => {
                    problems.push(format!(
                        "the stored app secret of app `{app_id}` is not valid ({fix})"
                    ));
                    format!("{app_id}: the stored value is not a valid client ID and secret")
                }
                Err(err) => {
                    problems.push(format!("the app secret of app `{app_id}` cannot be read"));
                    format!("{app_id}: unavailable ({err})")
                }
            }
        };
        let label = if index == 0 { "App secrets:" } else { "" };
        ui.say(&format!("{label:<14}{line}"));
    }
    problems
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::{MemoryStore, Region};
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::cli::auth::{StatusArgs, logout, status as auth_status};
    use crate::testing::credentials;
    use crate::ui::testing::ScriptedUi;

    const APP: &str = "100000001";

    fn secret(tag: &str) -> String {
        ["7c1d", "e9a0", tag, "44b2"].concat().repeat(4)
    }

    fn app_token() -> String {
        ["app", "-level-", "tok", "en-", "Cg9x"].concat()
    }

    struct Setup {
        dir: tempfile::TempDir,
        repo: ProfileRepo,
        store: MemoryStore,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("config"));
        let store = MemoryStore::new();
        store.set("work", &credentials("team")).unwrap();
        repo.upsert(
            "work",
            ProfileSettings {
                region: Region::Ru,
                ..ProfileSettings::default()
            },
        )
        .unwrap();
        Setup { dir, repo, store }
    }

    /// Huawei OAuth на `wiremock`: токен выдаётся только за секрет `tag`.
    async fn oauth_server(tag: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v3/token"))
            .and(body_string_contains(format!("client_id={APP}").as_str()))
            .and(body_string_contains(
                format!("client_secret={}", secret(tag)).as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"access_token": app_token(), "expires_in": 3600}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v3/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": 1101,
                "sub_error": 20152,
                "error_description": "invalid client secret",
            })))
            .mount(&server)
            .await;
        server
    }

    fn args(app: &str) -> AppSecretSetArgs {
        AppSecretSetArgs {
            profile: ProfileArg {
                profile: "work".into(),
            },
            app: app.into(),
            client_id: None,
            secret_file: None,
            secret_stdin: false,
            no_verify: false,
            secret_argument: None,
            stray: Vec::new(),
        }
    }

    fn assert_clean(text: &str) {
        for tag in ["good", "bad1"] {
            assert!(!text.contains(&secret(tag)), "{text}");
        }
        assert!(!text.contains(&app_token()), "{text}");
    }

    #[tokio::test]
    async fn set_stores_only_after_a_token_and_status_shows_it() {
        let s = setup();
        let server = oauth_server("good").await;
        let verifier = Verifier::with_base_url(&server.uri());

        // Неверный секрет: токена нет — ничего не сохраняется.
        let mut wrong = secret("bad1").into_bytes();
        let mut ui = ScriptedUi::default();
        let err = set(
            &s.store,
            &s.repo,
            Some(&verifier),
            &mut ui,
            &args(APP),
            SecretSource::Reader(&mut wrong.as_slice()),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("Nothing was stored"), "{text}");
        assert!(text.contains("1101/20152"), "{text}");
        assert_clean(&text);
        assert_clean(&ui.text());
        assert!(s.store.get_app_secret("work", APP).is_err());
        assert!(s.repo.get("work").unwrap().unwrap().app_secrets.is_empty());
        wrong.fill(0);

        // Верный секрет из файла: сохраняется после выдачи токена.
        let file = s.dir.path().join("app-secret.txt");
        std::fs::write(&file, format!("  {}\n", secret("good"))).unwrap();
        let mut ui = ScriptedUi::default();
        set(
            &s.store,
            &s.repo,
            Some(&verifier),
            &mut ui,
            &AppSecretSetArgs {
                app: format!(" {APP} "),
                secret_file: Some(file.clone()),
                ..args(APP)
            },
            SecretSource::File(&file),
        )
        .await
        .unwrap();
        assert_clean(&ui.text());
        assert!(ui.text().contains("work/app/100000001"), "{}", ui.text());
        let stored = s.store.get_app_secret("work", APP).unwrap();
        assert_eq!(
            stored,
            AppCredentials::new(APP, None, secret("good")).unwrap()
        );
        assert_eq!(s.repo.get("work").unwrap().unwrap().app_secrets, [APP]);

        // `auth status --check` показывает секрет приложения без значения.
        let mut ui = ScriptedUi::default();
        let checker = Verifier::with_base_url(&server.uri());
        let result = auth_status(
            &s.store,
            &s.repo,
            Some(&checker),
            &mut ui,
            &StatusArgs {
                profile: Some("work".into()),
                check: true,
            },
        )
        .await;
        let text = ui.text();
        assert_clean(&text);
        assert!(text.contains("App secrets:  100000001: stored"), "{text}");
        assert!(text.contains("token ok"), "{text}");
        // Клиент команды на этом сервере токена не получит: статус может быть ошибкой, но
        // строка секрета приложения есть в любом случае.
        let _ = result;

        // Выход удаляет и секрет приложения.
        let mut ui = ScriptedUi::default();
        logout(&s.store, &s.repo, &checker, &mut ui, "work", false)
            .await
            .unwrap();
        assert!(s.store.get_app_secret("work", APP).is_err());
        assert!(ui.text().contains("App secret of app `100000001` removed"));
    }

    #[tokio::test]
    async fn bad_input_is_rejected_before_anything_is_stored() {
        let s = setup();
        let mut ui = ScriptedUi::default();
        // Секрет аргументом не принимается.
        let err = set(
            &s.store,
            &s.repo,
            None,
            &mut ui,
            &AppSecretSetArgs {
                secret_argument: Some(Discarded),
                ..args(APP)
            },
            SecretSource::Prompt,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--secret-file"), "{err}");
        // App ID — только цифры.
        let err = set(
            &s.store,
            &s.repo,
            None,
            &mut ui,
            &args("com.example.app"),
            SecretSource::Prompt,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("invalid --app"), "{err}");
        // Нет профиля.
        let err = set(
            &s.store,
            &s.repo,
            None,
            &mut ui,
            &AppSecretSetArgs {
                profile: ProfileArg {
                    profile: "other".into(),
                },
                ..args(APP)
            },
            SecretSource::Prompt,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("auth login --profile other"),
            "{err}"
        );
        // Без терминала секрет не спросить.
        let err = set(
            &s.store,
            &s.repo,
            None,
            &mut ui,
            &args(APP),
            SecretSource::Prompt,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--secret-stdin"), "{err}");
        assert!(s.store.get_app_secret("work", APP).is_err());
    }

    #[tokio::test]
    async fn no_verify_with_client_id_and_remove() {
        let s = setup();
        let mut ui = ScriptedUi::default();
        let mut input = secret("good").into_bytes();
        set(
            &s.store,
            &s.repo,
            None,
            &mut ui,
            &AppSecretSetArgs {
                client_id: Some("2000000002".into()),
                no_verify: true,
                secret_stdin: true,
                ..args(APP)
            },
            SecretSource::Reader(&mut input.as_slice()),
        )
        .await
        .unwrap();
        input.fill(0);
        assert!(ui.text().contains("--no-verify"), "{}", ui.text());
        let stored = s.store.get_app_secret("work", APP).unwrap();
        assert!(!stored.client_id_is_app_id());
        assert!(!ui.text().contains("2000000002"), "{}", ui.text());

        let mut ui = ScriptedUi::default();
        remove(&s.store, &s.repo, &mut ui, "work", APP).unwrap();
        assert!(s.store.get_app_secret("work", APP).is_err());
        assert!(s.repo.get("work").unwrap().unwrap().app_secrets.is_empty());
        // Повторное удаление — не ошибка.
        remove(&s.store, &s.repo, &mut ui, "work", APP).unwrap();
        assert!(ui.text().contains("No app secret"), "{}", ui.text());
    }
}
