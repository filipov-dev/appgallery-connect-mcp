"""Офлайн-тесты update_spec.py: разбор страниц, нормализация, сравнение списков страниц.

Сеть не нужна: страницы — небольшие выдуманные фрагменты HTML.

    python3 -I -B -m unittest discover -s crates/appgallery-connect-api/spec -p 'test_*.py'
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

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

UPLOAD_PAGE = page(
    "Uploading a File",
    "PUT",
    "URL obtained in Obtaining the File Upload URL.",
    '<div class="section"><h4>[h2]Header</h4>' + table(("x-amz-date", "Yes", "String", "Date.")) + "</div>",
    "<p>If the API returns the HTTP status code 200, the file is successfully uploaded.</p>",
)


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

    def test_envelope_from_broken_example(self):
        result = update_spec.detect_envelope([], '{"ret": {"code": 0, "msg": "success" } "list": [ ... ] }')
        self.assertEqual((result["field"], result["codeField"], result["source"]), ("ret", "code", "example-text"))
        self.assertIsNone(update_spec.detect_envelope([{"name": "fileURL"}], ""))


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
]


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

    def test_missing_root_is_portal_error(self):
        with self.assertRaises(update_spec.PortalError):
            update_spec.collect_pages(update_spec.SOURCES[0], [{"nodeName": "Other"}])


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
        for page_id, html in {"token-1": SUBMIT_PAGE, "pms-0": "<h1>PMS API</h1><p>Overview.</p>",
                              "pms-1": PRODUCT_PAGE, "pms-2": MODEL_PAGE, "new-1": REVIEWS_PAGE,
                              "upload-1": UPLOAD_PAGE}.items():
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
                         ["token-1", "pms-0", "pms-1", "pms-2", "new-1", "upload-1"])
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
        self.assertEqual(code, 1)
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

    def test_unavailable_page_is_exit_code_2(self):
        self.run_script("--lock")
        (self.dir / "pages" / "pms-2.json").unlink()
        code, _, err = self.run_script("--check")
        self.assertEqual(code, 2)
        self.assertIn("портал документации недоступен", err)

    def test_page_without_content_is_exit_code_2(self):
        (self.dir / "pages" / "pms-2.json").write_text(json.dumps({"code": 500, "value": None}))
        code, _, _ = self.run_script("--lock")
        self.assertEqual(code, 2)
        self.assertFalse(self.lock_file.exists())

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
                          ("GET", "/api/reviews/v1/manage/dev/reviews/{reviewId}", "other")])
        self.assertEqual([item["title"] for item in draft["external"]], ["Uploading a File"])
        self.assertEqual([item["title"] for item in draft["models"]], ["ProductPriceInfo"])
        self.assertEqual([item["id"] for item in draft["other"]], ["pms-0"])
        self.assertTrue(draft["operations"][1]["reference"].endswith("/pms-1"))


if __name__ == "__main__":
    unittest.main()
