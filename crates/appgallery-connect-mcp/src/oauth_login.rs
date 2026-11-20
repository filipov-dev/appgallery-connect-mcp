//! Вход пользователя HUAWEI ID через OAuth-клиент: общая часть `auth login --oauth`.
//!
//! Порядок: адрес возврата `http://127.0.0.1:<порт>/` на свободном порту (или заданный
//! `--redirect-uri` с явным портом) → страница входа HUAWEI ID с PKCE (S256),
//! `access_type=offline` и случайным `state` → код с loopback-адреса (или из адреса, который
//! пользователь вставил, если браузер на другой машине) → обмен кода на refresh и access
//! token.
//!
//! Loopback-сервер отвечает только на `GET` пути адреса возврата с правильным `state`;
//! остальные запросы получают 404, 400 или 405 и вход не прерывают. Запрос без query
//! (HUAWEI ID сообщает об ошибке во фрагменте `#error=…`, его сервер не видит) получает
//! страницу с подсказкой, вход ждёт дальше до таймаута. Страницы ответа — фиксированный
//! текст без значений из запроса. Код, verifier и токены не печатаются.

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use appgallery_connect_api::user_oauth::{
    OAuthClient, OAuthEndpoints, Pkce, UserGrant, authorization_url, exchange_code, random_state,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::browser::{BrowserDecision, SkipReason};
use crate::ui::Ui;

/// Сколько ждать входа в браузере.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Сколько ждать запрос браузера после того, как пользователь нажал Enter.
const AFTER_ENTER_TIMEOUT: Duration = Duration::from_secs(10);

/// Таймаут чтения одного запроса на loopback-адрес.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Предел заголовков запроса на loopback-адрес.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// Адрес возврата, заданный пользователем (`--redirect-uri`): если HUAWEI ID принимает только
/// адрес, указанный у OAuth-клиента, а не любой порт loopback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedRedirect {
    uri: String,
    port: u16,
    path: String,
}

/// Проверяет `--redirect-uri`: только `http://127.0.0.1:<порт>/<путь>` или
/// `http://localhost:<порт>/<путь>` с явным портом, без query и фрагмента.
pub fn parse_redirect_uri(value: &str) -> anyhow::Result<FixedRedirect> {
    let value = value.trim();
    let invalid = || {
        anyhow!(
            "--redirect-uri `{value}` is not a loopback address: use \
             http://127.0.0.1:<port>/ (or http://localhost:<port>/) with an explicit port and \
             no query, the same as the redirect URL of the OAuth client"
        )
    };
    let url = url::Url::parse(value).map_err(|_| invalid())?;
    let host_ok = matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    if url.scheme() != "http"
        || !host_ok
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let Some(port) = url.port().filter(|port| *port != 0) else {
        return Err(invalid());
    };
    Ok(FixedRedirect {
        uri: value.to_owned(),
        port,
        path: url.path().to_owned(),
    })
}

/// Параметры входа.
#[derive(Debug, Clone)]
pub struct OAuthLoginOptions {
    pub client: OAuthClient,
    /// Запрошенные области (`openid` и области Connect API).
    pub scopes: Vec<String>,
    /// Открывать ли браузер.
    pub browser: BrowserDecision,
    /// Адреса HUAWEI ID (подмена в тестах).
    pub endpoints: OAuthEndpoints,
    /// Адрес возврата из `--redirect-uri`; `None` — свободный порт 127.0.0.1.
    pub redirect: Option<FixedRedirect>,
    /// Сколько ждать входа.
    pub timeout: Duration,
}

/// Что пришло на loopback-адрес с правильным `state`.
enum Callback {
    Code(Zeroizing<String>),
    /// HUAWEI ID вернул ошибку.
    Denied(String),
}

/// Входит под пользователем HUAWEI ID: возвращает запись для хранилища и первый access
/// token. Ничего не сохраняет.
pub async fn sign_in(ui: &mut dyn Ui, options: &OAuthLoginOptions) -> anyhow::Result<UserGrant> {
    let browser = matches!(options.browser, BrowserDecision::Open);
    if !browser && !ui.is_interactive() {
        bail!(
            "signing in without a browser on this machine needs a terminal to paste the address \
             from the browser; run `auth login --oauth` in a terminal"
        );
    }
    let (listener, redirect_uri, callback_path) = match &options.redirect {
        Some(fixed) => {
            let listener = TcpListener::bind(("127.0.0.1", fixed.port))
                .await
                .with_context(|| {
                    format!(
                        "cannot listen on 127.0.0.1:{} for the redirect of the HUAWEI ID \
                         sign-in (is the port in use?)",
                        fixed.port
                    )
                })?;
            (listener, fixed.uri.clone(), fixed.path.clone())
        }
        None => {
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .await
                .context("cannot listen on a loopback port for the HUAWEI ID sign-in")?;
            let port = listener.local_addr()?.port();
            (
                listener,
                format!("http://127.0.0.1:{port}/"),
                "/".to_owned(),
            )
        }
    };
    let pkce = Pkce::generate()?;
    let state = random_state()?;
    let url = authorization_url(
        &options.client,
        &options.endpoints,
        &redirect_uri,
        &options.scopes,
        &pkce,
        &state,
    )?;

    let (tx, mut rx) = mpsc::channel(1);
    let server = tokio::spawn(serve_loopback(listener, callback_path, state.clone(), tx));
    let code = wait_for_code(ui, options, url.as_str(), &state, &mut rx).await;
    server.abort();
    let code = code?;

    ui.say("Exchanging the authorization code for tokens...");
    exchange_code(
        &options.client,
        &options.endpoints,
        &code,
        &pkce,
        &redirect_uri,
        &options.scopes,
    )
    .await
    .map_err(|err| anyhow!("{err}; nothing was saved"))
}

async fn wait_for_code(
    ui: &mut dyn Ui,
    options: &OAuthLoginOptions,
    url: &str,
    state: &str,
    rx: &mut mpsc::Receiver<Callback>,
) -> anyhow::Result<Zeroizing<String>> {
    let mut paste = true;
    match options.browser {
        BrowserDecision::Open => match ui.open_url(url) {
            Ok(()) => {
                ui.say(
                    "Opened the HUAWEI ID sign-in page in your browser. If it did not open, \
                     open this link:",
                );
                ui.say(url);
                paste = false;
            }
            Err(err) => {
                if !ui.is_interactive() {
                    bail!(
                        "could not open a browser ({err}); run `auth login --oauth` in a \
                         terminal to paste the address from the browser"
                    );
                }
                ui.say(&format!(
                    "Could not open a browser ({err}). Open this link in a browser:"
                ));
            }
        },
        BrowserDecision::Skip(reason) => {
            let why = match reason {
                SkipReason::Flag => "--no-browser",
                SkipReason::Ssh => "this is an SSH session",
                SkipReason::NoDisplay => "there is no graphical session",
            };
            ui.say(&format!(
                "Not opening a browser ({why}). Open this link in a browser on any device:"
            ));
        }
    }
    if !paste {
        ui.say(&format!(
            "Waiting for the sign-in in the browser (up to {} minutes)...",
            options.timeout.as_secs().div_ceil(60)
        ));
        let callback = tokio::time::timeout(options.timeout, rx.recv())
            .await
            .map_err(|_| {
                anyhow!(
                    "timed out waiting for the HUAWEI ID sign-in; nothing was saved. If the \
                     browser showed an error, check the redirect URL and the API scopes of the \
                     OAuth client"
                )
            })?;
        return callback_code(callback);
    }

    ui.say(url);
    ui.say(
        "Sign in and allow access. The browser then goes to an http://127.0.0.1 address. On \
         this machine the sign-in finishes by itself; on another device that page does not \
         load: copy its full address from the address bar and paste it here.",
    );
    let answer = Zeroizing::new(
        ui.ask("Address from the browser (or press Enter when the page says you are signed in): ")
            .context("cannot read the answer")?,
    );
    if answer.trim().is_empty() {
        let callback = tokio::time::timeout(AFTER_ENTER_TIMEOUT, rx.recv())
            .await
            .map_err(|_| {
                anyhow!(
                    "the browser did not return to this machine; run the command again and \
                     paste the address from the browser"
                )
            })?;
        return callback_code(callback);
    }
    parse_pasted(&answer, state)
}

fn callback_code(callback: Option<Callback>) -> anyhow::Result<Zeroizing<String>> {
    match callback {
        Some(Callback::Code(code)) => Ok(code),
        Some(Callback::Denied(error)) => Err(denied(&error)),
        None => Err(anyhow!("the loopback listener stopped before the sign-in")),
    }
}

fn denied(error: &str) -> anyhow::Error {
    anyhow!(
        "HUAWEI ID did not grant access (error {error}). Check that Huawei approved the \
         requested API scopes for the OAuth client and that its redirect URL matches; nothing \
         was saved"
    )
}

/// Код из адреса, который пользователь скопировал из браузера, или сам код.
fn parse_pasted(answer: &str, state: &str) -> anyhow::Result<Zeroizing<String>> {
    let text = answer.trim();
    let query = match text.split_once('?') {
        Some((_, query)) => Some(query),
        None if text.contains("code=") || text.contains("error=") => Some(text),
        None => None,
    };
    let Some(query) = query else {
        return checked_code(text);
    };
    // HUAWEI ID сообщает об ошибке во фрагменте: берём и его.
    let (query, fragment) = query.split_once('#').unwrap_or((query, ""));
    let mut code = None;
    let mut got_state = None;
    let mut error = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match name.as_ref() {
            "code" => code = Some(Zeroizing::new(value.into_owned())),
            "state" => got_state = Some(value.into_owned()),
            "error" => error = Some(safe_error(&value)),
            _ => {}
        }
    }
    if error.is_none() {
        error = url::form_urlencoded::parse(fragment.as_bytes())
            .find(|(name, _)| name == "error")
            .map(|(_, value)| safe_error(&value));
    }
    if let Some(error) = error
        && code.is_none()
    {
        return Err(denied(&error));
    }
    if got_state.as_deref() != Some(state) {
        bail!(
            "the pasted address is not from this sign-in (the state does not match); paste the \
             address the browser showed after this sign-in"
        );
    }
    match code {
        Some(code) => checked_code(&code),
        None => bail!("the pasted address has no authorization code"),
    }
}

fn checked_code(code: &str) -> anyhow::Result<Zeroizing<String>> {
    let code = code.trim();
    if code.is_empty() || code.len() > 2048 || !code.chars().all(|c| c.is_ascii_graphic()) {
        bail!("this is not an authorization code or an address from the browser");
    }
    Ok(Zeroizing::new(code.to_owned()))
}

/// Код ошибки для вывода: только короткий идентификатор.
fn safe_error(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 64
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        value.to_owned()
    } else {
        "unknown".to_owned()
    }
}

/// Принимает запросы браузера на loopback-адрес, пока не придёт ответ с правильным `state`.
async fn serve_loopback(
    listener: TcpListener,
    path: String,
    state: String,
    tx: mpsc::Sender<Callback>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        if let Some(callback) = handle_request(stream, &path, &state).await {
            let _ = tx.send(callback).await;
            return;
        }
    }
}

const PAGE_DONE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>appgallery-connect-mcp</title></head><body><h1>Signed in</h1><p>You can close this tab and return to the terminal.</p></body></html>";
const PAGE_DENIED: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>appgallery-connect-mcp</title></head><body><h1>Access was not granted</h1><p>Return to the terminal.</p></body></html>";
const PAGE_NO_CODE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>appgallery-connect-mcp</title></head><body><h1>No authorization code</h1><p>HUAWEI ID returned without a code. If it showed an error or you denied access, return to the terminal and press Ctrl-C, then check the redirect URL and the API scopes of the OAuth client.</p></body></html>";
const PAGE_UNEXPECTED: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>appgallery-connect-mcp</title></head><body><h1>Unexpected request</h1><p>This address only receives the HUAWEI ID sign-in of appgallery-connect-mcp.</p></body></html>";

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Один запрос браузера. `Some` — ответ входа с правильным `state`.
async fn handle_request(
    mut stream: TcpStream,
    expected_path: &str,
    state: &str,
) -> Option<Callback> {
    let mut buf = Zeroizing::new(Vec::with_capacity(2048));
    let read = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut chunk = [0u8; 2048];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return Some(());
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > MAX_REQUEST_BYTES {
                return Some(());
            }
        }
    })
    .await;
    if !matches!(read, Ok(Some(()))) {
        return None;
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    let (method, target) = (parts.next(), parts.next());
    let Some(target) = target.filter(|_| method == Some("GET")) else {
        respond(&mut stream, "405 Method Not Allowed", PAGE_UNEXPECTED).await;
        return None;
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != expected_path {
        respond(&mut stream, "404 Not Found", PAGE_UNEXPECTED).await;
        return None;
    }
    if query.is_empty() {
        respond(&mut stream, "200 OK", PAGE_NO_CODE).await;
        return None;
    }
    let mut code = None;
    let mut got_state = None;
    let mut error = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match name.as_ref() {
            "code" => code = Some(Zeroizing::new(value.into_owned())),
            "state" => got_state = Some(value.into_owned()),
            "error" => error = Some(safe_error(&value)),
            _ => {}
        }
    }
    if got_state.as_deref() != Some(state) {
        respond(&mut stream, "400 Bad Request", PAGE_UNEXPECTED).await;
        return None;
    }
    if let Some(error) = error {
        respond(&mut stream, "200 OK", PAGE_DENIED).await;
        return Some(Callback::Denied(error));
    }
    match code.and_then(|code| checked_code(&code).ok()) {
        Some(code) => {
            respond(&mut stream, "200 OK", PAGE_DONE).await;
            Some(Callback::Code(code))
        }
        None => {
            respond(&mut stream, "400 Bad Request", PAGE_UNEXPECTED).await;
            None
        }
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Адрес страницы входа из вывода.
    pub fn sign_in_url(output: &[String]) -> url::Url {
        let line = output
            .iter()
            .find(|line| line.contains("/oauth2/v3/authorize?"))
            .expect("the sign-in URL is printed");
        url::Url::parse(line.trim()).unwrap()
    }

    /// Параметр адреса.
    pub fn param(url: &url::Url, name: &str) -> String {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    }

    /// Запрос «браузера» на loopback-адрес 127.0.0.1; возвращает строку статуса ответа.
    pub async fn browser_get(url: &str) -> String {
        let parsed = url::Url::parse(url).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", parsed.port().unwrap()))
            .await
            .unwrap();
        let target = match parsed.query() {
            Some(q) => format!("{}?{q}", parsed.path()),
            None => parsed.path().to_owned(),
        };
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response.lines().next().unwrap_or_default().to_owned()
    }

    /// Запрос, который пришлёт «браузер» после входа: код или ошибка.
    #[derive(Debug, Clone)]
    pub enum BrowserReply {
        Code(String),
        Error(String),
    }

    /// Ui, который «открывает браузер»: в фоне проходит вход на loopback-адрес. Ответы на
    /// вопросы — из сценария.
    pub struct BrowserUi {
        pub inner: crate::ui::testing::ScriptedUi,
        pub reply: BrowserReply,
        /// Статусы ответов loopback-сервера: чужие запросы, затем ответ входа.
        pub statuses: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl BrowserUi {
        pub fn new(inner: crate::ui::testing::ScriptedUi, reply: BrowserReply) -> Self {
            Self {
                inner,
                reply,
                statuses: Default::default(),
            }
        }
    }

    impl Ui for BrowserUi {
        fn say(&mut self, line: &str) {
            self.inner.say(line);
        }
        fn ask(&mut self, prompt: &str) -> std::io::Result<String> {
            self.inner.ask(prompt)
        }
        fn ask_secret(&mut self, prompt: &str) -> std::io::Result<Zeroizing<String>> {
            self.inner.ask_secret(prompt)
        }
        fn confirm(&mut self, question: &str, default: bool) -> std::io::Result<bool> {
            self.inner.confirm(question, default)
        }
        fn is_interactive(&self) -> bool {
            self.inner.is_interactive()
        }
        fn open_url(&mut self, url: &str) -> std::io::Result<()> {
            self.inner.opened.push(url.to_owned());
            let sign_in = url::Url::parse(url).unwrap();
            let redirect = param(&sign_in, "redirect_uri");
            let state = param(&sign_in, "state");
            let reply = match &self.reply {
                BrowserReply::Code(code) => format!("code={code}"),
                BrowserReply::Error(error) => format!("error={error}"),
            };
            let statuses = std::sync::Arc::clone(&self.statuses);
            tokio::spawn(async move {
                // Чужие запросы и возврат без кода не прерывают вход.
                let base = redirect.trim_end_matches('/');
                let favicon = browser_get(&format!("{base}/favicon.ico")).await;
                let forged = browser_get(&format!("{redirect}?state=other&code=evil")).await;
                let empty = browser_get(&redirect).await;
                let done = browser_get(&format!("{redirect}?state={state}&{reply}")).await;
                statuses
                    .lock()
                    .unwrap()
                    .extend([favicon, forged, empty, done]);
            });
            Ok(())
        }
    }

    /// Ui без браузера: на вопрос об адресе вставляет адрес возврата с кодом (`state` — из
    /// напечатанной ссылки), остальные ответы — из сценария.
    pub struct PasteUi {
        pub inner: crate::ui::testing::ScriptedUi,
        pub code: String,
    }

    impl Ui for PasteUi {
        fn say(&mut self, line: &str) {
            self.inner.say(line);
        }
        fn ask(&mut self, prompt: &str) -> std::io::Result<String> {
            if !prompt.starts_with("Address") {
                return self.inner.ask(prompt);
            }
            self.inner.questions.push(prompt.to_owned());
            let url = sign_in_url(&self.inner.output);
            Ok(format!(
                "  http://127.0.0.1:1/?code={}&state={}  ",
                self.code,
                param(&url, "state")
            ))
        }
        fn ask_secret(&mut self, prompt: &str) -> std::io::Result<Zeroizing<String>> {
            self.inner.ask_secret(prompt)
        }
        fn confirm(&mut self, question: &str, default: bool) -> std::io::Result<bool> {
            self.inner.confirm(question, default)
        }
        fn is_interactive(&self) -> bool {
            true
        }
        fn open_url(&mut self, _: &str) -> std::io::Result<()> {
            panic!("the browser must not be opened");
        }
    }
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::{ClientId, ClientSecret};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::testing::*;
    use super::*;
    use crate::ui::testing::ScriptedUi;
    use crate::verify::Verifier;

    fn code() -> String {
        ["auth", "-code-", "8899"].concat()
    }

    fn refresh() -> String {
        ["Rt", "-login-", "0123456789"].concat()
    }

    async fn token_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v3/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains(format!("code={}", code())))
            .and(body_string_contains("code_verifier="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": (["CV", "login-token"].concat()),
                "expires_in": 3600,
                "refresh_token": refresh(),
                "scope": "openid https://www.huawei.com/auth/agc/publish",
                "token_type": "Bearer"
            })))
            .mount(&server)
            .await;
        server
    }

    fn options(server: &MockServer, browser: BrowserDecision) -> OAuthLoginOptions {
        OAuthLoginOptions {
            client: OAuthClient::new(
                ClientId::new("108800001").unwrap(),
                Some(ClientSecret::new(["Sec", "ret"].concat().repeat(10)).unwrap()),
            ),
            scopes: appgallery_connect_api::user_oauth::requested_scopes(&[]).unwrap(),
            browser,
            endpoints: Verifier::with_base_url(&server.uri()).endpoints().clone(),
            redirect: None,
            timeout: Duration::from_secs(20),
        }
    }

    #[tokio::test]
    async fn signs_in_through_the_loopback_redirect() {
        let server = token_server().await;
        let mut ui = BrowserUi::new(ScriptedUi::default(), BrowserReply::Code(code()));
        let grant = sign_in(&mut ui, &options(&server, BrowserDecision::Open))
            .await
            .unwrap();
        assert_eq!(grant.credentials.expose_refresh_token(), refresh());
        let statuses = ui.statuses.lock().unwrap().clone();
        assert_eq!(
            statuses,
            [
                "HTTP/1.1 404 Not Found",
                "HTTP/1.1 400 Bad Request",
                "HTTP/1.1 200 OK",
                "HTTP/1.1 200 OK"
            ]
        );
        let url = url::Url::parse(&ui.inner.opened[0]).unwrap();
        assert!(param(&url, "redirect_uri").starts_with("http://127.0.0.1:"));
        assert_eq!(param(&url, "code_challenge_method"), "S256");
        assert_eq!(param(&url, "access_type"), "offline");
        let out = ui.inner.text();
        assert!(!out.contains(&refresh()), "{out}");
        assert!(!out.contains(&code()), "{out}");
        assert!(!out.contains("SecretSecret"), "{out}");
        assert!(ui.inner.questions.is_empty());
    }

    #[tokio::test]
    async fn fixed_redirect_uri_with_its_path() {
        let server = token_server().await;
        // Свободный порт: занимаем и сразу отпускаем.
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut opts = options(&server, BrowserDecision::Open);
        opts.redirect = Some(parse_redirect_uri(&format!("http://127.0.0.1:{port}/cb")).unwrap());
        let mut ui = BrowserUi::new(ScriptedUi::default(), BrowserReply::Code(code()));
        sign_in(&mut ui, &opts).await.unwrap();
        let url = url::Url::parse(&ui.inner.opened[0]).unwrap();
        assert_eq!(
            param(&url, "redirect_uri"),
            format!("http://127.0.0.1:{port}/cb")
        );
        // Запрос к другому пути (`/favicon.ico`) — 404, к пути адреса возврата — вход.
        let statuses = ui.statuses.lock().unwrap().clone();
        assert_eq!(statuses[0], "HTTP/1.1 404 Not Found");
        assert_eq!(statuses[3], "HTTP/1.1 200 OK");

        for bad in [
            "https://127.0.0.1:8080/",
            "http://127.0.0.1/",
            "http://example.com:8080/",
            "http://127.0.0.1:8080/?x=1",
            "http://user@127.0.0.1:8080/",
            "not a url",
        ] {
            assert!(parse_redirect_uri(bad).is_err(), "{bad}");
        }
        assert!(parse_redirect_uri("http://localhost:8080/").is_ok());
    }

    #[tokio::test]
    async fn denied_access_and_pasted_addresses() {
        let server = token_server().await;
        let mut ui = BrowserUi::new(ScriptedUi::default(), BrowserReply::Error("1107".into()));
        let err = sign_in(&mut ui, &options(&server, BrowserDecision::Open))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("error 1107") && err.contains("nothing"),
            "{err}"
        );

        let mut ui = PasteUi {
            inner: ScriptedUi::default(),
            code: code(),
        };
        let grant = sign_in(
            &mut ui,
            &options(&server, BrowserDecision::Skip(SkipReason::Ssh)),
        )
        .await
        .unwrap();
        assert_eq!(grant.credentials.expose_refresh_token(), refresh());
        assert!(ui.inner.text().contains("this is an SSH session"));
        assert_eq!(ui.inner.questions.len(), 1);

        assert!(parse_pasted("http://127.0.0.1:1/?state=x&code=c", "y").is_err());
        let err = parse_pasted("http://127.0.0.1:1/#error=1107&error_description=x", "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("1107"), "{err}");
        assert_eq!(parse_pasted("Abc-123=", "s").unwrap().as_str(), "Abc-123=");
        assert!(parse_pasted("two words", "s").is_err());
    }

    #[tokio::test]
    async fn needs_a_browser_or_a_terminal() {
        let server = MockServer::start().await;
        let mut ui = crate::ui::SilentUi;
        let err = sign_in(
            &mut ui,
            &options(&server, BrowserDecision::Skip(SkipReason::Flag)),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("needs a terminal"), "{err}");
        let mut ui = ScriptedUi {
            open_fails: true,
            ..ScriptedUi::default()
        };
        let err = sign_in(&mut ui, &options(&server, BrowserDecision::Open))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not open a browser"), "{err}");
    }

    #[tokio::test]
    async fn sign_in_times_out() {
        let server = token_server().await;
        let mut opts = options(&server, BrowserDecision::Open);
        opts.timeout = Duration::from_millis(200);
        let mut ui = ScriptedUi::default();
        let err = sign_in(&mut ui, &opts).await.unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
        assert_eq!(ui.opened.len(), 1);
    }
}
