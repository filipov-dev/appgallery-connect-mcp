# API-клиент и роли в AppGallery Connect

Обычно сервер работает с AppGallery Connect API в режиме API-клиента: у него есть идентификатор (Client ID) и ключ — секрет клиента. По ним сервер получает access token (`POST /api/oauth2/v1/token`, живёт обычно 48 часов) и подписывает им запросы. Что может сервер в кабинете, определяют роли API-клиента; флаги доступа сервера (`--allow-write`, `--allow-destructive`) только сужают это.

Один профиль сервера — один API-клиент одного кабинета. Другой способ — [вход пользователя](#вход-пользователя-oauth-клиент) HUAWEI ID через OAuth-клиент в одной из его команд.

## Создание API-клиента

API-клиентами управляет только владелец аккаунта команды (Account holder).

1. Войдите в [AppGallery Connect](https://developer.huawei.com/consumer/en/service/josp/agc/index.html) и откройте **Users and permissions**.
2. В меню слева выберите **API key** → **Connect API**, откройте вкладку **API client** и нажмите **Create**.
3. Введите любое имя.
4. Поле **Project** оставьте **N/A**. Так создаётся API-клиент уровня команды, а он и нужен серверу. Клиент с выбранным проектом получает HTTP 403 на вызовы API.
5. Выберите роли (ниже).
6. После создания AppGallery Connect покажет Client ID и ключ. Сохраните ключ в файл или менеджер паролей. Не вставляйте его в чат с агентом.

Те же шаги печатает `connect` (и `auth login` без `--client-id` в терминале) перед вопросом об идентификаторе.

Затем сохраните учётные данные в профиль:

```bash
# Скрытый ввод секрета в терминале
appgallery-connect-mcp auth login --client-id <Client ID>

# Секрет из файла: в файле только ключ, путь не запоминается
appgallery-connect-mcp auth login --client-id <Client ID> --secret-file ~/agc-secret.txt

# Секрет из менеджера паролей
pass show agc/secret | appgallery-connect-mcp auth login --client-id <Client ID> --secret-stdin
```

Или сразу `appgallery-connect-mcp connect claude-code` (`codex`): он спросит то же самое и зарегистрирует сервер. Секрет не принимается аргументом командной строки и не читается из переменных окружения. Перед сохранением сервер проверяет учётные данные: получает access token в регионе профиля. Если проверка не прошла, ничего не сохраняется.

## Роли

Роли API-клиента задают права так же, как роли участников команды. Описание ролей — на странице Huawei [Roles and Permissions](https://developer.huawei.com/consumer/en/doc/app/agc-help-rolepermission-0000002271930352); точный набор прав показывает сама AppGallery Connect.

| Роль | Что о ней пишет Huawei |
| --- | --- |
| Administrator | Почти все права: приложения, отправка версий, товары, отчёты, сертификаты и профили. |
| App administrator | Управление приложениями: разработка, выпуск, цены, операции; отправка версий. |
| Operations | Работа с приложением после выпуска: сведения о приложении, товары и акции. Отправки версий (Submit version) в списке нет. |
| Development | Разработка и поставка приложений. |
| Customer service | Ответы на отзывы и вопросы пользователей. |
| Finance | Финансовые данные: встроенные покупки, платные приложения, финансовые отчёты. |

Проще всего дать **Administrator**: тогда роли не ограничивают ни один инструмент, а границу задают флаги доступа сервера. Если хотите уже, выберите роли под свои задачи. Нехватка роли проявляется как HTTP 403 или ошибка с кодом в теле ответа ([Нет прав](troubleshooting.md#нет-прав-http-403-и-коды-прав)): добавьте роль API-клиенту в AppGallery Connect.

## Регион и хост

У AppGallery Connect API несколько сайтов. Профиль работает с одним из них, и хост API выбирается только из этого списка:

| Регион | Сайт | Хост API |
| --- | --- | --- |
| `cn` (по умолчанию) | основной | `connect-api.cloud.huawei.com` |
| `de` | Германия | `connect-api-dre.cloud.huawei.com` |
| `sg` | Сингапур | `connect-api-dra.cloud.huawei.com` |
| `ru` | Россия | `connect-api-drru.cloud.huawei.com` |

Регион задаётся флагом `--region` у `auth login` и `connect` и хранится в профиле. Без флага берётся регион профиля, у нового профиля — `cn`.

```bash
appgallery-connect-mcp auth login --profile default --region ru
```

Приложения кабинета на другом сайте сервер не видит. Если приложение не находится, проверьте регион ([Регион не тот](troubleshooting.md#регион-не-тот)). Отзывы, оценки и часть отчётов тоже привязаны к сайту: страны в одном запросе должны обслуживаться одним сайтом.

## Приложения по умолчанию

Инструменты адресуют приложение числовым App ID (`appId`, в AppGallery Connect: My apps → приложение → App information), а не именем пакета. Приложения, с которыми вы работаете чаще всего, можно сохранить в профиле:

```bash
appgallery-connect-mcp auth login --default-app 100000001 --default-package com.example.app
```

Сервер называет их агенту в инструкциях. Если в профиле ровно одно приложение по умолчанию, составные инструменты берут его `appId`, когда агент его не передал. Флаги повторяются или принимают значения через запятую и заменяют то, что было в профиле.

Найти App ID по имени пакета агент может сам: `list_app_ids` (Publishing API) или `search_apps`.

## Project Management API

Справочник Huawei описывает операции Project Management API (набор `projects`: команды, проекты и их приложения, сервисы проекта, файл конфигурации `agconnect-services.json`, отпечатки сертификатов) только для OAuth-клиента, а часть из них ещё и для Service Account. Для API-клиента они в справочнике не описаны, поэтому с учётными данными этого сервера могут не работать и отвечать ошибкой прав ([403 у Project API](troubleshooting.md#403-у-project-management-api)).

Что это значит на практике:

- `search_apps` берёт список приложений из проектов, а если Project Management API отказывает, ищет App ID по именам пакетов через Publishing API (`list_app_ids`): тогда приложения находятся только по известному имени пакета;
- `config_download` при отказе советует скачать `agconnect-services.json` в AppGallery Connect вручную (Project settings → General information);
- остальные наборы (приложения, выпуск, страница приложения, отзывы, отчёты, товары) этим не затронуты.

С [входом пользователя](#вход-пользователя-oauth-клиент) эти операции описаны в справочнике, но OAuth-клиенту нужна область `project` (по умолчанию она запрашивается).

## API-клиент проекта

API-клиент из раздела Users and permissions → API key → Connect API — клиент уровня команды (в столбце Project у него `N/A`). Его хватает для Publishing API, отчётов, отзывов, товаров и остальных наборов сервера. Серверные API сервисов проекта (например, Remote Configuration и App Linking) принимают только access token API-клиента уровня проекта: он создаётся в самом проекте и действует только в нём. Среди операций сервера это наборы `remote-config` ([Remote Configuration](tools.md#remote-configuration-remote-config)) и `app-linking` ([Ссылки App Linking](tools.md#ссылки-app-linking-app-linking): короткие и единые ссылки); описание таких инструментов говорит, что нужен API-клиент проекта.

Поэтому в профиле может быть второй вид учётных данных — API-клиенты проектов, по одному на проект. Сервер выбирает клиент для каждого запроса сам: операции, которым нужен API-клиент проекта, подписываются клиентом проекта из их аргумента `productId` или `projectId`, остальные — API-клиентом команды. Регион у них общий: хост API — по региону профиля, и место обработки данных проекта должно с ним совпадать.

Как создать API-клиент проекта:

1. AppGallery Connect → My projects → проект → Project settings → Server SDK.
2. В разделе API client создайте клиент (если его нет) и нажмите Download credential. Скачается файл `agc-apiclient-<…>.json`: в нём `type` (`project_client_id`), `client_id`, `client_secret`, `project_id`, `region` и служебные поля.
3. Сохраните клиент в профиль:

```bash
appgallery-connect-mcp auth project add --profile <профиль> --credential-file ~/Downloads/agc-apiclient-<…>.json
```

Команда берёт из файла Project ID, Client ID и секрет, проверяет, что это клиент проекта, а не команды, и что его регион совпадает с регионом профиля, получает access token и только после этого сохраняет клиент в системное хранилище. Путь к файлу не запоминается: файл можно удалить. Вместо файла можно передать `--project-id` и `--client-id`, а секрет — через `--secret-file`, `--secret-stdin` или скрытым вводом ([auth project add](cli.md#auth-project-add)).

Профиль с этим именем уже должен быть создан (`auth login`). Запущенный сервер читает клиенты проектов при старте: после `auth project add` перезапустите MCP-клиент. Если агент вызывает операцию проекта, для которого клиента нет, сервер не отправляет запрос и отвечает ошибкой с командой `auth project add` для этого проекта.

Убрать клиент проекта: `appgallery-connect-mcp auth project remove --profile <профиль> --project-id <ID>`. В AppGallery Connect клиент остаётся, пока вы его не удалите в Server SDK настроек проекта.

### Для Remote Configuration

Сервер работает с двумя сервисами проекта: Remote Configuration и App Linking. Для App Linking достаточно сохранить API-клиент проекта и включить набор `app-linking` с `--allow-write` ([Ссылки App Linking](tools.md#ссылки-app-linking-app-linking)). Чтобы агент мог читать и менять параметры Remote Configuration:

1. Сохраните API-клиент проекта, в котором лежит приложение, как описано выше. Project ID — в Project settings → General information проекта; его же агент передаёт в аргументе `productId`. Если в профиле один проект с клиентом, `remote_config_set` берёт его сам.
2. По справочнику Remote Configuration вызывают роли Administrator, App administrator, Development и Operations: у клиента проекта должна быть одна из них, если при создании клиента выбирается роль.
3. Регион профиля должен совпадать с местом обработки данных проекта: токен и запросы Remote Configuration идут на один хост региона (`connect-api*.cloud.huawei.com`).
4. Запустите сервер с набором `remote-config` (`--toolsets remote-config` или `enable_toolset`). Чтение — без флагов; запись и откат — только с `--allow-destructive`: новая версия конфигурации сразу действует для пользователей приложения.

Лимит Remote Configuration — 5 запросов в секунду и 1000 в час с одного IP-адреса.

## Вход пользователя (OAuth-клиент)

Вместо API-клиента профиль может работать от имени пользователя HUAWEI ID: справочник называет это режимом OAuth-клиента (Connect API → Getting Started → OAuth Client Mode). Так можно работать в команде, куда вас пригласили, с правами вашей роли, не создавая API-клиент в чужом кабинете. Режим рассчитан на платформенных разработчиков, и Huawei одобряет его вручную.

Что понадобится:

1. OAuth-клиент — приложение с включённым HUAWEI ID (Account Kit): в AppGallery Connect (My projects → проект → приложение) или веб-приложение в HUAWEI Developers Console (App services → HUAWEI ID).
2. Адрес возврата (redirect URL, callback URL) этого приложения — loopback-адрес с постоянным портом, например `http://127.0.0.1:8765/`. Тот же адрес передайте в `--redirect-uri`. Без флага команда слушает случайный порт на 127.0.0.1; это работает, только если HUAWEI ID принимает любой порт loopback-адреса.
3. Области Connect API для этого клиента. Их запрашивают письмом на agconnect@huawei.com, как описано на странице Getting Started; Huawei рассматривает заявку. Области: `publish` (Publishing API), `develop`, `report/read` (отчёты), `project` (Project Management API), `product` и `product/read` (товары). По умолчанию команда запрашивает `publish` и `project`, остальные добавляет `--oauth-scope`. Область `openid` запрашивается всегда.
4. Client ID (OAuth 2.0 client ID, он же App ID) и client secret (App secret) из App information. Секрет храните в файле или менеджере паролей.

Вход:

```bash
appgallery-connect-mcp auth login --oauth --profile guest \
  --oauth-client-id <client ID> --secret-file ~/secrets/oauth-secret.txt \
  --redirect-uri http://127.0.0.1:8765/
```

Секрет читается из `--secret-file`, из stdin (`--secret-stdin`) или скрытым вводом в терминале; клиент без секрета — пустой ввод (Enter). Команда открывает в браузере страницу входа HUAWEI ID (PKCE S256, `access_type=offline`, случайный `state`). Браузер возвращается на loopback-адрес, команда обменивает код на refresh token и сразу проверяет, что по нему выдаётся access token. Затем она получает список команд пользователя (Project Management API, `user-team-list`) и предлагает выбрать команду. Если команда одна, она выбирается сама, а `--team-id` задаёт её заранее. Регион профиля берётся по сайту команды: cn, de, sg или ru. Если `--region` с ним не совпадает, это ошибка. Вход ждёт 5 минут.

Если браузер открыть нельзя (сервер без графики, SSH), или с `--no-browser`, команда печатает ссылку. Откройте её на любой машине, войдите и вставьте в терминал адрес, на который вернулся браузер: страница не откроется, но адрес в строке браузера содержит код.

Если список команд не получен (например, Huawei ещё не одобрил область `project`), передайте `--team-id`: команда сохранит вход без проверки команды и предупредит об этом. ID команды есть в AppGallery Connect → Users and permissions.

Что сохраняется:

- в системном хранилище (запись `<профиль>/oauth`, служба `dev.filipov.appgallery-connect-mcp`) — client ID и секрет OAuth-клиента, refresh token и выданные области;
- в `profiles.toml` — `type = "oauth"`, `team_id` и регион, без секретов.

API-клиент команды этого профиля после входа удаляется из хранилища: у профиля один способ входа. Обычный `auth login` без `--oauth` переключает профиль обратно на API-клиент и удаляет запись входа. API-клиенты проектов (`auth project add`) работают с обоими способами.

Как работает сервер с таким профилем:

- access token живёт час (`expires_in` 3600). Сервер получает его по refresh token при первом запросе, держит в памяти и обновляет заранее. Параллельные запросы ждут одного обновления, а на ответ 401 сервер один раз обновляет токен и повторяет запрос;
- запросы идут с заголовками `oauth2Token` (access token без `Bearer`) и `teamId`. Агент не может их задать или подменить;
- если HUAWEI ID выдаст новый refresh token, сервер, `auth status --check` и `connect` сохранят его в хранилище;
- операции, которые справочник описывает только для API-клиента (Comments API: `list_reviews`, `reply_review`), сервер не отправляет и отвечает ошибкой с подсказкой. Для них нужен профиль с API-клиентом.

Refresh token действует 180 дней. Он перестаёт работать раньше, если пользователь сменил пароль HUAWEI ID или снял доступ в Privacy center → Control account access. Тогда войдите снова: `auth login --oauth --profile <профиль>`. Выйти и отозвать вход: `auth logout --profile <профиль> --forget` ([Отзыв доступа](#отзыв-доступа)).

## Секрет приложения

Серверные API покупок в приложении (HMS In-App Purchases: Order service и Subscription service, набор `purchases`) — не часть AppGallery Connect API. Они не принимают ни API-клиент команды, ни API-клиент проекта: запрос подписывается токеном уровня приложения (app-level access token), который Huawei OAuth выдаёт по client ID и client secret самого приложения. Поэтому в профиле может быть третий вид учётных данных — секреты приложений, по одному на приложение.

Где взять:

1. AppGallery Connect → My apps → приложение. App ID — в App information. Client ID и client secret по справочнику IAP — в All services → Earn → In-App Purchases (In-App Purchases должен быть включён у приложения); они же показаны в App information.
2. По справочнику IAP OAuth client ID приложения — это его App ID. Если в консоли client ID другой, передайте его флагом `--client-id`.
3. Сохраните секрет в профиль:

```bash
appgallery-connect-mcp auth app-secret set --profile <профиль> --app <App ID> --secret-file ~/app-secret.txt
```

Команда получает токен уровня приложения у Huawei OAuth (`POST https://oauth-login.cloud.huawei.com/oauth2/v3/token`) и только после этого сохраняет секрет в системное хранилище ([auth app-secret set](cli.md#auth-app-secret-set)). Профиль уже должен быть создан (`auth login`): сервер профиля по-прежнему работает от API-клиента команды или входа пользователя, а секреты приложений нужны только операциям IAP и подписывают их при любом способе входа.

Как сервер подписывает запросы IAP:

- приложение выбирается по аргументу `appId` операции; в Huawei `appId` не отправляется;
- токен — свой у каждого приложения, в памяти процесса; он заменяется заранее и после ответа 401;
- заголовок — `Authorization: Basic base64("APPAT:" + token)`, без `client_id`;
- хост — Order service или Subscription service в регионе профиля: `orders-drcn.iap.cloud.huawei.com.cn` и `subscr-drcn.iap.cloud.huawei.com.cn` (Китай), `orders-dre.iap.cloud.huawei.eu` и `subscr-dre…` (Германия), `orders-dra.iap.cloud.huawei.asia` и `subscr-dra…` (Сингапур), `orders-drru.iap.cloud.huawei.ru` и `subscr-drru…` (Россия);
- результат — в корне тела ответа: `responseCode` (`"0"` — успех) и `responseMessage`. Ненулевой код при HTTP 200 сервер возвращает агенту как ошибку.

Если секрета приложения нет, сервер не отправляет запрос и отвечает ошибкой с командой `auth app-secret set` для этого приложения. Запущенный сервер читает секреты при старте: после команды перезапустите MCP-клиент. Убрать секрет: `appgallery-connect-mcp auth app-secret remove --profile <профиль> --app <App ID>`; если секрет мог утечь, сбросьте его в AppGallery Connect.

## Что не поддерживается

- **Service Account.** Huawei рекомендует для новых интеграций режим Service Account и пишет, что он заменит API-клиенты. Этот сервер работает только с API-клиентом (Client ID и ключ). Если у вас уже есть Service Account, для сервера всё равно нужен API-клиент.
- **API-клиент проекта вместо API-клиента команды.** Профиль всегда начинается с API-клиента команды; API-клиент проекта добавляется к нему ([API-клиент проекта](#api-клиент-проекта)).
- **API-клиенты проектов в HTTP-режиме с `--secret-file`.** Сервер с файлом секрета работает только с API-клиентом команды; клиенты проектов он берёт только из профиля в системном хранилище хоста.

## Несколько кабинетов

Один профиль — один API-клиент одного кабинета. Для другого кабинета, другого региона или другого набора ролей создайте отдельный API-клиент и отдельный профиль:

```bash
appgallery-connect-mcp connect claude-code --profile client --region de
```

Сервер этого профиля в клиенте называется `agc-client` ([Имя сервера](clients.md#имя-сервера)).

## Отзыв доступа

- Удалите учётные данные из системного хранилища и профиль: `appgallery-connect-mcp auth logout --profile <профиль>`. Команда удаляет и API-клиенты проектов и секреты приложений профиля.
- Вход пользователя `auth logout --forget` сначала отзывает в HUAWEI ID (`oauth2/v3/revoke`), потом удаляет. Если отозвать не удалось, ничего не удаляется. Без `--forget` refresh token удаляется только с этой машины и остаётся действительным до истечения срока; снять доступ сразу можно и в HUAWEI ID (Privacy center → Control account access).
- Удалите API-клиент в AppGallery Connect (Users and permissions → API key → Connect API) или смените ему роли: так доступ снимается и на других машинах, где сохранён этот клиент.
- API-клиенты проектов удаляются в самом проекте (Project settings → Server SDK).
- Секреты приложений `auth logout` тоже удаляет из хранилища; сам секрет приложения действует, пока его не сбросят в AppGallery Connect.
- Запущенный сервер держит выданный access token в памяти, пока работает. Перезапустите MCP-клиент, чтобы процесс сервера завершился.

Если ключ попал в чат, лог или репозиторий, удалите этот API-клиент в AppGallery Connect и создайте новый.
