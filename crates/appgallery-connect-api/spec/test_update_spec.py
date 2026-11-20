"""Офлайн-тесты update_spec.py: разбор страниц, нормализация, сравнение списков страниц,
повторы при сбоях портала и коды выхода.

Сеть не нужна: страницы — небольшие выдуманные фрагменты HTML, загрузка, пауза и часы
подменяются.

    python3 -I -B -m unittest discover -s crates/appgallery-connect-api/spec -p 'test_*.py'
"""

from __future__ import annotations

import contextlib
import email.message
import importlib.util
import io
import json
import sys
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

SPEC_DIR = Path(__file__).resolve().parent


def load_module():
    spec = importlib.util.spec_from_file_location("update_spec", SPEC_DIR / "update_spec.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules["update_spec"] = module
    spec.loader.exec_module(module)
    return module


update_spec = load_module()


def table(*rows: tuple[str, ...], header: tuple[str, ...] = ("Parameter", "Mandatory", "Type", "Description")) -> str:
    def row(cells, tag):
        return "<tr>" + "".join(f'<{tag} class="cellrowborder" width="20%"><p id="p{i}">{cell}</p></{tag}>'
                                for i, cell in enumerate(cells)) + "</tr>"

    return ('<div class="tablenoborder"><table id="t1"><thead>' + row(header, "th") + "</thead><tbody>"
            + "".join(row(cells, "td") for cells in rows) + "</tbody></table></div>")


AUTH_TABLES = (
    "<p>API client mode</p>"
    + table(("client_id", "Yes", "String", "Client ID."), ("Authorization", "Yes", "String", "Bearer token."))
    + "<p>OAuth client mode</p>"
    + table(("teamId", "Yes", "String", "Team ID."), ("oauth2Token", "Yes", "String", "Token."))
)


def page(title: str, method: str, url: str, request: str, response: str, example: str = "",
         request_example: str = "") -> str:
    return f"""<html><head><style>p {{}}</style></head><body><a name="TOPIC_1"></a>
<h1>{title}</h1>
<div class="section" id="s1"><h4>Function</h4><p id="f1">This API is used to do <strong>something</strong>.</p></div>
<div class="section" id="s2"><h4>Prototype</h4><div class="tablenoborder"><table>
<tr><th class="firstcol"><p>Protocol</p></th><td><p>HTTPS {method}</p></td></tr>
<tr><th class="firstcol"><p>Direction</p></th><td><p>Your server -&gt; Huawei server</p></td></tr>
<tr><th class="firstcol"><p>URL</p></th><td><p>{url}</p></td></tr>
</table></div></div>
<div class="section" id="s3"><h4>Request Parameters</h4></div>
{request}
<div class="section" id="s5"><h4>Request Example</h4><pre class="screen">{request_example}</pre></div>
<div class="section" id="s6"><h4>Response Parameters</h4>{response}</div>
<div class="section" id="s7"><h4>Response Example</h4><pre class="screen">{example}</pre></div>
</body></html>"""


PRODUCT_PAGE = page(
    "Querying Details of a Product",
    "GET",
    "https://connect-api.cloud.huawei.com/api/pms/product-price-service/v1/manage/product?productNo={productNo}",
    '<div class="section"><h4>[h2]Header</h4>' + AUTH_TABLES
    + table(("appId", "Yes", "String", "App ID.")) + "</div>"
    + '<div class="section"><h4>[h2]Query</h4>'
    + table(("productNo", "Yes", "String(148)", "Product ID."), ("country", "No", "String", "Country code."))
    + "</div>",
    table(("error", "Yes", "ErrorResult", "Result."), ("product", "No", "ProductDetailInfo", "Product.")),
    '{"error": {"errorCode": 0, "errorMsg": null}, "product": {}}',
    "GET /api/pms/product-price-service/v1/manage/product?productNo=demo HTTP/1.1\nappId: 1",
)

REVIEWS_PAGE = page(
    "Querying the Comment List",
    "GET",
    "https://connect-api.cloud.huawei.com/api/reviews/v1/manage/dev/reviews/{reviewId}"
    "</p><p>Domain name for Germany: connect-api-dre.cloud.huawei.com",
    '<div class="section"><h4>[h2]Header</h4><p>API client mode</p>'
    + table(("client_id", "Yes", "String", "Client ID."), ("Authorization", "Yes", "String", "Bearer token."))
    + "</div>"
    + '<div class="section"><h4>[h2]Query</h4>'
    + table(
        ("appId", "Yes", "String", "App ID."),
        ("reviewId", "Yes", "String", "Review ID."),
        ("beginTime", "Yes", "Long", "Start."),
        ("filterCondition", "No", "List&lt;String&gt;", "Filters."),
    )
    + "</div>",
    table(("ret", "Yes", "CommentBaseResult", "Result."), ("data", "No", "QueryDevReviewsRspData", "Data.")),
    '{"ret": {"rtnCode": 0, "rtnDesc": "success"}}',
    "GET https://connect-api.cloud.huawei.com/api/reviews/v1/manage/dev/reviews/1?appid=1&beginTime=1",
)

SUBMIT_PAGE = page(
    "Submitting Something",
    "POST",
    "https://connect-api.cloud.huawei.com/api/publish/v2/app-submit",
    '<div class="section"><h4>[h2]Query</h4>' + table(("appId", "Yes", "String", "App ID.")) + "</div>"
    + '<div class="section"><h4>[h2]Body</h4>'
    + table(("requestId", "Yes", "String(64)", "Request ID."),
            ("info", "No", "PhasedReleaseInfo", "Phased release."),
            ("parts", "No", "HashMap&lt;String, FilePartInfo&gt;", "Chunks."))
    + "<p>PhasedReleaseInfo</p>"
    + table(("state", "No", "String", "State."))
    + "</div>",
    table(("ret", "Yes", "String(100)", 'Result in {"code":retcode, "msg": "description"} format.')),
)

MODEL_PAGE = """<h1>ProductPriceInfo</h1><div id="body1">
<div class="tablenoborder"><table><thead><tr><th>Parameter</th><th>Mandatory</th><th>Type</th><th>Description</th></tr></thead>
<tbody><tr><td>country</td><td>Yes</td><td>String</td><td>Country code.</td></tr>
<tr><td>price</td><td>Yes</td><td>String</td><td>Price, in cents.</td></tr></tbody></table></div></div>"""

CONFIRM_PAGE = page(
    "API for Confirming the Purchase for the Order Service",
    "POST",
    "{rootUrl}/applications/v2/purchases/confirm</p><p>NOTE: The value of rootUrl varies depending on the site.",
    '<div class="section"><h4>Request Header</h4>'
    + table(("Content-Type", "Yes", "String", "application/json."), ("Authorization", "Yes", "String", "Token."))
    + '</div><div class="section"><h4>Request Body</h4>'
    + table(("purchaseToken", "Yes", "String", "Purchase token."), ("productId", "Yes", "String", "Product ID."))
    + "</div>",
    table(("responseCode", "Yes", "String", "Result code."), ("responseMessage", "No", "String", "Description.")),
    '{\n "responseCode": "0"\n "responseMessage": "consume success"\n}',
)

UPLOAD_PAGE = page(
    "Uploading a File",
    "PUT",
    "URL obtained in Obtaining the File Upload URL.",
    '<div class="section"><h4>[h2]Header</h4>' + table(("x-amz-date", "Yes", "String", "Date.")) + "</div>",
    "<p>If the API returns the HTTP status code 200, the file is successfully uploaded.</p>",
)

CODES_PAGE = "<h1>Result Codes</h1><p>Codes of the App Linking REST API.</p>"


class NormalizationTest(unittest.TestCase):
    def test_markup_attributes_do_not_change_hash(self):
        first = '<h1 id="a1">Title</h1><p class="x" id="p1">Some   text&nbsp;here.</p>' + table(("a", "Yes", "String", "A."))
        second = ('<a name="anchor"></a><h1>Title</h1>\n<div class="section" id="other"><p id="p777">Some text here.</p>'
                  + table(("a", "Yes", "String", "A.")).replace('width="20%"', 'width="33%"').replace('id="t1"', 'id="t9"')
                  + "</div>")
        self.assertEqual(update_spec.normalized_text(first), update_spec.normalized_text(second))
        self.assertEqual(update_spec.page_hash(first), update_spec.page_hash(second))

    def test_text_change_changes_hash(self):
        first = "<h1>Title</h1>" + table(("a", "Yes", "String", "A."))
        second = "<h1>Title</h1>" + table(("a", "No", "String", "A."))
        self.assertNotEqual(update_spec.page_hash(first), update_spec.page_hash(second))

    def test_normalized_text_layout(self):
        text = update_spec.normalized_text(
            "<h1>Title</h1><h4>[h2]Query</h4><p>One <strong>two</strong></p>"
            + table(("a", "Yes", "String", "A.")) + "<pre>{\n  \"a\": 1\n}</pre><script>ignored()</script>"
        )
        self.assertEqual(
            text.split("\n"),
            ["# Title", "# Query", "One two", "Parameter | Mandatory | Type | Description",
             "a | Yes | String | A.", "{", '  "a": 1', "}"],
        )


class TypeTest(unittest.TestCase):
    def test_types(self):
        cases = {
            "String": {"type": "string"},
            "String(256)": {"type": "string", "maxLength": 256},
            "Integer(64)": {"type": "integer"},
            "Long": {"type": "integer"},
            "boolean": {"type": "boolean"},
            "String(header)": {"type": "string"},
            "List<String>": {"type": "array", "items": {"type": "string"}},
            "List<String(32)>": {"type": "array", "items": {"type": "string", "maxLength": 32}},
            "List<ProductInfo>": {"type": "array", "items": {"model": "ProductInfo"}},
            "ServiceInfo[]": {"type": "array", "items": {"model": "ServiceInfo"}},
            "ProductInfo": {"model": "ProductInfo"},
            "Map<String,String>": {"type": "object", "additionalProperties": {"type": "string"}},
            "HashMap<String, FilePartInfo>": {"type": "object", "additionalProperties": {"model": "FilePartInfo"}},
        }
        for raw, expected in cases.items():
            with self.subTest(raw=raw):
                self.assertEqual(update_spec.parse_type(raw), expected)

    def test_unknown_type_is_kept_for_review(self):
        self.assertEqual(update_spec.parse_type("JSON string"), {"type": "string", "raw": "JSON string"})


class PageTest(unittest.TestCase):
    def test_operation_with_header_parameter_and_pms_envelope(self):
        draft = update_spec.parse_page(PRODUCT_PAGE)
        self.assertEqual(draft["title"], "Querying Details of a Product")
        self.assertEqual(draft["function"], "This API is used to do something.")
        self.assertEqual(draft["method"], "GET")
        self.assertEqual(draft["path"], "/api/pms/product-price-service/v1/manage/product")
        self.assertEqual(draft["urlQueryNames"], ["productNo"])
        # Заголовки авторизации обоих режимов в параметры не попадают.
        self.assertEqual([item["name"] for item in draft["parameters"]["header"]], ["appId"])
        self.assertEqual(draft["authHeaders"], ["client_id", "Authorization", "teamId", "oauth2Token"])
        self.assertEqual(draft["authModes"], ["api-client", "oauth-client"])
        query = draft["parameters"]["query"]
        self.assertEqual([(item["name"], item["required"]) for item in query], [("productNo", True), ("country", False)])
        self.assertEqual(query[0]["schema"], {"type": "string", "maxLength": 148})
        self.assertEqual([item["name"] for item in draft["response"]], ["error", "product"])
        self.assertEqual(
            {key: draft["result"][key] for key in ("field", "codeField", "messageField")},
            {"field": "error", "codeField": "errorCode", "messageField": "errorMsg"},
        )
        self.assertEqual(draft["notes"], [])

    def test_path_parameter_comments_envelope_and_spelling_note(self):
        draft = update_spec.parse_page(REVIEWS_PAGE)
        # В ячейке URL после адреса идут пояснения про домены: берётся только адрес.
        self.assertEqual(draft["path"], "/api/reviews/v1/manage/dev/reviews/{reviewId}")
        self.assertEqual([item["name"] for item in draft["parameters"]["path"]], ["reviewId"])
        self.assertTrue(draft["parameters"]["path"][0]["required"])
        self.assertEqual([item["name"] for item in draft["parameters"]["query"]],
                         ["appId", "beginTime", "filterCondition"])
        self.assertEqual(draft["parameters"]["query"][2]["schema"], {"type": "array", "items": {"type": "string"}})
        self.assertEqual(draft["authModes"], ["api-client"])
        self.assertEqual(draft["result"]["codeField"], "rtnCode")
        self.assertEqual(draft["result"]["messageField"], "rtnDesc")
        self.assertIn("path parameter reviewId was listed in the Query table", draft["notes"])
        self.assertIn("query parameter appId is spelled differently in the request example", draft["notes"])

    def test_body_nested_model_and_envelope_from_table(self):
        draft = update_spec.parse_page(SUBMIT_PAGE)
        self.assertEqual(draft["method"], "POST")
        self.assertEqual([item["name"] for item in draft["parameters"]["body"]], ["requestId", "info", "parts"])
        self.assertEqual(draft["parameters"]["body"][1]["schema"], {"model": "PhasedReleaseInfo"})
        self.assertEqual(draft["parameters"]["body"][2]["schema"],
                         {"type": "object", "additionalProperties": {"model": "FilePartInfo"}})
        self.assertEqual(len(draft["nestedTables"]), 1)
        self.assertEqual(draft["nestedTables"][0]["in"], "body")
        self.assertEqual(draft["nestedTables"][0]["caption"], "PhasedReleaseInfo")
        self.assertEqual(draft["nestedTables"][0]["fields"][0]["name"], "state")
        # Примера ответа нет: конверт берётся из таблицы параметров ответа.
        self.assertEqual(draft["result"]["field"], "ret")
        self.assertEqual(draft["result"]["codeField"], "code")
        self.assertEqual(draft["result"]["source"], "table")

    def test_data_model_page(self):
        draft = update_spec.parse_page(MODEL_PAGE)
        self.assertIsNone(draft["method"])
        self.assertIsNone(draft["path"])
        self.assertEqual([(item["name"], item["required"]) for item in draft["fields"]],
                         [("country", True), ("price", True)])

    def test_upload_to_returned_url_has_no_path(self):
        draft = update_spec.parse_page(UPLOAD_PAGE)
        self.assertEqual(draft["method"], "PUT")
        self.assertIsNone(draft["path"])
        self.assertEqual([item["name"] for item in draft["parameters"]["header"]], ["x-amz-date"])

    def test_split_url(self):
        self.assertEqual(
            update_spec.split_url("https://connect-api.cloud.huawei.com/api/a/{appId}/b?x={x}&y=1 Domain name: host"),
            ("/api/a/{appId}/b", ["appId"], ["x", "y"]),
        )
        self.assertEqual(update_spec.split_url("https://{domain}/api/oauth2/v1/token"), ("/api/oauth2/v1/token", [], []))
        self.assertEqual(update_spec.split_url("URL returned by another API."), (None, [], []))
        self.assertEqual(
            update_spec.split_url("{rootUrl}/sub/applications/v2/purchases/get NOTE: site."),
            ("/sub/applications/v2/purchases/get", [], []),
        )

    def test_iap_operation_with_root_url_and_root_envelope(self):
        draft = update_spec.parse_page(CONFIRM_PAGE)
        self.assertEqual((draft["method"], draft["path"]), ("POST", "/applications/v2/purchases/confirm"))
        self.assertEqual(draft["parameters"]["path"], [])
        self.assertEqual([item["name"] for item in draft["parameters"]["body"]], ["purchaseToken", "productId"])
        # Пример ответа без запятой — не JSON: конверт находится по тексту.
        self.assertEqual(
            {key: draft["result"][key] for key in ("field", "codeField", "messageField", "source")},
            {"field": "", "codeField": "responseCode", "messageField": "responseMessage", "source": "example-text"},
        )
        self.assertEqual(
            update_spec.detect_envelope([], '{"responseCode": "0"}')["source"], "example"
        )
        self.assertEqual(
            update_spec.detect_envelope([{"name": "responseCode"}], "")["field"], ""
        )

    def test_envelope_from_broken_example(self):
        result = update_spec.detect_envelope([], '{"ret": {"code": 0, "msg": "success" } "list": [ ... ] }')
        self.assertEqual((result["field"], result["codeField"], result["source"]), ("ret", "code", "example-text"))
        self.assertIsNone(update_spec.detect_envelope([{"name": "fileURL"}], ""))


# Каталог HMS Core: узел «Server» раздела In-App Purchases.
IAP_TREE = [{"nodeName": "In-App Purchases", "children": [{"nodeName": "Server", "children": [
    {"nodeName": "API for Confirming the Purchase for the Order Service", "relateDocument": "iap-1"}]}]}]
# Каталог HarmonyOS: узлы «REST API» есть и у других Kit, корень — путь «IAP Kit / REST API».
KIT_TREE = [
    {"nodeName": "Push Kit", "children": [{"nodeName": "REST API", "children": [
        {"nodeName": "Sending Messages", "relateDocument": "push-1"}]}]},
    {"nodeName": "IAP Kit", "children": [{"nodeName": "REST API", "children": [
        {"nodeName": "Overview", "relateDocument": "kit-1"}]}]},
]
KIT_PAGE = "<h1>Overview</h1><p>Site information.</p>"

TREE = [
    {"nodeName": "Other Service", "children": [{"nodeName": "Overview", "relateDocument": "other-1"}]},
    {
        "nodeName": "Connect API",
        "relateDocument": None,
        "children": [
            {"nodeName": "Obtaining a Token", "relateDocument": "token-1"},
            {
                "nodeName": "PMS API",
                "relateDocument": "pms-0",
                "children": [
                    {"nodeName": "Querying Details of a Product", "relateDocument": "pms-1"},
                    {"nodeName": "Data Models", "relateDocument": None,
                     "children": [{"nodeName": "ProductPriceInfo", "relateDocument": "pms-2"}]},
                ],
            },
            {"nodeName": "Something New", "relateDocument": "new-1"},
        ],
    },
    # Узел «REST API» есть в разных разделах: раздел задаётся путём.
    {"nodeName": "Auth Service", "children": [{"nodeName": "REST API", "children": [
        {"nodeName": "Registering a User", "relateDocument": "auth-1"},
        {"nodeName": "Signing Out a User", "relateDocument": "auth-2"},
        {"nodeName": "Result Codes", "relateDocument": "auth-3"},
    ]}]},
    {"nodeName": "Remote Configuration", "children": [{"nodeName": "Server", "children": [
        {"nodeName": "REST API", "relateDocument": None, "children": [
            {"nodeName": "Querying Configuration Information", "relateDocument": "rc-1"},
            {"nodeName": "Data Models", "relateDocument": None,
             "children": [{"nodeName": "ConfigItem", "relateDocument": "rc-2"}]},
        ]},
    ]}]},
    # Раздел App Linking: узлы «Server» и «REST» есть и у других сервисов.
    {"nodeName": "Cloud DB", "children": [
        {"nodeName": "Server", "children": [
            {"nodeName": "REST", "children": [{"nodeName": "Querying Data", "relateDocument": "cdb-1"}]}]}]},
    {"nodeName": "App Linking", "children": [
        {"nodeName": "Android", "relateDocument": "al-android",
         "children": [{"nodeName": "AppLinking", "relateDocument": "al-sdk"}]},
        {"nodeName": "Server", "relateDocument": "al-server", "children": [
            {"nodeName": "REST", "relateDocument": "al-rest", "children": [
                {"nodeName": "Result Codes", "relateDocument": "link-1"}]}]}]},
]
# Страницы раздела Remote Configuration из TREE.
RC_PAGES = {"rc-1": "<h1>Remote Configuration</h1><p>Overview.</p>", "rc-2": "<h1>ConfigItem</h1><p>Model.</p>"}


class TreeTest(unittest.TestCase):
    def test_pages_and_groups(self):
        pages = update_spec.collect_pages(update_spec.SOURCES[0], TREE)
        self.assertEqual(
            [(item.id, item.group) for item in pages],
            [("token-1", "auth"), ("pms-0", "pms"), ("pms-1", "pms"), ("pms-2", "pms"), ("new-1", "other")],
        )
        self.assertEqual(pages[3].path, ("PMS API", "Data Models", "ProductPriceInfo"))
        self.assertTrue(pages[0].url.endswith("/AppGallery-connect-References/token-1"))

    def test_root_with_children_wins(self):
        nodes = [{"nodeName": "AppGallery Connect API", "relateDocument": "overview"},
                 {"nodeName": "Help", "children": [
                     {"nodeName": "AppGallery Connect API", "relateDocument": "x",
                      "children": [{"nodeName": "Provisioning API Reference", "relateDocument": "prov-0"}]}]}]
        pages = update_spec.collect_pages(update_spec.SOURCES[1], nodes)
        self.assertEqual([(item.id, item.group) for item in pages], [("prov-0", "provisioning")])

    def test_root_path_selects_the_section(self):
        pages = update_spec.collect_pages(update_spec.SOURCES[2], TREE)
        self.assertEqual([(item.id, item.group) for item in pages], [("rc-1", "remoteconfig"), ("rc-2", "remoteconfig")])
        self.assertEqual(pages[1].path, ("Data Models", "ConfigItem"))
        self.assertIsNone(update_spec.find_root(TREE, "Remote Configuration / Android / REST API"))
        # Без пути выбирается узел с большим числом дочерних страниц — чужой раздел.
        self.assertEqual(update_spec.find_root(TREE, "REST API")["children"][0]["relateDocument"], "auth-1")

    def test_missing_root_is_reference_change(self):
        with self.assertRaises(update_spec.ReferenceChanged):
            update_spec.collect_pages(update_spec.SOURCES[0], [{"nodeName": "Other"}])

    def test_root_path_selects_nested_node(self):
        source = next(item for item in update_spec.SOURCES if item.default_group == "applinking")
        self.assertEqual(source.root, "App Linking / Server / REST")
        pages = update_spec.collect_pages(source, TREE)
        # Только страницы под «App Linking / Server / REST»: без SDK, без самого узла REST и
        # без одноимённого узла Cloud DB.
        self.assertEqual([(item.id, item.group, item.path) for item in pages],
                         [("link-1", "applinking", ("Result Codes",))])
        self.assertTrue(pages[0].url.endswith("/AppGallery-connect-References/link-1"))
        broken = json.loads(json.dumps(TREE))
        broken[-1]["children"][1]["nodeName"] = "Server SDK"
        with self.assertRaises(update_spec.ReferenceChanged) as caught:
            update_spec.collect_pages(source, broken)
        self.assertIn("App Linking / Server / REST", str(caught.exception))


def lock(*pages: tuple[str, str, str, str]) -> dict:
    return {"version": 1, "pages": [
        {"id": page_id, "title": title, "group": group, "catalog": "AppGallery-connect-References", "sha256": digest}
        for page_id, title, group, digest in pages
    ]}


class LockTest(unittest.TestCase):
    def test_no_changes(self):
        saved = lock(("a", "A", "pms", "1"), ("b", "B", "pms", "2"))
        diff = update_spec.compare_locks(saved, lock(("b", "B", "pms", "2"), ("a", "A", "pms", "1")))
        self.assertTrue(diff.is_empty())
        self.assertEqual(update_spec.format_diff(diff), "Справочник не изменился.\n")

    def test_changed_added_removed_moved(self):
        saved = lock(("a", "A", "pms", "1"), ("b", "B", "pms", "2"), ("c", "C", "pms", "3"), ("d", "D", "pms", "4"))
        fresh = lock(("a", "A", "pms", "1x"), ("c", "C renamed", "pms", "3"), ("d", "D", "pms", "4"), ("e", "E", "upload", "5"))
        diff = update_spec.compare_locks(saved, fresh)
        self.assertEqual([item["id"] for item in diff.changed], ["a"])
        self.assertEqual([item["id"] for item in diff.added], ["e"])
        self.assertEqual([item["id"] for item in diff.removed], ["b"])
        self.assertEqual([(old["title"], new["title"]) for old, new in diff.moved], [("C", "C renamed")])
        report = update_spec.format_diff(diff)
        self.assertIn("Изменённые страницы (1):", report)
        self.assertIn("Новые страницы (1):", report)
        self.assertIn("Пропавшие страницы (1):", report)
        self.assertIn("[upload] E", report)
        self.assertIn("https://developer.huawei.com/consumer/en/doc/development/AppGallery-connect-References/a", report)

    def test_dump_is_stable(self):
        data = lock(("a", "A", "pms", "1"))
        text = update_spec.dump_lock(data)
        self.assertTrue(text.endswith("}\n"))
        self.assertEqual(json.loads(text), data)


class CommandTest(unittest.TestCase):
    """Режимы скрипта на каталоге с сохранёнными страницами (`--from-dir`), без сети."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        (self.dir / "pages").mkdir()
        self.write_tree(TREE)
        (self.dir / "tree.app.json").write_text(json.dumps({"value": {"catalogTreeList": [
            {"nodeName": "AppGallery Connect API", "children": [
                {"nodeName": "Upload Management API Reference", "relateDocument": "upload-1"}]}]}}))
        (self.dir / "tree.HMSCore-References.json").write_text(
            json.dumps({"value": {"catalogTreeList": IAP_TREE}}))
        (self.dir / "tree.harmonyos-references.json").write_text(
            json.dumps({"value": {"catalogTreeList": KIT_TREE}}))
        for page_id, html in {"token-1": SUBMIT_PAGE, "pms-0": "<h1>PMS API</h1><p>Overview.</p>",
                              "pms-1": PRODUCT_PAGE, "pms-2": MODEL_PAGE, "new-1": REVIEWS_PAGE,
                              "upload-1": UPLOAD_PAGE, **RC_PAGES, "link-1": CODES_PAGE,
                              "iap-1": CONFIRM_PAGE, "kit-1": KIT_PAGE}.items():
            self.write_page(page_id, html)
        self.lock_file = self.dir / "pages.lock.json"

    def write_tree(self, tree):
        (self.dir / "tree.AppGallery-connect-References.json").write_text(
            json.dumps({"value": {"catalogTreeList": tree}}))

    def write_page(self, page_id, html):
        (self.dir / "pages" / f"{page_id}.json").write_text(
            json.dumps({"value": {"title": page_id, "content": {"content": html}}}))

    def run_script(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = update_spec.main([*args, "--from-dir", str(self.dir), "--lock-file", str(self.lock_file), "--quiet"])
        return code, out.getvalue(), err.getvalue()

    def test_lock_then_check_is_clean(self):
        code, _, _ = self.run_script("--lock")
        self.assertEqual(code, 0)
        saved = json.loads(self.lock_file.read_text())
        self.assertEqual([item["id"] for item in saved["pages"]],
                         ["token-1", "pms-0", "pms-1", "pms-2", "new-1", "upload-1", "rc-1", "rc-2", "link-1",
                          "iap-1", "kit-1"])
        self.assertEqual(saved["pages"][9]["group"], "orders")
        self.assertEqual(saved["sources"][4]["catalog"], "HMSCore-References")
        self.assertEqual(saved["pages"][10]["group"], "iapkit")
        self.assertEqual(saved["sources"][5]["catalog"], "harmonyos-references")
        self.assertEqual(saved["pages"][2]["sha256"], update_spec.page_hash(PRODUCT_PAGE))
        code, out, _ = self.run_script("--check")
        self.assertEqual(code, 0)
        self.assertIn("не изменился", out)

    def test_check_reports_changes_and_does_not_write(self):
        self.run_script("--lock")
        before = self.lock_file.read_text()
        self.write_page("pms-1", PRODUCT_PAGE.replace("Country code.", "Country or region code."))
        tree = json.loads(json.dumps(TREE))
        tree[1]["children"].pop()  # страница new-1 пропала
        tree[1]["children"].append({"nodeName": "Brand New", "relateDocument": "new-2"})
        self.write_tree(tree)
        self.write_page("new-2", "<h1>Brand New</h1>")
        code, out, _ = self.run_script("--check")
        self.assertEqual(code, 3)
        self.assertIn("Справочник изменился: изменено 1, новых 1, пропало 1", out)
        self.assertIn("Изменённые страницы (1):", out)
        self.assertIn("Querying Details of a Product", out)
        self.assertIn("Новые страницы (1):", out)
        self.assertIn("Пропавшие страницы (1):", out)
        self.assertEqual(self.lock_file.read_text(), before)

    def test_cosmetic_change_is_not_reported(self):
        self.run_script("--lock")
        self.write_page("pms-1", PRODUCT_PAGE.replace('width="20%"', 'width="25%"').replace('id="s1"', 'id="s100"'))
        code, _, _ = self.run_script("--check")
        self.assertEqual(code, 0)

    def test_missing_saved_page_is_exit_code_1(self):
        self.run_script("--lock")
        (self.dir / "pages" / "pms-2.json").unlink()
        code, _, err = self.run_script("--check")
        self.assertEqual(code, 1)
        self.assertIn("pms-2.json", err)

    def test_saved_page_without_content_is_exit_code_1(self):
        (self.dir / "pages" / "pms-2.json").write_text(json.dumps({"code": 500, "value": None}))
        code, _, err = self.run_script("--lock")
        self.assertEqual(code, 1)
        self.assertIn("value.content.content", err)
        self.assertFalse(self.lock_file.exists())

    def test_missing_lock_is_exit_code_1(self):
        code, _, _ = self.run_script("--check")
        self.assertEqual(code, 1)

    def test_missing_root_is_exit_code_3(self):
        self.run_script("--lock")
        self.write_tree([{"nodeName": "Renamed API", "children": TREE[0]["children"]}])
        code, out, _ = self.run_script("--check")
        self.assertEqual(code, 3)
        self.assertIn("нет узла «Connect API»", out)

    def test_bad_arguments_are_exit_code_2(self):
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(update_spec.main(["--check", "--lock"]), 2)

    def test_draft(self):
        target = self.dir / "out"
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = update_spec.main(["--draft", str(target), "--from-dir", str(self.dir), "--quiet"])
        self.assertEqual(code, 0)
        draft = json.loads((target / "draft.json").read_text())
        self.assertEqual([(item["method"], item["path"], item["group"]) for item in draft["operations"]],
                         [("POST", "/api/publish/v2/app-submit", "auth"),
                          ("GET", "/api/pms/product-price-service/v1/manage/product", "pms"),
                          ("GET", "/api/reviews/v1/manage/dev/reviews/{reviewId}", "other"),
                          ("POST", "/applications/v2/purchases/confirm", "orders")])
        self.assertEqual([item["title"] for item in draft["external"]], ["Uploading a File"])
        self.assertEqual([item["title"] for item in draft["models"]], ["ProductPriceInfo"])
        self.assertEqual([item["id"] for item in draft["other"]], ["pms-0", "rc-1", "rc-2", "link-1", "kit-1"])
        self.assertTrue(draft["operations"][1]["reference"].endswith("/pms-1"))


class FakeClock:
    """Часы и пауза: `sleep` только сдвигает время и запоминает паузы."""

    def __init__(self):
        self.now = 1000.0
        self.sleeps: list[float] = []

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


class FakeResponse:
    def __init__(self, data: bytes):
        self._data = data

    def read(self):
        return self._data

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def http_error(code: int, retry_after: str | None = None) -> urllib.error.HTTPError:
    headers = email.message.Message()
    if retry_after is not None:
        headers["Retry-After"] = retry_after
    return urllib.error.HTTPError("https://portal.test", code, "error", headers, io.BytesIO(b""))


class FakePortal:
    """Портал документации в памяти: дерево по catalogName, страницы по objectId.

    `failures[key]` — список исключений или байтов, которые отдаются до настоящего ответа
    (ключ — имя каталога или id страницы, `"*"` — для всех запросов).
    """

    def __init__(self, clock: FakeClock, trees: dict, pages: dict):
        self.clock = clock
        self.trees = trees
        self.pages = pages
        self.failures: dict[str, list] = {}
        self.down = False
        self.requests: list[str] = []
        self.request_cost = 1.0

    def urlopen(self, request, timeout):
        self.clock.now += self.request_cost
        payload = json.loads(request.data.decode("utf-8"))
        key = payload.get("catalogName") or payload["objectId"]
        self.requests.append(key)
        if self.down:
            raise urllib.error.URLError("Temporary failure in name resolution")
        queue = self.failures.get(key) or self.failures.get("*") or []
        if queue:
            failure = queue.pop(0)
            if isinstance(failure, BaseException):
                raise failure
            return FakeResponse(failure)
        if "catalogName" in payload:
            body = {"value": {"catalogTreeList": self.trees[key]}}
        else:
            body = {"value": {"title": key, "content": {"content": self.pages[key]}}}
        return FakeResponse(json.dumps(body).encode("utf-8"))


APP_TREE = [{"nodeName": "AppGallery Connect API", "children": [
    {"nodeName": "Upload Management API Reference", "relateDocument": "upload-1"}]}]
PAGES = {"token-1": SUBMIT_PAGE, "pms-0": "<h1>PMS API</h1><p>Overview.</p>", "pms-1": PRODUCT_PAGE,
         "pms-2": MODEL_PAGE, "new-1": REVIEWS_PAGE, "upload-1": UPLOAD_PAGE, **RC_PAGES, "link-1": CODES_PAGE,
         "iap-1": CONFIRM_PAGE, "kit-1": KIT_PAGE}


class NetworkTest(unittest.TestCase):
    """Загрузка из сети с подменённым urlopen: повторы, лимит времени, коды выхода."""

    def setUp(self):
        self.clock = FakeClock()
        self.portal = FakePortal(
            self.clock,
            {"AppGallery-connect-References": json.loads(json.dumps(TREE)), "app": APP_TREE,
             "HMSCore-References": IAP_TREE, "harmonyos-references": KIT_TREE},
            dict(PAGES),
        )
        for name, value in (("_urlopen", self.portal.urlopen), ("_sleep", self.clock.sleep),
                            ("_now", self.clock.monotonic)):
            patcher = mock.patch.object(update_spec, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.lock_file = Path(self.tmp.name) / "pages.lock.json"

    def post(self, key="pms-1", deadline=None):
        return update_spec.post_json(update_spec.PAGE_URL, {"language": "en", "objectId": key},
                                     deadline, update_spec._page_problem)

    def run_script(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = update_spec.main([*args, "--lock-file", str(self.lock_file), "--quiet"])
        return code, out.getvalue(), err.getvalue()

    def pauses(self):
        """Паузы повторов без коротких пауз между запросами."""
        return [seconds for seconds in self.clock.sleeps if seconds >= 1]

    def test_retry_after_server_error(self):
        self.portal.failures["pms-1"] = [http_error(503), http_error(502)]
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            data = self.post()
        self.assertEqual(data["value"]["title"], "pms-1")
        self.assertEqual(self.clock.sleeps, [5, 15])
        self.assertIn("HTTP 503; попытка 1 из 4, повтор через 5 с", err.getvalue())

    def test_retry_after_header_is_respected_and_capped(self):
        self.portal.failures["pms-1"] = [http_error(429, "20"), http_error(429, "3600")]
        with contextlib.redirect_stderr(io.StringIO()):
            self.post()
        self.assertEqual(self.clock.sleeps, [20, 60])

    def test_response_without_content_is_retried(self):
        self.portal.failures["pms-1"] = [b'{"code": 500, "value": null}', b"<html>busy</html>",
                                         b'{"value": "busy"}']
        with contextlib.redirect_stderr(io.StringIO()):
            data = self.post()
        self.assertIn("content", data["value"])
        self.assertEqual(len(self.portal.requests), 4)

    def test_network_failure_after_all_attempts(self):
        self.portal.down = True
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(update_spec.PortalError) as caught:
                self.post()
        self.assertEqual(self.clock.sleeps, [5, 15, 30])
        self.assertEqual(len(self.portal.requests), 4)
        self.assertIn("name resolution", str(caught.exception))
        self.assertIn("попыток: 4", str(caught.exception))

    def test_client_error_is_not_retried(self):
        self.portal.failures["pms-1"] = [http_error(404)]
        with self.assertRaises(update_spec.ScriptError):
            self.post()
        self.assertEqual(self.clock.sleeps, [])

    def test_deadline_stops_retries(self):
        self.portal.down = True
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(update_spec.PortalError) as caught:
                self.post(deadline=self.clock.now + 10)
        self.assertEqual(self.clock.sleeps, [5])
        self.assertIn("лимита времени", str(caught.exception))

    def test_check_clean(self):
        self.assertEqual(self.run_script("--lock")[0], 0)
        code, out, err = self.run_script("--check")
        self.assertEqual(code, 0)
        self.assertIn("не изменился", out)
        self.assertIn("страниц: 11", err)
        # Дерево каталога скачивается один раз за прогон, хотя разделов в нём два.
        self.assertEqual(self.portal.requests.count("AppGallery-connect-References"), 2)

    def test_check_survives_transient_failures(self):
        self.run_script("--lock")
        self.portal.failures["app"] = [urllib.error.URLError("timed out")]
        self.portal.failures["pms-2"] = [http_error(500), TimeoutError("read timed out")]
        code, _, err = self.run_script("--check")
        self.assertEqual(code, 0)
        self.assertEqual(err.count("повтор через"), 3)

    def test_changed_page_is_exit_code_3_with_report(self):
        self.run_script("--lock")
        self.portal.pages["pms-1"] = PRODUCT_PAGE.replace("Country code.", "Country or region code.")
        self.portal.trees["AppGallery-connect-References"][1]["children"].append(
            {"nodeName": "Brand New", "relateDocument": "new-2"})
        self.portal.pages["new-2"] = "<h1>Brand New</h1>"
        self.portal.trees["app"] = [{"nodeName": "AppGallery Connect API", "children": []}]
        code, out, _ = self.run_script("--check")
        self.assertEqual(code, 3)
        self.assertIn("Справочник изменился: изменено 1, новых 1, пропало 1", out)
        self.assertIn("Пропавшие страницы (1):\n  [upload] Upload Management API Reference\n"
                      "      https://developer.huawei.com/consumer/en/doc/app/upload-1", out)
        self.assertIn("Изменённые страницы (1):\n  [pms] Querying Details of a Product\n"
                      "      https://developer.huawei.com/consumer/en/doc/development/"
                      "AppGallery-connect-References/pms-1", out)
        self.assertIn("Новые страницы (1):\n  [other] Brand New\n", out)

    def test_portal_down_is_exit_code_4(self):
        self.run_script("--lock")
        before = self.lock_file.read_text()
        self.portal.down = True
        code, out, err = self.run_script("--check")
        self.assertEqual(code, 4)
        self.assertEqual(out, "")
        self.assertIn("портал документации недоступен", err)
        self.assertIn("Это не изменение справочника", err)
        # Портал недоступен с первого запроса: одна серия повторов, а не по серии на страницу.
        self.assertEqual(self.pauses(), [5, 15, 30])
        self.assertEqual(self.lock_file.read_text(), before)

    def test_time_limit_is_exit_code_4(self):
        self.run_script("--lock")
        self.portal.request_cost = 30.0
        self.portal.requests.clear()
        code, _, err = self.run_script("--check", "--time-limit", "100")
        self.assertEqual(code, 4)
        self.assertIn("лимит времени", err)
        # 10 запросов по 30 с в 100 с не помещаются: проверка останавливается на четвёртом.
        self.assertEqual(len(self.portal.requests), 4)

    def test_time_limit_zero_disables_limit(self):
        self.run_script("--lock")
        self.portal.request_cost = 600.0
        code, _, _ = self.run_script("--check", "--time-limit", "0")
        self.assertEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
