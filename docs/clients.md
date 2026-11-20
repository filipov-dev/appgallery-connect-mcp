# Подключение к MCP-клиентам

Сервер работает через stdio: MCP-клиент сам запускает `appgallery-connect-mcp serve --profile <профиль>` и общается с ним через stdin и stdout. Учётные данные API-клиента сервер берёт из системного хранилища по имени профиля, поэтому в конфигурации клиента их нет. Клиентам без локального бинарника (облачные агенты) — HTTP-режим с токеном доступа: [HTTP-режим и Docker](http.md#подключение-клиента).

Один профиль — один API-клиент одного кабинета. Для нескольких кабинетов, регионов или разных прав подключите несколько серверов с разными профилями ([Несколько профилей](#несколько-профилей-и-кабинетов)).

## Claude Code и Codex: connect

```bash
appgallery-connect-mcp connect claude-code
appgallery-connect-mcp connect codex
```

`connect` по шагам:

1. Если у профиля уже есть учётные данные в хранилище и по ним выдаётся access token, использует их без вопросов.
2. Иначе печатает, как создать API-клиент в AppGallery Connect ([API-клиент](api-client.md)), и спрашивает Client ID и секрет. Секрет вводится скрыто или читается из `--secret-file` или `--secret-stdin`. Без терминала и с `--silent` нужны `--client-id` и `--secret-file` или `--secret-stdin`.
3. Проверяет учётные данные получением access token в регионе профиля и сохраняет их в системное хранилище, а настройки — в профиль `~/.config/appgallery-connect-mcp/profiles.toml` (в Windows — `%APPDATA%\appgallery-connect-mcp\profiles.toml`). Если проверка не прошла, ничего не сохраняется и не регистрируется.
4. Показывает, что изменится в конфигурации клиента, и после подтверждения регистрирует сервер: в Claude Code — через `claude mcp add`, в Codex — таблицей `[mcp_servers.<имя>]` в `$CODEX_HOME/config.toml` (по умолчанию `~/.codex/config.toml`; комментарии и форматирование файла сохраняются).

После этого перезапустите клиент. Все флаги — в [справочнике CLI](cli.md#connect).

Регион по умолчанию — основной сайт `cn`. Для кабинета на другом сайте добавьте `--region ru`, `de` или `sg` ([Регион и хост](api-client.md#регион-и-хост)):

```bash
appgallery-connect-mcp connect claude-code --region ru
```

В клиент записывается только команда запуска, например:

```
/opt/homebrew/bin/appgallery-connect-mcp serve --profile default --dynamic-toolsets
```

Путь — абсолютный путь бинарника, из которого запущен `connect`; дальше — профиль, флаги доступа, наборы и каталоги. Client ID, секрета, токена и переменных окружения в записи нет.

Посмотреть, что `connect` сделает, ничего не меняя: `appgallery-connect-mcp connect claude-code --dry-run`. Повторный `connect` с теми же настройками ничего не спрашивает и не меняет.

Для скриптов и агентов есть тихий режим: `--silent` ничего не спрашивает и печатает только ошибки, а конфигурацию клиента записывает как с `--yes`. Учётные данные берутся из профиля или из флагов:

```bash
appgallery-connect-mcp connect codex --profile ci --client-id <Client ID> --secret-file ./agc-secret.txt --silent
```

### Права, наборы и каталоги

Флаги `connect` сохраняются в профиль и попадают в команду запуска сервера:

```bash
# Только чтение (по умолчанию)
appgallery-connect-mcp connect codex

# Загрузка пакетов, сведения о приложении, локализации, изображения — без отправки на проверку
appgallery-connect-mcp connect codex --allow-write

# Плюс отправка на проверку, поэтапный выпуск, ответы на отзывы, цены и товары, удаление
appgallery-connect-mcp connect codex --allow-destructive

# Наборы, включённые при запуске
appgallery-connect-mcp connect claude-code --toolsets apps,releases,listing,reviews

# Каталог со сборками и страницей приложения, кроме рабочего и временного
appgallery-connect-mcp connect claude-code --allow-write --allow-path ~/Projects/my-app/build
```

По умолчанию `connect` регистрирует сервер с `--dynamic-toolsets`: при запуске у агента только инструменты поиска, `api_request` и управления наборами (и наборы из `--toolsets`), а нужные разделы он включает сам. `--no-dynamic-toolsets` регистрирует сервер со всеми инструментами включённых наборов сразу ([Динамические наборы](tools.md#динамические-наборы)).

`--allow-path DIR` (повторяемый) записывается в команду сервера абсолютным путём: в этих каталогах, кроме рабочего и временного, инструменты могут читать и сохранять файлы ([Рабочий каталог и локальные файлы](#рабочий-каталог-и-локальные-файлы)). В профиль каталоги не сохраняются.

Чтобы поменять права, запустите `connect` ещё раз с нужными флагами: сохранённые учётные данные остаются, а флаги доступа, наборы и каталоги в профиле и в записи клиента перезаписываются после подтверждения (или сразу с `--yes`). Флаги `--client-id`, `--secret-file` и `--secret-stdin` заменяют сохранённые учётные данные.

### Область регистрации

| `--scope` | Claude Code | Codex |
| --- | --- | --- |
| `user` (по умолчанию) | для всех проектов пользователя | `~/.codex/config.toml` |
| `project` | `.mcp.json` текущего каталога, общий для проекта | нет |
| `local` | только для вас в текущем проекте | нет |

С `--scope project` учётные данные остаются в хранилище того, кто запускал `connect`, а в `.mcp.json` записан абсолютный путь к бинарнику на его машине: другим участникам проекта нужны свои учётные данные под тем же именем профиля (`appgallery-connect-mcp auth login --profile <профиль>`) и бинарник по тому же пути.

## Имя сервера

| Профиль | Имя сервера в клиенте |
| --- | --- |
| `default` | `appgallery` |
| любой другой, например `client` | `agc-client` (точки в имени профиля заменяются на `-`) |
| явно | `--name <имя>` |

Имя короткое из-за ограничения клиентов: они называют инструменты `mcp__<сервер>__<инструмент>` и ограничивают это имя 64 символами, а имена инструментов сервера бывают длиной до 40 символов. Значит, имя сервера должно быть не длиннее 17 символов: `appgallery` — 10, `agc-` и до 13 символов профиля. Более длинное имя `connect` отклоняет до первого вопроса: выберите короткий профиль или задайте `--name`.

Запись с тем же именем, которая запускает другой сервер или `appgallery-connect-mcp` с другим профилем, `connect` заменяет только после явного ответа в терминале или с `--force`; `--yes` без `--force` её не трогает.

## Несколько профилей и кабинетов

Каждый кабинет — свой API-клиент и свой профиль. Профили не мешают друг другу: у каждого своя запись в системном хранилище, свой регион, свои приложения по умолчанию и права.

```bash
# Основной кабинет на сайте «Россия», только чтение
appgallery-connect-mcp connect claude-code --region ru

# Кабинет клиента на сайте «Германия», с записью
appgallery-connect-mcp connect claude-code --profile client --region de --allow-write

# Тот же кабинет, но отдельный сервер с правом отправки на проверку
appgallery-connect-mcp connect claude-code --profile client-pub --region de --allow-destructive
```

В клиенте появятся серверы `appgallery`, `agc-client` и `agc-client-pub`. Профиль `client-pub` — отдельный профиль, ему нужны свои учётные данные (тот же API-клиент или другой). Агент видит, к какому серверу относится инструмент, по префиксу `mcp__<сервер>__`.

Список профилей и их состояние: `appgallery-connect-mcp auth status` (без `--profile` — все профили файла).

## Другие клиенты: ручное подключение

`connect` настраивает только Claude Code и Codex. Любой клиент, который запускает stdio MCP-серверы, подключается вручную:

1. Сохраните учётные данные в профиль:

   ```bash
   appgallery-connect-mcp auth login --profile cursor --region ru --client-id <Client ID>
   ```

2. Узнайте абсолютный путь к бинарнику: `command -v appgallery-connect-mcp`. Клиенты, запущенные не из терминала, часто не видят `PATH` оболочки.

3. Зарегистрируйте в клиенте stdio-сервер: команда — путь к `appgallery-connect-mcp`, аргументы — `serve --profile <профиль>` и нужные флаги. Формат `mcpServers`, который понимает большинство клиентов:

   ```json
   {
     "mcpServers": {
       "agc-cursor": {
         "command": "/opt/homebrew/bin/appgallery-connect-mcp",
         "args": ["serve", "--profile", "cursor", "--allow-write"]
       }
     }
   }
   ```

   Переменные окружения и учётные данные в записи не нужны. Если клиент кладёт в контекст описания всех инструментов сразу и поддерживает `notifications/tools/list_changed`, добавьте `"--dynamic-toolsets"`.

4. Перезапустите клиент и проверьте, что у сервера появились инструменты.

Если флаги доступа и `--toolsets` в `args` не указаны, сервер берёт их из настроек профиля, а без настроек работает только на чтение с наборами по умолчанию. Проверить, с какими настройками запустится сервер:

```bash
appgallery-connect-mcp serve --status --profile cursor
```

### Ручная регистрация в Claude Code и Codex

Если вы не хотите, чтобы `connect` менял конфигурацию, зарегистрируйте сервер сами после `auth login`:

```bash
claude mcp add --scope user appgallery -- "$(command -v appgallery-connect-mcp)" serve --profile default --dynamic-toolsets
```

```toml
# ~/.codex/config.toml
[mcp_servers.appgallery]
command = "/opt/homebrew/bin/appgallery-connect-mcp"
args = ["serve", "--profile", "default", "--dynamic-toolsets"]
```

### Windows

`connect`, `auth` и `serve` работают в Windows так же, команды можно запускать из PowerShell, `cmd` или Git Bash. Установка — [Установка: Windows](install.md#windows).

- **Учётные данные** хранятся в Windows Credential Manager: общие учётные данные `<профиль>.dev.filipov.appgallery-connect-mcp` (Панель управления → Диспетчер учётных данных → Учётные данные Windows). Удаляйте их командой `auth logout`, а не вручную. Настройки профилей — в `%APPDATA%\appgallery-connect-mcp\profiles.toml`. Credential Manager доступен только в сеансе, вошедшем с паролем ([Windows: Credential Manager недоступен](troubleshooting.md#windows-credential-manager-недоступен)): запускайте `connect`, `auth` и MCP-клиент в обычном сеансе пользователя.
- **Claude Code**: `connect claude-code` вызывает `claude mcp add`, поэтому команда `claude` должна быть в `PATH` (подходят и `claude.exe`, и `claude.cmd` из npm).
- **Codex**: `connect codex` пишет в `%USERPROFILE%\.codex\config.toml` (или в `config.toml` каталога из `CODEX_HOME`).
- В конфигурацию клиента записывается полный путь к `appgallery-connect-mcp.exe`, из которого запущен `connect`, например `C:\Users\<имя>\AppData\Local\Programs\appgallery-connect-mcp\appgallery-connect-mcp.exe`. Обновляйте бинарник заменой файла по тому же пути, тогда `connect` повторять не нужно.

Ручная регистрация в Codex в Windows (в TOML обратные косые черты внутри одинарных кавычек не экранируются):

```toml
# %USERPROFILE%\.codex\config.toml
[mcp_servers.appgallery]
command = 'C:\Users\<имя>\AppData\Local\Programs\appgallery-connect-mcp\appgallery-connect-mcp.exe'
args = ["serve", "--profile", "default", "--dynamic-toolsets"]
```

## Рабочий каталог и локальные файлы

Инструменты, которые читают или сохраняют файлы (загрузка пакетов, экспорт и применение страницы приложения, синхронизация изображений, отчёты, файл конфигурации, `save_to`), работают только с путями внутри рабочего каталога сервера, системного временного каталога и каталогов из `--allow-path`. Рабочий каталог сервера — тот, в котором клиент его запустил (обычно каталог проекта). Если клиент запускает сервер в домашнем каталоге или в корне файловой системы, рабочий каталог в разрешённые не попадает: добавьте нужные каталоги через `--allow-path`. Подробнее — [Безопасность](security.md#локальные-файлы).

## Проверка профиля

```bash
appgallery-connect-mcp auth status --profile client --check
```

Команда показывает профиль, регион и хост, Client ID в маске, есть ли учётные данные в хранилище (не их значение), приложения и пакеты по умолчанию, сохранённые настройки доступа и, с `--check`, получилось ли получить access token.

## Первый вызов

После перезапуска клиента попросите агента, например:

- «Какие приложения есть в кабинете AppGallery?» — `search_apps`;
- «Что с версией приложения `com.example.app`?» — `release_status`;
- «Сводка по приложению 100000001» — `app_overview`;
- «Разбери отзывы за месяц в России» — `reviews_summary`.

Если агент получает HTTP 403 или код ошибки про права, проверьте роли API-клиента и регион профиля ([Решение проблем](troubleshooting.md#нет-прав-http-403-и-коды-прав)).

## Отключение

1. Удалите запись из клиента: `claude mcp remove --scope user appgallery` (или `agc-<профиль>`), таблицу `[mcp_servers.<имя>]` из `~/.codex/config.toml` или запись из конфигурации другого клиента.
2. Удалите учётные данные из хранилища и профиль: `appgallery-connect-mcp auth logout --profile <профиль>`.
3. Если API-клиент больше нигде не нужен, удалите его в AppGallery Connect ([Отзыв доступа](api-client.md#отзыв-доступа)).
