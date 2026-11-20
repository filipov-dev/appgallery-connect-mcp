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
//!
//! `auth project add|remove` управляет API-клиентами уровня проекта — вторыми учётными
//! данными профиля для серверных API сервисов проекта, которые клиента команды не
//! принимают. Клиент проекта задаётся файлом `agc-apiclient-*.json` (`--credential-file`)
//! или парой `--project-id` + `--client-id` с секретом из файла, stdin или скрытого ввода;
//! проверяется получением токена на хосте региона профиля и сохраняется в хранилище
//! (запись `<профиль>/project/<projectId>`), а идентификатор проекта — в файл профилей.
//! Путь к файлу не запоминается: после этого файл можно удалить.
//!
//! `auth login --oauth` — вход от имени пользователя HUAWEI ID через OAuth-клиент вместо
//! API-клиента ([`oauth`]): профиль получает `type = "oauth"` и команду, хранилище — запись
//! `<профиль>/oauth` с refresh token. `auth logout --forget` отзывает такой вход на сервере
//! HUAWEI ID.
//!
//! `auth app-secret set|remove` управляет секретами приложений (подпись серверных API
//! покупок в приложении, [`app_secret`]).

use std::path::PathBuf;

use anyhow::{Context, bail};
use appgallery_connect_api::{
    ApiClientCredentials, ClientLevel, CredentialStore, KEYRING_SERVICE, KeyringStore, Region,
    StoreError, project_account, validate_project_id,
};
use clap::ArgGroup;
use clap::{Args, Subcommand};

pub use super::ProfileArg;
use crate::browser::{BrowserEnv, decide};
pub use crate::login::Discarded;
use crate::login::{
    Previous, SecretSource, default_apps, default_packages, discard, read_client_id,
    read_client_secret, read_credential_file, read_project_client_id, reject_secret_arguments,
    restore_stored, store_credentials, stored_message,
};
use crate::oauth_login::SIGN_IN_TIMEOUT;
use crate::profiles::{
    PROFILE_ENV, ProfileKind, ProfileRepo, ProfileSettings, describe_region, parse_region,
    validate_profile_name,
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
    ///
    /// With --oauth the profile signs in as a HUAWEI ID user through an OAuth client (an app
    /// with HUAWEI ID for which Huawei approved the Connect API scopes) instead of an API
    /// client: use it to work in a team you were invited to, with the permissions of your
    /// role. The command opens the HUAWEI ID sign-in page in the browser (PKCE, a loopback
    /// redirect to 127.0.0.1), lists the teams of the user and saves the chosen team in the
    /// profile; the OAuth client and the refresh token are saved only in the system keychain.
    Login(LoginArgs),
    /// Show the profile, its region, the client ID (masked) and whether the credentials are
    /// stored; with --check also get an access token.
    ///
    /// Without --profile, shows every profile in profiles.toml. The secret and the access
    /// token are never printed. Exits with an error if credentials are missing or the check
    /// fails.
    Status(StatusArgs),
    /// Remove the stored API client credentials or user sign-in of the profile (and its
    /// project-level API clients) from the system keychain and the profile from
    /// profiles.toml.
    ///
    /// With --forget a user sign-in (auth login --oauth) is also revoked at HUAWEI ID first;
    /// if revoking fails, nothing is removed.
    Logout(LogoutArgs),
    /// Manage the project-level API clients of a profile.
    ///
    /// The server uses the team-level API client of the profile (Project = N/A). The server
    /// APIs of project services (Remote Configuration, App Linking, Auth Service) accept only
    /// a project-level API client: add one per project, and the tools of these services sign
    /// their requests with the client of the project they are called for.
    #[command(subcommand)]
    Project(ProjectCommand),
    /// Manage the app secrets of a profile (HMS In-App Purchases server APIs).
    ///
    /// The tools of the `purchases` toolset call the HMS IAP Order and Subscription services,
    /// which accept only an app-level access token: store the client ID and client secret of
    /// each app whose purchases the tools work with.
    #[command(subcommand)]
    AppSecret(app_secret::AppSecretCommand),
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Store the project-level API client of a project in the system keychain.
    ///
    /// Download the credential file in AppGallery Connect > the project > Project settings >
    /// Server SDK > API client > Download credential and pass it with --credential-file: the
    /// project ID, the client ID and the client secret are read from it, and its region must
    /// match the region of the profile. Or pass --project-id and --client-id, with the client
    /// secret from --secret-file, --secret-stdin or typed hidden in the terminal. Unless
    /// --no-verify, the client is checked by getting an access token in the region of the
    /// profile. The client is saved only in the system keychain and the project ID in
    /// profiles.toml; the path of the file is not saved, so the file can be deleted
    /// afterwards. Replaces the stored client of the project. If anything fails, nothing is
    /// stored. Restart the MCP server to use the client.
    Add(ProjectAddArgs),
    /// Remove the project-level API client of a project from the system keychain and the
    /// project from the profile.
    Remove(ProjectRemoveArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ProjectAddArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Project ID (Project settings > General information) [default: the project_id of
    /// --credential-file].
    #[arg(long, value_name = "ID")]
    pub project_id: Option<String>,
    /// The credential file agc-apiclient-*.json of the project-level API client (read once,
    /// its path is not saved).
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["client_id", "secret_file", "secret_stdin"]
    )]
    pub credential_file: Option<PathBuf>,
    /// Client ID of the project-level API client [default: asked in the terminal].
    #[arg(long, value_name = "ID")]
    pub client_id: Option<String>,
    /// Path to a file that contains only the client secret (read once, its path is not
    /// saved).
    #[arg(long, value_name = "PATH", conflicts_with = "secret_stdin")]
    pub secret_file: Option<PathBuf>,
    /// Read the client secret from stdin. Needs --client-id.
    #[arg(long, requires = "client_id")]
    pub secret_stdin: bool,
    /// Store the client without getting an access token.
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
pub struct ProjectRemoveArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Project ID of the project-level API client to remove.
    #[arg(long, value_name = "ID")]
    pub project_id: String,
}

/// Откуда взять клиента проекта.
pub enum ProjectSource<'a> {
    /// Файл `agc-apiclient-*.json`.
    CredentialFile(&'a std::path::Path),
    /// `--project-id`, `--client-id` и секрет.
    Secret(SecretSource<'a>),
}

#[derive(Debug, Clone, Args)]
#[command(group(ArgGroup::new("client_ids").args(["client_id", "oauth_client_id"])))]
pub struct LoginArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Region (site) of the AppGallery Connect account: cn (main site, default), de
    /// (Germany), sg (Singapore) or ru (Russia) [default: the region saved in the profile,
    /// else cn].
    #[arg(long, value_name = "REGION", value_parser = parse_region)]
    pub region: Option<Region>,
    /// Client ID of the API client [default: asked in the terminal].
    #[arg(long, value_name = "ID", conflicts_with = "oauth")]
    pub client_id: Option<String>,
    /// Path to a file that contains only the client secret (the file is read once and its
    /// path is not saved).
    #[arg(long, value_name = "PATH", conflicts_with = "secret_stdin")]
    pub secret_file: Option<PathBuf>,
    /// Read the client secret from stdin, for example from a password manager CLI. Needs
    /// --client-id (or --oauth-client-id).
    #[arg(long, requires = "client_ids")]
    pub secret_stdin: bool,
    /// Sign in as a HUAWEI ID user through an OAuth client instead of storing an API
    /// client.
    #[arg(long, conflicts_with = "no_verify")]
    pub oauth: bool,
    /// Client ID of the OAuth client (the OAuth 2.0 client ID of the app with HUAWEI ID)
    /// [default: asked in the terminal]. Its client secret comes from --secret-file,
    /// --secret-stdin or the hidden input (Enter for none).
    #[arg(long, value_name = "ID", requires = "oauth")]
    pub oauth_client_id: Option<String>,
    /// Team to work in (its team ID) [default: the only team of the user, else asked].
    #[arg(long, value_name = "ID", requires = "oauth")]
    pub team_id: Option<String>,
    /// Connect API scope to request (publish, develop, report/read, project, product,
    /// product/read, or a full scope URL; repeat or separate with commas) [default: publish,
    /// project]. openid is always requested.
    #[arg(
        long = "oauth-scope",
        value_name = "SCOPE",
        value_delimiter = ',',
        requires = "oauth"
    )]
    pub oauth_scopes: Vec<String>,
    /// Fixed redirect URL of the OAuth client, like http://127.0.0.1:8765/ [default: a
    /// random port on 127.0.0.1].
    #[arg(long, value_name = "URL", requires = "oauth")]
    pub redirect_uri: Option<String>,
    /// Do not open a browser: print the sign-in link and paste the address the browser
    /// returns to.
    #[arg(long, requires = "oauth")]
    pub no_browser: bool,
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
    /// Also revoke the user sign-in (auth login --oauth) at HUAWEI ID.
    #[arg(long)]
    pub forget: bool,
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
            if args.oauth {
                let env = oauth::SignInEnv {
                    browser: decide(args.no_browser, &BrowserEnv::current()),
                    timeout: SIGN_IN_TIMEOUT,
                };
                let verifier = Verifier::new();
                return oauth::login(&store, &repo, &verifier, &mut ui, &args, source, env).await;
            }
            login(&store, &repo, verifier.as_ref(), &mut ui, &args, source).await
        }
        AuthCommand::Status(args) => {
            let verifier = args.check.then(Verifier::new);
            status(&store, &repo, verifier.as_ref(), &mut ui, &args).await
        }
        AuthCommand::Logout(args) => {
            logout(
                &store,
                &repo,
                &Verifier::new(),
                &mut ui,
                &args.profile.profile,
                args.forget,
            )
            .await
        }
        AuthCommand::Project(ProjectCommand::Add(args)) => {
            let verifier = (!args.no_verify).then(Verifier::new);
            let mut stdin = std::io::stdin();
            let source = match (&args.credential_file, &args.secret_file, args.secret_stdin) {
                (Some(path), _, _) => ProjectSource::CredentialFile(path),
                (None, Some(path), _) => ProjectSource::Secret(SecretSource::File(path)),
                (None, None, true) => ProjectSource::Secret(SecretSource::Reader(&mut stdin)),
                (None, None, false) => ProjectSource::Secret(SecretSource::Prompt),
            };
            project_add(&store, &repo, verifier.as_ref(), &mut ui, &args, source).await
        }
        AuthCommand::Project(ProjectCommand::Remove(args)) => project_remove(
            &store,
            &repo,
            &mut ui,
            &args.profile.profile,
            &args.project_id,
        ),
        AuthCommand::AppSecret(app_secret::AppSecretCommand::Set(args)) => {
            let verifier = (!args.no_verify).then(Verifier::new);
            let mut stdin = std::io::stdin();
            let source = match (&args.secret_file, args.secret_stdin) {
                (Some(path), _) => SecretSource::File(path),
                (None, true) => SecretSource::Reader(&mut stdin),
                (None, false) => SecretSource::Prompt,
            };
            app_secret::set(&store, &repo, verifier.as_ref(), &mut ui, &args, source).await
        }
        AuthCommand::AppSecret(app_secret::AppSecretCommand::Remove(args)) => {
            app_secret::remove(&store, &repo, &mut ui, &args.profile.profile, &args.app)
        }
    }
}

/// Проверяет идентификатор проекта из флага (пробелы по краям обрезаются).
fn project_id_arg(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    validate_project_id(value).map_err(|err| anyhow::anyhow!("invalid --project-id: {err}"))?;
    Ok(value.to_owned())
}

pub async fn project_add(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    args: &ProjectAddArgs,
    source: ProjectSource<'_>,
) -> anyhow::Result<()> {
    reject_secret_arguments(
        "auth project add",
        None,
        args.secret_argument.is_some(),
        args.stray.len(),
    )?;
    let profile = args.profile.profile.as_str();
    validate_profile_name(profile)?;
    let flag_project = args.project_id.as_deref().map(project_id_arg).transpose()?;
    // Клиент проекта — вторые учётные данные профиля: регион и сам профиль уже должны быть.
    let Some(mut settings) = repo.get(profile)? else {
        bail!(
            "profile `{profile}` does not exist: store its team-level API client first with \
             `appgallery-connect-mcp auth login --profile {profile}`, then add the project-level \
             API client to it"
        );
    };
    let region = settings.region;

    let from_file = matches!(source, ProjectSource::CredentialFile(_));
    let (project_id, credentials) = match source {
        ProjectSource::CredentialFile(path) => {
            let file = read_credential_file(path)?;
            if file.level == ClientLevel::Team {
                bail!(
                    "`{}` is the credential file of a team-level API client (type \
                     team_client_id): store it as the main client of a profile with \
                     `appgallery-connect-mcp auth login`; `auth project add` needs the file of \
                     a project-level API client (type project_client_id). Nothing was stored",
                    path.display()
                );
            }
            let Some(project_id) = file.project_id else {
                bail!("the credential file has no project ID; nothing was stored");
            };
            if let Some(flag) = &flag_project
                && flag != &project_id
            {
                bail!(
                    "--project-id {flag} does not match the project `{project_id}` of the \
                     credential file; nothing was stored"
                );
            }
            if let Some(file_region) = file.region
                && file_region != region
            {
                bail!(
                    "the project's data processing location is {} ({}), but profile `{profile}` \
                     uses region {}: a project-level API client gets its token only on the host \
                     of its region. Use a profile in region {} (`auth login --region {}`); \
                     nothing was stored",
                    file_region.slug(),
                    file_region.title(),
                    region.slug(),
                    file_region.slug(),
                    file_region.slug()
                );
            }
            (project_id, file.credentials)
        }
        ProjectSource::Secret(secret) => {
            let Some(project_id) = flag_project else {
                bail!(
                    "pass --credential-file <agc-apiclient-*.json>, or --project-id <ID> (Project \
                     settings > General information > Project ID) with --client-id"
                );
            };
            let client_id = read_project_client_id(ui, args.client_id.as_deref())?;
            let client_secret = read_client_secret(ui, secret)?;
            (
                project_id,
                ApiClientCredentials::new(client_id, client_secret),
            )
        }
    };
    ui.say(&format!(
        "Project: {project_id}. Client ID: {}. Region: {}.",
        credentials.client_id().masked(),
        describe_region(region)
    ));

    match verifier {
        Some(verifier) => {
            verifier.check(region, &credentials).await.map_err(|err| {
                anyhow::anyhow!(
                    "{err:#} Nothing was stored. Re-run with --no-verify to store the client \
                     without verification."
                )
            })?;
            ui.say(
                "The project-level API client works: AppGallery Connect issued an access token.",
            );
        }
        None => ui.say("Skipping the check (--no-verify)."),
    }

    let account = project_account(profile, &project_id);
    let previous = store_credentials(store, &account, &credentials)?;
    if !settings.projects.contains(&project_id) {
        settings.projects.push(project_id.clone());
    }
    if let Err(err) = repo.upsert(profile, settings) {
        if let Err(restore) = restore_stored(store, &account, &previous) {
            return Err(err.context(format!(
                "the profile was not saved, and the keychain could not be restored ({restore})"
            )));
        }
        return Err(err.context("the profile was not saved; the keychain was left unchanged"));
    }
    ui.say(&format!(
        "Project-level API client of project `{project_id}` {} in the system keychain (service \
         `{KEYRING_SERVICE}`, account `{account}`); project added to profile `{profile}` in {}.",
        if matches!(previous, Previous::None) {
            "saved"
        } else {
            "replaced"
        },
        repo.path().display()
    ));
    if from_file {
        ui.say(
            "The credential file is not needed any more: delete it, or keep it only where the \
             Server SDK reads it. Never commit it.",
        );
    }
    ui.say("Restart the MCP server of the profile to use the client.");
    Ok(())
}

pub fn project_remove(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
    project_id: &str,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let project_id = project_id_arg(project_id)?;
    match store.delete_project(profile, &project_id) {
        Ok(()) => ui.say(&format!(
            "Project-level API client of project `{project_id}` removed from the system \
             keychain. If it is no longer needed anywhere, also delete it in AppGallery Connect."
        )),
        Err(StoreError::ProjectNotFound { .. }) => ui.say(&format!(
            "No project-level API client of project `{project_id}` is stored for profile \
             `{profile}`."
        )),
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "cannot remove the client of project `{project_id}`; the profile was kept"
            )));
        }
    }
    if let Some(mut settings) = repo.get(profile)?
        && settings.projects.contains(&project_id)
    {
        settings.projects.retain(|id| id != &project_id);
        repo.upsert(profile, settings)
            .with_context(|| format!("cannot update profile `{profile}`"))?;
        ui.say(&format!(
            "Project `{project_id}` removed from profile `{profile}`."
        ));
    }
    Ok(())
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
    if args.oauth {
        bail!("internal error: `auth login --oauth` goes through the user sign-in");
    }
    let saved = repo.get(profile)?;
    let mut settings = saved.clone().unwrap_or_default();
    let was_user = settings.kind == ProfileKind::Oauth;
    // У профиля один способ входа: API-клиент заменяет вход пользователя.
    settings.kind = ProfileKind::ApiClient;
    settings.team_id = None;
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
    if was_user {
        oauth::forget_after_api_client_login(store, ui, profile);
    }
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
    let mut team_problems = false;
    for (name, saved) in &profiles {
        ui.say("");
        let (problems, projects) = profile_status(store, verifier, ui, name, saved.as_ref()).await;
        team_problems |= !problems.is_empty();
        let all: Vec<String> = problems.into_iter().chain(projects).collect();
        if !all.is_empty() {
            not_ready.push(format!("profile `{name}`: {}", all.join("; ")));
        }
    }
    ui.say("");
    ui.say(&format!("Profiles:     {}", repo.path().display()));

    if !not_ready.is_empty() {
        if !team_problems {
            // Проблемы только у клиентов проектов и секретов приложений: их тексты уже
            // называют `auth project add` и `auth app-secret set`.
            bail!("not ready: {}", not_ready.join(". "));
        }
        let fix = match profiles.as_slice() {
            [(name, Some(settings))] if settings.kind == ProfileKind::Oauth => {
                format!("appgallery-connect-mcp auth login --oauth --profile {name}")
            }
            [(name, _)] => format!("appgallery-connect-mcp auth login --profile {name}"),
            _ => "appgallery-connect-mcp auth login [--oauth] --profile <name>".to_owned(),
        };
        bail!("not ready: {}. Fix it with `{fix}`", not_ready.join(". "));
    }
    Ok(())
}

/// Состояние одного профиля. Возвращает проблемы клиента команды (или входа пользователя)
/// и клиентов проектов; оба списка пустые — профиль готов.
async fn profile_status(
    store: &dyn CredentialStore,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    name: &str,
    saved: Option<&ProfileSettings>,
) -> (Vec<String>, Vec<String>) {
    let settings = saved.cloned().unwrap_or_default();
    ui.say(&format!("Profile:      {name}"));
    ui.say(&format!(
        "Region:       {}",
        describe_region(settings.region)
    ));
    let problems = match settings.kind {
        ProfileKind::ApiClient => api_client_status(store, verifier, ui, name, &settings).await,
        ProfileKind::Oauth => oauth::status(store, verifier, ui, name, &settings).await,
    };
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
    let mut project_problems = projects_status(store, verifier, ui, name, &settings).await;
    project_problems.extend(app_secret::status(store, verifier, ui, name, &settings).await);
    (problems, project_problems)
}

/// Строки `auth status` об API-клиенте профиля. Возвращает проблемы.
async fn api_client_status(
    store: &dyn CredentialStore,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    name: &str,
    settings: &ProfileSettings,
) -> Vec<String> {
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

    ui.say(&format!(
        "Client ID:    {}",
        credentials
            .as_ref()
            .map_or_else(|| "-".to_owned(), |c| c.client_id().masked())
    ));
    ui.say(&format!("Credentials:  {state}"));
    ui.say(&format!("Check:        {check}"));
    problems
}

/// Клиенты проектов профиля: маска идентификатора клиента, есть ли запись и, с `--check`,
/// получение токена. Возвращает проблемы с командой `auth project add`.
async fn projects_status(
    store: &dyn CredentialStore,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    name: &str,
    settings: &ProfileSettings,
) -> Vec<String> {
    if settings.projects.is_empty() {
        ui.say("Projects:     -");
        return Vec::new();
    }
    let mut problems = Vec::new();
    for (index, project_id) in settings.projects.iter().enumerate() {
        let fix = format!(
            "`appgallery-connect-mcp auth project add --profile {name} --project-id {project_id}`"
        );
        let line = match store.get_project(name, project_id) {
            Ok(credentials) => {
                let check = match verifier {
                    None => String::new(),
                    Some(verifier) => match verifier.check(settings.region, &credentials).await {
                        Ok(()) => ", token ok".to_owned(),
                        Err(err) => {
                            problems.push(format!(
                                "getting an access token for project `{project_id}` failed \
                                 (check its client, then {fix})"
                            ));
                            format!(", token failed: {err:#}")
                        }
                    },
                };
                format!(
                    "{project_id}: client {}, stored (account `{}`){check}",
                    credentials.client_id().masked(),
                    project_account(name, project_id)
                )
            }
            Err(StoreError::ProjectNotFound { .. }) => {
                problems.push(format!(
                    "no project-level API client of project `{project_id}` in the keychain \
                     ({fix})"
                ));
                format!("{project_id}: not found in the system keychain")
            }
            Err(StoreError::ProjectCorrupted { .. }) => {
                problems.push(format!(
                    "the stored client of project `{project_id}` is not valid ({fix})"
                ));
                format!("{project_id}: the stored value is not a valid API client ID and secret")
            }
            Err(err) => {
                problems.push(format!(
                    "the client of project `{project_id}` cannot be read"
                ));
                format!("{project_id}: unavailable ({err})")
            }
        };
        let label = if index == 0 { "Projects:" } else { "" };
        ui.say(&format!("{label:<14}{line}"));
    }
    problems
}

pub async fn logout(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    verifier: &Verifier,
    ui: &mut dyn Ui,
    profile: &str,
    forget: bool,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let settings = repo.get(profile)?;
    // Вход пользователя — первым: если отзыв (`--forget`) не удался, ничего не удаляется.
    let had_sign_in = oauth::logout(store, verifier, ui, profile, forget).await?;
    if forget && !had_sign_in {
        ui.say(&format!(
            "Profile `{profile}` has no user sign-in to revoke (--forget applies to `auth login \
             --oauth`)."
        ));
    }
    // Затем клиенты проектов и секреты приложений: их списки — в файле профилей, который
    // удаляется ниже.
    let (projects, apps) = settings
        .map(|settings| (settings.projects, settings.app_secrets))
        .unwrap_or_default();
    app_secret::remove_all(store, ui, profile, &apps)?;
    for project_id in &projects {
        match store.delete_project(profile, project_id) {
            Ok(()) => ui.say(&format!(
                "Project-level API client of project `{project_id}` removed from the system \
                 keychain."
            )),
            Err(StoreError::ProjectNotFound { .. }) => {}
            Err(err) => {
                return Err(anyhow::Error::new(err).context(format!(
                    "cannot remove the client of project `{project_id}` of profile `{profile}`; \
                     the profile was kept"
                )));
            }
        }
    }
    match store.delete(profile) {
        Ok(()) => ui.say(&format!(
            "API client credentials of profile `{profile}` removed from the system keychain. If \
             the API client is no longer needed anywhere, also delete it in AppGallery Connect \
             (Users and permissions > API key > Connect API)."
        )),
        Err(StoreError::NotFound { .. }) if had_sign_in => {}
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

pub mod app_secret;
mod oauth;

pub(crate) use oauth::save_rotated;

#[cfg(test)]
mod tests;
