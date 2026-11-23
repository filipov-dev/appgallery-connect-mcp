# Установка и обновление

`appgallery-connect-mcp` — один бинарник: и MCP-сервер (`serve`), и CLI для настройки (`connect`, `auth`, `tools`, `toolsets`). Готовые сборки есть для macOS, Linux и Windows.

## Платформы

| Платформа | Сборка | Хранилище учётных данных |
| --- | --- | --- |
| macOS, Apple silicon и Intel | универсальный бинарник, подписан Developer ID и нотаризован | Keychain |
| Linux x86_64 | статический бинарник (musl), без зависимостей | Secret Service |
| Linux aarch64 | статический бинарник (musl), без зависимостей | Secret Service |
| Windows x86_64 | архив `.zip`, статический CRT (Visual C++ Redistributable не нужен), без подписи кода | Credential Manager |

Linux-бинарнику не нужны glibc определённой версии, `libdbus-1`, OpenSSL и другие системные библиотеки: он запускается в любом дистрибутиве, включая Alpine и slim-образы. Для HTTPS-запросов к API Huawei нужны корневые сертификаты системы (пакет `ca-certificates`).

В Linux учётные данные хранятся через Secret Service (GNOME Keyring, KDE Wallet, KeePassXC и т. п.): провайдер должен быть запущен в сессии D-Bus пользователя. Запасного файлового хранилища нет — см. [Linux: Secret Service недоступен](troubleshooting.md#linux-secret-service-недоступен).

## Установочный скрипт

Одна команда для Linux (x86_64, aarch64) и macOS, Homebrew не нужен:

```bash
curl -fsSL https://codeload.github.com/filipov-dev/appgallery-connect-mcp/tar.gz/refs/heads/install | tar -xzO | sh
```

Адрес — архив ветки `install` копии репозитория на GitHub: в ней лежит только `install.sh` со списком релизов (адреса архивов и их SHA-256). `tar -xzO` печатает скрипт, `sh` выполняет. Архивы релизов тоже лежат на GitHub. Прежний адрес `https://codeload.sourcecraft.tech/filipov-dev/appgallery-connect-mcp/tarball/refs/heads/install` работает и ставит те же архивы.

Что делает скрипт:

- определяет ОС и архитектуру и выбирает архив релиза (`linux-x86_64`, `linux-aarch64` или `macos-universal`);
- скачивает архив и `SHA256SUMS` релиза и сверяет сумму архива; при расхождении ничего не устанавливает;
- кладёт бинарник в `~/.local/bin/appgallery-connect-mcp` без `sudo`; уже запущенный сервер продолжает работать со старой копией;
- если каталога нет в `PATH`, подсказывает, как его добавить.

Повторный запуск обновляет бинарник до последней версии (если она уже стоит — ничего не скачивает). Путь не меняется, поэтому `connect` повторять не нужно.

Параметры передаются через `sh -s --`:

```bash
# Конкретная версия
curl -fsSL https://codeload.github.com/filipov-dev/appgallery-connect-mcp/tar.gz/refs/heads/install | tar -xzO | sh -s -- --version v0.2.0

# Другой каталог: бинарник попадёт в /opt/agc/bin
curl -fsSL https://codeload.github.com/filipov-dev/appgallery-connect-mcp/tar.gz/refs/heads/install | tar -xzO | sh -s -- --prefix /opt/agc
```

| Параметр | Что делает |
| --- | --- |
| `--version vX.Y.Z` | Версия вместо последней. Список — `--list`. |
| `--prefix DIR` | Установить в `DIR/bin` (по умолчанию `~/.local`). |
| `--bin-dir DIR` | Установить ровно в `DIR` (важнее `--prefix`). |
| `--force` | Переустановить, даже если эта версия уже стоит. |
| `--list` | Показать доступные версии и выйти. |

Нужны `sh`, `tar`, `curl` или `wget`, `sha256sum` или `shasum`. Чтобы прочитать скрипт перед запуском, сохраните его в файл: `curl -fsSL https://codeload.github.com/filipov-dev/appgallery-connect-mcp/tar.gz/refs/heads/install | tar -xzO > install.sh`.

## Homebrew

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/appgallery-connect-mcp
appgallery-connect-mcp --version
```

- Tap размещён на SourceCraft, а не на GitHub, поэтому `brew tap` нужен полный адрес репозитория.
- Формулу устанавливайте по полному имени `filipov-dev/tap/appgallery-connect-mcp`: так Homebrew фиксирует, что вы доверяете формуле из стороннего tap.
- В Linux формула ставит статический бинарник, зависимостей у неё нет.

Обновление и удаление:

```bash
brew update && brew upgrade filipov-dev/tap/appgallery-connect-mcp
brew uninstall filipov-dev/tap/appgallery-connect-mcp
```

`connect` записывает в конфигурацию клиента путь, по которому был запущен бинарник. Если запускать его как `appgallery-connect-mcp` из `PATH` Homebrew, в записи окажется ссылка `…/bin/appgallery-connect-mcp`, которая переживает `brew upgrade`. Если в записи клиента путь внутри `Cellar` (с номером версии), после обновления повторите `connect`.

## Готовый архив

Архивы и файл `SHA256SUMS` публикуются на [странице релизов](https://github.com/filipov-dev/appgallery-connect-mcp/releases). Имя архива: `appgallery-connect-mcp-<тег>-<платформа>.tar.gz`, например `appgallery-connect-mcp-v0.2.0-linux-x86_64.tar.gz` (для Windows — `.zip`, см. [Windows](#windows)). Внутри — каталог с бинарником `appgallery-connect-mcp`, `README.md` и лицензиями.

Проверьте контрольную сумму (из каталога, куда скачаны архив и `SHA256SUMS`):

```bash
# Linux
grep linux-x86_64 SHA256SUMS | sha256sum -c -

# macOS
grep macos-universal SHA256SUMS | shasum -a 256 -c -
```

Распакуйте и установите бинарник в каталог из `PATH`:

```bash
tar -xzf appgallery-connect-mcp-v0.2.0-linux-x86_64.tar.gz
sudo install -m 0755 appgallery-connect-mcp-v0.2.0-linux-x86_64/appgallery-connect-mcp /usr/local/bin/
appgallery-connect-mcp --version
```

Чтобы обновиться, замените бинарник новым по тому же пути: конфигурацию клиентов трогать не нужно.

На macOS проверить подпись можно так (в выводе второй команды — строка `Authority=Developer ID Application: …`):

```bash
codesign --verify --strict --verbose=2 /usr/local/bin/appgallery-connect-mcp
codesign -dv --verbose=2 /usr/local/bin/appgallery-connect-mcp
```

## Windows

Для Windows есть только архив `appgallery-connect-mcp-<тег>-windows-x86_64.zip` на [странице релизов](https://github.com/filipov-dev/appgallery-connect-mcp/releases): установочный скрипт и Homebrew Windows не поддерживают. Внутри архива — каталог с `appgallery-connect-mcp.exe`, `README.md` и лицензиями.

Скачайте архив и `SHA256SUMS` в один каталог и выполните в PowerShell из него (тег замените на нужный):

```powershell
$tag = 'v0.3.1'
$zip = "appgallery-connect-mcp-$tag-windows-x86_64.zip"

# Контрольная сумма: значение должно совпасть со строкой архива в SHA256SUMS
(Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
Select-String -SimpleMatch $zip SHA256SUMS

# Распаковка и установка в профиль пользователя (права администратора не нужны)
Unblock-File $zip
Expand-Archive $zip -DestinationPath "$env:TEMP\appgallery-connect-mcp" -Force
$dir = "$env:LOCALAPPDATA\Programs\appgallery-connect-mcp"
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item "$env:TEMP\appgallery-connect-mcp\appgallery-connect-mcp-$tag-windows-x86_64\appgallery-connect-mcp.exe" $dir -Force
```

Добавьте каталог в `PATH` пользователя (один раз) и откройте новое окно терминала:

```powershell
$dir = "$env:LOCALAPPDATA\Programs\appgallery-connect-mcp"
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($path -split ';') -notcontains $dir) {
  [Environment]::SetEnvironmentVariable('Path', "$path;$dir", 'User')
}
```

```powershell
appgallery-connect-mcp --version
```

Бинарник не подписан сертификатом подписи кода. Если открыть его из Проводника или из браузера, SmartScreen может показать «Система Windows защитила ваш компьютер»: проверьте контрольную сумму и нажмите «Подробнее» → «Выполнить в любом случае». `Unblock-File` снимает с архива отметку «загружено из интернета», и распакованный бинарник её не получает. В Windows 11 со включённым Smart App Control неподписанные программы блокируются без возможности запуска — используйте другую машину или соберите бинарник из исходников.

Учётные данные хранятся в Windows Credential Manager (общие учётные данные `<профиль>.dev.filipov.appgallery-connect-mcp`), настройки профилей — в `%APPDATA%\appgallery-connect-mcp\profiles.toml`.

Чтобы обновиться, замените `appgallery-connect-mcp.exe` новым по тому же пути. Пока MCP-клиент держит сервер запущенным, файл занят: закройте клиент или сначала переименуйте старый файл (`Rename-Item appgallery-connect-mcp.exe appgallery-connect-mcp.old.exe`; переименовать запущенный файл Windows разрешает) и удалите его после перезапуска клиента.

Особенности `connect` и Credential Manager в Windows — в разделе [Подключение к MCP-клиентам](clients.md#windows).

## Из исходников

Нужен Rust 1.88 или новее (`rustup` подхватит канал из `rust-toolchain.toml`) и C-компилятор для криптографии TLS. Системные библиотеки D-Bus и OpenSSL не нужны.

```bash
cargo build --release
```

Бинарник: `target/release/appgallery-connect-mcp`. Скопируйте его в каталог из `PATH` или установите сразу:

```bash
cargo install --path crates/appgallery-connect-mcp
```

На macOS собранный локально бинарник не подписан Developer ID: после пересборки Keychain может снова спросить разрешение на доступ к записи ([Решение проблем](troubleshooting.md#macos-keychain-спрашивает-разрешение)).

## Проверка

```bash
appgallery-connect-mcp --version
appgallery-connect-mcp toolsets          # наборы инструментов; учётные данные и сеть не нужны
appgallery-connect-mcp tools list        # инструменты, которые зарегистрирует serve с настройками по умолчанию
```

После сохранения учётных данных:

```bash
appgallery-connect-mcp auth status --check   # учётные данные в хранилище и получение access token
appgallery-connect-mcp serve --status        # с какими настройками запустится сервер (без сети)
```

## Первое подключение

После установки создайте API-клиент в AppGallery Connect ([API-клиент](api-client.md)), затем подключите сервер к клиенту: `appgallery-connect-mcp connect claude-code` или `appgallery-connect-mcp connect codex` ([Подключение к MCP-клиентам](clients.md)).

## Удаление

1. Отключите сервер в клиентах: `claude mcp remove --scope user appgallery` (или `agc-<профиль>`), таблица `[mcp_servers.appgallery]` в `~/.codex/config.toml` или запись в конфигурации другого клиента.
2. Удалите учётные данные и настройки профилей: `appgallery-connect-mcp auth logout --profile <профиль>` для каждого профиля.
3. Удалите API-клиент в AppGallery Connect, если он больше не нужен ([Отзыв доступа](api-client.md#отзыв-доступа)).
4. Удалите бинарник (`brew uninstall filipov-dev/tap/appgallery-connect-mcp` или файл из `PATH`) и, если осталась, папку `~/.config/appgallery-connect-mcp`.
