#!/usr/bin/env python3
"""Сверка снимка AppGallery Connect API со справочником Huawei.

У Huawei нет машинной спецификации Connect API: снимок `openapi.json` ведётся вручную
по страницам справочника. Скрипт не правит снимок, а помогает его вести:

    update_spec.py --check            сравнить страницы справочника с pages.lock.json
    update_spec.py --lock             записать pages.lock.json по текущим страницам
    update_spec.py --draft DIR        скачать страницы в DIR/pages и собрать черновик
                                      операций DIR/draft.json для ручной правки снимка

`--from-dir DIR` читает дерево и страницы из каталога, заполненного `--draft`, вместо
сети (для `--lock` сразу после `--draft` и для отладки разбора).

Коды выхода: 0 — расхождений нет (или файл записан), 1 — `--check` нашёл расхождения,
2 — портал недоступен (сеть, таймаут, ошибка портала после повторов) или неверные
аргументы.

Только стандартная библиотека, Python 3.10+.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from html.parser import HTMLParser
from pathlib import Path

SPEC_DIR = Path(__file__).resolve().parent
LOCK_FILE = SPEC_DIR / "pages.lock.json"
LOCK_VERSION = 1

PORTAL = "https://svc-drcn.developer.huawei.com/community/servlet/consumer/cn/documentPortal"
TREE_URL = f"{PORTAL}/getCatalogTree"
PAGE_URL = f"{PORTAL}/getDocumentById"
LANGUAGE = "en"
TIMEOUT_SECONDS = 45
ATTEMPTS = 4
PAUSE_SECONDS = 0.4
USER_AGENT = "appgallery-connect-spec-check/1"

EXIT_OK = 0
EXIT_CHANGED = 1
EXIT_UNAVAILABLE = 2


@dataclass(frozen=True)
class Source:
    """Раздел справочника: каталог портала, страница-якорь и узел дерева с операциями."""

    catalog: str
    # Любая страница каталога: портал отдаёт дерево только по ней.
    anchor: str
    # Имя корневого узла; если таких несколько, берётся узел с дочерними страницами.
    root: str
    # Адрес страницы на сайте: к нему дописывается id страницы.
    page_url: str
    # Узел первого уровня под корнем → группа. Ключ — начало имени узла.
    groups: tuple[tuple[str, str], ...]
    # Группа страниц, которые не попали ни под один ключ `groups`.
    default_group: str


SOURCES: tuple[Source, ...] = (
    Source(
        catalog="AppGallery-connect-References",
        anchor="agcapi-app-submit-0000001158245061",
        root="Connect API",
        page_url="https://developer.huawei.com/consumer/en/doc/development/AppGallery-connect-References/",
        groups=(
            ("Obtaining a Token", "auth"),
            ("Obtaining a Project-Level Token", "auth"),
            ("Publishing API", "publishing"),
            ("Upload Management API", "upload"),
            ("Provisioning API", "provisioning"),
            ("Reports API", "reports"),
            ("Project Management API", "project"),
            ("Comments API", "comments"),
            ("PMS API", "pms"),
            ("Appendix", "appendix"),
        ),
        default_group="other",
    ),
    Source(
        catalog="app",
        anchor="agc-help-connect-api-introduction-0000002270974725",
        root="AppGallery Connect API",
        page_url="https://developer.huawei.com/consumer/en/doc/app/",
        groups=(
            ("Upload Management API", "upload"),
            ("Provisioning API", "provisioning"),
            ("Appendix", "appendix"),
        ),
        default_group="guide",
    ),
)

# Заголовки авторизации: их добавляет исполнитель запросов, в параметры операций они
# не входят (режим API-клиента — client_id и Authorization, режим OAuth-клиента —
# teamId и oauth2Token).
AUTH_HEADERS = frozenset({"client_id", "authorization", "teamid", "oauth2token"})

# Режимы авторизации, как их называет справочник, и их короткие имена.
AUTH_MODES: tuple[tuple[str, str], ...] = (
    ("api client", "api-client"),
    ("oauth client", "oauth-client"),
    ("service account", "service-account"),
)

# Известные конверты результата: (поле, поле кода, поле текста).
RESULT_ENVELOPES: tuple[tuple[str, str, str], ...] = (
    ("ret", "code", "msg"),
    ("ret", "rtnCode", "rtnDesc"),
    ("error", "errorCode", "errorMsg"),
)


class PortalError(Exception):
    """Портал документации недоступен или ответил не тем, что ожидалось."""


# ---------------------------------------------------------------------------
# Разбор HTML: лёгкое дерево и блоки (заголовки, абзацы, таблицы, примеры).
# ---------------------------------------------------------------------------

VOID_TAGS = frozenset(
    {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "wbr"}
)
SKIP_TAGS = frozenset({"script", "style", "head"})
HEADING_TAGS = frozenset({"h1", "h2", "h3", "h4", "h5", "h6"})
BLOCK_TAGS = frozenset({"p", "div", "li", "tr", "td", "th", "table", "ul", "ol", "pre", "br"}) | HEADING_TAGS


@dataclass
class Node:
    tag: str
    children: list["Node | str"] = field(default_factory=list)


class _TreeBuilder(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.root = Node("#root")
        self._stack = [self.root]

    def handle_starttag(self, tag: str, attrs) -> None:
        node = Node(tag)
        self._stack[-1].children.append(node)
        if tag not in VOID_TAGS:
            self._stack.append(node)

    def handle_startendtag(self, tag: str, attrs) -> None:
        self._stack[-1].children.append(Node(tag))

    def handle_endtag(self, tag: str) -> None:
        if tag in VOID_TAGS:
            return
        for index in range(len(self._stack) - 1, 0, -1):
            if self._stack[index].tag == tag:
                del self._stack[index:]
                return

    def handle_data(self, data: str) -> None:
        self._stack[-1].children.append(data)


def parse_html(html: str) -> Node:
    builder = _TreeBuilder()
    builder.feed(html)
    builder.close()
    return builder.root


def squash(text: str) -> str:
    """Схлопывает пробельные символы (включая неразрывный пробел) в один пробел."""
    return re.sub(r"[\s ​]+", " ", text).strip()


def node_text(node: "Node | str", preformatted: bool = False) -> str:
    """Текст узла. Блочные элементы разделяются переводом строки, строки схлопываются."""
    parts: list[str] = []

    def walk(item: "Node | str") -> None:
        if isinstance(item, str):
            parts.append(item)
            return
        if item.tag in SKIP_TAGS:
            return
        block = item.tag in BLOCK_TAGS
        if block:
            parts.append("\n")
        for child in item.children:
            walk(child)
        if block:
            parts.append("\n")

    walk(node)
    raw = "".join(parts)
    if preformatted:
        return raw.replace(" ", " ").strip("\n")
    lines = [squash(line) for line in raw.split("\n")]
    return "\n".join(line for line in lines if line)


@dataclass
class Block:
    kind: str  # heading | text | table | code
    text: str = ""
    level: int = 0
    rows: list[list[str]] = field(default_factory=list)


def _table_rows(table: Node) -> list[list[str]]:
    rows: list[list[str]] = []

    def walk(node: Node) -> None:
        for child in node.children:
            if isinstance(child, str):
                continue
            if child.tag == "tr":
                cells = [
                    node_text(cell)
                    for cell in child.children
                    if isinstance(cell, Node) and cell.tag in ("td", "th")
                ]
                if cells:
                    rows.append(cells)
            elif child.tag != "table":
                walk(child)

    walk(table)
    return rows


def blocks_of(root: Node) -> list[Block]:
    """Блоки страницы в порядке чтения."""
    blocks: list[Block] = []

    def flush(buffer: list[str]) -> None:
        text = squash("".join(buffer))
        buffer.clear()
        if text:
            blocks.append(Block("text", text))

    def walk(node: Node) -> None:
        buffer: list[str] = []
        for child in node.children:
            if isinstance(child, str):
                buffer.append(child)
                continue
            tag = child.tag
            if tag in SKIP_TAGS:
                continue
            if tag in HEADING_TAGS:
                flush(buffer)
                text = squash(node_text(child))
                level = int(tag[1])
                # Портал помечает подзаголовки префиксом вида «[h2]».
                marker = re.match(r"^\[h(\d)\]\s*", text)
                if marker:
                    level = 4 + int(marker.group(1))
                    text = text[marker.end():]
                if text:
                    blocks.append(Block("heading", text, level=level))
            elif tag == "table":
                flush(buffer)
                rows = _table_rows(child)
                if rows:
                    blocks.append(Block("table", rows=rows))
            elif tag == "pre":
                flush(buffer)
                text = node_text(child, preformatted=True)
                if text.strip():
                    blocks.append(Block("code", text))
            elif tag == "p" or tag == "li":
                flush(buffer)
                if _has_block_child(child):
                    walk(child)
                else:
                    text = squash(node_text(child))
                    if text:
                        blocks.append(Block("text", text))
            elif tag == "br":
                buffer.append(" ")
            elif _has_block_child(child) or tag in ("div", "ul", "ol", "body", "html", "section"):
                flush(buffer)
                walk(child)
            else:
                buffer.append(node_text(child))
        flush(buffer)

    walk(root)
    return blocks


def _has_block_child(node: Node) -> bool:
    for child in node.children:
        if isinstance(child, Node):
            if child.tag in ("table", "pre", "p", "div", "ul", "ol") or child.tag in HEADING_TAGS:
                return True
            if _has_block_child(child):
                return True
    return False


def normalized_text(html: str) -> str:
    """Текст страницы без разметки: по строке на блок, ячейки таблиц — через « | ».

    Идентификаторы элементов, классы, якоря и ширина колонок в текст не попадают,
    поэтому косметика вёрстки не меняет хеш.
    """
    lines: list[str] = []
    for block in blocks_of(parse_html(html)):
        if block.kind == "heading":
            lines.append(f"# {block.text}")
        elif block.kind == "table":
            for row in block.rows:
                lines.append(" | ".join(squash(cell) for cell in row))
        elif block.kind == "code":
            lines.extend(line.rstrip() for line in block.text.split("\n") if line.strip())
        else:
            lines.append(block.text)
    return "\n".join(lines)


def page_hash(html: str) -> str:
    return hashlib.sha256(normalized_text(html).encode("utf-8")).hexdigest()


# ---------------------------------------------------------------------------
# Черновик операции из страницы справочника.
# ---------------------------------------------------------------------------

SECTION_ALIASES = {
    "function": "function",
    "description": "function",
    "restrictions": "restrictions",
    "constraints": "restrictions",
    "prototype": "prototype",
    "api prototype": "prototype",
    "request parameters": "request",
    "request parameter": "request",
    "request example": "request_example",
    "example request": "request_example",
    "response parameters": "response",
    "response parameter": "response",
    "response example": "response_example",
    "example response": "response_example",
    "call example": "call_example",
    "sample code": "call_example",
}
LOCATION_ALIASES = {
    "header": "header",
    "headers": "header",
    "request header": "header",
    "query": "query",
    "query parameters": "query",
    "path": "path",
    "path parameters": "path",
    "body": "body",
    "request body": "body",
}
TYPE_ALIASES = {
    "string": "string",
    "integer": "integer",
    "int": "integer",
    "long": "integer",
    "number": "number",
    "double": "number",
    "bigdecimal": "number",
    "float": "number",
    "boolean": "boolean",
    "bool": "boolean",
    "object": "object",
}


def split_sections(blocks: list[Block]) -> tuple[str, list[tuple[str, list[Block]]]]:
    """Заголовок страницы и разделы: (имя раздела, блоки с подзаголовками)."""
    title = ""
    sections: list[tuple[str, list[Block]]] = []
    current: list[Block] | None = None
    for block in blocks:
        if block.kind == "heading":
            if block.level == 1 and not title:
                title = block.text
                continue
            key = SECTION_ALIASES.get(block.text.lower().rstrip(":"))
            if key is not None:
                current = []
                sections.append((key, current))
                continue
            # Внутри параметров запроса и ответа подзаголовки (Header, Query, имена моделей)
            # остаются в разделе; в остальных местах заголовок открывает новый раздел.
            inside = bool(sections) and sections[-1][0] in ("request", "response")
            if current is None or not inside:
                current = []
                sections.append((block.text, current))
                continue
        if current is not None:
            current.append(block)
    return title, sections


def parse_type(raw: str) -> dict:
    """Тип из колонки Type.

    `String(256)` — строка с предельной длиной, `Integer(64)` — целое (число в скобках —
    разрядность, она в схему не идёт), `List<ProductInfo>` и `String[]` — массивы,
    `Map<String, FilePartInfo>` — объект с произвольными ключами, одно слово с заглавной
    буквы — модель данных (`{"model": ...}`).
    """
    # Пометка расположения в колонке типа («String(header)») к типу не относится.
    text = re.sub(r"\s*\((?:header|query|path|body)\)$", "", squash(raw), flags=re.I)
    lowered = text.lower()
    if lowered.endswith("[]"):
        return {"type": "array", "items": parse_type(text[:-2])}
    generic = re.match(r"^([A-Za-z]+)\s*<\s*(.+?)\s*>$", text)
    if generic:
        kind, inner = generic.group(1).lower(), generic.group(2)
        if kind in ("list", "array", "set", "arraylist", "collection"):
            return {"type": "array", "items": parse_type(inner)}
        if kind in ("map", "hashmap"):
            value = inner.split(",", 1)[1] if "," in inner else "string"
            return {"type": "object", "additionalProperties": parse_type(value)}
    array = re.match(r"^(?:list|array)\s+of\s+(.+)$", text, flags=re.I)
    if array:
        return {"type": "array", "items": parse_type(array.group(1))}
    sized = re.match(r"^([A-Za-z]+)\s*\(\s*(\d+)\s*\)$", text)
    if sized:
        simple = TYPE_ALIASES.get(sized.group(1).lower())
        if simple == "string":
            return {"type": "string", "maxLength": int(sized.group(2))}
        if simple:
            return {"type": simple}
    simple = TYPE_ALIASES.get(lowered)
    if simple:
        return {"type": simple}
    if re.fullmatch(r"[A-Z][A-Za-z0-9]*", text):
        return {"model": text}
    return {"type": "string", "raw": text}


def parse_mandatory(raw: str) -> bool | None:
    text = squash(raw).lower()
    if text in ("yes", "m", "mandatory", "true", "y"):
        return True
    if text in ("no", "o", "optional", "false", "n"):
        return False
    return None


def table_fields(rows: list[list[str]]) -> list[dict] | None:
    """Строки таблицы параметров. `None`, если это не таблица параметров."""
    if len(rows) < 1:
        return None
    header = [squash(cell).lower() for cell in rows[0]]
    names = {"parameter": "name", "name": "name", "field": "name", "parameter name": "name",
             "mandatory": "required", "mandatory (m)/optional (o)": "required", "required": "required",
             "type": "type", "data type": "type",
             "description": "description", "remarks": "description"}
    columns = {names[cell]: index for index, cell in enumerate(header) if cell in names}
    if "name" not in columns or ("type" not in columns and "description" not in columns):
        return None
    fields: list[dict] = []
    for row in rows[1:]:
        def cell(key: str) -> str:
            index = columns.get(key)
            return row[index] if index is not None and index < len(row) else ""

        name = squash(cell("name"))
        if not name:
            continue
        item: dict = {"name": name}
        if "required" in columns:
            item["required"] = parse_mandatory(cell("required"))
        if "type" in columns:
            item["schema"] = parse_type(cell("type"))
            item["typeText"] = squash(cell("type"))
        item["description"] = cell("description").replace("\n", " ").strip()
        fields.append(item)
    return fields


def split_url(url: str) -> tuple[str | None, list[str], list[str]]:
    """Путь от корня хоста, параметры пути и имена параметров запроса из ячейки URL.

    В ячейке кроме адреса бывают пояснения (домены регионов); адрес — первое слово,
    похожее на URL. `None`, если адреса нет: файл отправляется на адрес из ответа API.
    """
    match = re.search(r"https?://\S+|(?<![\w.])/api/\S+", url)
    if not match:
        return None, [], []
    text = re.sub(r"^https?://[^/]+", "", match.group(0)).rstrip(".,;")
    path, _, query = text.partition("?")
    path_params = re.findall(r"\{([^{}]+)\}", path)
    query_names = [part.split("=", 1)[0] for part in query.split("&") if part]
    return path, path_params, query_names


def detect_envelope(response_fields: list[dict], example: str) -> dict | None:
    """Конверт результата: по примеру ответа, иначе по таблице параметров ответа."""
    try:
        body = json.loads(example) if example.strip() else None
    except ValueError:
        body = None
    if isinstance(body, dict):
        for name, code, message in RESULT_ENVELOPES:
            inner = body.get(name)
            if isinstance(inner, dict) and code in inner:
                return {"field": name, "codeField": code, "messageField": message, "source": "example"}
    # Пример бывает невалидным JSON: ищем поля текстом.
    for name, code, message in RESULT_ENVELOPES:
        if re.search(rf'"{name}"\s*:\s*\{{[^{{}}]*"{code}"', example):
            return {"field": name, "codeField": code, "messageField": message, "source": "example-text"}
    for item in response_fields:
        if item["name"] == "ret":
            comments = "rtncode" in item.get("description", "").lower() or item.get("schema", {}).get(
                "model"
            ) == "CommentBaseResult"
            code, message = ("rtnCode", "rtnDesc") if comments else ("code", "msg")
            return {"field": "ret", "codeField": code, "messageField": message, "source": "table"}
        if item["name"] == "error":
            return {"field": "error", "codeField": "errorCode", "messageField": "errorMsg", "source": "table"}
    return None


def parse_page(html: str) -> dict:
    """Черновик страницы справочника: прототип, параметры по расположению, ответ.

    У страниц без раздела Prototype (модели данных, приложения, коды результата)
    `method` и `path` пусты, а первая таблица параметров попадает в `fields`.
    """
    blocks = blocks_of(parse_html(html))
    title, sections = split_sections(blocks)
    draft: dict = {
        "title": title,
        "function": "",
        "restrictions": "",
        "method": None,
        "direction": "",
        "url": None,
        "path": None,
        "parameters": {"path": [], "query": [], "header": [], "body": []},
        "authHeaders": [],
        "authModes": [],
        "urlQueryNames": [],
        "response": [],
        "nestedTables": [],
        "requestExample": "",
        "responseExample": "",
        "result": None,
        "fields": [],
        "notes": [],
    }
    for key, content in sections:
        texts = [block.text for block in content if block.kind == "text"]
        if key == "function":
            draft["function"] = " ".join(texts)
        elif key == "restrictions":
            draft["restrictions"] = " ".join(texts)
        elif key == "prototype":
            for block in content:
                if block.kind != "table":
                    continue
                for row in block.rows:
                    if len(row) < 2:
                        continue
                    name, value = squash(row[0]).lower(), squash(row[1])
                    if name in ("protocol", "method", "request method"):
                        match = re.search(r"\b(GET|POST|PUT|PATCH|DELETE)\b", value.upper())
                        if match:
                            draft["method"] = match.group(1)
                    elif name == "direction":
                        draft["direction"] = value
                    elif name in ("url", "uri", "request url"):
                        draft["url"] = value
                        path, path_params, query_names = split_url(value)
                        draft["path"] = path
                        draft["urlQueryNames"] = query_names
                        draft["parameters"]["path"] = [
                            {"name": param, "required": True, "schema": {"type": "string"},
                             "description": ""}
                            for param in path_params
                        ]
        elif key == "request":
            _parse_request(content, draft)
        elif key == "response":
            first = True
            caption = ""
            for block in content:
                if block.kind in ("text", "heading"):
                    caption = block.text
                elif block.kind == "table":
                    fields = table_fields(block.rows)
                    if fields is None:
                        continue
                    if first:
                        draft["response"] = fields
                        first = False
                    else:
                        draft["nestedTables"].append({"in": "response", "caption": caption, "fields": fields})
        elif key in ("request_example", "response_example"):
            code = "\n".join(block.text for block in content if block.kind == "code")
            if not code:
                code = "\n".join(texts)
            draft["requestExample" if key == "request_example" else "responseExample"] = code
        else:
            for block in content:
                if block.kind == "table" and not draft["fields"]:
                    fields = table_fields(block.rows)
                    if fields:
                        draft["fields"] = fields
    if not draft["method"] and not draft["fields"]:
        # Модель данных без известных разделов: берём первую таблицу параметров.
        for block in blocks:
            if block.kind == "table":
                fields = table_fields(block.rows)
                if fields:
                    draft["fields"] = fields
                    break
    if not draft["authModes"]:
        headers = {name.lower() for name in draft["authHeaders"]}
        if "client_id" in headers:
            draft["authModes"].append("api-client")
        if headers & {"teamid", "oauth2token"}:
            draft["authModes"].append("oauth-client")
    draft["result"] = detect_envelope(draft["response"], draft["responseExample"])
    _cross_check(draft)
    return draft


def _parse_request(content: list[Block], draft: dict) -> None:
    location: str | None = None
    caption = ""
    seen: set[str] = set()
    for block in content:
        if block.kind == "heading":
            location = LOCATION_ALIASES.get(block.text.lower().rstrip(":"), location)
            caption = block.text
        elif block.kind == "text":
            caption = block.text
            lowered = block.text.lower().rstrip(":.")
            for marker, mode in AUTH_MODES:
                if marker in lowered and "mode" in lowered and mode not in draft["authModes"]:
                    draft["authModes"].append(mode)
            if lowered in LOCATION_ALIASES:
                location = LOCATION_ALIASES[lowered]
        elif block.kind == "table":
            fields = table_fields(block.rows)
            if fields is None:
                continue
            if location is None:
                draft["notes"].append(f"request table without location: {caption!r}")
                location = "query" if draft["method"] == "GET" else "body"
            if location == "header":
                for item in fields:
                    if item["name"].lower() in AUTH_HEADERS:
                        if item["name"] not in draft["authHeaders"]:
                            draft["authHeaders"].append(item["name"])
                    elif item["name"] not in {p["name"] for p in draft["parameters"]["header"]}:
                        draft["parameters"]["header"].append(item)
            elif location == "path":
                known = {param["name"]: param for param in draft["parameters"]["path"]}
                for item in fields:
                    item["required"] = True
                    if item["name"] in known:
                        known[item["name"]].update(item)
                    else:
                        draft["parameters"]["path"].append(item)
            elif location in seen:
                # Вторая таблица того же расположения — вложенная структура.
                draft["nestedTables"].append({"in": location, "caption": caption, "fields": fields})
            else:
                draft["parameters"][location].extend(fields)
                seen.add(location)


def _cross_check(draft: dict) -> None:
    """Сверяет таблицы с URL и примерами: расхождения — в `notes` для ручной вычитки."""
    if not draft["method"] or not draft["path"]:
        return
    path_names = {param["name"] for param in draft["parameters"]["path"]}
    query = draft["parameters"]["query"]
    # Параметр пути часто описан в таблице Query: переносим описание.
    for item in list(query):
        if item["name"] in path_names:
            for param in draft["parameters"]["path"]:
                if param["name"] == item["name"]:
                    param.update({**item, "required": True})
            query.remove(item)
            draft["notes"].append(f"path parameter {item['name']} was listed in the Query table")
    table_names = {item["name"] for item in query}
    for name in draft["urlQueryNames"]:
        if name not in table_names:
            close = [other for other in table_names if other.lower() == name.lower()]
            hint = f" (table has {close[0]})" if close else ""
            draft["notes"].append(f"query parameter {name} is in the URL but not in the Query table{hint}")
    example = draft["requestExample"]
    for location in ("query", "header"):
        for item in draft["parameters"][location]:
            name = item["name"]
            if example and name not in example and re.search(re.escape(name), example, flags=re.I):
                draft["notes"].append(f"{location} parameter {name} is spelled differently in the request example")
    if draft["result"] is None:
        draft["notes"].append("result envelope not detected")


# ---------------------------------------------------------------------------
# Портал: дерево и страницы.
# ---------------------------------------------------------------------------


def post_json(url: str, payload: dict) -> dict:
    """POST с повторами. Ошибка сети, таймаут, HTTP 429/5xx и неверный JSON — PortalError."""
    body = json.dumps(payload).encode("utf-8")
    last = ""
    for attempt in range(1, ATTEMPTS + 1):
        request = urllib.request.Request(
            url,
            data=body,
            headers={"Content-Type": "application/json", "User-Agent": USER_AGENT},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
                data = json.loads(response.read().decode("utf-8"))
            if isinstance(data, dict):
                return data
            last = "ответ не является объектом JSON"
        except urllib.error.HTTPError as error:
            last = f"HTTP {error.code}"
            if error.code not in (408, 429) and error.code < 500:
                break
        except (urllib.error.URLError, TimeoutError, OSError, ValueError) as error:
            last = str(error) or type(error).__name__
        if attempt < ATTEMPTS:
            time.sleep(min(2 ** attempt, 20))
    raise PortalError(f"{url}: {last}")


class Portal:
    """Источник дерева и страниц: сеть или каталог с ранее скачанными файлами."""

    def __init__(self, from_dir: Path | None = None, save_dir: Path | None = None) -> None:
        self.from_dir = from_dir
        self.save_dir = save_dir
        self._last_request = 0.0

    def _pause(self) -> None:
        wait = PAUSE_SECONDS - (time.monotonic() - self._last_request)
        if wait > 0:
            time.sleep(wait)
        self._last_request = time.monotonic()

    def _cached(self, name: str) -> dict | None:
        if self.from_dir is None:
            return None
        path = self.from_dir / name
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            raise PortalError(f"{path}: {error}") from error

    def _save(self, name: str, data: dict) -> None:
        if self.save_dir is None:
            return
        path = self.save_dir / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")

    def tree(self, source: Source) -> list[dict]:
        name = f"tree.{source.catalog}.json"
        data = self._cached(name)
        if data is None:
            self._pause()
            data = post_json(
                TREE_URL,
                {"language": LANGUAGE, "catalogName": source.catalog, "objectId": source.anchor},
            )
            self._save(name, data)
        nodes = (data.get("value") or {}).get("catalogTreeList")
        if not isinstance(nodes, list):
            raise PortalError(f"дерево каталога {source.catalog}: нет value.catalogTreeList")
        return nodes

    def page(self, page_id: str) -> tuple[str, str]:
        """Заголовок и HTML страницы."""
        name = f"pages/{page_id}.json"
        data = self._cached(name)
        if data is None:
            self._pause()
            data = post_json(PAGE_URL, {"language": LANGUAGE, "objectId": page_id})
            self._save(name, data)
        value = data.get("value") or {}
        content = (value.get("content") or {}).get("content")
        if not isinstance(content, str) or not content.strip():
            raise PortalError(f"страница {page_id}: нет value.content.content")
        return str(value.get("title") or ""), content


@dataclass(frozen=True)
class PageRef:
    id: str
    title: str
    group: str
    catalog: str
    # Путь узла в дереве под корнем раздела.
    path: tuple[str, ...]

    @property
    def url(self) -> str:
        for source in SOURCES:
            if source.catalog == self.catalog:
                return source.page_url + self.id
        return ""


def find_root(nodes: list[dict], name: str) -> dict | None:
    """Узел с именем `name`; из нескольких — тот, у которого больше дочерних страниц."""
    best: dict | None = None
    best_size = -1

    def walk(node: dict) -> None:
        nonlocal best, best_size
        if squash(str(node.get("nodeName") or "")) == name:
            size = len(node.get("children") or [])
            if size > best_size:
                best, best_size = node, size
        for child in node.get("children") or []:
            walk(child)

    for node in nodes:
        walk(node)
    return best


def collect_pages(source: Source, nodes: list[dict]) -> list[PageRef]:
    """Страницы под корневым узлом раздела в порядке дерева."""
    root = find_root(nodes, source.root)
    if root is None:
        raise PortalError(f"каталог {source.catalog}: нет узла «{source.root}»")
    pages: list[PageRef] = []
    seen: set[str] = set()

    def group_of(top: str) -> str:
        for prefix, group in source.groups:
            if top.startswith(prefix):
                return group
        return source.default_group

    def walk(node: dict, path: tuple[str, ...]) -> None:
        name = squash(str(node.get("nodeName") or ""))
        here = path + (name,)
        page_id = node.get("relateDocument")
        if isinstance(page_id, str) and page_id and page_id not in seen:
            seen.add(page_id)
            pages.append(PageRef(page_id, name, group_of(here[0]), source.catalog, here))
        for child in node.get("children") or []:
            walk(child, here)

    for child in root.get("children") or []:
        walk(child, ())
    return pages


def all_pages(portal: Portal) -> list[PageRef]:
    pages: list[PageRef] = []
    seen: set[str] = set()
    for source in SOURCES:
        for page in collect_pages(source, portal.tree(source)):
            if page.id not in seen:
                seen.add(page.id)
                pages.append(page)
    return pages


# ---------------------------------------------------------------------------
# pages.lock.json
# ---------------------------------------------------------------------------


def build_lock(portal: Portal, progress=None) -> dict:
    entries = []
    pages = all_pages(portal)
    for index, page in enumerate(pages, 1):
        if progress:
            progress(index, len(pages), page)
        _, html = portal.page(page.id)
        entries.append(
            {
                "id": page.id,
                "title": page.title,
                "group": page.group,
                "catalog": page.catalog,
                "sha256": page_hash(html),
            }
        )
    return {
        "version": LOCK_VERSION,
        "language": LANGUAGE,
        "sources": [
            {"catalog": source.catalog, "anchor": source.anchor, "root": source.root}
            for source in SOURCES
        ],
        "pages": entries,
    }


def dump_lock(lock: dict) -> str:
    return json.dumps(lock, ensure_ascii=False, indent=2) + "\n"


def load_lock(path: Path) -> dict:
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict) or not isinstance(data.get("pages"), list):
        raise ValueError(f"{path}: нет списка pages")
    return data


@dataclass
class LockDiff:
    changed: list[dict] = field(default_factory=list)
    added: list[dict] = field(default_factory=list)
    removed: list[dict] = field(default_factory=list)
    # Тот же текст, но другое название или группа в дереве.
    moved: list[tuple[dict, dict]] = field(default_factory=list)

    def is_empty(self) -> bool:
        return not (self.changed or self.added or self.removed or self.moved)


def compare_locks(old: dict, new: dict) -> LockDiff:
    """Что изменилось в справочнике относительно сохранённого списка страниц."""
    diff = LockDiff()
    before = {page["id"]: page for page in old["pages"]}
    after = {page["id"]: page for page in new["pages"]}
    for page_id, page in after.items():
        previous = before.get(page_id)
        if previous is None:
            diff.added.append(page)
        elif previous.get("sha256") != page.get("sha256"):
            diff.changed.append(page)
        elif (previous.get("title"), previous.get("group")) != (page.get("title"), page.get("group")):
            diff.moved.append((previous, page))
    for page_id, page in before.items():
        if page_id not in after:
            diff.removed.append(page)
    return diff


def page_url(page: dict) -> str:
    for source in SOURCES:
        if source.catalog == page.get("catalog"):
            return source.page_url + page["id"]
    return page["id"]


def format_diff(diff: LockDiff) -> str:
    if diff.is_empty():
        return "Справочник не изменился.\n"
    lines: list[str] = []

    def section(title: str, pages: list[dict]) -> None:
        if not pages:
            return
        lines.append(f"{title} ({len(pages)}):")
        for page in pages:
            lines.append(f"  [{page.get('group')}] {page.get('title')}")
            lines.append(f"      {page_url(page)}")

    section("Изменённые страницы", diff.changed)
    section("Новые страницы", diff.added)
    section("Пропавшие страницы", diff.removed)
    if diff.moved:
        lines.append(f"Переименованные или перенесённые страницы ({len(diff.moved)}):")
        for previous, page in diff.moved:
            lines.append(
                f"  [{previous.get('group')}] {previous.get('title')} -> "
                f"[{page.get('group')}] {page.get('title')}"
            )
            lines.append(f"      {page_url(page)}")
    lines.append("")
    lines.append(
        "Просмотри страницы, поправь openapi.json и overlay.toml вручную, "
        "затем обнови список: update_spec.py --lock"
    )
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Черновик
# ---------------------------------------------------------------------------


def build_draft(portal: Portal, progress=None) -> dict:
    operations: list[dict] = []
    external: list[dict] = []
    models: list[dict] = []
    others: list[dict] = []
    pages = all_pages(portal)
    for index, page in enumerate(pages, 1):
        if progress:
            progress(index, len(pages), page)
        _, html = portal.page(page.id)
        draft = parse_page(html)
        entry = {
            "id": page.id,
            "group": page.group,
            "catalog": page.catalog,
            "node": " / ".join(page.path),
            "reference": page.url,
            **draft,
        }
        if draft["path"]:
            operations.append(entry)
        elif draft["method"] or draft["url"]:
            # Не вызов Connect API: отправка файла на адрес из ответа или обратный вызов.
            external.append(entry)
        elif draft["fields"]:
            models.append(entry)
        else:
            others.append({key: entry[key] for key in ("id", "group", "catalog", "node", "reference", "title")})
    return {"operations": operations, "external": external, "models": models, "other": others}


# ---------------------------------------------------------------------------
# Командная строка
# ---------------------------------------------------------------------------


def _progress(index: int, total: int, page: PageRef) -> None:
    print(f"[{index}/{total}] {page.group}: {page.title}", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Сверка снимка AppGallery Connect API со справочником Huawei."
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true",
                      help="сравнить страницы справочника с pages.lock.json, ничего не менять")
    mode.add_argument("--lock", action="store_true", help="записать pages.lock.json")
    mode.add_argument("--draft", metavar="DIR", type=Path,
                      help="скачать страницы в DIR/pages и записать черновик DIR/draft.json")
    parser.add_argument("--from-dir", metavar="DIR", type=Path,
                        help="читать дерево и страницы из каталога, заполненного --draft")
    parser.add_argument("--lock-file", metavar="FILE", type=Path, default=LOCK_FILE,
                        help="путь к pages.lock.json (по умолчанию рядом со скриптом)")
    parser.add_argument("--quiet", action="store_true", help="не печатать ход загрузки")
    try:
        args = parser.parse_args(argv)
    except SystemExit as error:
        return EXIT_OK if error.code == 0 else EXIT_UNAVAILABLE
    progress = None if args.quiet else _progress

    try:
        if args.draft is not None:
            portal = Portal(from_dir=args.from_dir, save_dir=None if args.from_dir else args.draft)
            draft = build_draft(portal, progress)
            args.draft.mkdir(parents=True, exist_ok=True)
            target = args.draft / "draft.json"
            target.write_text(json.dumps(draft, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            print(
                f"{target}: операций {len(draft['operations'])}, "
                f"не вызовов API {len(draft['external'])}, моделей {len(draft['models'])}, "
                f"прочих страниц {len(draft['other'])}"
            )
            return EXIT_OK

        portal = Portal(from_dir=args.from_dir)
        lock = build_lock(portal, progress)
        if args.lock:
            args.lock_file.write_text(dump_lock(lock), encoding="utf-8")
            print(f"{args.lock_file}: страниц {len(lock['pages'])}")
            return EXIT_OK

        try:
            saved = load_lock(args.lock_file)
        except (OSError, ValueError) as error:
            print(f"ошибка: {error}", file=sys.stderr)
            return EXIT_UNAVAILABLE
        diff = compare_locks(saved, lock)
        sys.stdout.write(format_diff(diff))
        return EXIT_OK if diff.is_empty() else EXIT_CHANGED
    except PortalError as error:
        print(f"ошибка: портал документации недоступен: {error}", file=sys.stderr)
        return EXIT_UNAVAILABLE


if __name__ == "__main__":
    sys.exit(main())
