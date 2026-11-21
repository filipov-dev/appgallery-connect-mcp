//! Обработчик MCP-сервера `rmcp` без привязки к транспорту.
//!
//! Набор инструментов строится из таблицы операций, поэтому `ServerHandler` реализован
//! вручную, без макросов `#[tool]`. Транспорт подключает бинарник:
//! `AppGalleryServer::new(...)?.serve(stdio())`.
//!
//! Access token API-клиента для каждого запроса берёт клиент API ([`ApiClient`]) из
//! источника токенов профиля; ядро ни секрета, ни токена не видит. Составные инструменты
//! получают общий [`Context`]: тот же клиент API, данные профиля и уровень доступа.
//!
//! Вызов инструмента операции: аргументы `jq` ([`take_jq`], фильтр проверяется до запроса)
//! и `save_to` (путь проверяется [`FilePolicy`] до запроса; вместе с `jq` — ошибка) →
//! проверка аргументов по схеме ([`check_operation_arguments`]) → запрос (путь, query,
//! заголовки, тело, конверт результата — исполнитель) → сохранение файла из ответа, если
//! передан `save_to` → разбор JSON-строк и удаление полей ответа по таблице
//! ([`crate::decode`], HMS IAP) → вырезание секретов: поля операции по таблице спецификации, поля по
//! имени, значения по виду ([`crate::redact`]) → фильтр `jq` ([`crate::jq::apply`]) →
//! обёртка с пометкой `note` о вырезанном (и `saved` о сохранённом файле) → лимит размера.
//! Ошибки API, конверта результата и сети — `CallToolResult` с `isError: true`, их текст
//! тоже проходит вырезание секретов. Составные и пользовательские инструменты проходят ту же
//! постобработку.
//!
//! `save_to` (операции с временной ссылкой на файл или содержимым файла в ответе: отчёты,
//! файл конфигурации приложения, сертификаты и профили): ссылку сервер скачивает сам через
//! [`ApiClient::download_to_file`] (только хосты Huawei), содержимое записывает
//! ([`write_file_content`]); агент получает путь и размер, а не ссылку и не содержимое. Файл
//! создаётся только новым. Если запрос изменил данные, а файл сохранить не удалось, это не
//! ошибка инструмента: агент не должен повторять изменение, пометка объясняет, что вышло.
//!
//! Состояние сессии — текущий реестр инструментов: в режиме динамических наборов
//! (`ServerOptions::dynamic_toolsets`) `enable_toolset`/`disable_toolset` подменяют его снимок,
//! и клиент получает `notifications/tools/list_changed`: в сессии с `initialize` — сразу после
//! вызова, в протоколе 2026-07-28 — через подписку `subscriptions/listen`. Клоны сервера делят
//! сессию; stdio-сервер обслуживает одну сессию.
//!
//! `api_search` отвечает из таблиц спецификации без запросов ([`crate::search`]);
//! `api_request` собирает запрос в [`crate::request`] и дальше идёт тем же путём, что и
//! инструмент операции: исполнитель, вырезание секретов (если путь — операция таблицы, то и
//! её секретных полей), `jq`, `note`, лимит размера. `save_to` у него нет.
//!
//! Промпты ([`crate::prompts`]): `prompts/list` отдаёт промпты, доступные при уровне доступа
//! сервера, `prompts/get` рендерит текст по уровню доступа, режиму наборов, наборам текущего
//! реестра сессии и приложениям профиля. Capability `prompts` объявляется, только если есть
//! хотя бы один доступный промпт.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use appgallery_connect_api::spec::{
    AccessLevel, OperationDef, ParamLocation, SecretFieldDef, SecretKind,
};
use appgallery_connect_api::{ApiClient, ApiError, ApiRequest, ApiResponse, Region, ResponseBody};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig,
    SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};

use crate::composite::{Context, HttpNoVault, NoVault, ProfileInfo, SecretVault};
use crate::decode::{join_notes, reshape_operation_json};
use crate::error::ToolError;
use crate::files::{FilePolicy, display_path, take_save_to, write_file_content};
use crate::jq::{FILTER_INPUT_LIMIT, JqFilter, take_jq, takes_jq};
use crate::redact::{Redactions, redact_json, redact_operation_json, redact_text, savable_values};
use crate::registry::{ToolKind, ToolRegistry, access_name};
use crate::response::{NOTE, SAVED, empty_response, failure, note_extra, success, wrap};
use crate::validate::{check_arguments, check_operation_arguments};
use crate::{
    DEFAULT_PROFILE_NAME, ServerOptions, jq, prompts, request, search, server_name, toolsets,
};

/// Клиенты и хранилища, с которыми работает сервер.
pub struct Services {
    /// Клиент AppGallery Connect API с токенами API-клиента профиля.
    pub api: ApiClient,
    /// Хранилище секретов, которые сохраняют составные инструменты (записи профиля в
    /// системном хранилище, `appgallery_connect_api::KeyringVault`).
    pub vault: Arc<dyn SecretVault>,
}

impl Services {
    /// Клиент API без хранилища секретов ([`NoVault`]): так сервер работает в тестах.
    pub fn new(api: ApiClient) -> Self {
        Self {
            api,
            vault: Arc::new(NoVault),
        }
    }

    /// Хранилище секретов составных инструментов.
    pub fn with_vault(mut self, vault: Arc<dyn SecretVault>) -> Self {
        self.vault = vault;
        self
    }
}

/// MCP-сервер AppGallery Connect. Дёшево клонируется; клоны делят сессию.
#[derive(Clone)]
pub struct AppGalleryServer {
    inner: Arc<Inner>,
    session: Arc<Session>,
}

impl std::fmt::Debug for AppGalleryServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppGalleryServer")
            .field("name", &self.inner.name)
            .field("tools", &self.registry().len())
            .finish_non_exhaustive()
    }
}

struct Inner {
    context: Arc<Context>,
    options: ServerOptions,
    /// Правила локальных файлов (`save_to`, файлы составных инструментов).
    files: FilePolicy,
    instructions: String,
    /// Имя сервера в `initialize` ([`server_name`]).
    name: String,
}

/// Изменяемое состояние MCP-сессии.
struct Session {
    registry: RwLock<Arc<ToolRegistry>>,
    /// Счётчик изменений списка инструментов: по нему подписки `subscriptions/listen` узнают,
    /// что пора отправить `notifications/tools/list_changed`.
    changes: tokio::sync::watch::Sender<u64>,
}

impl Session {
    fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry: RwLock::new(registry),
            changes: tokio::sync::watch::Sender::new(0),
        }
    }

    fn registry(&self) -> Arc<ToolRegistry> {
        // Отравленная блокировка не мешает: внутри только `Arc`, его замена атомарна.
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn generation(&self) -> u64 {
        *self.changes.borrow()
    }
}

impl AppGalleryServer {
    /// Создаёт сервер. Ошибка — неизвестный набор инструментов в `options.toolsets`,
    /// некорректные составные инструменты или недоступный каталог `--allow-path`.
    ///
    /// Локальные файлы — рабочий каталог, временный каталог и
    /// [`ServerOptions::allowed_paths`] ([`FilePolicy::with_defaults`]); в HTTP-режиме —
    /// только [`ServerOptions::allowed_paths`] ([`http_file_policy`]).
    pub fn new(services: Services, options: ServerOptions) -> Result<Self, String> {
        let registry = Arc::new(ToolRegistry::new(&options)?);
        let files = if options.http {
            http_file_policy(&options.allowed_paths)
        } else {
            FilePolicy::with_defaults(&options.allowed_paths)
        }
        .map_err(|err| err.0)?;
        let config = services.api.config();
        let region = config.region;
        // Вход пользователя: инструкции называют команду и `auth login --oauth`.
        let team = config
            .oauth
            .as_ref()
            .map(|session| session.team_id.clone().unwrap_or_default());
        let mut instructions = server_instructions(&options, &registry, region, team.as_deref());
        instructions.push_str(&files_instructions(&options, &registry, &files));
        instructions.push_str(&crate::purchases::purchases_instructions(
            &options,
            &registry,
            &services.api.app_ids(),
        ));
        let profile = options
            .profile
            .clone()
            .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_owned());
        let context = Arc::new(Context {
            api: Arc::new(services.api),
            files: Arc::new(files.clone()),
            // В HTTP-режиме хранилища секретов нет при любом `Services`: сервер удалённый и
            // общий, сохранять секреты клиентов ему негде и незачем.
            vault: if options.http {
                Arc::new(HttpNoVault)
            } else {
                services.vault
            },
            profile: ProfileInfo {
                name: profile.clone(),
                apps: options.apps.clone(),
                packages: options.packages.clone(),
            },
            access: options.max_access,
            http: options.http,
        });
        Ok(Self {
            inner: Arc::new(Inner {
                context,
                options,
                files,
                instructions,
                name: server_name(&profile),
            }),
            session: Arc::new(Session::new(registry)),
        })
    }

    /// Текущий реестр инструментов сессии (снимок).
    pub fn registry(&self) -> Arc<ToolRegistry> {
        self.session.registry()
    }

    pub fn instructions(&self) -> &str {
        &self.inner.instructions
    }

    pub fn options(&self) -> &ServerOptions {
        &self.inner.options
    }

    /// Имя сервера в `initialize`: `appgallery` или `agc-<profile>`.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    fn client(&self) -> &ApiClient {
        &self.inner.context.api
    }

    /// Правила локальных файлов сервера.
    pub fn files(&self) -> &FilePolicy {
        &self.inner.files
    }

    /// Вызов инструмента без транспорта. Ошибки возвращаются как `isError: true`;
    /// `Err` — только для неизвестного инструмента (ошибка протокола).
    pub async fn call(&self, name: &str, args: JsonObject) -> Result<CallToolResult, ErrorData> {
        let registry = self.registry();
        let Some(tool) = registry.get(name) else {
            // Известный, но выключенный инструмент: агенту объясняем, какой флаг нужен.
            return match registry.unavailable_reason(name) {
                Some(reason) => Ok(failure(&ToolError::new(reason))),
                None => Err(ErrorData::invalid_params(
                    format!("unknown tool `{name}`"),
                    None,
                )),
            };
        };
        let result = match tool.kind {
            ToolKind::Operation(op) => self.call_operation(op, &tool.tool, args).await,
            ToolKind::Composite(index) => self.call_composite(index, &tool.tool, args).await,
            ToolKind::ApiSearch => search::search(&args, &registry),
            ToolKind::ApiRequest => self.call_api_request(args).await,
            ToolKind::Custom(index) => self.call_custom(index, args).await,
            ToolKind::ListToolsets => toolsets::list(&args, &registry),
            ToolKind::EnableToolset => self.toggle_toolsets(&args, true),
            ToolKind::DisableToolset => self.toggle_toolsets(&args, false),
        };
        // Защита в глубину: известные клиенту секреты и секреты по виду не попадают в ответ
        // агенту, даже если что-то прошло мимо разбора JSON.
        let client = self.client();
        Ok(match result {
            Ok(text) => success(client.scrub(&text)),
            Err(err) => {
                let err = ToolError::new(client.scrub(err.message()));
                tracing::debug!(tool = name, error = %err, "инструмент завершился ошибкой");
                failure(&err)
            }
        })
    }

    /// `enable_toolset`/`disable_toolset`: пересобирает реестр сессии. Замена делается под
    /// блокировкой записи от текущего реестра, чтобы параллельные вызовы не потеряли изменения.
    fn toggle_toolsets(&self, args: &JsonObject, enable: bool) -> Result<String, ToolError> {
        let mut guard = self
            .session
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let toggled = toolsets::toggle(&guard, args, enable)?;
        if let Some(next) = toggled.registry {
            tracing::info!(
                toolsets = ?next.enabled_toolsets(),
                tools = next.len(),
                "наборы инструментов изменены"
            );
            *guard = Arc::new(next);
            drop(guard);
            self.session
                .changes
                .send_modify(|generation| *generation += 1);
        }
        Ok(toggled.text)
    }

    async fn call_operation(
        &self,
        op: &'static OperationDef,
        tool: &Tool,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        // У операций с пустым ответом аргумента `jq` нет: его отклонит проверка аргументов.
        let jq = if takes_jq(op) {
            take_jq(&mut args)?
        } else {
            None
        };
        let save_to = take_save_to(op, &mut args)?;
        self.run_operation(op, &tool.input_schema, args, jq, save_to)
            .await
    }

    /// Операция с проверкой аргументов по схеме `schema`, фильтром `jq` (уже проверенным) и
    /// путём `save_to` (ещё не проверенным).
    async fn run_operation(
        &self,
        op: &'static OperationDef,
        schema: &JsonObject,
        mut args: JsonObject,
        jq: Option<JqFilter>,
        save_to: Option<String>,
    ) -> Result<String, ToolError> {
        if jq.is_some() && save_to.is_some() {
            return Err(ToolError::new(
                "jq cannot be combined with save_to: the file is saved as the API returns it. \
                 Call the tool with `save_to` and without `jq`, or with `jq` and without \
                 `save_to`",
            ));
        }
        check_operation_arguments(op, schema, &mut args)?;
        // Тело записи Remote Configuration с маской `***` затёрло бы настоящие значения.
        crate::composite::remote_config_set::guard_masked_body(op, args.get("body"))?;
        // Путь проверяется до запроса: ошибка в пути не должна стоить вызова API (а у
        // операций записи — изменения данных).
        let target = match save_to.as_deref() {
            Some(path) => Some(self.inner.files.check_write(path)?),
            None => None,
        };
        tracing::debug!(
            tool = op.tool_name,
            path = op.path,
            save_to = target.is_some(),
            "вызов операции AppGallery Connect API"
        );
        let request =
            ApiRequest::for_operation(op, &Value::Object(args)).map_err(ToolError::from)?;
        let response = self.execute(request, jq.as_ref(), Some(op)).await?;
        let changes = op.access != AccessLevel::Read;
        match target {
            Some(target) => self.save_response(op, response, &target, changes).await,
            None => self.finish(Some(op), response, changes, jq.as_ref()).await,
        }
    }

    /// Выполняет запрос. С фильтром тело читается до [`FILTER_INPUT_LIMIT`]: лимит ответа
    /// сервера применяется к результату фильтра, ради этого фильтр и нужен. `op` — операция
    /// таблицы: к ошибке размера добавляется подсказка, какими её аргументами сузить ответ.
    async fn execute(
        &self,
        request: ApiRequest,
        jq: Option<&JqFilter>,
        op: Option<&'static OperationDef>,
    ) -> Result<ApiResponse, ToolError> {
        let client = self.client();
        let limit = match jq {
            Some(_) => FILTER_INPUT_LIMIT.max(client.config().max_response_bytes),
            None => client.config().max_response_bytes,
        };
        let http = self.registry().http();
        client
            .execute_with_limit(request, limit)
            .await
            .map_err(|err| api_error(op, err, http))
    }

    /// Пользовательский инструмент: фиксированные аргументы и фильтр определения, дальше —
    /// тот же путь, что у инструмента его операции (со схемой инструмента операции).
    async fn call_custom(&self, index: usize, args: JsonObject) -> Result<String, ToolError> {
        let Some(tool) = self.inner.options.custom_tools.get(index) else {
            return Err(ToolError::new("custom tool is not available"));
        };
        let (mut args, jq) = tool.build_call(args)?;
        let save_to = take_save_to(tool.operation, &mut args)?;
        let op = tool.operation;
        let schema: JsonObject = serde_json::from_str(op.input_schema).map_err(|err| {
            ToolError::new(format!("{}: invalid input schema: {err}", op.tool_name))
        })?;
        tracing::debug!(tool = %tool.name, operation = op.tool_name, "пользовательский инструмент");
        self.run_operation(op, &schema, args, jq, save_to).await
    }

    /// `api_request`: уровень доступа и конверт результата — по операции, которой
    /// соответствует путь ([`request::prepare`]); фильтр `jq` и остальная постобработка
    /// ответа — те же, что у инструментов операций.
    async fn call_api_request(&self, mut args: JsonObject) -> Result<String, ToolError> {
        // Фильтр проверяется до запроса, как у инструментов операций.
        let jq = jq::take_jq(&mut args)?;
        let prepared =
            request::prepare(&args, self.registry().max_access(), self.registry().http())?;
        tracing::debug!(
            method = prepared.request.method.as_str(),
            path = %prepared.request.path,
            access = prepared.access.as_str(),
            known = prepared.matched.is_some(),
            "произвольный запрос к AppGallery Connect API"
        );
        let op = prepared.operation();
        if let Some(op) = op {
            crate::composite::remote_config_set::guard_masked_body(
                op,
                prepared.request.body.as_ref(),
            )?;
        }
        let response = self.execute(prepared.request, jq.as_ref(), op).await?;
        self.finish(
            op,
            response,
            prepared.access != AccessLevel::Read,
            jq.as_ref(),
        )
        .await
    }

    /// Ответ операции с `save_to`: сохраняет файл из ответа (ссылку скачивает, содержимое
    /// записывает) и возвращает ответ без секретов с `saved` — путём и размером файла.
    async fn save_response(
        &self,
        op: &'static OperationDef,
        response: ApiResponse,
        target: &Path,
        changes: bool,
    ) -> Result<String, ToolError> {
        let status = response.status;
        let mut value = match response.body {
            ResponseBody::Json(value) => value,
            other => {
                let text = match other {
                    ResponseBody::Text(_) => "the response is not JSON",
                    _ => "the response is empty",
                };
                return self.not_saved(op, None, status, changes, text);
            }
        };
        let found: Vec<(&'static SecretFieldDef, Value)> = savable_values(op, &value)
            .into_iter()
            .map(|(field, value)| (field, value.clone()))
            .collect();
        let saved = match found.as_slice() {
            [(field, file)] => self.save_value(field, file, target).await,
            [] => Err(format!(
                "the response has no file to save ({} is missing or empty)",
                savable_names(op)
            )),
            many => Err(format!(
                "the response has {} files ({}), and `save_to` saves one file: narrow the \
                 request to one item",
                many.len(),
                savable_names(op)
            )),
        };
        let saved = match saved {
            Ok(saved) => saved,
            Err(reason) => return self.not_saved(op, Some(value), status, changes, &reason),
        };
        tracing::debug!(
            tool = op.tool_name,
            field = saved.field.path,
            bytes = saved.bytes,
            "файл из ответа сохранён"
        );
        let redactions = redact_operation_json(op, &mut value, Some(saved.field.path), &|text| {
            self.client().scrub(text)
        });
        let mut extra = Map::new();
        let mut info = json!({
            "path": display_path(&saved.path),
            "bytes": saved.bytes,
            "field": saved.field.path,
        });
        if let Some(content_type) = saved.content_type {
            info["content_type"] = Value::String(content_type);
        }
        extra.insert(SAVED.to_owned(), info);
        if let Some(note) = redactions.note() {
            extra.insert(NOTE.to_owned(), Value::String(note));
        }
        let text = wrap(value, Some(extra)).to_string();
        self.check_size(&text, changes.then_some(status), Some(op))?;
        Ok(text)
    }

    /// Файл не сохранён. Запрос чтения — ошибка инструмента; запрос, который изменил
    /// данные, — успешный ответ без секретов с пометкой: агент не должен его повторять.
    fn not_saved(
        &self,
        op: &'static OperationDef,
        value: Option<Value>,
        status: u16,
        changes: bool,
        reason: &str,
    ) -> Result<String, ToolError> {
        let reason = self.client().scrub(reason);
        if !changes {
            return Err(ToolError::new(format!(
                "the request succeeded, but the file was not saved to `save_to`: {reason}"
            )));
        }
        let mut value = value.unwrap_or(Value::Null);
        let redactions =
            redact_operation_json(op, &mut value, None, &|text| self.client().scrub(text));
        let mut note = format!(
            "The request succeeded (HTTP {status}) and changed data, but the file was not saved \
             to `save_to`: {reason}. Do not repeat the request: it would apply the change again."
        );
        if let Some(hidden) = redactions.note() {
            note.push(' ');
            note.push_str(&hidden);
        }
        let text = wrap(value, note_extra(Some(note))).to_string();
        self.check_size(&text, Some(status), Some(op))?;
        Ok(text)
    }

    /// Сохраняет одно значение из ответа в новый файл `target`.
    async fn save_value(
        &self,
        field: &'static SecretFieldDef,
        value: &Value,
        target: &Path,
    ) -> Result<SavedFile, String> {
        match field.kind {
            SecretKind::DownloadUrl => {
                let Some(url) = value.as_str() else {
                    return Err(format!("`{}` in the response is not a link", field.path));
                };
                let file = self
                    .client()
                    .download_to_file(url.trim(), target)
                    .await
                    .map_err(|err| format!("could not download the file: {err}"))?;
                Ok(SavedFile {
                    field,
                    path: file.path,
                    bytes: file.bytes,
                    content_type: file.content_type,
                })
            }
            SecretKind::FileContent => {
                let written = write_file_content(target, value).map_err(|err| err.message)?;
                Ok(SavedFile {
                    field,
                    path: target.to_path_buf(),
                    bytes: written,
                    content_type: None,
                })
            }
            SecretKind::Credential => Err("credentials are never saved to files".to_owned()),
        }
    }

    /// Составной инструмент: аргументы проверяются по его схеме строго, результат проходит
    /// ту же постобработку, что и ответы операций.
    async fn call_composite(
        &self,
        index: usize,
        tool: &Tool,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        let Some(composite) = self.inner.options.composite_tools.get(index) else {
            return Err(ToolError::new("composite tool is not available"));
        };
        let jq = take_jq(&mut args)?;
        check_arguments(&Value::Object((*tool.input_schema).clone()), &mut args)?;
        tracing::debug!(tool = composite.name, "составной инструмент");
        let mut value = (composite.run)(&self.inner.context, Value::Object(args)).await?;
        let redactions = self.redact(&mut value);
        let status = (composite.access != AccessLevel::Read).then_some(200);
        let value = self.filter(value, jq.as_ref(), status).await?;
        let text = wrap(value, note_extra(redactions.note())).to_string();
        self.check_size(&text, status, None)?;
        Ok(text)
    }

    fn redact(&self, value: &mut Value) -> Redactions {
        let client = self.client();
        redact_json(value, &|text| client.scrub(text))
    }

    /// Текст успешного ответа для агента — общая постобработка всех инструментов, которые
    /// вызывают API: разбор JSON-строк и удаление полей по таблице ([`crate::decode`]) →
    /// вырезание секретов → фильтр `jq` → обёртка с `note` → лимит размера.
    /// `op` — операция таблицы, если запрос — она: тогда до фильтра скрываются и её секретные
    /// поля по таблице спецификации. `changes` — запрос изменяет данные: тогда ошибка
    /// фильтра или размера говорит, что изменение выполнено.
    async fn finish(
        &self,
        op: Option<&'static OperationDef>,
        response: ApiResponse,
        changes: bool,
        jq: Option<&JqFilter>,
    ) -> Result<String, ToolError> {
        let status = changes.then_some(response.status);
        let text = match response.body {
            ResponseBody::Json(mut value) => {
                let changes = op
                    .map(|op| reshape_operation_json(op, &mut value))
                    .unwrap_or_default();
                let redactions = match op {
                    Some(op) => redact_operation_json(op, &mut value, None, &|text| {
                        self.client().scrub(text)
                    }),
                    None => self.redact(&mut value),
                };
                let value = self.filter(value, jq, status).await?;
                let note = join_notes(changes.note(), redactions.note());
                wrap(value, note_extra(note)).to_string()
            }
            ResponseBody::Text(text) => {
                let client = self.client();
                let (mut text, redactions) = redact_text(&text, &|text| client.scrub(text));
                if let Some(note) = redactions.note() {
                    text.push_str("\n\n");
                    text.push_str(&note);
                }
                if jq.is_some() {
                    text.push_str(
                        "\n\n[The `jq` filter was not applied: the response is not JSON.]",
                    );
                }
                text
            }
            ResponseBody::Empty => empty_response(response.status),
        };
        self.check_size(&text, status, op)?;
        Ok(text)
    }

    /// Фильтр `jq` для очищенного JSON ([`jq::apply`]); без фильтра — значение как есть.
    async fn filter(
        &self,
        value: Value,
        jq: Option<&JqFilter>,
        status: Option<u16>,
    ) -> Result<Value, ToolError> {
        let max = self.client().config().max_response_bytes;
        jq::apply(&self.inner.options.jq, jq, value, max, status).await
    }

    /// Проверяет итоговый размер ответа.
    fn check_size(
        &self,
        text: &str,
        status: Option<u16>,
        op: Option<&OperationDef>,
    ) -> Result<(), ToolError> {
        check_size(text, self.client().config().max_response_bytes, status, op)
    }
}

/// Файл, сохранённый из ответа.
struct SavedFile {
    field: &'static SecretFieldDef,
    path: PathBuf,
    bytes: u64,
    content_type: Option<String>,
}

/// Имена сохраняемых полей операции для сообщений: `` `fileURL` ``.
fn savable_names(op: &OperationDef) -> String {
    op.secrets
        .iter()
        .filter(|field| field.kind.savable())
        .map(|field| format!("`{}`", field.path))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Ошибка исполнителя для агента; к ошибке размера ответа добавляется подсказка, какими
/// аргументами операции сузить запрос. `http` — HTTP-режим сервера
/// ([`crate::error::describe_api_error_for`]).
fn api_error(op: Option<&OperationDef>, err: ApiError, http: bool) -> ToolError {
    let too_large = matches!(err, ApiError::ResponseTooLarge { .. });
    let mut error = ToolError::new(crate::error::describe_api_error_for(&err, http));
    if too_large && let Some(hint) = op.and_then(narrowing_arguments) {
        error.message.push_str(". ");
        error.message.push_str(&hint);
    }
    error
}

/// Слова в именах параметров, которые сужают ответ: размер страницы, отбор, период, язык.
const NARROWING_WORDS: &[&str] = &[
    "size", "count", "limit", "page", "filter", "time", "date", "lang", "status", "state", "type",
    "ids",
];

/// Аргументы операции, которыми можно сузить ответ; `None` — таких нет.
fn narrowing_arguments(op: &OperationDef) -> Option<String> {
    let names: Vec<String> = op
        .params
        .iter()
        .filter(|param| matches!(param.location, ParamLocation::Query | ParamLocation::Header))
        .filter(|param| {
            let name = param.name.to_ascii_lowercase();
            NARROWING_WORDS.iter().any(|word| name.contains(word))
        })
        .map(|param| format!("`{}`", param.name))
        .collect();
    (!names.is_empty()).then(|| {
        format!(
            "Arguments of `{}` that narrow the response: {}",
            op.tool_name,
            names.join(", ")
        )
    })
}

/// Проверяет итоговый размер ответа: `status` — статус изменяющего запроса, `op` —
/// операция (подсказка, какими аргументами сузить запрос).
fn check_size(
    text: &str,
    max: usize,
    status: Option<u16>,
    op: Option<&OperationDef>,
) -> Result<(), ToolError> {
    if text.len() <= max {
        return Ok(());
    }
    let mut narrow = "Narrow the request: a smaller page, stricter filters, fewer languages or \
                      a shorter time range"
        .to_owned();
    if let Some(hint) = op.and_then(narrowing_arguments) {
        narrow.push_str(". ");
        narrow.push_str(&hint);
    }
    Err(ToolError::new(match status {
        Some(status) => format!(
            "the API request succeeded (HTTP {status}), but the response is larger than {max} \
             bytes and was not returned. Do not repeat the request: it changed data, and \
             repeating it would apply the change again; read the result with a narrower read \
             request instead. {narrow}"
        ),
        None => format!("the response is larger than {max} bytes and was not returned. {narrow}"),
    }))
}

impl ServerHandler for AppGalleryServer {
    fn get_info(&self) -> ServerConfig {
        let options = &self.inner.options;
        let mut capabilities = if options.dynamic_toolsets {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build()
        } else {
            ServerCapabilities::builder().enable_tools().build()
        };
        if prompts::available(&options.prompts, options.max_access)
            .next()
            .is_some()
        {
            capabilities.prompts = Some(rmcp::model::PromptsCapability::default());
        }
        ServerConfig::new(capabilities)
            .with_server_info(
                Implementation::new(self.inner.name.clone(), env!("CARGO_PKG_VERSION"))
                    .with_title("AppGallery Connect"),
            )
            .with_instructions(self.inner.instructions.clone())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.registry().tools()))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListPromptsResult, ErrorData> {
        let options = &self.inner.options;
        let prompts = prompts::available(&options.prompts, options.max_access).map(prompts::info);
        Ok(rmcp::model::ListPromptsResult::with_all_items(
            prompts.collect(),
        ))
    }

    async fn get_prompt(
        &self,
        request: rmcp::model::GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::GetPromptResponse, ErrorData> {
        let options = &self.inner.options;
        let registry = self.registry();
        let setup = prompts::PromptSetup {
            access: options.max_access,
            dynamic: registry.is_dynamic(),
            enabled_toolsets: registry.enabled_toolsets().to_vec(),
            apps: options.apps.clone(),
            http: options.http,
        };
        prompts::get(&options.prompts, &setup, request).map(Into::into)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let before = self.session.generation();
        let result = self.call(&request.name, args).await;
        // В сессии с `initialize` уведомление уходит сразу; в протоколе 2026-07-28 клиент
        // получает его через подписку (`listen`), а незапрошенные уведомления отклоняет.
        if self.session.generation() != before
            && uses_legacy_session(&context)
            && let Err(err) = context.peer.notify_tool_list_changed().await
        {
            tracing::warn!(error = %err, "не удалось отправить tools/list_changed");
        }
        result.map(Into::into)
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        // Подписка нужна только для смены списка инструментов в динамическом режиме.
        self.inner
            .options
            .dynamic_toolsets
            .then(|| requested.supported_by(&self.get_info().capabilities))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        if context.accepted().tools_list_changed != Some(true) {
            context.cancelled().await;
            return Ok(());
        }
        let mut changes = self.session.changes.subscribe();
        changes.mark_unchanged();
        loop {
            tokio::select! {
                () = context.cancelled() => return Ok(()),
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    changes.mark_unchanged();
                    if let Err(err) = context.sink().notify_tool_list_changed().await {
                        tracing::debug!(error = %err, "подписка на tools/list_changed закрыта");
                        return Ok(());
                    }
                }
            }
        }
    }

    fn get_tool(&self, _name: &str) -> Option<Tool> {
        // Аргументы проверяет ядро с понятными агенту сообщениями; проверку схемы на
        // стороне rmcp не включаем.
        None
    }
}

fn uses_legacy_session(context: &RequestContext<RoleServer>) -> bool {
    // Версии `YYYY-MM-DD` сравниваются лексикографически так же, как по времени.
    context
        .protocol_version()
        .is_none_or(|version| version.as_str() < ProtocolVersion::V_2026_07_28.as_str())
}

/// Локальные файлы HTTP-сервера: только каталоги сервера из `--allow-path`, относительные
/// пути — от первого из них. Рабочий и временный каталоги сервера к клиенту отношения не
/// имеют; без каталогов файлов нет ([`FilePolicy::disabled`]).
pub fn http_file_policy(allowed: &[PathBuf]) -> Result<FilePolicy, crate::files::FilePolicyError> {
    if allowed.is_empty() {
        return Ok(FilePolicy::disabled());
    }
    let policy = FilePolicy::new(allowed)?;
    Ok(match policy.roots().first().cloned() {
        Some(base) => policy.with_base_dir(base),
        None => policy,
    })
}

/// Составной инструмент поиска приложений: инструкции советуют его, если он есть.
const SEARCH_APPS: &str = crate::composite::search_apps::NAME;

/// Инструкции сервера для агента (поле `instructions` в `initialize`).
pub fn server_instructions(
    options: &ServerOptions,
    registry: &ToolRegistry,
    region: Region,
    team: Option<&str>,
) -> String {
    let profile = options.profile.as_deref().unwrap_or(DEFAULT_PROFILE_NAME);
    let enabled = registry.enabled_toolsets();
    let enabled_list = if enabled.is_empty() {
        "none".to_owned()
    } else {
        enabled.join(", ")
    };
    let defaults = if options.apps.is_empty() && options.packages.is_empty() {
        String::new()
    } else {
        let list = |values: &[String]| {
            values
                .iter()
                .map(|value| format!("`{value}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts = Vec::new();
        if !options.apps.is_empty() {
            parts.push(format!("app IDs {}", list(&options.apps)));
        }
        if !options.packages.is_empty() {
            parts.push(format!("package names {}", list(&options.packages)));
        }
        format!(
            " The user's default apps for this profile: {}. Use them when the user does not \
             name an app.",
            parts.join("; ")
        )
    };
    let toolsets = if registry.is_dynamic() {
        format!(
            "Toolsets are enabled on demand; enabled at start: {enabled_list}. `list_toolsets` \
             lists all toolsets, `enable_toolset` adds the tools of a toolset to this session \
             and `disable_toolset` removes them (the client gets \
             `notifications/tools/list_changed`). Enable only the toolsets the task needs; the \
             access level does not change, and tools above it are never registered. If the \
             enabled tools do not appear in your tool list, call the same operations with \
             `api_request`."
        )
    } else {
        format!(
            "Enabled toolsets: {enabled_list}; tools of other toolsets (and above the access \
             level) are not registered, and calling them explains what enables them."
        )
    };
    let custom = if options.custom_tools.is_empty() {
        ""
    } else {
        "\n- Custom tools: some tools are defined by the user in `tools.toml`: each calls one \
         operation with some arguments fixed (you cannot pass them) and may filter the \
         response; its description says which operation. A custom tool has the access level \
         and the toolset of its operation."
    };
    let composite = if registry
        .iter()
        .any(|tool| matches!(tool.kind, ToolKind::Composite(_)))
    {
        "\n- Composite tools: some tools run several API calls for a common task (their \
         description says what they do). Prefer them to chains of operation tools."
    } else {
        ""
    };
    // В динамическом режиме набор `apps` может включиться позже.
    let search_apps = if registry.get(SEARCH_APPS).is_some()
        || (registry.is_dynamic()
            && options
                .composite_tools
                .iter()
                .any(|tool| tool.name == SEARCH_APPS))
    {
        " When the user names an app by its name or package name, or the app ID is unknown, \
         call `search_apps` first: it lists the apps of the account with their app IDs, \
         package names and projects, finds them by package name or app name, and falls back \
         to `list_app_ids` when the project list is not available to the API client."
    } else {
        ""
    };
    format!(
        "AppGallery Connect MCP server ({level} access). Every operation tool calls one Huawei \
         AppGallery Connect API operation (Publishing, Comments, Reports, PMS, Project \
         Management, Provisioning, Remote Configuration) and returns its JSON response. {account} \
         {toolsets}\n\
         \n\
         - Access: {access}\n\
         - Region: the profile works with the AppGallery Connect site `{region_slug}` \
         ({region_title}, `{host}`). Apps of accounts on another site are not visible here: \
         if an app the user names is not found, ask whether the account is on another site \
         (China, Germany, Singapore or Russia); {region_change}\n\
         - Apps: tools address an app by its numeric app ID (`appId`, like `100000001`; \
         AppGallery Connect shows it in My apps > the app > App information), not by the \
         package name. `list_app_ids` returns the app IDs for package names (`packageName`); \
         otherwise ask the user for the app ID.{search_apps}{defaults}\n\
         - Arguments: path, query and header parameters are plain arguments named as in the \
         Huawei reference (`appId`, `lang`, `releaseType`); a JSON request body goes in the \
         `body` argument as a JSON object. Unknown arguments are rejected, required \
         arguments and the types of arguments are checked before the request is sent. Inside \
         `body` only the types of known fields are checked: the reference is not always \
         exact about required fields, so AppGallery Connect decides.\n\
         - Filtering: operation tools with a JSON response, composite tools, custom tools and \
         `api_request` accept an optional `jq` argument, a jq filter (jaq dialect) applied to the JSON \
         response body including its result envelope, for example `[.languages[] | {{lang, \
         appName}}]` for `get_app_info` or `[.appids[] | .value]` for `list_app_ids`. Use it \
         to keep large responses small. Credentials are redacted before the filter runs, and \
         the `note` about them stays outside the filter: such a result is wrapped as \
         `{{\"result\": <filter output>, \"note\": ...}}`. The size limit applies to the \
         filtered result. Several outputs are returned as a JSON array, no output as `[]`. \
         Filters cannot read the environment, files or other inputs. A broken filter is \
         reported before the request is sent; a filter that fails at runtime (after the \
         request succeeded) returns an error that says so: do not repeat a request that \
         changed data just to fix the filter. `jq` cannot be combined with `save_to`.{custom}\n\
         - Reference: every operation tool description ends with a link to its page in the \
         Huawei API reference; read it when the arguments or the request body are unclear.\n\
         - Finding operations: `api_search` searches all AppGallery Connect API operations by \
         words in English or Russian (tool names, operation IDs, summaries, paths), including \
         tools not enabled here and operations without a tool, and tells the toolset, the \
         access level and which flag or toolset enables a tool; with `tool_name` it returns \
         the full argument schema of one tool. Use it when no listed tool fits. `api_request` \
         calls any Connect API path on the host of the profile region (`method`, a `path` \
         starting with `/api/` such as `/api/publish/v2/app-info`, `query`, `headers` for \
         header parameters such as `appId` of the PMS API, and `body`) within the server's \
         access level; toolsets do not limit it. Its access level comes from the operation \
         the method and path match (unknown paths: GET reads, POST, PUT and PATCH write, \
         DELETE deletes), and errors with HTTP 200 are reported as for operation tools. Prefer \
         the dedicated tool when it is registered: it validates arguments and documents the \
         schema. Secret fields of the matched operation are hidden as in its tool; \
         `api_request` has no `save_to`.\n\
         - Errors: AppGallery Connect also reports errors with HTTP 200 and a non-zero result \
         code in the response body (`ret.code`, `ret.rtnCode` or `error.errorCode`). The \
         server returns such responses as tool errors with the Huawei code and message, so a \
         successful tool result means the result code was 0. HTTP 401 and 403 and \
         permission codes mean that the API client lacks a role or the app belongs to another \
         account: tell the user what to check instead of retrying.\n\
         {projects}\n\
         - Remote Configuration (toolset `remote-config`): requests are signed with the \
         project-level API client of the project in `productId` (the project ID), not with \
         the team-level API client; when it is missing, the error says what the user has to \
         run. A new version takes effect for the users of the app at once: change parameters \
         with `remote_config_set`, show its dry run to the user and write only after they \
         confirm. Never send values hidden as \"***\" back to the API.\n\
         - Uploads: the upload steps of the Upload Management API are not available as tools \
         or calls: their responses are one-time upload URLs and authorization headers.\
         {composite}\n\
         - Large responses are rejected with an error; narrow the request (a smaller page, \
         stricter filters, fewer languages) or select less with `jq`.\n\
         - Secrets: {credentials} Never ask the user to paste \
         client secrets, tokens, `agc-apiclient-*.json` or `agconnect-services.json` files \
         into the conversation, and never write them to files, commits or messages. \
         {rejected} Values that look like credentials (client and app secrets, access \
         tokens, API keys, upload codes and URLs, authorization headers, private keys, signed \
         URLs) are replaced with \"***\" in every response, including errors, and a `note` \
         says what was hidden and why. Some responses are mostly secret: the app secret \
         (`get_app_fingerprints`) is never returned; the content of `agconnect-services.json` \
         (`get_app_config_file`) and temporary links to reports, certificates and profiles \
         are hidden, and those tools take `save_to`: the server saves the file locally and \
         returns its path and size. Do not try to recover hidden values, and do not print \
         or commit saved configuration files.\n\
         - Personal data: reviews contain data of real users; use it only for the task at \
         hand.",
        level = access_name(options.max_access),
        access = access_text(options.max_access, options.http),
        account = account_text(options.http, profile, team),
        region_change = region_change_text(options.http, profile, team),
        credentials = credentials_text(options.http, team),
        rejected = rejected_text(options.http, profile, team),
        projects = projects_text(team),
        region_slug = region.slug(),
        region_title = region.title(),
        host = region.host(),
    )
}

/// Раздел инструкций о локальных файлах: только если среди инструментов есть такие, что
/// пишут файлы (`save_to`; свои инструменты — поверх таких операций). В динамическом режиме
/// наборы включаются позже, поэтому смотрятся все операции в пределах уровня доступа.
/// Пустая строка — раздела нет.
pub fn files_instructions(
    options: &ServerOptions,
    registry: &ToolRegistry,
    files: &FilePolicy,
) -> String {
    // Составной инструмент пишет файл, если у него есть аргумент `save_to`.
    let composite_saves = |tool: &crate::composite::CompositeTool| {
        (tool.input_schema)()
            .pointer(&format!("/properties/{}", crate::files::SAVE_TO_ARG))
            .is_some()
    };
    let saves = if options.dynamic_toolsets {
        appgallery_connect_api::spec::operations()
            .iter()
            .any(|op| op.saves_file() && options.max_access.allows(op.access))
            || options
                .composite_tools
                .iter()
                .any(|tool| options.max_access.allows(tool.access) && composite_saves(tool))
    } else {
        registry.iter().any(|tool| match tool.kind {
            ToolKind::Operation(op) => op.saves_file(),
            ToolKind::Custom(index) => options
                .custom_tools
                .get(index)
                .is_some_and(|custom| custom.operation.saves_file()),
            ToolKind::Composite(index) => options
                .composite_tools
                .get(index)
                .is_some_and(composite_saves),
            _ => false,
        })
    };
    if options.http {
        return http_files_instructions(files, saves);
    }
    if !saves {
        return String::new();
    }
    format!(
        "\n- Local files: `save_to` takes a path (absolute, or relative to the server's \
         working directory) of a new file inside the allowed directories: {}. Other paths \
         are rejected; the user can allow more directories for the server's profile with \
         `appgallery-connect-mcp ui`. Existing files are never \
         overwritten, and the parent directory must exist. Files are downloaded only from \
         Huawei hosts. `save_to` cannot be combined with `jq`.",
        files.describe_roots()
    )
}

/// Раздел о локальных файлах HTTP-сервера: файлы клиента серверу недоступны, файловые
/// аргументы — пути в каталогах сервера из `--allow-path`, без них — ни одного.
fn http_files_instructions(files: &FilePolicy, saves: bool) -> String {
    if files.is_disabled() {
        return "\n- Local files: this server runs remotely over HTTP and has no file directory: \
                tools that need files (package uploads, listing export and import, image sync, \
                saving the app configuration file) are not registered, and `save_to` is \
                rejected. Hidden values stay hidden."
            .to_owned();
    }
    let save_to = if saves {
        " `save_to` cannot be combined with `jq`."
    } else {
        ""
    };
    format!(
        "\n- Local files: this server runs remotely over HTTP. File arguments (`save_to`, the \
         files and directories of composite tools) are paths on the server, not on the user's \
         machine (relative paths start in the first directory), inside: {}. Other paths are \
         rejected; the server operator copies files in and out of these directories. Existing \
         files are never overwritten, and the parent directory must exist. Files are \
         downloaded only from Huawei hosts.{save_to}",
        files.describe_roots()
    )
}

/// Чей вход у сервера: API-клиент или пользователь HUAWEI ID в команде `team` (режим
/// OAuth-клиента); профиль пользователя (stdio) или учётные данные оператора сервера (HTTP).
fn account_text(http: bool, profile: &str, team: Option<&str>) -> String {
    match (http, team) {
        (true, None) => "This server runs remotely over HTTP and works as one AppGallery Connect \
                         API client of one developer account, set up by the server operator."
            .to_owned(),
        (true, Some(team)) => format!(
            "This server runs remotely over HTTP and works as a HUAWEI ID user signed in \
             through an OAuth client, in the team `{team}` of one developer account, with the \
             permissions of that user's role there; the sign-in is set up by the server \
             operator."
        ),
        (false, None) => format!(
            "This server works as one AppGallery Connect API client of one developer account: \
             the profile `{profile}`. For another account the user starts another server with \
             another profile."
        ),
        (false, Some(team)) => format!(
            "This server works as a HUAWEI ID user signed in through an OAuth client, in the \
             team `{team}` of one developer account, with the permissions of the user's role \
             there: the profile `{profile}`. For another team or account the user starts \
             another server with another profile."
        ),
    }
}

/// Как сменить регион (сайт AppGallery Connect). У входа пользователя регион — сайт команды.
fn region_change_text(http: bool, profile: &str, team: Option<&str>) -> String {
    match (http, team) {
        (true, _) => "the region of this server is set by the server operator.".to_owned(),
        (false, None) => format!(
            "the region is changed with `appgallery-connect-mcp auth login --profile {profile} \
             --region <cn|de|sg|ru>`."
        ),
        (false, Some(_)) => format!(
            "the region is the site of the team; the user chooses another team with \
             `appgallery-connect-mcp auth login --oauth --profile {profile}`."
        ),
    }
}

/// Где учётные данные: API-клиента или входа пользователя.
fn credentials_text(http: bool, team: Option<&str>) -> &'static str {
    match (http, team.is_some()) {
        (true, false) => {
            "the API client ID and secret are kept on the server (set up by the server \
             operator) and the access token only in the server's memory; they are used only by \
             the server, never leave it and are never passed in tool arguments."
        }
        (true, true) => {
            "the OAuth client and the refresh token of the user's sign-in are kept on the \
             server (set up by the server operator) and the access token only in the server's \
             memory; they are used only by the server, never leave it and are never passed in \
             tool arguments."
        }
        (false, false) => {
            "the API client ID and secret of the profile are kept in the system keychain and \
             the access token only in the server's memory; they are used only by the server \
             and are never passed in tool arguments."
        }
        (false, true) => {
            "the OAuth client and the refresh token of the user's sign-in are kept in the \
             system keychain and the access token only in the server's memory; they are used \
             only by the server and are never passed in tool arguments."
        }
    }
}

/// Что делать, если токен не выпускается или API его отверг (HTTP 401).
fn rejected_text(http: bool, profile: &str, team: Option<&str>) -> String {
    match (http, team) {
        (true, None) => "If getting a token fails or the API rejects it (HTTP 401), ask the \
                         user to have the server operator check the API client of the server."
            .to_owned(),
        (true, Some(_)) => "If getting a token fails or the API rejects it (HTTP 401), the \
                            user's sign-in has expired or was revoked: ask the user to have \
                            the server operator sign in again."
            .to_owned(),
        (false, None) => format!(
            "If getting a token fails or the API rejects it (HTTP 401), ask the user to check \
             the profile with `appgallery-connect-mcp auth status --profile {profile} --check` \
             and to sign in again with `appgallery-connect-mcp auth login --profile {profile}` \
             themselves."
        ),
        (false, Some(_)) => format!(
            "If getting a token fails or the API rejects it (HTTP 401), the sign-in has \
             expired or was revoked: ask the user to check it with `appgallery-connect-mcp auth \
             status --profile {profile} --check` and to sign in again with \
             `appgallery-connect-mcp auth login --oauth --profile {profile}` themselves."
        ),
    }
}

/// Пункт о Project Management API и о том, чего нет при входе пользователя.
fn projects_text(team: Option<&str>) -> &'static str {
    match team {
        Some(_) => {
            "- Projects and sign-in: the Project Management API (toolset `projects`: teams, \
             projects, services, the app configuration file, certificate fingerprints) works \
             for the signed-in user. Operations that the Huawei reference describes only for \
             API clients (the Comments API: reviews and replies) are refused before the \
             request with an error that says so; tell the user that they need a server with an \
             API client for them. Every call needs the API scope of its API (publish, \
             report/read, product, develop, project) granted to the OAuth client and at \
             sign-in: a permission or scope error means the sign-in needs more scopes \
             (`--oauth-scope`) or Huawei has to approve them."
        }
        None => {
            "- Projects: the Huawei reference describes the Project Management API (toolset \
             `projects`: teams, projects, services, the app configuration file, certificate \
             fingerprints) only for OAuth clients and service accounts. With the API client of \
             this server these operations may fail with a permission error; then tell the user \
             and use the Publishing API tools instead."
        }
    }
}

/// Уровень доступа для инструкций. В HTTP-режиме уровень задаёт токен доступа клиента
/// (оператор сервера), а не флаги, которые пользователь передаёт при запуске.
fn access_text(level: AccessLevel, http: bool) -> &'static str {
    match (level, http) {
        (AccessLevel::Read, true) => {
            "This server is read-only for this access token: only tools that read data are \
             available. Tools that change data (app information, localized listings, package \
             files, certificate fingerprints, HarmonyOS certificates, devices and profiles) and \
             tools that release to users, affect money or delete data (submitting for review, \
             phased release and release time, public replies to reviews, products, prices and \
             promotions, deleting) need an access token with a higher access level from the \
             server operator. If a task needs them, ask the user instead of looking for \
             workarounds."
        }
        (AccessLevel::Write, true) => {
            "This server can read and change data with this access token, but not release, \
             delete or affect money: submitting for review, phased release and release time, \
             public replies to reviews, products, prices and promotions and deleting need an \
             access token with the destructive level from the server operator. Changes of app \
             information and listings before a submission stay invisible to users until the \
             version is released."
        }
        (AccessLevel::Read, false) => {
            "This server is read-only: only tools that read data are available. Tools that \
             change data (app information, localized listings, package files, certificate \
             fingerprints, HarmonyOS certificates, devices and profiles) are enabled when the \
             user allows write access for the server's profile, and tools that release to \
             users, affect money or delete data (submitting for review, phased release and \
             release time, public replies to reviews, products, prices and promotions, \
             deleting) with destructive access (the user does it with \
             `appgallery-connect-mcp ui`, then restarts the client). If a task needs them, ask \
             the user instead of looking for workarounds."
        }
        (AccessLevel::Write, false) => {
            "This server can read and change data, but not release, delete or affect money: \
             submitting for review, phased release and release time, public replies to \
             reviews, products, prices and promotions and deleting need destructive access \
             for the server's profile (`appgallery-connect-mcp ui`). Changes of app \
             information and listings before a submission stay invisible to users until the \
             version is released."
        }
        (AccessLevel::Destructive, _) => {
            "This server can read, change, release and delete data. Submitting a version for \
             review releases it to AppGallery users after approval, replies to reviews are \
             public, product and promotion changes affect what users pay, and deleting is \
             irreversible: confirm such calls with the user unless they explicitly asked for \
             them."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instructions(options: &ServerOptions, region: Region) -> String {
        let registry = ToolRegistry::new(options).unwrap();
        server_instructions(options, &registry, region, None)
    }

    #[test]
    fn instructions_name_the_team_of_a_user_sign_in() {
        let options = ServerOptions {
            profile: Some("guest".into()),
            ..ServerOptions::default()
        };
        let registry = ToolRegistry::new(&options).unwrap();
        let text = server_instructions(&options, &registry, Region::Ru, Some("7700"));
        assert!(text.contains("team `7700`"), "{text}");
        assert!(
            text.contains("auth login --oauth --profile guest"),
            "{text}"
        );
        assert!(text.contains("Comments API"), "{text}");
        assert!(text.contains("Remote Configuration"), "{text}");
        assert!(
            !text.contains("one AppGallery Connect API client"),
            "{text}"
        );
        let api = instructions(&options, Region::Ru);
        assert!(!api.contains("--oauth"), "{api}");
        assert!(api.contains("one AppGallery Connect API client"), "{api}");

        // HTTP-режим: вход настраивает оператор, команд CLI агенту не называют.
        let http = ServerOptions {
            http: true,
            ..options.clone()
        };
        let registry = ToolRegistry::new(&http).unwrap();
        let text = server_instructions(&http, &registry, Region::Ru, Some("7700"));
        assert!(text.contains("team `7700`"), "{text}");
        assert!(text.contains("server operator sign in again"), "{text}");
        assert!(!text.contains("auth login"), "{text}");
        assert!(!text.contains("--profile"), "{text}");
    }

    #[test]
    fn instructions_depend_on_access_profile_and_region() {
        let read = instructions(&ServerOptions::default(), Region::Cn);
        assert!(read.contains("(read-only access)"), "{read}");
        assert!(read.contains("write access") && read.contains("destructive access"));
        assert!(
            read.contains("Enabled toolsets: apps, releases, reviews;"),
            "{read}"
        );
        assert!(read.contains("auth login --profile default"), "{read}");
        assert!(read.contains("connect-api.cloud.huawei.com"), "{read}");
        assert!(read.contains("`list_app_ids`"), "{read}");
        // Набор `apps` включён по умолчанию: инструкции советуют `search_apps`.
        assert!(read.contains("call `search_apps` first"), "{read}");
        assert!(read.contains("numeric app ID"), "{read}");
        assert!(
            read.contains("HTTP 200 and a non-zero result code"),
            "{read}"
        );
        assert!(read.contains("`body`"), "{read}");
        assert!(read.contains("`api_search` searches"), "{read}");
        assert!(read.contains("`api_request` calls"), "{read}");
        assert!(read.contains("in English or Russian"), "{read}");
        assert!(read.contains("only for OAuth clients and service accounts"));
        assert!(read.contains("Never ask the user to paste"), "{read}");
        assert!(read.contains("\"***\""), "{read}");
        assert!(!read.contains("default apps"), "{read}");
        // Набор `reviews` включён по умолчанию: с ним регистрируется `reviews_summary`.
        assert!(read.contains("- Composite tools:"), "{read}");
        assert!(read.contains("optional `jq` argument"), "{read}");
        assert!(read.contains("no output as `[]`"), "{read}");
        assert!(read.contains("do not repeat a request"), "{read}");
        assert!(!read.contains("- Custom tools:"), "{read}");
        assert!(!read.contains("enable_toolset"), "{read}");

        let dynamic = instructions(
            &ServerOptions {
                dynamic_toolsets: true,
                ..ServerOptions::default()
            },
            Region::Cn,
        );
        assert!(
            dynamic.contains("enabled on demand; enabled at start: none."),
            "{dynamic}"
        );
        assert!(
            dynamic.contains("`enable_toolset`") && dynamic.contains("`disable_toolset`"),
            "{dynamic}"
        );
        assert!(
            dynamic.contains("access level does not change"),
            "{dynamic}"
        );
        assert!(dynamic.contains("call `search_apps` first"), "{dynamic}");
        let started = instructions(
            &ServerOptions {
                dynamic_toolsets: true,
                toolsets: Some(vec!["reviews".into()]),
                ..ServerOptions::default()
            },
            Region::Cn,
        );
        assert!(started.contains("enabled at start: reviews."), "{started}");

        let custom = crate::CustomTool::compile(
            serde_json::from_value(serde_json::json!({
                "name": "main_app_info",
                "description": "Main app",
                "operation": "get_app_info",
                "args": {"appId": "100000001"}
            }))
            .unwrap(),
            "tools.toml",
        )
        .unwrap();
        let with_custom = instructions(
            &ServerOptions {
                custom_tools: vec![custom],
                ..ServerOptions::default()
            },
            Region::Cn,
        );
        assert!(with_custom.contains("- Custom tools:"), "{with_custom}");

        let write = instructions(
            &ServerOptions {
                max_access: AccessLevel::Write,
                toolsets: Some(vec!["listing".into()]),
                profile: Some("work".into()),
                apps: vec!["100000001".into()],
                packages: vec!["com.example.app".into()],
                ..ServerOptions::default()
            },
            Region::Ru,
        );
        assert!(write.contains("(write access)"), "{write}");
        // В наборе `listing` есть составные инструменты страницы приложения.
        assert!(write.contains("- Composite tools:"), "{write}");
        assert!(!write.contains("`search_apps`"), "{write}");
        assert!(write.contains("Enabled toolsets: listing;"), "{write}");
        assert!(write.contains("auth login --profile work"), "{write}");
        assert!(write.contains("the profile `work`"), "{write}");
        assert!(
            write.contains("connect-api-drru.cloud.huawei.com"),
            "{write}"
        );
        assert!(
            write.contains("app IDs `100000001`; package names `com.example.app`"),
            "{write}"
        );

        let all = instructions(
            &ServerOptions {
                max_access: AccessLevel::Destructive,
                ..ServerOptions::default()
            },
            Region::De,
        );
        assert!(all.contains("(destructive access)"), "{all}");
        assert!(all.contains("irreversible"), "{all}");
    }

    #[test]
    fn http_mode_has_no_secret_storage() {
        let api =
            || ApiClient::new(appgallery_connect_api::ClientConfig::new(Region::Cn, None)).unwrap();
        let vault = Arc::new(crate::composite::MemoryVault::default());
        let options = ServerOptions {
            http: true,
            ..ServerOptions::default()
        };
        let server =
            AppGalleryServer::new(Services::new(api()).with_vault(vault.clone()), options).unwrap();
        let err = server
            .inner
            .context
            .vault
            .store("app", "value")
            .unwrap_err();
        let message = err.message();
        assert!(message.contains("HTTP mode"), "{message}");
        assert!(message.contains("not available"), "{message}");
        assert!(!message.contains("keychain"), "{message}");
        assert!(vault.value("app").is_none());

        // В stdio — хранилище, которое передал бинарник.
        let server = AppGalleryServer::new(
            Services::new(api()).with_vault(vault.clone()),
            ServerOptions::default(),
        )
        .unwrap();
        server.inner.context.vault.store("app", "value").unwrap();
        assert_eq!(vault.value("app").as_deref(), Some("value"));
    }

    #[test]
    fn instructions_for_http_mode() {
        let http = |level, allowed: Vec<PathBuf>| {
            let options = ServerOptions {
                max_access: level,
                profile: Some("ci".into()),
                allowed_paths: allowed,
                http: true,
                ..ServerOptions::default()
            };
            let registry = ToolRegistry::new(&options).unwrap();
            let files = http_file_policy(&options.allowed_paths).unwrap();
            let mut text = server_instructions(&options, &registry, Region::Cn, None);
            text.push_str(&files_instructions(&options, &registry, &files));
            text
        };
        let read = http(AccessLevel::Read, Vec::new());
        assert!(read.contains("runs remotely over HTTP"), "{read}");
        assert!(read.contains("server operator"), "{read}");
        assert!(read.contains("kept on the server"), "{read}");
        assert!(read.contains("has no file directory"), "{read}");
        // Флаги запуска, профиль и команды CLI к пользователю HTTP-сервера не относятся.
        assert!(!read.contains("--allow-write"), "{read}");
        assert!(!read.contains("auth login"), "{read}");
        assert!(!read.contains("auth status"), "{read}");
        assert!(!read.contains("system keychain"), "{read}");
        assert!(!read.contains("the profile `ci`"), "{read}");
        let write = http(AccessLevel::Write, Vec::new());
        assert!(write.contains("destructive level"), "{write}");
        assert!(!write.contains("--allow-destructive"), "{write}");

        let dir = tempfile::tempdir().unwrap();
        let with_files = http(AccessLevel::Write, vec![dir.path().to_path_buf()]);
        assert!(with_files.contains("paths on the server"), "{with_files}");
        assert!(!with_files.contains("--allow-path"), "{with_files}");
        assert!(!with_files.contains("working directory"), "{with_files}");
    }

    #[test]
    fn http_file_policy_uses_only_allowed_paths() {
        let disabled = http_file_policy(&[]).unwrap();
        assert!(disabled.is_disabled());
        assert!(disabled.check_write("report.csv").is_err());

        let dir = tempfile::tempdir().unwrap();
        let policy = http_file_policy(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(policy.roots().len(), 1);
        // Относительный путь — от каталога сервера, а не от рабочего каталога процесса.
        let target = policy.check_write("report.csv").unwrap();
        assert!(target.starts_with(dir.path().canonicalize().unwrap()));
        let tmp = std::env::temp_dir().join("outside-http-root.csv");
        if !tmp.starts_with(dir.path()) {
            assert!(policy.check_write(tmp.to_str().unwrap()).is_err());
        }
        assert!(http_file_policy(&[dir.path().join("missing")]).is_err());
    }

    #[test]
    fn size_limit_mentions_completed_changes() {
        assert!(check_size("abc", 3, None, None).is_ok());
        let err = check_size("abcd", 3, None, None).unwrap_err();
        assert!(err.message().contains("larger than 3 bytes"), "{err}");
        let err = check_size("abcd", 3, Some(200), None).unwrap_err();
        assert!(err.message().contains("succeeded (HTTP 200)"), "{err}");
        // Подсказка называет аргументы операции, которыми сужается ответ.
        let op = appgallery_connect_api::spec::find("list_reviews").unwrap();
        let err = check_size("abcd", 3, None, Some(op)).unwrap_err();
        assert!(
            err.message().contains("Arguments of `list_reviews`"),
            "{err}"
        );
        assert!(narrowing_arguments(op).is_some_and(|hint| hint.contains("`limit`")));
    }
}
