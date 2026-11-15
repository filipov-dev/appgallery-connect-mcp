# appgallery-connect-mcp

MCP-сервер и CLI для [AppGallery Connect API](https://developer.huawei.com/consumer/en/agconnect/) (Huawei). Даёт Claude Code, Codex и другим MCP-клиентам работать с кабинетом разработчика AppGallery: приложения, загрузка сборок, отправка на проверку и поэтапный выпуск, страница приложения и изображения, отзывы, отчёты, товары и подписки, проекты, подпись HarmonyOS.

- **Операции API — MCP-инструменты.** У Huawei нет машинной спецификации Connect API, поэтому в проекте лежит свой снимок в формате OpenAPI, собранный по страницам справочника: Publishing API, Upload Management API, Reports API, Comments API, PMS API, Project Management API и Provisioning API. Инструменты генерируются из него при сборке. `api_search` находит нужную операцию по-английски или по-русски, `api_request` вызывает любую в пределах уровня доступа.
- **Составные инструменты** делают за один вызов то, что в API требует нескольких шагов: загрузка APK и AAB (крупных — по частям), отправка на проверку, управление поэтапным выпуском, сводка выпуска и приложения, экспорт и применение страницы приложения, синхронизация изображений, отчёты, сводка отзывов, поиск приложений, файл конфигурации.
- **Учётные данные — только в системном хранилище.** Идентификатор и секрет API-клиента лежат в Keychain (macOS), Secret Service (Linux) или Credential Manager (Windows) и не читаются из переменных окружения. Access token живёт только в памяти процесса. Один профиль — один API-клиент одного кабинета.
- **По умолчанию только чтение.** Запись и действия, которые видят пользователи AppGallery или которые затрагивают деньги, включаются явно; инструменты выше разрешённого уровня агенту не видны.
- **Секреты не попадают к агенту.** Секрет клиента, access token, секрет приложения, адреса и коды загрузки и временные ссылки на отчёты вырезаются из ответов и ошибок.

Сервер работает через stdio: MCP-клиент сам запускает `appgallery-connect-mcp serve`.

## Установка

### Установочный скрипт (Linux и macOS)

```bash
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/appgallery-connect-mcp/tarball/refs/heads/install | tar -xzO | sh
```

Скрипт ставит последнюю версию в `~/.local/bin` без `sudo` и сверяет архив с `SHA256SUMS` релиза; повторный запуск обновляет бинарник.

### Homebrew (macOS и Linux)

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/appgallery-connect-mcp
```

### Архивы и Windows

Готовые архивы — на [странице релизов](https://sourcecraft.dev/filipov-dev/appgallery-connect-mcp/releases): macOS (универсальный, подписан и нотаризован), Linux x86_64 и aarch64 (статические, без зависимостей), Windows x86_64 (архив `.zip`, без подписи кода). Проверка контрольных сумм, установка в Windows, сборка из исходников и удаление — в [docs/install.md](docs/install.md).

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

Учётные данные можно сохранить и отдельно: `appgallery-connect-mcp auth login --client-id <ID> --secret-file ~/agc-secret.txt`. Для второго кабинета заведите отдельный профиль: `appgallery-connect-mcp connect codex --profile client` — сервер в клиенте будет называться `agc-client`. Другие клиенты и ручное подключение — [docs/clients.md](docs/clients.md).

## Уровни доступа

| Уровень | Как включить | Что доступно |
| --- | --- | --- |
| Чтение | по умолчанию | Приложения, сведения о версии и пакетах, отзывы и оценки, отчёты, товары, проекты, экспорт страницы приложения, сводки выпуска и отзывов. |
| Запись | `--allow-write` | Изменения до отправки на проверку: загрузка пакетов, сведения о приложении, локализации, изображения, группы подписок, сертификаты и профили HarmonyOS. Пользователи их не видят, пока версия не одобрена. |
| Удаление | `--allow-destructive` (включает запись) | Удаление и действия с последствиями для пользователей и денег: отправка на проверку, поэтапный выпуск и время выпуска, публичные ответы на отзывы, товары, цены и акции, удаление локализаций, сертификатов, профилей и устройств. |

```bash
appgallery-connect-mcp connect claude-code --allow-write
```

Инструменты выше разрешённого уровня не регистрируются: агент их не видит и не может вызвать. `--toolsets` выбирает разделы API: по умолчанию `apps`, `releases`, `reviews`; `--toolsets all` включает все. `connect` по умолчанию включает динамические наборы: агент сам подключает нужные разделы. Уровень доступа сервера не расширяет роли API-клиента в AppGallery Connect. Подробнее: [docs/tools.md](docs/tools.md).

## Документация

- [Установка и обновление](docs/install.md)
- [API-клиент и роли в AppGallery Connect](docs/api-client.md)
- [Подключение к MCP-клиентам](docs/clients.md)
- [Справочник CLI](docs/cli.md)
- [MCP-инструменты](docs/tools.md)
- [Безопасность](docs/security.md)
- [Решение проблем](docs/troubleshooting.md)
- [Разработка](docs/development.md)

## Лицензия

`MIT OR Apache-2.0` — на ваш выбор: [LICENSE-MIT](LICENSE-MIT) или [LICENSE-APACHE](LICENSE-APACHE).
