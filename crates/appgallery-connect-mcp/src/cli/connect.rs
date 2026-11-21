//! `appgallery-connect-mcp connect` — подключение сервера к MCP-клиенту одной командой.
//!
//! Сценарий: учётные данные API-клиента (сохранённые рабочие используются как есть; иначе —
//! флаги или вопросы в терминале с пошаговой подсказкой, пробное получение токена, запись в
//! системное хранилище, как у `auth login`) → настройки в `profiles.toml` (права — только
//! если заданы флаги доступа; на macOS повышение прав одобряется Touch ID или паролем) →
//! запись сервера в конфигурацию клиента после подтверждения, без флагов доступа.
//!
//! Проверка доступа — только получение access token. Лёгкого GET без обязательного `appId`
//! или имени пакета, который справочник разрешает API-клиенту, в таблице операций нет
//! (кроме списка устройств Provisioning API, доступного не каждому клиенту), поэтому
//! пробного вызова чтения, как в `play-console-mcp`, здесь нет.
//!
//! `--silent` — тихий режим: ни вопросов, ни вывода, кроме ошибок; запись в клиент — как с
//! `--yes` (чужую запись без `--force` не заменяет).
//!
//! Профиль со входом пользователя (`auth login --oauth`) `connect` не создаёт и не меняет
//! его вход: рабочий сохранённый вход используется как есть (проверка — обновление access
//! token и команда в списке команд), иначе — ошибка с подсказкой `auth login --oauth`.
//! Скрытый флаг `--oauth` отклоняется с той же подсказкой.
//!
//! Секрет не проходит через агента и не попадает ни в аргументы, ни в окружение, ни в
//! конфигурацию клиента: клиент получает только команду
//! `appgallery-connect-mcp serve --profile <профиль> ...`.

use std::path::PathBuf;

use anyhow::{Context, bail};
use appgallery_connect_api::spec::{find_toolset, toolsets};
use appgallery_connect_api::{
    ApiClientCredentials, ClientId, CredentialStore, KeyringStore, Region, StoreError,
};
use clap::Args;
use mcp_kit::approval::{self, ApprovalStore, Presence};
use mcp_kit::{AccessLevel, ProfileAccess};

use super::ProfileArg;
use crate::clients::claude::{self, CommandRunner, SystemRunner};
use crate::clients::{
    ClientKind, EXE_STEM, MAX_SERVER_NAME_LEN, PROFILE_SERVER_PREFIX, RegisterOptions,
    Registration, Scope, ServeFlags, ServerSpec, codex, default_server_name,
};
use crate::login::{
    Discarded, SecretSource, WHERE_TO_FIND, default_apps, default_packages, discard,
    read_client_id, read_client_secret, reject_secret_arguments, restore_stored, store_credentials,
    stored_message,
};
use crate::profiles::{
    ProfileKind, ProfileRepo, ProfileSettings, describe_region, parse_region, profile_team,
    validate_profile_name,
};
use crate::ui::{SilentUi, TerminalUi, Ui};
use crate::verify::Verifier;

const CONNECT_HELP: &str = "\
The server reads the AppGallery Connect API client (client ID and key) of the profile from the \
system keychain. If the profile has no working credentials yet, connect explains step by step \
how to create an API client in AppGallery Connect (Users and permissions > API key > Connect \
API) and asks for the client ID and the key; the key is typed hidden, or read from \
--secret-file or --secret-stdin, never from a command-line argument or the environment. The \
credentials are checked by getting an access token in the region of the profile and are \
stored only in the keychain. If the profile already has working credentials (for example, \
after `auth login`), they are reused: re-running connect only changes the settings and the \
client registration. Pass --client-id, --secret-file or --secret-stdin to replace them.

The client config gets only the command `appgallery-connect-mcp serve --profile <profile> ...` \
without access flags; never the client ID, the key or tokens. The access (the level and the \
extra directories for local files) is a setting of the profile: --allow-write, \
--allow-destructive and --allow-path change it (without them the profile keeps its access, \
read-only for a new profile), and `appgallery-connect-mcp ui` changes it later. On macOS, \
raising the access asks for Touch ID or the macOS password; access that is not approved does \
not take effect. Re-running connect also rewrites a registration of an older version that had \
access flags.

By default the server is registered with --dynamic-toolsets: the agent starts with the search, \
request and toolset tools and enables the toolsets it needs, which keeps the tool list of the \
client short. --no-dynamic-toolsets lists every enabled tool at start instead. Either way the \
access of the profile limits what the enabled toolsets register.

One profile is one API client of one AppGallery Connect account. The server name in the client \
is appgallery for the default profile and agc-<profile> for others; it may not be longer than \
17 characters, so that tool names mcp__<server>__<tool> stay within the 64-character limit of \
clients.

Re-running connect with the same settings changes nothing. An entry with the same name that \
runs another server or another profile is replaced only after an explicit answer or with \
--force. --dry-run shows what would change and changes nothing.

A profile that signs in as a HUAWEI ID user (`auth login --oauth`) is connected with its stored \
sign-in: connect checks it (unless --no-verify) and never signs in itself; run `auth login \
--oauth --profile <profile>` first. The API client flags are refused for such a profile.

--silent is for scripts and agents: no questions and no output except errors. It implies --yes \
(an entry of another server is still replaced only with --force) and needs --client-id with \
--secret-file or --secret-stdin unless the profile already has working credentials.";

#[derive(Debug, Clone, Args)]
#[command(after_help = CONNECT_HELP)]
pub struct ConnectArgs {
    /// MCP client to register the server in.
    #[arg(value_enum)]
    pub client: ClientKind,
    #[command(flatten)]
    pub profile: ProfileArg,
    /// MCP server name in the client config, up to 17 characters. Default: appgallery for the
    /// default profile, agc-<profile> for others.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Where to register the server in the client. Codex supports only `user`.
    #[arg(long, value_enum, default_value_t = Scope::User)]
    pub scope: Scope,
    /// Region (site) of the AppGallery Connect account: cn (main site), de (Germany), sg
    /// (Singapore) or ru (Russia) [default: the region saved in the profile, else cn].
    #[arg(long, value_name = "REGION", value_parser = parse_region)]
    pub region: Option<Region>,
    /// Client ID of the API client. Replaces the stored credentials of the profile [default:
    /// asked in the terminal when the profile has no working credentials].
    #[arg(long, value_name = "ID")]
    pub client_id: Option<String>,
    /// Path to a file that contains only the client secret (the key of the API client). The
    /// file is read once and its path is not saved. Replaces the stored credentials.
    #[arg(long, value_name = "PATH", conflicts_with = "secret_stdin")]
    pub secret_file: Option<PathBuf>,
    /// Read the client secret from stdin. Needs --client-id; replaces the stored credentials.
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
    /// Set the access of the profile to write: tools that create or modify data in
    /// AppGallery Connect.
    #[arg(long)]
    pub allow_write: bool,
    /// Set the access of the profile to destructive: also tools with irreversible or
    /// user-visible effects: deletions, submitting for review, releases, prices and
    /// availability (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable (passed to `serve --toolsets`). Default: the
    /// default set of `serve`.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Register the server without --dynamic-toolsets: all enabled tools are listed at
    /// start instead of being loaded by the agent on demand.
    #[arg(long)]
    pub no_dynamic_toolsets: bool,
    /// Set the extra directories of the profile: the server may read files from and write
    /// files to them, besides the working and temporary directories (saved as absolute
    /// paths; repeat for several directories). Replaces the directories of the profile.
    #[arg(long = "allow-path", value_name = "DIR")]
    pub allow_paths: Vec<PathBuf>,
    /// Do not ask for confirmation before changing the client config.
    ///
    /// An entry with the same name that runs another server or another profile is never
    /// replaced with --yes alone; add --force for that.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Replace an entry with the same name even if it runs another server or another
    /// profile.
    #[arg(long)]
    pub force: bool,
    /// Show what would change in the keychain, the profile and the client config, and change
    /// nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Store new credentials without getting an access token, and keep stored ones unchecked.
    #[arg(long)]
    pub no_verify: bool,
    /// Silent mode for scripts and agents: no questions and no output except errors. Implies
    /// --yes; needs --client-id with --secret-file or --secret-stdin unless the profile has
    /// working credentials.
    #[arg(long, conflicts_with = "dry_run")]
    pub silent: bool,
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
    /// Вход пользователя здесь не делается: флаг принимается, чтобы подсказать `auth login
    /// --oauth`.
    #[arg(long, hide = true)]
    pub oauth: bool,
}

impl ConnectArgs {
    fn register_options(&self) -> RegisterOptions {
        RegisterOptions {
            yes: self.yes || self.silent,
            force: self.force,
            dry_run: self.dry_run,
        }
    }

    /// Новые учётные данные заданы флагами: сохранённые заменяются, даже если работают.
    fn new_credentials_requested(&self) -> bool {
        self.client_id.is_some() || self.secret_file.is_some() || self.secret_stdin
    }
}

pub async fn run(args: ConnectArgs) -> anyhow::Result<()> {
    let exe =
        std::env::current_exe().context("cannot determine the path of appgallery-connect-mcp")?;
    let codex_home = match args.client {
        ClientKind::Codex => codex::default_codex_home()?,
        ClientKind::ClaudeCode => PathBuf::new(),
    };
    let verifier = (!args.no_verify).then(Verifier::new);
    let (mut terminal, mut silent) = (TerminalUi, SilentUi);
    let ui: &mut dyn Ui = if args.silent {
        &mut silent
    } else {
        &mut terminal
    };
    // Без постоянной блокировки stdin: вопросы `Ui` читают его же.
    let mut stdin = std::io::stdin();
    let source = match (&args.secret_file, args.secret_stdin) {
        (Some(path), _) => SecretSource::File(path),
        (None, true) => SecretSource::Reader(&mut stdin),
        (None, false) => SecretSource::Prompt,
    };
    let repo = ProfileRepo::open_default()?;
    let approvals = approval::platform_store(EXE_STEM);
    let presence = approval::platform_presence();
    let mut env = ConnectEnv {
        approval: approvals.as_deref().zip(presence.as_deref()),
        store: &KeyringStore,
        repo: &repo,
        verifier: verifier.as_ref(),
        runner: &mut SystemRunner,
        ui,
        exe,
        codex_home,
    };
    connect(&mut env, &args, source).await
}

/// Зависимости `connect`; в тестах подменяются хранилищем в памяти, временными каталогами,
/// `wiremock` и записью вызовов `claude`.
pub struct ConnectEnv<'a> {
    /// Одобрение прав (macOS); `None` — платформа без одобрения.
    pub approval: Option<(&'a dyn ApprovalStore, &'a dyn Presence)>,
    pub store: &'a dyn CredentialStore,
    pub repo: &'a ProfileRepo,
    /// Проверка учётных данных; `None` — `--no-verify`.
    pub verifier: Option<&'a Verifier>,
    pub runner: &'a mut dyn CommandRunner,
    pub ui: &'a mut dyn Ui,
    pub exe: PathBuf,
    pub codex_home: PathBuf,
}

/// Наборы инструментов из флага: без пустых и повторов, только известные.
fn normalize_toolsets(values: &[String]) -> anyhow::Result<Vec<String>> {
    let mut result: Vec<String> = Vec::new();
    for value in values {
        let slug = value.trim();
        if slug.is_empty() || result.iter().any(|known| known == slug) {
            continue;
        }
        let valid_chars = slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
        if !valid_chars {
            bail!("invalid toolset `{slug}`: use lowercase letters, digits, `_` and `-`");
        }
        if find_toolset(slug).is_none() {
            let known: Vec<&str> = toolsets().iter().map(|toolset| toolset.slug).collect();
            bail!("unknown toolset `{slug}`; available: {}", known.join(", "));
        }
        result.push(slug.to_owned());
    }
    Ok(result)
}

/// Каталоги `--allow-path`: абсолютные (сервер запускается из другого каталога),
/// существующие, без повторов. Канонизирует их сохранение прав ([`approval::save_access_with`]).
fn normalize_allow_paths(values: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut result: Vec<PathBuf> = Vec::new();
    for value in values {
        let absolute = std::path::absolute(value)
            .with_context(|| format!("cannot resolve --allow-path `{}`", value.display()))?;
        if !absolute.is_dir() {
            bail!(
                "--allow-path `{}` is not an existing directory",
                absolute.display()
            );
        }
        if !result.contains(&absolute) {
            result.push(absolute);
        }
    }
    Ok(result)
}

/// Имя сервера: `--name` или имя по умолчанию для профиля. Слишком длинное имя по умолчанию
/// — ошибка про имя профиля, до любых вопросов и действий.
fn server_name(args: &ConnectArgs, profile: &str) -> anyhow::Result<String> {
    if let Some(name) = &args.name {
        return Ok(name.clone());
    }
    let name = default_server_name(profile);
    if name.len() > MAX_SERVER_NAME_LEN {
        bail!(
            "profile name `{profile}` is too long for the default server name: `{name}` is \
             longer than {MAX_SERVER_NAME_LEN} characters (clients name tools \
             mcp__<server>__<tool> and limit that to 64 characters). Use a profile name of up \
             to {} characters, or choose the server name with --name",
            MAX_SERVER_NAME_LEN - PROFILE_SERVER_PREFIX.len()
        );
    }
    Ok(name)
}

/// Права профиля после `connect`: уровень из флагов (иначе прежний) и каталоги из
/// `--allow-path` (иначе прежние). `None` — флагов доступа нет, права не меняются.
fn requested_access(
    level: Option<AccessLevel>,
    allow_path: &[PathBuf],
    current: &ProfileAccess,
) -> Option<ProfileAccess> {
    if level.is_none() && allow_path.is_empty() {
        return None;
    }
    Some(ProfileAccess::new(
        level.unwrap_or(current.level),
        if allow_path.is_empty() {
            current.allow_path.clone()
        } else {
            allow_path.to_vec()
        },
    ))
}

/// Меняет права профиля. На macOS повышение сначала одобряется владельцем; отказ не
/// останавливает `connect`: профиль сохраняет прежние права, учётные данные и регистрация
/// сохраняются.
fn change_access(
    ui: &mut dyn Ui,
    approval: Option<(&dyn ApprovalStore, &dyn Presence)>,
    profile: &str,
    settings: &mut ProfileSettings,
    access: &ProfileAccess,
) {
    let before = settings.access();
    let what = format!("{EXE_STEM} profile \"{profile}\"");
    match approval::save_access_with(approval, profile, &what, access, |access| {
        settings.set_access(access);
        Ok(())
    }) {
        Ok(saved) if saved.prompted => ui.say(&format!(
            "Access `{}` approved on this Mac.",
            saved.access.summary()
        )),
        Ok(_) => {}
        Err(err) => ui.say(&format!(
            "The access was not changed: {err}. The profile keeps `{}` access; change it later \
             with `appgallery-connect-mcp ui` (profile `{profile}`).",
            before.summary()
        )),
    }
}

pub async fn connect(
    env: &mut ConnectEnv<'_>,
    args: &ConnectArgs,
    source: SecretSource<'_>,
) -> anyhow::Result<()> {
    reject_secret_arguments(
        "connect",
        Some("the client name"),
        args.secret_argument.is_some(),
        args.stray.len(),
    )?;
    let client = args.client;
    let profile = args.profile.profile.as_str();
    validate_profile_name(profile)?;
    if args.oauth {
        bail!(
            "connect does not sign in users: run `appgallery-connect-mcp auth login --oauth \
             --profile {profile}` first, then `appgallery-connect-mcp connect {} --profile \
             {profile}` reuses the sign-in",
            client.name()
        );
    }

    // Всё, что можно проверить без вопросов, — до первого вопроса пользователю.
    let flags = ServeFlags {
        toolsets: normalize_toolsets(&args.toolsets)?,
        dynamic_toolsets: !args.no_dynamic_toolsets,
    };
    let requested_level = AccessLevel::requested_by_flags(args.allow_write, args.allow_destructive);
    let allow_paths = normalize_allow_paths(&args.allow_paths)?;
    let name = server_name(args, profile)?;
    let spec = ServerSpec::new(&name, &env.exe, profile, &flags)?;
    let apps = default_apps(&args.default_apps)?;
    let packages = default_packages(&args.default_packages)?;
    let saved = env.repo.get(profile)?;
    let mut settings = saved.clone().unwrap_or_default();
    let user_sign_in = settings.kind == ProfileKind::Oauth;
    if user_sign_in {
        if args.new_credentials_requested() {
            bail!(
                "profile `{profile}` signs in as a HUAWEI ID user (auth login --oauth): connect \
                 does not replace the sign-in with an API client. Drop --client-id, \
                 --secret-file and --secret-stdin, or switch the profile with \
                 `appgallery-connect-mcp auth login --profile {profile}` first"
            );
        }
        if let Some(region) = args.region
            && region != settings.region
        {
            bail!(
                "the region of profile `{profile}` comes from the site of its team ({}); \
                 --region {} does not match it. Sign in again with `appgallery-connect-mcp auth \
                 login --oauth --profile {profile}` to choose another team",
                settings.region.slug(),
                region.slug()
            );
        }
    }
    if let Some(region) = args.region {
        settings.region = region;
    }
    if !apps.is_empty() {
        settings.apps = apps;
    }
    if !packages.is_empty() {
        settings.packages = packages;
    }
    match client {
        ClientKind::ClaudeCode => claude::ensure_available(env.runner)?,
        ClientKind::Codex => codex::ensure_scope(args.scope)?,
    }

    let region = settings.region;
    let mut region_line = format!("Region: {}.", describe_region(region));
    if args.region.is_none() && saved.is_none() {
        region_line.push_str(" Pass --region de, sg or ru for an account on another site.");
    }
    env.ui.say(&region_line);

    if user_sign_in {
        // Вход пользователя проверяется до записи чего бы то ни было.
        user_sign_in_works(env, profile, &settings).await?;
    }
    if args.dry_run {
        if !user_sign_in {
            dry_run_credentials(env, args, profile, region, source).await?;
        }
        let current = settings.access();
        let access = match requested_access(requested_level, &allow_paths, &current) {
            Some(access) => format!(
                "would get access `{}`{}",
                access.summary(),
                if env.approval.is_some() && !access.is_read_only() {
                    " (raising it asks for Touch ID or the macOS password)"
                } else {
                    ""
                }
            ),
            None => format!("keeps access `{}`", current.summary()),
        };
        env.ui.say(&format!(
            "Profile `{profile}` in {} {access}, toolsets: {}.",
            env.repo.path().display(),
            if flags.toolsets.is_empty() {
                "the default set".to_owned()
            } else {
                flags.toolsets.join(", ")
            }
        ));
    } else {
        // Учётные данные проверяются до записи чего бы то ни было: при ошибке проверки
        // ничего не сохраняется и клиент не меняется.
        let new_credentials = if user_sign_in {
            None
        } else {
            profile_credentials(env, args, profile, region, source).await?
        };
        settings.client = Some(client.name().to_owned());
        if let Some(access) = requested_access(requested_level, &allow_paths, &settings.access()) {
            change_access(env.ui, env.approval, profile, &mut settings, &access);
        }
        settings.toolsets = flags.toolsets.clone();
        settings.dynamic_toolsets = flags.dynamic_toolsets;
        save(env, profile, settings, new_credentials.as_ref())?;
    }

    let options = args.register_options();
    let outcome = match client {
        ClientKind::ClaudeCode => claude::register(env.runner, env.ui, args.scope, &spec, options)?,
        ClientKind::Codex => codex::register(env.ui, &env.codex_home, &spec, options)?,
    };
    let display_name = client.display_name();
    match outcome {
        Registration::Changed => env.ui.say(&format!(
            "Done: {display_name} now runs `{}` as MCP server `{name}`. Restart the client to \
             pick it up.",
            spec.display()
        )),
        Registration::UpToDate if !args.dry_run => env.ui.say(&format!(
            "{display_name} already runs `{}` as MCP server `{name}`. Restart the client if it \
             does not show the server yet.",
            spec.display()
        )),
        Registration::Declined => env.ui.say(&format!(
            "{display_name} config was not changed. The credentials and the profile are kept; \
             re-run `appgallery-connect-mcp connect {} --profile {profile}` to register later.",
            client.name()
        )),
        Registration::UpToDate | Registration::DryRun => {}
    }
    if args.dry_run {
        env.ui.say(
            "Dry run: nothing was changed (keychain, profiles and client config). Re-run \
             without --dry-run to apply.",
        );
    }
    Ok(())
}

/// Сохраняет новые учётные данные (если есть) и профиль. Если профиль записать не удалось,
/// хранилище возвращается в прежнее состояние.
fn save(
    env: &mut ConnectEnv<'_>,
    profile: &str,
    settings: ProfileSettings,
    credentials: Option<&ApiClientCredentials>,
) -> anyhow::Result<()> {
    let previous = credentials
        .map(|credentials| store_credentials(env.store, profile, credentials))
        .transpose()?;
    if let Err(err) = env.repo.upsert(profile, settings) {
        let Some(previous) = &previous else {
            return Err(err.context("the profile was not saved"));
        };
        if let Err(restore) = restore_stored(env.store, profile, previous) {
            return Err(err.context(format!(
                "the profile was not saved, and the keychain could not be restored ({restore})"
            )));
        }
        return Err(err.context("the profile was not saved; the keychain was left unchanged"));
    }
    if let Some(previous) = &previous {
        env.ui.say(&stored_message(profile, previous));
    }
    env.ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        env.repo.path().display()
    ));
    Ok(())
}

/// Сохранённые учётные данные профиля: `None`, если их нет или значение испорчено.
fn stored_credentials(
    store: &dyn CredentialStore,
    ui: &mut dyn Ui,
    profile: &str,
) -> anyhow::Result<Option<ApiClientCredentials>> {
    match store.get(profile) {
        Ok(credentials) => Ok(Some(credentials)),
        Err(StoreError::NotFound { .. }) => Ok(None),
        Err(StoreError::Corrupted { .. }) => {
            ui.say(&format!(
                "The value stored for profile `{profile}` is not a valid API client ID and \
                 secret; new credentials are needed."
            ));
            Ok(None)
        }
        Err(err) => Err(anyhow::Error::new(err).context("cannot read the system keychain")),
    }
}

/// Проверяет сохранённые учётные данные (если проверка не отключена). `true` — их можно
/// оставить.
async fn stored_credentials_work(
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    profile: &str,
    region: Region,
    credentials: &ApiClientCredentials,
) -> bool {
    ui.say(&format!(
        "Profile `{profile}` already has an API client in the system keychain: client ID {}.",
        credentials.client_id().masked()
    ));
    let Some(verifier) = verifier else {
        ui.say("Keeping the stored credentials (not checked: --no-verify).");
        return true;
    };
    match verifier.check(region, credentials).await {
        Ok(()) => {
            ui.say(
                "The stored credentials work: AppGallery Connect issued an access token. \
                 Keeping them.",
            );
            true
        }
        Err(err) => {
            ui.say(&format!(
                "The stored credentials do not work ({err:#}); new credentials are needed."
            ));
            false
        }
    }
}

/// Проверяет вход пользователя профиля (если проверка не отключена): без рабочего входа
/// `connect` ничего не меняет и подсказывает `auth login --oauth`.
async fn user_sign_in_works(
    env: &mut ConnectEnv<'_>,
    profile: &str,
    settings: &ProfileSettings,
) -> anyhow::Result<()> {
    let relogin = format!("appgallery-connect-mcp auth login --oauth --profile {profile}");
    let team = profile_team(profile, settings)?;
    let credentials = env.store.get_oauth(profile).map_err(|err| match err {
        StoreError::OAuthNotFound { .. } | StoreError::OAuthCorrupted { .. } => {
            anyhow::anyhow!("{err}; nothing was changed")
        }
        err => anyhow::Error::new(err).context("cannot read the system keychain"),
    })?;
    env.ui.say(&format!(
        "Profile `{profile}` signs in as a HUAWEI ID user: OAuth client {}, team {team}.",
        credentials.client().client_id().masked()
    ));
    let Some(verifier) = env.verifier else {
        env.ui
            .say("Keeping the stored sign-in (not checked: --no-verify).");
        return Ok(());
    };
    let (latest, checked) = verifier
        .check_user(settings.region, &credentials, &team)
        .await;
    crate::cli::auth::save_rotated(env.store, profile, &credentials, &latest);
    checked.map_err(|err| {
        anyhow::anyhow!(
            "the user sign-in of profile `{profile}` does not work ({err:#}); sign in again \
                 with `{relogin}`. Nothing was changed"
        )
    })?;
    env.ui
        .say("The stored sign-in works: HUAWEI ID issued an access token. Keeping it.");
    Ok(())
}

/// Подсказка для запуска без терминала, когда учётных данных нет.
fn no_credentials_error(profile: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "profile `{profile}` has no working API client credentials; pass --client-id <ID> with \
         --secret-file <PATH> or --secret-stdin (create the API client in {WHERE_TO_FIND}), or \
         run connect in a terminal without --silent to type them"
    )
}

/// Учётные данные профиля: рабочие сохранённые используются как есть (`None`), иначе
/// принимаются и проверяются новые (`Some`, ещё не сохранённые).
async fn profile_credentials(
    env: &mut ConnectEnv<'_>,
    args: &ConnectArgs,
    profile: &str,
    region: Region,
    source: SecretSource<'_>,
) -> anyhow::Result<Option<ApiClientCredentials>> {
    let stored = stored_credentials(env.store, env.ui, profile)?;
    if args.new_credentials_requested() {
        if stored.is_some() {
            env.ui.say(&format!(
                "Profile `{profile}` already has an API client in the system keychain; it will \
                 be replaced with the one from the flags."
            ));
        }
    } else {
        if let Some(credentials) = &stored
            && stored_credentials_work(env.verifier, env.ui, profile, region, credentials).await
        {
            return Ok(None);
        }
        if !env.ui.is_interactive() {
            return Err(no_credentials_error(profile));
        }
    }

    let client_id = read_client_id(env.ui, args.client_id.as_deref())?;
    let client_secret = read_client_secret(env.ui, source)?;
    let credentials = ApiClientCredentials::new(client_id, client_secret);
    env.ui
        .say(&format!("Client ID: {}.", credentials.client_id().masked()));
    match env.verifier {
        Some(verifier) => {
            verifier.check(region, &credentials).await.map_err(|err| {
                anyhow::anyhow!(
                    "{err:#} Nothing was stored and the client config was not changed. Re-run \
                     with --no-verify to store the credentials without verification."
                )
            })?;
            env.ui
                .say("Credentials work: AppGallery Connect issued an access token.");
        }
        None => env.ui.say("Skipping the check (--no-verify)."),
    }
    Ok(Some(credentials))
}

/// `--dry-run`: что стало бы с учётными данными профиля. Ничего не сохраняет и ничего не
/// спрашивает. Секрет из файла читается только для проверки формы; stdin не читается.
async fn dry_run_credentials(
    env: &mut ConnectEnv<'_>,
    args: &ConnectArgs,
    profile: &str,
    region: Region,
    source: SecretSource<'_>,
) -> anyhow::Result<()> {
    if args.new_credentials_requested() {
        if let Some(id) = args.client_id.as_deref() {
            let id = ClientId::new(id.trim().to_owned())
                .map_err(|err| anyhow::anyhow!("invalid API client ID: {err}"))?;
            env.ui.say(&format!("Client ID: {}.", id.masked()));
        }
        if let SecretSource::File(_) = source {
            read_client_secret(env.ui, source)?;
        }
        env.ui.say(&format!(
            "The API client from the flags would be {}stored in the system keychain for \
             profile `{profile}`{}.",
            if env.verifier.is_some() {
                "checked and "
            } else {
                ""
            },
            if args.client_id.is_none() {
                " (connect would ask for the client ID)"
            } else {
                ""
            }
        ));
        return Ok(());
    }
    let works = match stored_credentials(env.store, env.ui, profile)? {
        Some(credentials) => {
            stored_credentials_work(env.verifier, env.ui, profile, region, &credentials).await
        }
        None => false,
    };
    if !works {
        env.ui.say(&format!(
            "Profile `{profile}` has no working API client credentials: connect would ask for \
             the client ID and secret (or pass --client-id with --secret-file or \
             --secret-stdin). Create the API client in {WHERE_TO_FIND}."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
