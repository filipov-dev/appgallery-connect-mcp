# appgallery-connect-mcp в HTTP-режиме (serve --http).
#
#   docker build -t appgallery-connect-mcp .
#   docker run --rm -p 127.0.0.1:8769:8769 \
#     -v "$PWD/tokens.toml:/etc/appgallery-connect-mcp/tokens.toml:ro" \
#     -v "$PWD/secrets/agc-client-secret:/run/secrets/agc-client-secret:ro" \
#     appgallery-connect-mcp \
#     serve --http --listen 0.0.0.0:8769 --region cn \
#       --access-tokens /etc/appgallery-connect-mcp/tokens.toml \
#       --secret-file /run/secrets/agc-client-secret --client-id <CLIENT_ID>
#
# Секрет API-клиента в образ не попадает: он монтируется файлом секрета
# (/run/secrets/agc-client-secret) с правами 0600 или 0400, владелец — пользователь
# контейнера (uid 10001). В файле — только секрет (идентификатор клиента — флагом
# --client-id) или JSON {"client_id": "...", "client_secret": "..."} без --client-id.
# Клиенты MCP приходят со своими токенами доступа в заголовке Authorization; в
# tokens.toml — только SHA-256 этих токенов. Подробности — docs/http.md.
#
# Бинарник собирается в Alpine статически (musl, без libdbus и OpenSSL), как бинарники
# релизов. Итоговый образ — минимальный alpine: из пакетов только корневые сертификаты
# для rustls, а busybox wget нужен HEALTHCHECK.

ARG RUST_VERSION=1.99.1
ARG ALPINE_VERSION=3.22

FROM docker.io/library/rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS build

# musl-dev — libc; perl, make и cmake — сборка aws-lc-sys (криптография rustls).
RUN apk add --no-cache musl-dev perl make cmake

WORKDIR /src
# rust-toolchain.toml не копируется: тулчейн задаёт образ (RUST_VERSION).
# Снимок спецификации (crates/appgallery-connect-api/spec/) входит в crates: его
# читает build.rs.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Цель musl по умолчанию линкуется статически (crt-static). Без символов бинарник
# заметно меньше.
ENV CARGO_PROFILE_RELEASE_STRIP=symbols \
    CARGO_TERM_COLOR=never
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p appgallery-connect-mcp \
    && cp target/release/appgallery-connect-mcp /usr/local/bin/appgallery-connect-mcp
# Статическая сборка: у бинарника нет интерпретатора (PT_INTERP), ldd musl на нём
# завершается ошибкой. Динамический бинарник здесь — регрессия, сборка падает.
RUN if ldd /usr/local/bin/appgallery-connect-mcp >/dev/null 2>&1; then \
        echo "appgallery-connect-mcp is dynamically linked" >&2; exit 1; \
    fi

FROM docker.io/library/alpine:${ALPINE_VERSION}

# ca-certificates — rustls проверяет сертификаты AppGallery Connect API по системным
# корневым сертификатам. Других библиотек статическому бинарнику не нужно.
RUN apk add --no-cache ca-certificates \
    && adduser -D -H -u 10001 -s /sbin/nologin agc \
    && mkdir -p /etc/appgallery-connect-mcp

COPY --from=build /usr/local/bin/appgallery-connect-mcp /usr/local/bin/appgallery-connect-mcp
RUN /usr/local/bin/appgallery-connect-mcp --version

# Непривилегированный пользователь; без --allow-path сервер файлов не пишет, образ можно
# запускать read-only. Каталога профилей в образе нет: учётные данные — только из файла
# секрета, регион — обязательным флагом --region (по умолчанию он не подставляется).
USER 10001:10001
EXPOSE 8769

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:8769/health || exit 1

ENTRYPOINT ["/usr/local/bin/appgallery-connect-mcp"]
# Регион кабинета у каждого свой, а по умолчанию он не подставляется, поэтому команды
# сервера по умолчанию нет: без аргументов образ печатает справку serve. Команду с
# --region передают при запуске (пример — в начале файла, compose.yaml). Внутри контейнера
# слушаем все интерфейсы; на хосте публикуйте порт только на loopback
# (-p 127.0.0.1:8769:8769) или за TLS reverse proxy.
CMD ["serve", "--help"]
