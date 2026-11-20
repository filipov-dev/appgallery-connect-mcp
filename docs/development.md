# Разработка

Страница описывает устройство кода, сборку, тесты, спецификацию API и релиз.

## Структура

| Крейт | Назначение |
| --- | --- |
| `crates/appgallery-connect-api` | Клиент AppGallery Connect API: снимок спецификации и генератор таблицы операций (`build.rs`), регионы и хосты, исполнитель запросов (повторы, конверт результата, загрузка и скачивание файлов), access token, хранилище учётных данных. |
| `crates/appgallery-connect-mcp-core` | MCP-инструменты поверх клиента API: реестр и уровни доступа, `api_search`, `api_request`, фильтр `jq`, пользовательские инструменты, динамические наборы, вырезание секретов, `FilePolicy`, составные инструменты (`src/composite/`). Не зависит от транспорта. |
| `crates/appgallery-connect-mcp` | Бинарник `appgallery-connect-mcp`: CLI (`auth`, `connect`, `serve`, `tools`, `toolsets`, скрытый `jq-worker`), профили, регистрация в клиентах, транспорты stdio и HTTP (`serve --http`: токены доступа, файл секрета). |

## Сборка и проверки

Нужен Rust 1.88 или новее; `rust-toolchain.toml` выбирает стабильный канал с `rustfmt` и `clippy`.

```bash
cargo build --release
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Запустить CLI из исходников:

```bash
cargo run -p appgallery-connect-mcp -- --version
cargo run -p appgallery-connect-mcp -- --help
cargo run -p appgallery-connect-mcp -- toolsets
cargo run -p appgallery-connect-mcp -- tools list --toolsets all --allow-destructive
```

`toolsets` и `tools list` не нужны ни учётные данные, ни сеть: так удобно проверить, какие инструменты получились после правки спецификации.

## Тесты

Тесты не обращаются к настоящему AppGallery Connect API и не трогают системное хранилище:

- HTTP подменяется `wiremock`: базовый адрес клиента API указывает на локальный сервер, загрузка и скачивание файлов в тестах разрешены и на loopback;
- учётные данные выдуманы в тесте, хранилище — реализация в памяти;
- сценарии MCP идут через сессию `rmcp` поверх `tokio::io::duplex`;
- подпроцесс `jq-worker` проверяется настоящим бинарником (`crates/appgallery-connect-mcp/tests/jq_worker.rs`).

Не используйте в тестах и fixtures настоящие идентификаторы клиентов, приложений и проектов, имена пакетов и ответы AppGallery Connect с данными кабинета: только выдуманные значения вроде `100000001` и `com.example.app`.

### Снапшоты

Описания и схемы инструментов операций проверяются снапшотами [insta](https://insta.rs) в `crates/appgallery-connect-api/tests/snapshots/`. Если после правки спецификации снапшот изменился, тест падает и показывает разницу. Обновляйте снапшоты только после просмотра изменений:

```bash
cargo insta review                                            # нужен cargo-insta
INSTA_UPDATE=always cargo test -p appgallery-connect-api       # без cargo-insta
git diff crates/appgallery-connect-api/tests/snapshots/
```

## Спецификация API

У Huawei нет машинной спецификации Connect API, поэтому снимок ведётся вручную по страницам справочника. Файлы — в `crates/appgallery-connect-api/spec/`, подробно — в его `README.md`:

| Файл | Что это |
| --- | --- |
| `openapi.json` | Снимок в формате OpenAPI 3: операции, параметры, тела, ответы, конверт результата (`x-agc-result`), ссылка на страницу справочника (`x-agc-reference`). |
| `overlay.toml` | Файл-дополнение: группы, наборы, имена инструментов, уровни доступа, исключённые операции, поля с секретами. |
| `pages.lock.json` | Страницы справочника, по которым собран снимок, и SHA-256 их текста. |
| `update_spec.py` | Сверка справочника со списком страниц и черновик операций. |

Инструменты генерируются при сборке (`build.rs`) без сети: тот же снимок и дополнение дают тот же результат. Операция без набора, повтор имени, имя длиннее 40 символов, неизвестное поле или ссылка на несуществующую операцию — ошибка сборки.

Скрипт обновления — Python 3.10+ без сторонних пакетов; к самому AppGallery Connect API он не ходит, только к порталу документации Huawei:

```bash
python3 -I crates/appgallery-connect-api/spec/update_spec.py --check        # что изменилось в справочнике
python3 -I crates/appgallery-connect-api/spec/update_spec.py --draft DIR    # страницы и черновик операций
python3 -I crates/appgallery-connect-api/spec/update_spec.py --lock --from-dir DIR
python3 -I -B -m unittest discover -s crates/appgallery-connect-api/spec -p 'test_*.py'   # офлайн-тесты
```

Коды `--check`: `0` — справочник не изменился, `3` — изменился (в отчёте изменённые, новые и пропавшие страницы с названием и адресом), `4` — портал документации недоступен после повторов или вышел лимит времени (15 минут, `--time-limit`), справочник не сверен; `1` — другая ошибка, `2` — неверные аргументы.

Порядок, когда `--check` нашёл изменения: `--draft DIR`, ручная правка `openapi.json` (черновик — подспорье: справочник местами неточен), новые операции — в `overlay.toml` (набор, имя, уровень доступа), `--lock --from-dir DIR`, затем `cargo test` и просмотр снапшотов. Каталог черновика в репозиторий не добавляется.

Скрипты оболочки проверяются `bash -n` (или `sh -n` для POSIX) и `shellcheck`:

```bash
shellcheck -S warning -s sh install.sh scripts/ci/musl-smoke.sh scripts/ci/install-test.sh
shellcheck -S warning scripts/ci/linux-verify.sh scripts/ci/macos-build.sh scripts/ci/macos-smoke.sh scripts/ci/install-fixture.sh scripts/release/*.sh
```

## Проверки в CI

Те же проверки идут в CI на каждый pull request (после слияния в `main` повторного прогона нет):

| Задача | Где | Что делает |
| --- | --- | --- |
| `verify-linux` | Linux, образ `rust:<версия>-bookworm` | `scripts/ci/linux-verify.sh`: `shellcheck`, `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`. |
| `verify-linux-musl-x86_64` | Linux | Статическая сборка `scripts/release/musl-build.sh x86_64`, запуск бинарника в Alpine и Debian slim (`scripts/ci/musl-smoke.sh`) и проверка `install.sh` там же (`scripts/ci/install-test.sh`). |
| `build-macos` | macOS | `scripts/ci/macos-build.sh`: universal-бинарник (arm64 и x86_64) без подписи и его проверка (`scripts/ci/macos-smoke.sh`); проверка `install.sh`. |
| `windows-check` | Windows | `scripts/ci/windows-build.ps1 -Check`: `cargo test` и `cargo clippy` для `x86_64-pc-windows-msvc`. |

Раз в неделю по расписанию (и вручную) идёт `spec-drift`: на Linux в образе `python:3.13-slim-bookworm` — офлайн-тесты скриптов спецификации и `update_spec.py --check`. Прогон красный, если справочник изменился (код `3`, в логе — список страниц) или портал документации не ответил после повторов (код `4`, в логе сказано, что справочник не сверен и проверку нужно повторить). Сборки Rust в нём нет, секреты не нужны.

Раннеров мало, поэтому всё, что можно, проверяйте локально до отправки изменений.

`install.sh` проверяется без сети на локальном «релизе»: `scripts/ci/install-fixture.sh <binary> <platform> <out-dir>` собирает архив, `SHA256SUMS` и копию `install.sh` со списком релизов, `scripts/ci/install-test.sh <out-dir>` прогоняет установку, обновление, флаги и подменённый архив. На macOS:

```bash
scripts/ci/macos-build.sh
scripts/ci/install-fixture.sh target/universal-apple-darwin/release/appgallery-connect-mcp macos-universal target/install-fixture-macos
sh scripts/ci/install-test.sh target/install-fixture-macos
```

## Релиз

Релиз собирает CI по тегу `vX.Y.Z` на коммите `main`. Тег ставится вручную и должен совпадать с версией workspace; CI тегов не создаёт.

1. Поднять версию: `scripts/release/bump-version.sh X.Y.Z` меняет `version` в `[workspace.package]`, версии зависимостей между крейтами и записи крейтов workspace в `Cargo.lock`. Изменение проходит обычным pull request.
2. После слияния поставить тег на коммит `main` и отправить его:

   ```bash
   git fetch origin && git tag -a vX.Y.Z -m "Release vX.Y.Z" origin/main
   git push origin vX.Y.Z
   ```

Дальше работает конвейер:

| Задача | Где | Что делает |
| --- | --- | --- |
| `release-source` | Linux | `check-version.sh` сверяет тег с версией; `sync-public.sh` публикует код тега в публичный репозиторий одним коммитом `Release <tag>`; `publish-release.sh --prepare` создаёт черновик релиза. |
| `release-linux-x86_64`, `release-linux-aarch64` | Linux | `linux-build.sh`: статический бинарник (musl, `cargo-zigbuild`) и архив `appgallery-connect-mcp-<tag>-linux-<arch>.tar.gz`, загрузка в черновик. |
| `release-macos` | macOS | Universal-бинарник, подпись Developer ID и нотаризация (`macos-sign.sh --notarize`), архив `…-macos-universal.tar.gz` (`package.sh`), загрузка в черновик. |
| `release-windows-x86_64` | Windows | Сборка MSVC со статическим CRT (`scripts/ci/windows-build.ps1`), проверка бинарника (`scripts/ci/windows-smoke.ps1`), архив `…-windows-x86_64.zip` без подписи кода (`windows-package.ps1`), загрузка в черновик (`windows-upload.ps1`). |
| `release-publish` | Linux | `write-checksums.sh` собирает `SHA256SUMS` из сумм четырёх сборок; `publish-release.sh` проверяет ассеты и публикует релиз. |
| `release-tap` | Linux | `update-tap.sh` обновляет формулу `appgallery-connect-mcp` в Homebrew tap; `update-installer.sh` обновляет ветку `install` публичного репозитория, где лежит только `install.sh` со списком релизов. Homebrew и `install.sh` — только для macOS и Linux. |

Скрипты лежат в `scripts/release/`, если не указано иное. В публичный репозиторий попадают только пути из белого списка `export-public.sh` (код, `README.md`, `docs/`, лицензии, `install.sh`); скрипты CI и релиза остаются в рабочем репозитории. Экспорт останавливается, если в тексте есть ссылки на внутренние системы или `gitleaks` находит секрет. Новый публичный файл в корне нужно добавить в белый список.

Проверить экспорт и скрипты локально, ничего не публикуя:

```bash
scripts/release/check-version.sh v0.2.0
scripts/release/export-public.sh v0.2.0 /tmp/agc-export --source-ref HEAD
scripts/release/sync-public.sh v0.2.0 --dry-run
scripts/release/publish-release.sh v0.2.0 --prepare --dry-run
```

`export-public.sh` скачивает `gitleaks`, если его нет в `PATH`. `sync-public.sh --dry-run` клонирует публичный репозиторий, но ничего в него не отправляет.

Подпись macOS-бинарника — `scripts/release/macos-sign.sh <binary> [--notarize]`; сертификат Developer ID и ключ нотаризации скрипт берёт из переменных окружения, которые CI передаёт только в куб подписи. Локально скрипт можно проверить ad-hoc подписью без сертификата: `SIGN_IDENTITY=- scripts/release/macos-sign.sh <binary>`. Перед первым релизом подпись и нотаризацию проверяет ручной запуск `sign-check` в CI: он собирает, подписывает и нотаризует бинарник, не публикуя релиз. Ручной запуск `tap-sync` заново обновляет формулу и ветку `install` для последнего опубликованного релиза.

## macOS

Universal-бинарник собирается из двух целевых платформ и склеивается `lipo`; нужен `rustup` (Rust из Homebrew не умеет добавлять вторую целевую платформу) и Xcode Command Line Tools:

```bash
scripts/ci/macos-build.sh
```

Скрипт ставит тулчейн из `rust-toolchain.toml` и целевые платформы `aarch64-apple-darwin` и `x86_64-apple-darwin`, собирает `target/universal-apple-darwin/release/appgallery-connect-mcp` и проверяет его запуск (`--version`, `--help`, ошибка на неизвестный флаг). Подписи нет.

## Статическая сборка под Linux

Linux-бинарник статический (musl): без `PT_INTERP` и `DT_NEEDED`, без зависимости от glibc и других разделяемых библиотек.

```bash
scripts/release/musl-build.sh x86_64 out/     # или aarch64
```

Скрипт запускается на Linux (например, в контейнере `rust:<версия>-bookworm`): ставит тулчейн и целевую платформу через `rustup`, Zig (`scripts/release/install-zig.sh`, архив сверяется по SHA-256) и `cargo-zigbuild`; Zig служит C-компилятором и линкером, поэтому любую из двух архитектур можно собрать на любом Linux-хосте. Нужны `readelf` (binutils) и `xz`. После сборки скрипт проверяет архитектуру ELF и отсутствие динамических зависимостей и кладёт бинарник в `out/appgallery-connect-mcp`.

Проверка бинарника в «голом» образе (Alpine, Debian slim):

```bash
scripts/ci/musl-smoke.sh out/appgallery-connect-mcp
scripts/ci/musl-smoke.sh out/appgallery-connect-mcp --with-secret-service   # от root, ставит пакеты
scripts/ci/musl-smoke.sh out/appgallery-connect-mcp --http   # HTTP-режим, при нужде ставит curl
```

Скрипт на POSIX sh. Без флага пакеты в образ не ставит: проверяет справку подкоманд, `tools list`, `toolsets`, `tools check` с маленьким `tools.toml`, фильтр `jq` в отдельном процессе и понятные ошибки `auth` и `serve` без Secret Service. С `--with-secret-service` ставит D-Bus, gnome-keyring и `secret-tool`, поднимает сессию с разблокированным хранилищем, входит с выдуманными учётными данными без проверки в сети (`auth login --no-verify`), проверяет `auth status`, `serve --status` и MCP-сессию через stdio (`initialize`, `tools/list`) и выходит (`auth logout`). С `--http` проверяет только HTTP-режим (и на macOS): запускает `serve --http` на `127.0.0.1:18769` с выдуманным файлом секрета и токенами чтения и записи, проверяет ошибки запуска (открытый файл секрета, токен вместо хеша, `--status` и `--dynamic-toolsets` вместе с `--http`), `/health`, `401` без токена и с чужим токеном, `403` для чужих `Host` и `Origin`, `413`, что токен чтения не видит и не вызывает инструменты записи, что секрет и токены не попадают в ответы и логи, пустой stdout и выход по SIGTERM. В сеть Huawei скрипт не ходит.

Образ Docker (`Dockerfile`, `compose.yaml`) в CI не собирается. Проверка локально: `docker build -t appgallery-connect-mcp .` (или `podman build`), затем `docker run` с файлом токенов и файлом секрета ([HTTP-режим и Docker](http.md#docker)).

На macOS те же проверки без записи в Keychain — `scripts/ci/macos-smoke.sh <binary>`, на Windows — `scripts/ci/windows-smoke.ps1 <binary>` (с учётными данными во временном профиле Credential Manager и `connect` с поддельным `claude.cmd`).

## Windows

Сборка — нативная, целевая платформа `x86_64-pc-windows-msvc`: нужны Visual Studio Build Tools с компонентом «Разработка классических приложений на C++» и `rustup`. Релизный бинарник собирается со статическим CRT, чтобы на машине пользователя не требовался Visual C++ Redistributable:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked --target x86_64-pc-windows-msvc -p appgallery-connect-mcp
cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc
```

Те же шаги на раннере — `scripts/ci/windows-build.ps1`: без флагов — релизная сборка, `-Check` — тесты и clippy. С `-BudgetSeconds N` скрипт останавливает `cargo`, если не уложился в N секунд, и завершается успешно, а следующий запуск продолжает с готовых артефактов; с `-Final` нехватка времени — ошибка. Скрипты PowerShell хранятся в UTF-8 с BOM: без BOM Windows PowerShell 5.1 читает их в кодировке ANSI.

Ветки кода для Windows (`cfg(windows)` и `cfg(not(unix))`) компилируются только там. С macOS и Linux их можно проверить clippy для `x86_64-pc-windows-gnu`:

```bash
rustup target add x86_64-pc-windows-gnu
cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
```

Когда в зависимостях есть C-код (например, `aws-lc-sys` для TLS), ему нужен C-компилятор под Windows. `cargo check --target x86_64-pc-windows-msvc` без Windows SDK тогда не проходит, а для `x86_64-pc-windows-gnu` компилятором служит Zig (обёртка убирает флаг `--target=…`, который `cc` передаёт в формате Rust):

```bash
cat > /tmp/zcc <<'SH'
#!/bin/sh
for a; do shift; case "$a" in --target=*) ;; *) set -- "$@" "$a";; esac; done
exec zig cc -target x86_64-windows-gnu "$@"
SH
chmod +x /tmp/zcc
CC_x86_64_pc_windows_gnu=/tmp/zcc AR_x86_64_pc_windows_gnu="zig ar" \
  cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
```

Тесты так не запускаются; их запускает `windows-build.ps1 -Check` на Windows.
