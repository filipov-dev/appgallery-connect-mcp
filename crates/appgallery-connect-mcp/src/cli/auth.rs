//! `appgallery-connect-mcp auth` — учётные данные API-клиентов и настройки профилей.
//!
//! Идентификатор клиента — флагом `--client-id` или вопросом в терминале. Секрет — только
//! из файла (`--secret-file`), stdin (`--secret-stdin`) или скрытым вводом в терминале; не
//! аргументом командной строки (там он остался бы в истории оболочки и списке процессов) и
//! не из переменных окружения. Похожие на секрет аргументы (`--secret`, `--client-secret`,
//! лишние позиционные) принимаются скрыто только затем, чтобы отклонить их, не повторив
//! значение в сообщении об ошибке, как это сделал бы `clap`.
//!
//! Учётные данные проверяются пробным получением access token (кроме `--no-verify`) и
//! сохраняются только в системном хранилище. При любой ошибке `auth login` ничего не
//! сохраняет: запись в хранилище — после проверки, а если затем не удалось записать файл
//! профилей, прежняя запись возвращается на место. Команды, которая выводит секрет или
//! токен, нет и быть не должно.

use std::path::PathBuf;

use anyhow::{Context, bail};
use appgallery_connect_api::{
    ApiClientCredentials, CredentialStore, KEYRING_SERVICE, KeyringStore, Region, StoreError,
};
use clap::{Args, Subcommand};

pub use super::ProfileArg;
pub use crate::login::Discarded;
use crate::login::{
    SecretSource, default_apps, default_packages, discard, read_client_id, read_client_secret,
    reject_secret_arguments, restore_stored, store_credentials, stored_message,
};
use crate::profiles::{
    PROFILE_ENV, ProfileRepo, ProfileSettings, describe_region, parse_region, validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};
use crate::verify::Verifier;

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store the AppGallery Connect API client of a profile in the system keychain.
    ///
    /// Create the API client in AppGallery Connect > Users and permissions > API key >
    /// Connect API. The client ID is taken from --client-id or asked in the terminal. The
    /// client secret is read from the file given with --secret-file, from stdin with
    /// --secret-stdin, or typed hidden in the terminal; it is never accepted as a
    /// command-line argument or from environment variables. Spaces and line breaks around
    /// both values are removed. Unless --no-verify, the credentials are checked by getting
    /// an access token in the region of the profile. They are saved only in the system
    /// keychain; profiles.toml gets the region and the default apps, never the client ID,
    /// the secret or the path of the secret file. Replaces the stored credentials of the
    /// profile if there are any. If anything fails, nothing is stored.
    Login(LoginArgs),
    /// Show the profile, its region, the client ID (masked) and whether the credentials are
    /// stored; with --check also get an access token.
    ///
    /// Without --profile, shows every profile in profiles.toml. The secret and the access
    /// token are never printed. Exits with an error if credentials are missing or the check
    /// fails.
    Status(StatusArgs),
    /// Remove the stored API client credentials of the profile from the system keychain and
    /// the profile from profiles.toml.
    Logout(LogoutArgs),
}

#[derive(Debug, Clone, Args)]
pub struct LoginArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Region (site) of the AppGallery Connect account: cn (main site, default), de
    /// (Germany), sg (Singapore) or ru (Russia) [default: the region saved in the profile,
    /// else cn].
    #[arg(long, value_name = "REGION", value_parser = parse_region)]
    pub region: Option<Region>,
    /// Client ID of the API client [default: asked in the terminal].
    #[arg(long, value_name = "ID")]
    pub client_id: Option<String>,
    /// Path to a file that contains only the client secret (the file is read once and its
    /// path is not saved).
    #[arg(long, value_name = "PATH", conflicts_with = "secret_stdin")]
    pub secret_file: Option<PathBuf>,
    /// Read the client secret from stdin, for example from a password manager CLI. Needs
    /// --client-id.
    #[arg(long, requires = "client_id")]
    pub secret_stdin: bool,
    /// Default app ID for the tools (the numeric App ID from App information; repeat or
    /// separate with commas). Replaces the apps saved in the profile.
    #[arg(long = "default-app", value_name = "APP_ID", value_delimiter = ',')]
    pub default_apps: Vec<String>,
    /// Default package name for the tools (repeat or separate with commas). Replaces the
    /// packages saved in the profile.
    #[arg(long = "default-package", value_name = "NAME", value_delimiter = ',')]
    pub default_packages: Vec<String>,
    /// Store the credentials without getting an access token.
    #[arg(long)]
    pub no_verify: bool,
    /// Секрет аргументом: принимается только затем, чтобы отклонить его.
    #[arg(
        long = "secret",
        aliases = ["client-secret", "key"],
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
pub struct StatusArgs {
    /// Profile name [default: every profile in profiles.toml].
    #[arg(long, env = PROFILE_ENV)]
    pub profile: Option<String>,
    /// Also get an access token with the stored credentials.
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, Clone, Args)]
pub struct LogoutArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
}

pub async fn run(command: AuthCommand) -> anyhow::Result<()> {
    let store = KeyringStore;
    let repo = ProfileRepo::open_default()?;
    let mut ui = TerminalUi;
    match command {
        AuthCommand::Login(args) => {
            let verifier = (!args.no_verify).then(Verifier::new);
            // Без постоянной блокировки stdin: вопросы `Ui` читают его же.
            let mut stdin = std::io::stdin();
            let source = match (&args.secret_file, args.secret_stdin) {
                (Some(path), _) => SecretSource::File(path),
                (None, true) => SecretSource::Reader(&mut stdin),
                (None, false) => SecretSource::Prompt,
            };
            login(&store, &repo, verifier.as_ref(), &mut ui, &args, source).await
        }
        AuthCommand::Status(args) => {
            let verifier = args.check.then(Verifier::new);
            status(&store, &repo, verifier.as_ref(), &mut ui, &args).await
        }
        AuthCommand::Logout(args) => logout(&store, &repo, &mut ui, &args.profile.profile),
    }
}

pub async fn login(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    args: &LoginArgs,
    source: SecretSource<'_>,
) -> anyhow::Result<()> {
    reject_secret_arguments(
        "auth login",
        None,
        args.secret_argument.is_some(),
        args.stray.len(),
    )?;
    let profile = args.profile.profile.as_str();
    validate_profile_name(profile)?;
    // Всё, что можно проверить без учётных данных, — до их ввода: ошибка не должна стоить
    // лишнего вопроса или запроса.
    let saved = repo.get(profile)?;
    let mut settings = saved.clone().unwrap_or_default();
    let apps = default_apps(&args.default_apps)?;
    let packages = default_packages(&args.default_packages)?;
    if let Some(region) = args.region {
        settings.region = region;
    }
    if !apps.is_empty() {
        settings.apps = apps;
    }
    if !packages.is_empty() {
        settings.packages = packages;
    }
    let region = settings.region;
    let mut region_line = format!("Region: {}.", describe_region(region));
    if args.region.is_none() && saved.is_none() {
        region_line.push_str(" Pass --region de, sg or ru for an account on another site.");
    }
    ui.say(&region_line);

    let client_id = read_client_id(ui, args.client_id.as_deref())?;
    let client_secret = read_client_secret(ui, source)?;
    let credentials = ApiClientCredentials::new(client_id, client_secret);
    ui.say(&format!("Client ID: {}.", credentials.client_id().masked()));

    match verifier {
        Some(verifier) => {
            verifier.check(region, &credentials).await.map_err(|err| {
                anyhow::anyhow!(
                    "{err:#} Nothing was stored. Re-run with --no-verify to store the \
                         credentials without verification."
                )
            })?;
            ui.say("Credentials work: AppGallery Connect issued an access token.");
        }
        None => ui.say("Skipping the check (--no-verify)."),
    }

    let previous = store_credentials(store, profile, &credentials)?;
    if let Err(err) = repo.upsert(profile, settings) {
        // Профиль не записан — возвращаем хранилище в прежнее состояние.
        if let Err(restore) = restore_stored(store, profile, &previous) {
            return Err(err.context(format!(
                "the profile was not saved, and the keychain could not be restored ({restore})"
            )));
        }
        return Err(err.context("the profile was not saved; the keychain was left unchanged"));
    }
    ui.say(&stored_message(profile, &previous));
    ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        repo.path().display()
    ));
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn list_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_owned()
    } else {
        values.join(", ")
    }
}

pub async fn status(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    args: &StatusArgs,
) -> anyhow::Result<()> {
    let profiles: Vec<(String, Option<ProfileSettings>)> = match &args.profile {
        Some(name) => {
            validate_profile_name(name)?;
            vec![(name.clone(), repo.get(name)?)]
        }
        None => {
            let all = repo.list()?;
            if all.is_empty() {
                ui.say(&format!("No profiles in {}.", repo.path().display()));
                bail!(
                    "no profiles yet: store API client credentials with `appgallery-connect-mcp \
                     auth login --profile <name>`"
                );
            }
            all.into_iter()
                .map(|(name, settings)| (name, Some(settings)))
                .collect()
        }
    };

    let keychain = store.check_available();
    match &keychain {
        Ok(()) => ui.say(&format!(
            "Keychain:     available (service `{KEYRING_SERVICE}`)"
        )),
        Err(err) => ui.say(&format!("Keychain:     not available: {err}")),
    }

    let mut not_ready: Vec<String> = Vec::new();
    for (name, saved) in &profiles {
        ui.say("");
        let problems = profile_status(store, verifier, ui, name, saved.as_ref()).await;
        if !problems.is_empty() {
            not_ready.push(format!("profile `{name}`: {}", problems.join("; ")));
        }
    }
    ui.say("");
    ui.say(&format!("Profiles:     {}", repo.path().display()));

    if !not_ready.is_empty() {
        let fix = match profiles.as_slice() {
            [(name, _)] => format!("appgallery-connect-mcp auth login --profile {name}"),
            _ => "appgallery-connect-mcp auth login --profile <name>".to_owned(),
        };
        bail!("not ready: {}. Fix it with `{fix}`", not_ready.join(". "));
    }
    Ok(())
}

/// Состояние одного профиля. Возвращает список проблем; пустой — профиль готов.
async fn profile_status(
    store: &dyn CredentialStore,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    name: &str,
    saved: Option<&ProfileSettings>,
) -> Vec<String> {
    let settings = saved.cloned().unwrap_or_default();
    let mut problems: Vec<String> = Vec::new();
    let (credentials, state) = match store.get(name) {
        Ok(credentials) => (
            Some(credentials),
            format!("stored in the system keychain (account `{name}`)"),
        ),
        Err(StoreError::NotFound { .. }) => {
            problems.push("no API client credentials in the keychain".into());
            (None, "not found in the system keychain".to_owned())
        }
        Err(err @ StoreError::Corrupted { .. }) => {
            problems.push(err.to_string());
            (
                None,
                "the stored value is not a valid API client ID and secret".to_owned(),
            )
        }
        Err(err) => {
            problems.push("the system keychain is not available".into());
            (None, format!("unavailable ({err})"))
        }
    };

    let check = match (&credentials, verifier) {
        (None, _) => "skipped: the credentials are not available".to_owned(),
        (Some(_), None) => "not checked (pass --check to get an access token)".to_owned(),
        (Some(credentials), Some(verifier)) => {
            match verifier.check(settings.region, credentials).await {
                Ok(()) => "ok, AppGallery Connect issued an access token".to_owned(),
                Err(err) => {
                    problems.push("getting an access token failed".into());
                    format!("failed: {err:#}")
                }
            }
        }
    };

    ui.say(&format!("Profile:      {name}"));
    ui.say(&format!(
        "Region:       {}",
        describe_region(settings.region)
    ));
    ui.say(&format!(
        "Client ID:    {}",
        credentials
            .as_ref()
            .map_or_else(|| "-".to_owned(), |c| c.client_id().masked())
    ));
    ui.say(&format!("Credentials:  {state}"));
    ui.say(&format!("Check:        {check}"));
    ui.say(&format!("Default apps: {}", list_or_dash(&settings.apps)));
    ui.say(&format!(
        "Packages:     {}",
        list_or_dash(&settings.packages)
    ));
    if saved.is_some() {
        // Действующий уровень, как у `serve`: удаление подразумевает запись.
        ui.say(&format!(
            "Write:        {}",
            yes_no(settings.writes_allowed())
        ));
        ui.say(&format!(
            "Delete:       {}",
            yes_no(settings.allow_destructive)
        ));
        let toolsets = if settings.toolsets.is_empty() {
            "default".to_owned()
        } else {
            settings.toolsets.join(",")
        };
        ui.say(&format!("Toolsets:     {toolsets}"));
    } else {
        ui.say("Settings:     none in profiles.toml (region cn, read-only defaults)");
    }
    problems
}

pub fn logout(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    match store.delete(profile) {
        Ok(()) => ui.say(&format!(
            "API client credentials of profile `{profile}` removed from the system keychain. If \
             the API client is no longer needed anywhere, also delete it in AppGallery Connect \
             (Users and permissions > API key > Connect API)."
        )),
        Err(StoreError::NotFound { .. }) => ui.say(&format!(
            "No API client credentials stored for profile `{profile}`."
        )),
        // Хранилище недоступно: профиль не трогаем, чтобы выход можно было повторить.
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "cannot remove the credentials of profile `{profile}`; the profile was kept"
            )));
        }
    }
    if repo
        .remove(profile)
        .with_context(|| format!("cannot remove profile `{profile}`"))?
    {
        ui.say(&format!(
            "Profile `{profile}` removed from {}.",
            repo.path().display()
        ));
    } else {
        ui.say(&format!("Profile `{profile}` has no saved settings."));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
