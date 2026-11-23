# appgallery-connect-mcp

MCP-сервер и CLI для [AppGallery Connect API](https://developer.huawei.com/consumer/en/agconnect/) (Huawei). Даёт Claude Code, Codex и другим MCP-клиентам работать с кабинетом разработчика AppGallery: приложения, загрузка сборок, отправка на проверку и поэтапный выпуск, страница приложения и изображения, отзывы, отчёты, товары и подписки, проекты, подпись HarmonyOS, Remote Configuration, ссылки App Linking, проверка покупок и управление подписками пользователей (HMS In-App Purchases).

- **Операции API — MCP-инструменты.** У Huawei нет машинной спецификации Connect API, поэтому в проекте лежит свой снимок в формате OpenAPI, собранный по страницам справочника: Publishing API, Upload Management API, Reports API, Comments API, PMS API, Project Management API, Provisioning API, REST API Remote Configuration и серверный REST API App Linking, а также серверные API HMS In-App Purchases (сервисы заказов и подписок). Инструменты генерируются из него при сборке. `api_search` находит нужную операцию по-английски или по-русски, `api_request` вызывает любую в пределах уровня доступа.
- **Составные инструменты** делают за один вызов то, что в API требует нескольких шагов: загрузка APK и AAB (крупных — по частям), отправка на проверку, управление поэтапным выпуском, сводка выпуска и приложения, экспорт и применение страницы приложения, синхронизация изображений, отчёты, сводка отзывов, поиск приложений, файл конфигурации, сертификат подписи HarmonyOS по файлу CSR, изменение параметров Remote Configuration с просмотром разницы.
- **Учётные данные — только в системном хранилище.** Идентификатор и секрет API-клиента лежат в Keychain (macOS), Secret Service (Linux) или Credential Manager (Windows) и не читаются из переменных окружения. Access token живёт только в памяти процесса. Один профиль — один API-клиент одного кабинета; для Remote Configuration и App Linking к профилю добавляются API-клиенты проектов (`auth project add`), для покупок в приложении — секреты приложений (`auth app-secret set`).
- **По умолчанию только чтение.** Запись и действия, которые видят пользователи AppGallery или которые затрагивают деньги, включаются в настройках профиля; инструменты выше разрешённого уровня агенту не видны. На macOS права сверх чтения действуют только после одобрения владельцем через Touch ID или пароль.
- **Консоль и TUI.** `appgallery-connect-mcp call` вызывает любой инструмент из терминала с правами профиля, `appgallery-connect-mcp ui` показывает профили, их права, регистрации в клиентах и то, что может API-клиент в AppGallery Connect.
- **Секреты не попадают к агенту.** Секрет клиента, access token, секрет приложения, адреса и коды загрузки и временные ссылки на отчёты вырезаются из ответов и ошибок.

Обычно сервер работает через stdio: MCP-клиент сам запускает `appgallery-connect-mcp serve`. Для облачных агентов и общего сервера на несколько клиентов есть HTTP-режим `serve --http` с токенами доступа и образом Docker: учётные данные API-клиента остаются на сервере ([docs/http.md](docs/http.md)).

## Установка

### Установочный скрипт (Linux и macOS)

```bash
curl -fsSL https://codeload.github.com/filipov-dev/appgallery-connect-mcp/tar.gz/refs/heads/install | tar -xzO | sh
```

Скрипт ставит последнюю версию в `~/.local/bin` без `sudo` и сверяет архив с `SHA256SUMS` релиза; повторный запуск обновляет бинарник.

### Homebrew (macOS и Linux)

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/appgallery-connect-mcp
```

### Архивы и Windows

Готовые архивы — на [странице релизов на GitHub](https://github.com/filipov-dev/appgallery-connect-mcp/releases) (код — в [SourceCraft](https://sourcecraft.dev/filipov-dev/appgallery-connect-mcp) и в его копии на GitHub): macOS (универсальный, подписан и нотаризован), Linux x86_64 и aarch64 (статические, без зависимостей), Windows x86_64 (архив `.zip`, без подписи кода). Проверка контрольных сумм, установка в Windows, сборка из исходников и удаление — в [docs/install.md](docs/install.md).

## Быстрый старт

1. Создайте API-клиент в AppGallery Connect (это может только владелец аккаунта команды, подробно — в [docs/api-client.md](docs/api-client.md)):
   - Users and permissions → API key → Connect API → вкладка API client → Create;
   - поле Project оставьте **N/A**;
   - выберите роли: Administrator для всех инструментов или только нужные;
   - сохраните Client ID и ключ (секрет клиента) в файл или менеджер паролей.
2. Подключите сервер к клиенту:

   ```bash
   appgallery-connect-mcp connect claude-code
   # или
   appgallery-connect-mcp connect codex
   ```

   `connect` покажет, как создать API-клиент, спросит Client ID и секрет (секрет вводится скрыто), проверит их получением access token, сохранит в системное хранилище и зарегистрирует сервер `appgallery`. Если кабинет на другом сайте AppGallery Connect, добавьте регион: `--region ru` (Россия), `de` (Германия) или `sg` (Сингапур); по умолчанию — основной сайт `cn`. Перезапустите клиент.

3. Попросите агента, например: «Покажи мои приложения в AppGallery и что с последней версией на проверке».

Учётные данные можно сохранить и отдельно: `appgallery-connect-mcp auth login --client-id <ID> --secret-file ~/agc-secret.txt`. Для второго кабинета заведите отдельный профиль: `appgallery-connect-mcp connect codex --profile client` — сервер в клиенте будет называться `agc-client`. В команде, куда вас пригласили, можно работать и без API-клиента — от имени своего HUAWEI ID через OAuth-клиент: `appgallery-connect-mcp auth login --oauth --profile guest` ([вход пользователя](docs/api-client.md#вход-пользователя-oauth-клиент)). Другие клиенты и ручное подключение — [docs/clients.md](docs/clients.md).

Gradle-плагину публикации, fastlane и своим скриптам учётные данные профиля отдаёт `appgallery-connect-mcp key exec --profile <имя> -- <команда>`: временный файл с правами 0600, путь в `APPGALLERY_CONNECT_CREDENTIALS_FILE`, файл удаляется после завершения команды ([docs/cli.md](docs/cli.md#key-exec)).

## Уровни доступа

| Уровень | В профиле | Что доступно |
| --- | --- | --- |
| Чтение | по умолчанию | Приложения, сведения о версии и пакетах, отзывы и оценки, отчёты, товары, проекты, экспорт страницы приложения, сводки выпуска и отзывов, проверка покупок и подписок. |
| Запись | `allow_write = true` | Изменения до отправки на проверку: загрузка пакетов, сведения о приложении, локализации, изображения, группы подписок, сертификаты и профили HarmonyOS; ещё создание ссылок App Linking. Изменения приложения пользователи не видят, пока версия не одобрена. |
| Удаление | `allow_destructive = true` (включает запись) | Удаление и действия с последствиями для пользователей и денег: отправка на проверку, поэтапный выпуск и время выпуска, публичные ответы на отзывы, товары, цены и акции, запись и откат Remote Configuration, удаление локализаций, сертификатов, профилей и устройств, подтверждение покупок, отмена, отсрочка, возврат и отзыв подписок. |

Уровень доступа и дополнительные каталоги для файлов (`allow_path`) — настройки профиля. Их меняет `appgallery-connect-mcp ui` (клавиши `r`, `w`, `d`, затем `s`) или `connect` с флагом:

```bash
appgallery-connect-mcp connect claude-code --allow-write
```

В конфигурацию клиента флаги доступа не попадают: клиент запускает `serve --profile <имя>`, а `serve` берёт права из профиля. Флаги `serve` (`--allow-write`, `--allow-destructive`, `--allow-path`) могут только сузить права профиля. На macOS права сверх чтения нужно одобрить: при сохранении `ui` и `connect` спрашивают Touch ID или пароль пользователя macOS, а без одобрения сервер работает только на чтение и пишет предупреждение в stderr. В HTTP-режиме с секретом из файла (`--secret-file`) уровень по-прежнему задают флаги `serve` ([docs/http.md](docs/http.md)).

Инструменты выше разрешённого уровня не регистрируются: агент их не видит и не может вызвать. `--toolsets` выбирает разделы API: по умолчанию `apps`, `releases`, `reviews`; `--toolsets all` включает все. `connect` по умолчанию включает динамические наборы: агент сам подключает нужные разделы. Уровень доступа сервера не расширяет роли API-клиента в AppGallery Connect. Подробнее: [docs/tools.md](docs/tools.md).

## Консоль и TUI

```bash
appgallery-connect-mcp call                       # инструменты, доступные профилю
appgallery-connect-mcp call get_app_info appId=100000001 jq=.appInfo.releaseState
appgallery-connect-mcp call get_app_info --schema # схема аргументов
appgallery-connect-mcp ui                         # профили, права, регистрации, API-клиент
```

`call` собирает сервер с теми же правами, что и `serve`, и вызывает его в этом же процессе: проверка аргументов, уровни доступа и вырезание секретов те же, что у агента. Инструменты уровня удаления спрашивают подтверждение (`--yes` — без вопроса). Подробнее: [docs/cli.md](docs/cli.md#call).

## Документация

- [Установка и обновление](docs/install.md)
- [API-клиент и роли в AppGallery Connect](docs/api-client.md)
- [Подключение к MCP-клиентам](docs/clients.md)
- [Справочник CLI](docs/cli.md)
- [HTTP-режим и Docker](docs/http.md)
- [MCP-инструменты](docs/tools.md)
- [Безопасность](docs/security.md)
- [Решение проблем](docs/troubleshooting.md)
- [Разработка](docs/development.md)

## Лицензия

`MIT OR Apache-2.0` — на ваш выбор: [LICENSE-MIT](LICENSE-MIT) или [LICENSE-APACHE](LICENSE-APACHE).
