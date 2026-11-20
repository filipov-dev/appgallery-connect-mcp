//! `auth login --oauth`, состояние и выход профиля со входом пользователя (режим
//! OAuth-клиента).
//!
//! Вход: OAuth-клиент (client ID флагом или вопросом, client secret — из файла, stdin или
//! скрытым вводом, может отсутствовать) → вход в браузере ([`crate::oauth_login`]) →
//! обновление access token по новому refresh token (так проверяется, что обновление
//! работает) → список команд пользователя → выбор команды (`--team-id`, единственная или
//! вопросом) → регион по сайту команды → запись в хранилище (`<профиль>/oauth`) и профиль
//! (`type = "oauth"`, `team_id`). При ошибке на любом шаге ничего не сохраняется; если не
//! удалось записать файл профилей, прежняя запись хранилища возвращается. Запись API-клиента
//! того же профиля после этого удаляется: у профиля один способ входа.

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use appgallery_connect_api::user_oauth::{self, OAuthClient, Team, short_scope, validate_team_id};
use appgallery_connect_api::{CredentialStore, OAuthCredentials, StoreError, oauth_account};

use super::LoginArgs;
use crate::browser::BrowserDecision;
use crate::login::{
    SecretSource, default_apps, default_packages, read_oauth_client_id, read_optional_secret,
    reject_secret_arguments,
};
use crate::oauth_login::{OAuthLoginOptions, parse_redirect_uri, sign_in};
use crate::profiles::{
    ProfileKind, ProfileRepo, ProfileSettings, describe_region, validate_profile_name,
};
use crate::ui::Ui;
use crate::verify::{TeamsError, Verifier, describe_teams};

/// Как идёт вход в браузере: решение об открытии браузера и таймаут (тесты их задают).
#[derive(Debug, Clone, Copy)]
pub struct SignInEnv {
    pub browser: BrowserDecision,
    pub timeout: Duration,
}

/// Области для вывода: без общего префикса, через запятую.
fn short_scopes(scopes: &[String]) -> String {
    scopes
        .iter()
        .map(|scope| short_scope(scope))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Выбранная команда.
struct Chosen {
    id: String,
    name: String,
    site: Option<appgallery_connect_api::Region>,
}

impl From<&Team> for Chosen {
    fn from(team: &Team) -> Self {
        Self {
            id: team.id.clone(),
            name: team.name.clone(),
            site: team.site,
        }
    }
}

/// Команда из списка: по `--team-id`, единственная или вопросом в терминале.
fn choose_team(
    ui: &mut dyn Ui,
    teams: Option<&[Team]>,
    flag: Option<&str>,
) -> anyhow::Result<Chosen> {
    let Some(teams) = teams else {
        // Список не получен; `--team-id` задан (иначе сюда не попасть).
        return Ok(Chosen {
            id: flag.unwrap_or_default().to_owned(),
            name: String::new(),
            site: None,
        });
    };
    if let Some(flag) = flag {
        return teams
            .iter()
            .find(|team| team.id == flag)
            .map(Chosen::from)
            .ok_or_else(|| {
                anyhow!(
                    "team `{flag}` is not among the teams of the signed-in user ({}); nothing \
                     was saved",
                    describe_teams(teams)
                )
            });
    }
    match teams {
        [] => bail!(
            "the signed-in HUAWEI ID user is not a member of any AppGallery Connect team: ask \
             the account holder to invite the user (Users and permissions > Members); nothing \
             was saved"
        ),
        [team] => Ok(Chosen::from(team)),
        _ if !ui.is_interactive() => bail!(
            "the signed-in user is a member of several teams ({}): pass --team-id <ID>; \
             nothing was saved",
            describe_teams(teams)
        ),
        _ => {
            ui.say("The signed-in user is a member of these teams:");
            for (index, team) in teams.iter().enumerate() {
                let site = team
                    .site
                    .map(|region| format!(", site {}", region.slug()))
                    .unwrap_or_default();
                ui.say(&format!(
                    "  {}. {} ({}{site})",
                    index + 1,
                    team.name,
                    team.id
                ));
            }
            let answer = ui
                .ask("Team (number or ID): ")
                .context("failed to read the team")?;
            let answer = answer.trim();
            let by_number = answer
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=teams.len()).contains(n))
                .map(|n| &teams[n - 1]);
            by_number
                .or_else(|| teams.iter().find(|team| team.id == answer))
                .map(Chosen::from)
                .ok_or_else(|| anyhow!("`{answer}` is not one of the teams; nothing was saved"))
        }
    }
}

/// `auth login --oauth`.
pub async fn login(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    verifier: &Verifier,
    ui: &mut dyn Ui,
    args: &LoginArgs,
    source: SecretSource<'_>,
    env: SignInEnv,
) -> anyhow::Result<()> {
    reject_secret_arguments(
        "auth login",
        None,
        args.secret_argument.is_some(),
        args.stray.len(),
    )?;
    let profile = args.profile.profile.as_str();
    validate_profile_name(profile)?;
    // Всё, что можно проверить без входа, — до него.
    let saved = repo.get(profile)?;
    let mut settings = saved.clone().unwrap_or_default();
    let apps = default_apps(&args.default_apps)?;
    let packages = default_packages(&args.default_packages)?;
    if !apps.is_empty() {
        settings.apps = apps;
    }
    if !packages.is_empty() {
        settings.packages = packages;
    }
    let scopes = user_oauth::requested_scopes(&args.oauth_scopes).map_err(|err| anyhow!(err))?;
    let team_flag = args
        .team_id
        .as_deref()
        .map(|team| {
            let team = team.trim();
            validate_team_id(team)
                .map(|()| team.to_owned())
                .map_err(|err| anyhow!("invalid --team-id: {err}"))
        })
        .transpose()?;
    let redirect = args
        .redirect_uri
        .as_deref()
        .map(parse_redirect_uri)
        .transpose()?;
    // Список команд — на хосте явного или сохранённого региона, иначе основного сайта.
    let list_region = args.region.unwrap_or(settings.region);

    let client_id = read_oauth_client_id(ui, args.oauth_client_id.as_deref())?;
    let client_secret = read_optional_secret(ui, source)?;
    ui.say(&format!(
        "OAuth client: {} ({}).",
        client_id.masked(),
        if client_secret.is_some() {
            "with a client secret"
        } else {
            "without a client secret"
        }
    ));
    ui.say(&format!("Requested scopes: {}.", short_scopes(&scopes)));
    let options = OAuthLoginOptions {
        client: OAuthClient::new(client_id, client_secret),
        scopes: scopes.clone(),
        browser: env.browser,
        endpoints: verifier.endpoints().clone(),
        redirect,
        timeout: env.timeout,
    };
    let grant = sign_in(ui, &options).await?;
    let missing = grant.credentials.missing_scopes(&scopes);
    if missing.is_empty() {
        ui.say(&format!(
            "Signed in; granted scopes: {}.",
            short_scopes(grant.credentials.scopes())
        ));
    } else {
        let missing: Vec<String> = missing.iter().map(|s| short_scope(s).to_owned()).collect();
        ui.say(&format!(
            "Signed in, but HUAWEI ID did not grant {}: calls of those APIs will fail until \
             Huawei approves the scopes for the OAuth client.",
            missing.join(", ")
        ));
    }

    let (teams, credentials) = match verifier.teams(list_region, &grant.credentials).await {
        Ok((teams, credentials)) => (Some(teams), credentials),
        Err(TeamsError::SignIn { error }) => bail!("{error:#}. Nothing was saved"),
        Err(TeamsError::List { error, credentials }) => {
            if team_flag.is_none() {
                bail!(
                    "{error:#}. Pass --team-id <ID> (Users and permissions > the team in \
                     AppGallery Connect) to sign in without the team list; nothing was saved"
                );
            }
            ui.say(&format!(
                "Warning: {error:#}. Using --team-id without checking it."
            ));
            (None, credentials)
        }
    };
    let team = choose_team(ui, teams.as_deref(), team_flag.as_deref())?;
    let region = match (team.site, args.region) {
        (Some(site), Some(flag)) if site != flag => bail!(
            "team `{}` is on the site {} ({}), not {}: drop --region or pass --region {}; \
             nothing was saved",
            team.id,
            site.slug(),
            site.title(),
            flag.slug(),
            site.slug()
        ),
        (Some(site), _) => site,
        (None, Some(flag)) => flag,
        (None, None) => settings.region,
    };
    settings.kind = ProfileKind::Oauth;
    settings.team_id = Some(team.id.clone());
    settings.region = region;

    save(store, repo, ui, profile, settings, &credentials)?;
    let name = if team.name.is_empty() {
        String::new()
    } else {
        format!(" ({})", team.name)
    };
    ui.say(&format!(
        "Team: {}{name}. Region: {}.",
        team.id,
        describe_region(region)
    ));
    Ok(())
}

/// Сохраняет вход и профиль; при сбое записи профиля возвращает хранилище. Затем удаляет
/// запись API-клиента профиля: у профиля один способ входа.
fn save(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
    settings: ProfileSettings,
    credentials: &OAuthCredentials,
) -> anyhow::Result<()> {
    // `Some(None)` — запись была, но не разбиралась.
    let previous = match store.get_oauth(profile) {
        Ok(previous) => Some(Some(previous)),
        Err(StoreError::OAuthNotFound { .. }) => None,
        Err(StoreError::OAuthCorrupted { .. }) => Some(None),
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context("cannot read the system keychain; nothing was stored"));
        }
    };
    store.set_oauth(profile, credentials).map_err(|err| {
        anyhow::Error::new(err).context("cannot save the user sign-in; nothing was stored")
    })?;
    if let Err(err) = repo.upsert(profile, settings) {
        let restored = match &previous {
            Some(Some(previous)) => store.set_oauth(profile, previous),
            _ => match store.delete_oauth(profile) {
                Ok(()) | Err(StoreError::OAuthNotFound { .. }) => Ok(()),
                Err(err) => Err(err),
            },
        };
        if let Err(restore) = restored {
            return Err(err.context(format!(
                "the profile was not saved, and the keychain could not be restored ({restore})"
            )));
        }
        return Err(err.context("the profile was not saved; the keychain was left unchanged"));
    }
    ui.say(&format!(
        "User sign-in {} in the system keychain (service `{}`, account `{}`).",
        if previous.is_some() {
            "replaced"
        } else {
            "saved"
        },
        appgallery_connect_api::KEYRING_SERVICE,
        oauth_account(profile)
    ));
    match store.delete(profile) {
        Ok(()) => ui.say(&format!(
            "The API client credentials of profile `{profile}` were removed from the keychain: \
             the profile now signs in as the user."
        )),
        Err(StoreError::NotFound { .. }) => {}
        Err(err) => ui.say(&format!(
            "Warning: the API client credentials of profile `{profile}` could not be removed \
             from the keychain ({err}); they are not used any more."
        )),
    }
    ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        repo.path().display()
    ));
    Ok(())
}

/// Удаляет вход пользователя после `auth login` с API-клиентом: у профиля один способ входа.
pub fn forget_after_api_client_login(store: &dyn CredentialStore, ui: &mut dyn Ui, profile: &str) {
    match store.delete_oauth(profile) {
        Ok(()) => ui.say(&format!(
            "The user sign-in of profile `{profile}` was removed from the keychain: the profile \
             now uses the API client. The sign-in stays valid at HUAWEI ID until it expires; \
             remove the access in HUAWEI ID (Privacy center > Control account access) if it is \
             not needed."
        )),
        Err(StoreError::OAuthNotFound { .. }) => {}
        Err(err) => ui.say(&format!(
            "Warning: the user sign-in of profile `{profile}` could not be removed from the \
             keychain ({err}); it is not used any more."
        )),
    }
}

/// Сохраняет вход, если проверка получила от HUAWEI ID новый refresh token: старый после
/// этого может не работать. Ошибка записи — только предупреждение в stderr, проверка от
/// неё не зависит.
pub(crate) fn save_rotated(
    store: &dyn CredentialStore,
    profile: &str,
    before: &OAuthCredentials,
    latest: &OAuthCredentials,
) {
    if before == latest {
        return;
    }
    if let Err(err) = store.set_oauth(profile, latest) {
        eprintln!(
            "warning: HUAWEI ID issued a new refresh token for profile `{profile}`, but it could \
             not be saved ({err}); sign in again if the next start fails"
        );
    }
}

/// Строки `auth status` о входе пользователя. Возвращает проблемы профиля.
pub async fn status(
    store: &dyn CredentialStore,
    verifier: Option<&Verifier>,
    ui: &mut dyn Ui,
    name: &str,
    settings: &ProfileSettings,
) -> Vec<String> {
    let mut problems = Vec::new();
    let team = match crate::profiles::profile_team(name, settings) {
        Ok(team) => Some(team),
        Err(err) => {
            problems.push(err.to_string());
            None
        }
    };
    let (credentials, state) = match store.get_oauth(name) {
        Ok(credentials) => (
            Some(credentials),
            format!(
                "stored in the system keychain (account `{}`)",
                oauth_account(name)
            ),
        ),
        Err(StoreError::OAuthNotFound { .. }) => {
            problems.push("no user sign-in in the keychain".into());
            (None, "not found in the system keychain".to_owned())
        }
        Err(err @ StoreError::OAuthCorrupted { .. }) => {
            problems.push(err.to_string());
            (
                None,
                "the stored value is not a valid user sign-in".to_owned(),
            )
        }
        Err(err) => {
            problems.push("the system keychain is not available".into());
            (None, format!("unavailable ({err})"))
        }
    };
    let check = match (&credentials, &team, verifier) {
        (None, _, _) | (_, None, _) => "skipped: the sign-in is not complete".to_owned(),
        (Some(_), Some(_), None) => {
            "not checked (pass --check to refresh the sign-in and list the teams)".to_owned()
        }
        (Some(credentials), Some(team), Some(verifier)) => {
            let (latest, checked) = verifier
                .check_user(settings.region, credentials, team)
                .await;
            save_rotated(store, name, credentials, &latest);
            match checked {
                Ok(()) => "ok, HUAWEI ID issued an access token and the team is listed".to_owned(),
                Err(err) => {
                    problems.push("the user sign-in check failed".into());
                    format!("failed: {err:#}")
                }
            }
        }
    };
    ui.say(&format!(
        "Sign-in:      user (OAuth client), team {}",
        team.as_deref().unwrap_or("-")
    ));
    ui.say(&format!(
        "Client ID:    {}",
        credentials.as_ref().map_or_else(
            || "-".to_owned(),
            |c| format!(
                "{} (OAuth client, {})",
                c.client().client_id().masked(),
                if c.client().client_secret().is_some() {
                    "with a client secret"
                } else {
                    "no client secret"
                }
            )
        )
    ));
    ui.say(&format!(
        "Scopes:       {}",
        credentials
            .as_ref()
            .map_or_else(|| "-".to_owned(), |c| short_scopes(c.scopes()))
    ));
    ui.say(&format!("Credentials:  {state}"));
    ui.say(&format!("Check:        {check}"));
    problems
}

/// Выход из входа пользователя: с `forget` — сначала отзыв на сервере HUAWEI ID (если не
/// удался — ничего не удаляется), затем удаление записи. Возвращает `true`, если запись
/// была.
pub async fn logout(
    store: &dyn CredentialStore,
    verifier: &Verifier,
    ui: &mut dyn Ui,
    profile: &str,
    forget: bool,
) -> anyhow::Result<bool> {
    let credentials = match store.get_oauth(profile) {
        Ok(credentials) => Some(credentials),
        Err(StoreError::OAuthNotFound { .. }) => return Ok(false),
        Err(StoreError::OAuthCorrupted { .. }) => None,
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "cannot read the user sign-in of profile `{profile}`; the profile was kept"
            )));
        }
    };
    if forget {
        match &credentials {
            Some(credentials) => {
                user_oauth::revoke(credentials, verifier.endpoints())
                    .await
                    .map_err(|err| {
                        anyhow!("{err}. Nothing was removed; run the command again later")
                    })?;
                ui.say("The sign-in was revoked at HUAWEI ID.");
            }
            None => ui.say(
                "The stored sign-in is not readable and cannot be revoked; remove the access in \
                 HUAWEI ID (Privacy center > Control account access).",
            ),
        }
    }
    match store.delete_oauth(profile) {
        Ok(()) | Err(StoreError::OAuthNotFound { .. }) => {}
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "cannot remove the user sign-in of profile `{profile}`; the profile was kept"
            )));
        }
    }
    ui.say(&format!(
        "User sign-in of profile `{profile}` removed from the system keychain."
    ));
    if !forget {
        ui.say(
            "The sign-in was not revoked at HUAWEI ID (`auth logout --forget` does that) and \
             stays valid there until it expires (180 days): remove the access in HUAWEI ID \
             (Privacy center > Control account access) if it must stop working now.",
        );
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
