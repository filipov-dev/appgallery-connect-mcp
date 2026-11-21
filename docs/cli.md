# Команды CLI

`appgallery-connect-mcp` — один бинарник: MCP-сервер и команды для настройки. Справка по любой команде — `appgallery-connect-mcp <команда> --help`, версия — `appgallery-connect-mcp --version`.

| Команда | Что делает |
| --- | --- |
| [`auth login`](#auth-login), [`auth status`](#auth-status), [`auth logout`](#auth-logout) | Учётные данные API-клиента профиля в системном хранилище. |
| [`auth project add`, `auth project remove`](#auth-project-add) | API-клиенты проектов профиля ([API-клиент проекта](api-client.md#api-клиент-проекта)). |
| [`auth app-secret set`, `auth app-secret remove`](#auth-app-secret-set) | Секреты приложений профиля для серверных API покупок в приложении ([Секрет приложения](api-client.md#секрет-приложения)). |
| [`key exec`](#key-exec) | Запуск команды с учётными данными профиля во временном файле (Gradle-плагин, fastlane, свои скрипты). |
| [`connect`](#connect) | Регистрация сервера в Claude Code или Codex (и сохранение учётных данных, если их нет). |
| [`serve`](#serve) | MCP-сервер через stdio или HTTP ([HTTP-режим](http.md)). |
| [`call`](#call) | Вызов инструмента сервера из консоли с правами профиля. |
| [`ui`](#ui) | Интерактивный просмотр профилей: права и их одобрение, регистрации в клиентах, проверка API-клиента, инструменты. |
| [`toolsets`](#toolsets) | Наборы инструментов и число инструментов по уровням доступа. |
| [`tools list`, `tools check`](#tools) | Инструменты, которые зарегистрирует сервер, и проверка файлов `tools.toml`. |

Ещё есть скрытая служебная команда `jq-worker`: в ней сервер выполняет фильтры `jq` ([Фильтр jq](tools.md#фильтр-jq)). Вызывать её вручную не нужно.

См. также: [API-клиент](api-client.md), [Подключение к MCP-клиентам](clients.md), [MCP-инструменты](tools.md), [Безопасность](security.md).

## Профили

Профиль — один API-клиент одного кабинета AppGallery Connect (или вход пользователя HUAWEI ID в одной команде, `auth login --oauth`) и настройки сервера для него.

- Учётные данные (Client ID и секрет) хранятся только в системном хранилище: Keychain в macOS, Secret Service в Linux (GNOME Keyring, KDE Wallet, KeePassXC), Credential Manager в Windows. Запись — сервис `dev.filipov.appgallery-connect-mcp`, учётная запись — имя профиля, у API-клиента проекта — `<профиль>/project/<Project ID>`, у входа пользователя (OAuth-клиент и refresh token) — `<профиль>/oauth`, у секрета приложения — `<профиль>/app/<App ID>`. Файлового хранилища нет: в Linux без Secret Service команды завершаются ошибкой с подсказкой.
- Несекретные настройки — в `profiles.toml` в каталоге настроек: `~/.config/appgallery-connect-mcp/` (в Linux — `$XDG_CONFIG_HOME/appgallery-connect-mcp/`, если переменная задана; в macOS тоже `~/.config`; в Windows — `%APPDATA%\appgallery-connect-mcp\`). Там же лежит файл пользовательских инструментов `tools.toml`.
- Имя профиля — флаг `--profile`, иначе переменная окружения `APPGALLERY_CONNECT_MCP_PROFILE`, иначе `default`. Допустимы `A-Z`, `a-z`, `0-9`, `.`, `_`, `-`, до 64 символов, без `-` в начале.
- Секрет не читается из переменных окружения и `.env` и не принимается аргументом командной строки.
- Ни одна команда не выводит секрет, access token и refresh token.
- Права профиля (`allow_write`, `allow_destructive`, `allow_path`) — граница доступа сервера: `serve`, `call` и `ui` не дают больше. Меняет их `appgallery-connect-mcp ui` (или `connect --allow-write`/`--allow-destructive`/`--allow-path`, или правка файла). На macOS права сверх чтения действуют только после одобрения владельцем: одобренная копия прав хранится в Keychain (сервис `appgallery-connect-mcp.access`, учётная запись — имя профиля), а `ui` и `connect` при повышении прав спрашивают Touch ID или пароль пользователя macOS. Права из файла, которые шире одобренных, сервер урезает до одобренных и пишет предупреждение в stderr. В Linux и Windows одобрения нет: права действуют как записаны.

Пример `profiles.toml`:

```toml
[profiles.default]
region = "ru"
apps = ["100000001"]
packages = ["com.example.app"]
client = "claude-code"
allow_write = true
allow_destructive = false
allow_path = ["/Users/me/Projects/my-app"]
toolsets = []
dynamic_toolsets = true
projects = ["100000002"]
app_secrets = ["100000001"]
```

| Поле | Что значит | Кто пишет |
| --- | --- | --- |
| `region` | Регион кабинета: `cn`, `de`, `sg` или `ru` ([Регион и хост](api-client.md#регион-и-хост)). У входа пользователя — по сайту команды. | `auth login`, `connect` (`--region`) |
| `type` | Способ входа: нет поля — API-клиент, `oauth` — вход пользователя. | `auth login` (`--oauth`) |
| `team_id` | Команда входа пользователя (заголовок `teamId`). | `auth login --oauth` |
| `apps` | App ID приложений по умолчанию (числа строкой). | `--default-app` |
| `packages` | Имена пакетов по умолчанию. | `--default-package` |
| `client` | MCP-клиент, в котором профиль подключён (`claude-code`, `codex`). | `connect` |
| `allow_write`, `allow_destructive` | Уровень доступа сервера: потолок для `serve` и `call`. | `ui`, `connect` (`--allow-write`, `--allow-destructive`) |
| `allow_path` | Дополнительные каталоги для локальных файлов (абсолютные пути). | `ui`, `connect` (`--allow-path`) |
| `toolsets` | Наборы при запуске; пустой список — наборы по умолчанию. | `connect` (`--toolsets`) |
| `dynamic_toolsets` | Наборы включает агент по запросу. | `connect` |
| `projects` | Project ID проектов, для которых в хранилище есть API-клиент проекта (без Client ID и секрета). | `auth project add`, `auth project remove` |
| `app_secrets` | App ID приложений, для которых в хранилище есть секрет приложения (без client ID и секрета). | `auth app-secret set`, `auth app-secret remove` |

Неизвестное поле в файле — ошибка. Файл можно править вручную.

## Переменные окружения

| Переменная | Что делает |
| --- | --- |
| `APPGALLERY_CONNECT_MCP_PROFILE` | Имя профиля, если нет `--profile`. |
| `APPGALLERY_CONNECT_MCP_CONFIG_DIR` | Каталог настроек вместо `~/.config/appgallery-connect-mcp` (`profiles.toml`, `tools.toml`). |
| `XDG_CONFIG_HOME` | В Linux — базовый каталог настроек. |
| `RUST_LOG` | Уровень логов `serve`, если нет `--log-level`. |
| `CODEX_HOME` | Каталог Codex для `connect codex` (по умолчанию `~/.codex`). |

Учётные данные из переменных окружения не читаются. Переменные, которые `key exec` задаёт дочерней команде, описаны в разделе [key exec](#key-exec).

## auth login

Сохраняет API-клиент профиля в системное хранилище.

```
appgallery-connect-mcp auth login [--profile <имя>] [--region cn|de|sg|ru]
    [--client-id <ID>] [--secret-file <путь> | --secret-stdin]
    [--default-app <App ID>]... [--default-package <имя пакета>]... [--no-verify]
```

- `--region` — сайт AppGallery Connect кабинета: `cn` (основной), `de` (Германия), `sg` (Сингапур), `ru` (Россия). Без флага — регион, сохранённый в профиле, иначе `cn`.
- `--client-id` — Client ID API-клиента. Без флага команда спрашивает его в терминале и перед вопросом печатает, как создать API-клиент ([API-клиент](api-client.md#создание-api-клиента)).
- `--secret-file` — файл, в котором только секрет клиента (ключ). Файл читается один раз, путь не запоминается.
- `--secret-stdin` — прочитать секрет из stdin, например из CLI менеджера паролей. Нужен `--client-id`.
- Без `--secret-file` и `--secret-stdin` секрет вводится в терминале скрыто. Аргументом командной строки секрет не принимается: такая команда завершается ошибкой, значение не повторяется.
- Пробелы и переводы строк по краям Client ID и секрета обрезаются.
- `--default-app` — App ID приложений по умолчанию (повторяемый флаг или через запятую); заменяет приложения профиля. Только число, как в AppGallery Connect (My apps → приложение → App information).
- `--default-package` — имена пакетов по умолчанию (повторяемый флаг или через запятую); заменяет пакеты профиля.
- `--no-verify` — сохранить без проверки. Без флага команда получает access token в регионе профиля; если не получилось, ничего не сохраняется.

Повторный `auth login` заменяет учётные данные профиля. В `profiles.toml` попадают регион и приложения по умолчанию, но не Client ID, не секрет и не путь к файлу секрета. Без терминала (CI, скрипты) вопросов нет: нужны `--client-id` и `--secret-file` или `--secret-stdin`.

```bash
appgallery-connect-mcp auth login --region ru --client-id <Client ID> --secret-file ~/agc-secret.txt
pass show agc/secret | appgallery-connect-mcp auth login --profile client --client-id <Client ID> --secret-stdin
```

### Вход пользователя (`--oauth`)

```
appgallery-connect-mcp auth login --oauth [--profile <имя>] [--oauth-client-id <ID>]
    [--secret-file <путь> | --secret-stdin] [--team-id <ID>] [--oauth-scope <область>]...
    [--redirect-uri http://127.0.0.1:<порт>/] [--no-browser] [--region cn|de|sg|ru]
    [--default-app <App ID>]... [--default-package <имя пакета>]...
```

Профиль работает от имени пользователя HUAWEI ID через OAuth-клиент, а не API-клиент ([Вход пользователя](api-client.md#вход-пользователя-oauth-клиент)).

- `--oauth-client-id` — client ID OAuth-клиента (App ID приложения с HUAWEI ID). Без флага его спрашивают в терминале, и перед вопросом печатается, где взять клиент.
- Секрет OAuth-клиента — как у API-клиента: `--secret-file`, `--secret-stdin` (нужен `--oauth-client-id`) или скрытый ввод; пустой ввод — клиент без секрета. `--client-id` и `--no-verify` с `--oauth` не сочетаются.
- `--team-id` — команда, в которой работать. Без флага: единственная команда пользователя выбирается сама, из нескольких выбирают в терминале, без терминала это ошибка. Если список команд не получен, без `--team-id` ничего не сохраняется, а с ним вход сохраняется без проверки команды.
- `--oauth-scope` — области Connect API: `publish`, `develop`, `report/read`, `project`, `product`, `product/read` или полный адрес области (повторяемый флаг или через запятую). По умолчанию — `publish` и `project`, `openid` запрашивается всегда. Если HUAWEI ID выдал не все области, команда об этом предупреждает.
- `--redirect-uri` — постоянный адрес возврата OAuth-клиента: только `http://127.0.0.1:<порт>/…` или `http://localhost:<порт>/…` с явным портом. Без флага используется случайный порт на 127.0.0.1.
- `--no-browser` — не открывать браузер: команда печатает ссылку, а вы вставляете адрес, на который вернулся браузер. То же происходит, если браузер открыть нельзя.
- `--region` — регион для списка команд. Регион профиля после входа берётся по сайту выбранной команды; если `--region` с ним не совпадает, это ошибка.

Команда проверяет, что по полученному refresh token выдаётся access token, и только после этого сохраняет вход в хранилище, а `type = "oauth"` и `team_id` — в профиль. Если что-то не так, ничего не сохраняется. API-клиент команды этого профиля после входа удаляется из хранилища. Обычный `auth login` возвращает профиль к API-клиенту и удаляет вход пользователя.

```bash
appgallery-connect-mcp auth login --oauth --profile guest --oauth-client-id <client ID> \
    --secret-file ~/oauth-secret.txt --redirect-uri http://127.0.0.1:8765/
```

## auth status

```
appgallery-connect-mcp auth status [--profile <имя>] [--check]
```

Показывает, доступно ли системное хранилище, и для профиля: регион и хост API, Client ID в маске, сохранены ли учётные данные, приложения и пакеты по умолчанию, сохранённые настройки доступа и наборы. Без `--profile` (и без `APPGALLERY_CONNECT_MCP_PROFILE`) — все профили `profiles.toml`; если профилей нет, это ошибка. Для каждого проекта из `projects` — Client ID клиента проекта в маске и сохранён ли он. Для каждого приложения из `app_secrets` — сохранён ли его секрет (строка `App secrets:`; client ID не выводится). `--check` ещё и получает access token по сохранённым учётным данным, в том числе по каждому клиенту проекта и токен уровня приложения по каждому секрету приложения.

Для профиля со входом пользователя — команда, client ID OAuth-клиента в маске, есть ли у него секрет, выданные области и запись хранилища `<профиль>/oauth`. `--check` обновляет access token по refresh token и проверяет, что команда профиля есть в списке команд пользователя. Новый refresh token, если HUAWEI ID его выдаст, сохраняется.

Код возврата ненулевой, если профиль не готов: нет учётных данных (в том числе клиента проекта из `projects` или секрета приложения из `app_secrets`), хранилище недоступно или проверка не прошла.

## auth logout

```
appgallery-connect-mcp auth logout [--profile <имя>] [--forget]
```

Удаляет учётные данные профиля (API-клиент или вход пользователя), API-клиенты его проектов и секреты приложений из системного хранилища и профиль из `profiles.toml`. `--forget` сначала отзывает вход пользователя в HUAWEI ID; если отозвать не удалось, ничего не удаляется. Без `--forget` refresh token остаётся действительным до истечения срока, и команда об этом предупреждает. Выход из несуществующего профиля — не ошибка. Если хранилище недоступно, профиль не трогается. В AppGallery Connect API-клиент остаётся, пока вы его не удалите ([Отзыв доступа](api-client.md#отзыв-доступа)).

## auth project add

Сохраняет API-клиент проекта в профиль ([API-клиент проекта](api-client.md#api-клиент-проекта)). Он нужен операциям серверных API сервисов проекта — сейчас это наборы `remote-config` (Remote Configuration) и `app-linking` (App Linking); остальные операции идут от API-клиента команды профиля.

```
appgallery-connect-mcp auth project add [--profile <имя>] --credential-file <agc-apiclient-….json> [--project-id <ID>] [--no-verify]
appgallery-connect-mcp auth project add [--profile <имя>] --project-id <ID> [--client-id <ID>]
    [--secret-file <путь> | --secret-stdin] [--no-verify]
```

- Профиль уже должен быть в `profiles.toml` (`auth login`): регион клиента проекта — регион профиля.
- `--credential-file` — файл `agc-apiclient-*.json` (проект → Project settings → Server SDK → API client → Download credential). Из него берутся `project_id`, `client_id` и `client_secret`. Файл API-клиента команды (`"type": "team_client_id"`) не принимается: для него — `auth login`. Если в файле есть `region`, он должен совпадать с регионом профиля. Если задан и `--project-id`, он должен совпадать с `project_id` файла. Неизвестные поля файла пропускаются; в сообщениях об ошибке разбора нет значений из файла. Путь не запоминается, после команды файл можно удалить.
- Без файла: `--project-id` (Project settings → General information), `--client-id` (без флага спрашивается в терминале) и секрет — `--secret-file`, `--secret-stdin` или скрытый ввод, как у `auth login`.
- Project ID — от 1 до 64 символов `A-Z`, `a-z`, `0-9`, `_`, `-`.
- Без `--no-verify` команда получает access token клиентом проекта на хосте региона профиля; если не получилось, ничего не сохраняется.
- Клиент записывается в хранилище под учётной записью `<профиль>/project/<Project ID>`, Project ID — в `projects` профиля. Повторная команда заменяет клиент проекта.
- Запущенный сервер клиента не увидит: перезапустите MCP-клиент.

## auth project remove

```
appgallery-connect-mcp auth project remove [--profile <имя>] --project-id <ID>
```

Удаляет API-клиент проекта из хранилища и Project ID из `projects` профиля. В AppGallery Connect клиент остаётся.

## auth app-secret set

Сохраняет секрет приложения в профиль ([Секрет приложения](api-client.md#секрет-приложения)). Он нужен инструментам набора `purchases` (серверные API покупок в приложении HMS IAP): они не принимают API-клиент.

```
appgallery-connect-mcp auth app-secret set [--profile <имя>] --app <App ID> [--client-id <ID>]
    [--secret-file <путь> | --secret-stdin] [--no-verify]
```

- Профиль уже должен быть в `profiles.toml` (`auth login`).
- `--app` — App ID (My apps → приложение → App information), только цифры, до 20 знаков.
- `--client-id` — OAuth client ID приложения, если он не совпадает с App ID; по справочнику IAP client ID — это App ID, поэтому обычно флаг не нужен.
- Секрет — client secret приложения (App information или All services → In-App Purchases): `--secret-file`, `--secret-stdin` или скрытый ввод, как у `auth login`. Аргументом командной строки он не принимается.
- Без `--no-verify` команда получает токен уровня приложения у Huawei OAuth (`oauth-login.cloud.huawei.com`); если не получилось, ничего не сохраняется.
- Секрет записывается в хранилище под учётной записью `<профиль>/app/<App ID>`, App ID — в `app_secrets` профиля. Повторная команда заменяет секрет.
- Запущенный сервер секрета не увидит: перезапустите MCP-клиент.

```bash
appgallery-connect-mcp auth app-secret set --profile work --app 100000001 --secret-file ~/app-secret.txt
```

## auth app-secret remove

```
appgallery-connect-mcp auth app-secret remove [--profile <имя>] --app <App ID>
```

Удаляет секрет приложения из хранилища и App ID из `app_secrets` профиля. Сам секрет в AppGallery Connect остаётся; если он мог утечь, сбросьте его там.

## key exec

Запускает команду с учётными данными API-клиента профиля во временном файле — для программ, которым нужны Client ID и секрет: Gradle-плагин публикации в AppGallery, fastlane, свои скрипты.

```
appgallery-connect-mcp key exec [--profile <имя>] [--project-id <ID>] [--format json|dotenv] [--env <ИМЯ>]...
    -- <команда> [аргументы...]
```

- `--project-id` — отдать вместо API-клиента команды API-клиент этого проекта ([auth project add](#auth-project-add)) в том же формате файла. Если его нет, команда завершается ошибкой с подсказкой `auth project add`.
- Учётные данные берутся из системного хранилища по профилю. Если их нет, команда завершается ошибкой с подсказкой `auth login --profile <имя>`, файл не создаётся и команда не запускается.
- Профиль со входом пользователя (`auth login --oauth`) API-клиента команды не имеет: `key exec` для него завершается ошибкой, refresh token другим программам не отдаётся. `--project-id` с таким профилем работает.
- Учётные данные записываются в новый файл с правами 0600 в новом временном каталоге с правами 0700. В Windows вместо режимов — ACL: доступ к каталогу и файлу только у текущего пользователя.
- Команда запускается без оболочки, stdin наследуется. Переменные окружения команды:

| Переменная | Что в ней |
| --- | --- |
| `APPGALLERY_CONNECT_CREDENTIALS_FILE` | Путь к файлу учётных данных. |
| `APPGALLERY_CONNECT_REGION` | Регион профиля: `cn`, `de`, `sg` или `ru` ([Регион и хост](api-client.md#регион-и-хост)). Не секрет. |

- `--env NAME` (повторяемый) кладёт путь в `NAME` вместо `APPGALLERY_CONNECT_CREDENTIALS_FILE`; тогда `APPGALLERY_CONNECT_CREDENTIALS_FILE` убирается из окружения команды, чтобы она не указала на другой файл.
- Сами Client ID и секрет в переменные окружения и аргументы не попадают. Унаследованные `HUAWEI_CLIENT_ID` и `HUAWEI_CLIENT_SECRET` убираются, чтобы команда не взяла чужой API-клиент; использовать их в `--env` нельзя.
- Аргументы команды не раскрываются: чтобы подставить переменную, запустите оболочку сами, в одинарных кавычках.

### Формат файла

| `--format` | Файл | Содержимое | Для чего |
| --- | --- | --- | --- |
| `json` (по умолчанию) | `credentials.json` | `{"client_id": "…", "client_secret": "…"}` | Gradle-плагин `ru.cian.huawei-publish-gradle-plugin` (`credentialsPath`), свои скрипты (`jq`, `JSON.parse`). |
| `dotenv` | `credentials.env` | `HUAWEI_CLIENT_ID='…'` и `HUAWEI_CLIENT_SECRET='…'` | fastlane: `Dotenv.load` в Fastfile и параметры `client_id`, `client_secret` плагина `huawei_appgallery_connect`; оболочка: `. "$APPGALLERY_CONNECT_CREDENTIALS_FILE"`. |

Почему так:

- Плагин fastlane `fastlane-plugin-huawei_appgallery_connect` файла учётных данных не читает: Client ID и секрет передаются параметрами действий, а в примерах они берутся из `ENV['HUAWEI_CLIENT_ID']` и `ENV['HUAWEI_CLIENT_SECRET']`. Класть секрет в окружение `key exec` не будет, поэтому `dotenv` — файл с этими же именами: Fastfile загружает его сам, и значения появляются только в процессе fastlane.
- Gradle-плагин `ru.cian.huawei-publish-gradle-plugin` читает JSON с полями `client_id` и `client_secret` по пути `credentialsPath` (или флагом `--credentialsPath`). Формат `json` совпадает с ним и с записью профиля в системном хранилище. Других полей в файле нет, чтобы его принимали и строгие разборщики; регион — в `APPGALLERY_CONNECT_REGION`.
- Файл в формате `agc-apiclient-*.json` (раздел «Server SDK» настроек проекта) `key exec` не создаёт и с `--project-id`: в файле те же два поля. Серверным SDK, которым нужен именно этот формат, передайте скачанный из AppGallery Connect файл.
- Значения в `dotenv` записываются в одинарных кавычках: ни оболочка, ни dotenv не подставляют в них `$`. Если в Client ID или секрете есть одинарная кавычка, `--format dotenv` завершается ошибкой (без значения) — используйте `json`.

Примеры:

```
appgallery-connect-mcp key exec --profile work -- sh -c \
    './gradlew publishHuaweiAppGalleryRelease --credentialsPath "$APPGALLERY_CONNECT_CREDENTIALS_FILE"'
appgallery-connect-mcp key exec --profile work --format dotenv -- fastlane android deploy
appgallery-connect-mcp key exec --profile work --env AGC_CREDENTIALS -- ./scripts/upload.sh
```

Fastfile для второго примера:

```ruby
Dotenv.load(ENV.fetch('APPGALLERY_CONNECT_CREDENTIALS_FILE'))

lane :deploy do
  huawei_appgallery_connect(
    client_id: ENV['HUAWEI_CLIENT_ID'],
    client_secret: ENV['HUAWEI_CLIENT_SECRET'],
    app_id: '100000001',
    apk_path: 'app/build/outputs/apk/release/app-release.apk'
  )
end
```

### Вывод, завершение и сигналы

- В stdout и stderr команды на `***` заменяются секрет и Client ID (как есть и в base64), содержимое файла целиком (и в base64) и access token AppGallery Connect (JWT). Это страховка от случайного вывода, а не граница безопасности: не выводите учётные данные.
- Файл и каталог удаляются после завершения команды — и при ошибке, и при ненулевом коде, и если команду не удалось запустить, и при прерывании: Ctrl-C команда получает от терминала, SIGTERM и SIGHUP пересылаются ей. От принудительного завершения (`kill -9`) защиты нет: тогда каталог останется во временном каталоге пользователя, доступный только ему.
- В Windows Ctrl+C и Ctrl+Break команда получает от консоли сама. Команда и запущенные ею процессы работают в объекте задания: когда команда завершилась или закрывается окно консоли (выход из системы, выключение), оставшиеся процессы останавливаются и файл удаляется. Команда без расширения ищется и как `.cmd` и `.bat` (`gradlew` → `gradlew.bat`). Переменные в аргументах подставляет оболочка, которую вы запустите сами, например PowerShell:

```powershell
appgallery-connect-mcp key exec --profile work -- powershell -NoProfile -Command './gradlew.bat publishHuaweiAppGalleryRelease --credentialsPath $env:APPGALLERY_CONNECT_CREDENTIALS_FILE'
```

- Код завершения — код команды (при завершении сигналом в Unix — `128 + номер сигнала`).

## connect

Подключает сервер к MCP-клиенту одной командой: при необходимости сохраняет учётные данные профиля, меняет права профиля (если заданы флаги доступа) и регистрирует сервер в клиенте.

```
appgallery-connect-mcp connect <claude-code|codex> [--profile <имя>] [--name <имя сервера>]
    [--scope user|project|local] [--region cn|de|sg|ru]
    [--client-id <ID>] [--secret-file <путь> | --secret-stdin]
    [--default-app <App ID>]... [--default-package <имя пакета>]...
    [--allow-write] [--allow-destructive] [--toolsets <набор,...>] [--no-dynamic-toolsets]
    [--allow-path <каталог>]... [--yes] [--force] [--dry-run] [--no-verify] [--silent]
```

Учётные данные:

- Если у профиля уже есть учётные данные и по ним выдаётся access token, они используются как есть. Повторный `connect` с другими флагами меняет только настройки профиля и запись в клиенте.
- Иначе команда печатает, как создать API-клиент, и спрашивает Client ID и секрет, как `auth login`. `--region`, `--client-id`, `--secret-file`, `--secret-stdin`, `--default-app` и `--default-package` значат то же, что у `auth login`; флаги учётных данных заменяют сохранённые.
- Учётные данные проверяются получением access token; если проверка не прошла, ничего не сохраняется и не регистрируется.
- `--no-verify` — сохранить новые учётные данные без проверки и не проверять сохранённые.
- Профиль со входом пользователя (`auth login --oauth`) `connect` не создаёт: сначала войдите через `auth login --oauth`. Для такого профиля `connect` проверяет сохранённый вход (обновляет access token и ищет команду в списке команд, без `--no-verify`), флаги API-клиента и `--region`, не совпадающий с регионом команды, отклоняет.
- `--silent` — тихий режим для скриптов и агентов: ни вопросов, ни вывода, кроме ошибок. Включает `--yes` (чужая запись с тем же именем всё равно заменяется только с `--force`). Нужны `--client-id` с `--secret-file` или `--secret-stdin`, если у профиля нет рабочих учётных данных.

Регистрация в клиенте:

- Клиент получает только команду `<абсолютный путь к appgallery-connect-mcp> serve --profile <профиль> [--toolsets …] [--dynamic-toolsets]`. Ни учётных данных, ни флагов доступа в конфигурации клиента нет: `serve` берёт учётные данные из системного хранилища, а права — из профиля.
- `--allow-write` ставит профилю уровень записи: инструменты, которые создают и меняют данные; `--allow-destructive` (включает и запись) — ещё удаление и действия с последствиями для пользователей AppGallery и денег. Без этих флагов права профиля не меняются (у нового профиля — только чтение). На macOS повышение прав спрашивает Touch ID или пароль пользователя macOS; если одобрения нет, `connect` всё равно регистрирует сервер, а права профиля остаются прежними.
- `--toolsets` — наборы через запятую (передаются в `serve --toolsets`); без флага — наборы по умолчанию `serve`.
- `--dynamic-toolsets` включён по умолчанию: агент подключает наборы по запросу. `--no-dynamic-toolsets` регистрирует сервер со всеми инструментами включённых наборов сразу.
- `--allow-path` — каталог, где сервер может читать и сохранять файлы, кроме рабочего и временного (повторяемый; каталог должен существовать). Флаг заменяет каталоги профиля (`allow_path`) абсолютными путями; в регистрацию не попадает.
- Эти же настройки сохраняются в профиле (`client`, `toolsets`, `dynamic_toolsets`, а с флагами доступа — `allow_write`, `allow_destructive`, `allow_path`).
- Повторный `connect` переписывает регистрацию старых версий, в которой были флаги доступа, на команду без них. Такие регистрации и до этого запускаются: флаги в пределах прав профиля их только сужают.
- Перед изменением команда показывает, что будет записано, и спрашивает подтверждение; `--yes` (`-y`) пропускает вопрос. Запись с тем же именем, которая запускает другой сервер или другой профиль, с одним `--yes` не заменяется: только после явного ответа в терминале или с `--force`. Если клиент уже запускает ровно эту команду, ничего не спрашивается и не меняется.
- `--dry-run` показывает, что изменилось бы в хранилище, профиле и конфигурации клиента, и ничего не меняет (но может прочитать хранилище и файл секрета и получить токен для проверки).
- `--scope`: `user` (по умолчанию) — для всех проектов пользователя; `project` — общий для проекта (Claude Code: `.mcp.json` в текущем каталоге); `local` — только для вас в текущем проекте (только Claude Code). Codex поддерживает только `user`.
- `--name` — имя сервера в клиенте, до 17 символов. По умолчанию `appgallery` для профиля `default` и `agc-<профиль>` для остальных ([Имя сервера](clients.md#имя-сервера)).

Примеры:

```
appgallery-connect-mcp connect claude-code --region ru
appgallery-connect-mcp connect codex --profile work --client-id <Client ID> --secret-file ~/agc-secret.txt \
    --allow-write --allow-path ~/Projects/my-app --yes
appgallery-connect-mcp connect claude-code --scope project --dry-run
appgallery-connect-mcp connect codex --profile ci --client-id <Client ID> --secret-file ./agc-secret.txt --silent
```

## serve

Запускает MCP-сервер через stdio. stdout занят протоколом MCP, логи и ошибки запуска — только в stderr. С `--http` сервер слушает сетевой адрес вместо stdio: флаги и устройство HTTP-режима — в [HTTP-режиме и Docker](http.md).

```
appgallery-connect-mcp serve [--profile <имя>] [--allow-write] [--allow-destructive]
    [--toolsets <набор,...>|all] [--dynamic-toolsets] [--tools-file <путь>]...
    [--allow-path <каталог>]... [--timeout <секунды>] [--max-response-bytes <байты>]
    [--log-level <уровень>] [--status]
appgallery-connect-mcp serve --http --access-tokens <путь> [--listen <IP:порт>]
    [--profile <имя> | --secret-file <путь> [--client-id <ID>]] [--region cn|de|sg|ru]
    [--allowed-host <имя>]... [--allowed-origin <origin>]... [--max-request-bytes <байты>]
    [--allow-write] [--allow-destructive] [--toolsets <набор,...>|all] [--tools-file <путь>]...
    [--allow-path <каталог>]... [--timeout <секунды>] [--max-response-bytes <байты>] [--log-level <уровень>]
```

- Учётные данные берутся только из системного хранилища по профилю, регион — из профиля. Профиль со входом пользователя работает от имени пользователя в команде профиля: заголовки `oauth2Token` и `teamId` ([Вход пользователя](api-client.md#вход-пользователя-oauth-клиент)). Access token запрашивается при первом вызове API и хранится только в памяти, поэтому запуск, `initialize` и `tools/list` обходятся без сети. Нет учётных данных или хранилище недоступно — сервер не запускается: код выхода 1, сообщение с подсказкой `auth login --profile` в stderr.
- Права — из профиля ([Профили](#профили)): уровень доступа и каталоги `allow_path`; без них — только чтение. Уровень записи регистрирует инструменты, которые создают и меняют данные (сведения о приложении, страница приложения, пакеты, настройки выпуска), уровень удаления (включает запись) — ещё и те, что затрагивают пользователей и деньги или необратимы: отправка на проверку, поэтапный выпуск и время выпуска, публичные ответы на отзывы, товары, цены и акции, удаление данных. Инструменты выше уровня не регистрируются; вызов такого инструмента объясняет, как поднять уровень.
- Флаги `--allow-write`, `--allow-destructive` и `--allow-path` права только сужают: флаг, который просит больше, чем разрешено в профиле, — ошибка запуска с подсказкой. На macOS права сверх одобренных отсекаются с предупреждением в stderr, сервер при этом запускается.
- `--toolsets` — наборы через запятую или `all`. По умолчанию — `apps,releases,reviews`. Неизвестный набор — ошибка запуска. Список наборов — `appgallery-connect-mcp toolsets`.
- `--dynamic-toolsets` (или `dynamic_toolsets = true` в профиле) — наборы по запросу агента: при запуске только `api_search`, `api_request`, `list_toolsets`, `enable_toolset`, `disable_toolset`, наборы из `--toolsets` или профиля (без них — ни одного) ([Динамические наборы](tools.md#динамические-наборы)).
- `--toolsets` заменяет наборы профиля; без флага действуют наборы профиля, без них — наборы по умолчанию.
- `--tools-file` — дополнительный файл пользовательских инструментов (повторяемый), см. [tools](#tools). Ошибка в файле — ошибка запуска с именем файла и инструмента.
- Каталоги для файлов: рабочий каталог сервера (если это не корень файловой системы и не домашний каталог), системный временный каталог и каталоги `allow_path` профиля. `--allow-path` (повторяемый) оставляет из каталогов профиля только указанные: каталог должен лежать внутри каталога профиля. Существующие файлы не перезаписываются ([Локальные файлы](security.md#локальные-файлы)).
- `--timeout` — таймаут одного запроса к API в секундах (по умолчанию 60).
- `--max-response-bytes` — предел размера ответа API и результата для агента (по умолчанию 262144 байта, 256 КиБ; к результату после фильтра `jq`). Больший результат — ошибка с подсказкой сузить запрос.
- `--log-level` — уровень или фильтр логов в stderr, например `info` или `appgallery_connect_mcp_core=debug`; без него — `RUST_LOG`, иначе `warn`.
- `--status` печатает профиль, имя сервера, сохранены ли учётные данные (без значений), регион и хост, уровень доступа и откуда он взят, одобрение прав (macOS) и предупреждение, если права урезаны до одобренных, наборы, режим динамических наборов, число инструментов, сколько в профиле API-клиентов проектов и каких из них нет в хранилище, пользовательские инструменты (сколько определено и сколько зарегистрировано), приложения и пакеты по умолчанию, разрешённые каталоги и путь к файлу профилей, и выходит без запуска сервера и без сети.

Аргументы инструментов операций — параметры пути, запроса и заголовков с именами, как в справочнике Huawei (`appId`, `lang`, `releaseType`); тело запроса — аргумент `body` (JSON-объект). Ответ — JSON-ответ AppGallery Connect API; ошибки, в том числе с HTTP 200 и ненулевым кодом в теле, возвращаются результатом с `isError: true` ([MCP-инструменты](tools.md#ответы-и-ошибки)).

## call

Вызывает инструмент сервера из консоли.

```
appgallery-connect-mcp call [--profile <имя>] [--allow-write] [--allow-destructive]
    [--allow-path <каталог>]... [--toolsets <набор,...>] [--tools-file <путь>]...
    [--timeout <секунды>] [--max-response-bytes <байты>]
    [<инструмент> [<имя>=<значение> | <имя>:=<JSON>]... [--args <JSON>|-] [--schema] [--yes] [--raw]]
```

- Без инструмента печатает инструменты, доступные профилю, с уровнем доступа и первой строкой описания. `--schema` печатает схему аргументов инструмента.
- Сервер собирается как в `serve` без флагов: права профиля (на macOS — одобренные), флаги только сужают. По умолчанию включены все наборы (`--toolsets` сужает), наборов по запросу нет. Клиент MCP работает в этом же процессе, поэтому проверка аргументов, уровни доступа, фильтр `jq` и вырезание секретов те же, что у агента.
- Аргументы: `имя=значение` — тип берётся из схемы инструмента (числа, логические значения, JSON-массивы и объекты; для массива строк — список через запятую), `имя:=JSON` — всегда JSON, `--args` — все аргументы одним JSON-объектом (`-` — из stdin). Флаги команды пишутся до имени инструмента.
- Инструменты уровня удаления спрашивают подтверждение в терминале; `--yes` — без вопроса, без терминала вызов отклоняется.
- Результат: текстовые блоки (JSON — с отступами); `--raw` — ответ MCP целиком. Код выхода — 0, если инструмент выполнился, и 1, если он вернул ошибку (`isError`) или вызов не удался.

```bash
appgallery-connect-mcp call list_app_ids packageName=com.example.app
appgallery-connect-mcp call get_app_info appId=100000001 jq=.appInfo.releaseState
appgallery-connect-mcp call --profile client api_request path=/api/publish/v2/app-info query:='{"appId": "100000001"}'
```

## ui

Интерактивный просмотр профилей в терминале.

```
appgallery-connect-mcp ui
```

Вкладки:

- **Profile** — настройки профиля и состояние учётных данных (сами значения не показываются): регион, API-клиент или вход пользователя с командой, API-клиенты проектов и секреты приложений (есть ли запись в хранилище), приложения и пакеты по умолчанию, наборы.
- **Access** — права профиля и их одобрение. `r`, `w`, `d` — уровень, `a` — добавить каталог, `x` — убрать, `u` — отменить, `s` — сохранить. На macOS сохранение прав шире одобренных спрашивает Touch ID или пароль пользователя macOS.
- **Clients** — регистрации сервера в Claude Code (`~/.claude.json`, `.mcp.json` текущего каталога) и Codex (`$CODEX_HOME/config.toml`): профиль, флаги и расхождения с правами профиля. `f` регистрирует запись заново без флагов доступа; запись проекта или локальную запись Claude Code — только из каталога этого проекта. Значения переменных окружения записей не читаются.
- **Account** — что может API-клиент в AppGallery Connect (`c` — проверить): выдаётся ли access token API-клиенту (или входу пользователя), API-клиентам проектов и секретам приложений, доступны ли приложения и пакеты по умолчанию и список устройств HarmonyOS. Роль API-клиента AppGallery Connect не сообщает: её видно в Users and permissions > API key > Connect API.
- **Tools** — инструменты, которые профиль даёт агенту (`c` — загрузить), `Enter` — вызвать с аргументами `имя=значение`, `?` — схема.

`q` или `Esc` — выход, `Tab` и `1`–`5` — вкладки, `F5` — перечитать. Без терминала (агент, CI) команда завершается ошибкой.

## toolsets

```
appgallery-connect-mcp toolsets
```

Печатает наборы инструментов: `TOOLSET`, число инструментов уровней чтения (`READ`), записи (`WRITE`) и удаления (`DESTRUCTIVE`) вместе с составными, `DEFAULT` (включён ли без `--toolsets`) и описание, в конце — итог. `api_search` и `api_request` в наборы не входят. Профиль, учётные данные и сеть не нужны.

## tools

```
appgallery-connect-mcp tools list [--allow-write] [--allow-destructive] [--toolsets <набор,...>|all]
    [--dynamic-toolsets] [--tools-file <путь>]...
appgallery-connect-mcp tools check [--tools-file <путь>]...
```

`tools list` печатает инструменты, которые `serve` зарегистрирует с этим уровнем и наборами: имя, уровень доступа, набор, группу API, метод и путь (у составных — `(composite)`, у пользовательских — `(custom: <операция>)`), в конце — число. Здесь флаги задают уровень напрямую, а настройки профиля не читаются: без флагов — инструменты чтения наборов по умолчанию. Полный список: `appgallery-connect-mcp tools list --toolsets all --allow-destructive`.

`tools check` проверяет файлы пользовательских инструментов: операции, фиксированные аргументы, фильтры `jq` и имена. Ошибка — ненулевой код выхода с именем файла и инструмента; `serve` с такой ошибкой не запускается.

Файл пользователя `tools.toml` в каталоге настроек (`~/.config/appgallery-connect-mcp/tools.toml`, в Windows `%APPDATA%\appgallery-connect-mcp\tools.toml`) читается всегда, если он есть. Другие файлы, например из репозитория проекта, — только по явному `--tools-file` у `serve` и у этих команд. Формат — в [MCP-инструментах](tools.md#пользовательские-инструменты-toolstoml).
