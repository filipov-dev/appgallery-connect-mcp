"""Согласованность снимка `openapi.json`, дополнения `overlay.toml` и `pages.lock.json`.

Те же правила проверяет генератор при сборке; тест ловит ошибки раньше и без Rust.
Нужен Python 3.11+ (`tomllib`); на 3.10 тесты пропускаются.

    python3 -I -B -m unittest discover -s crates/appgallery-connect-api/spec -p 'test_*.py'
"""

from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10
    tomllib = None

SPEC_DIR = Path(__file__).resolve().parent

HTTP_METHODS = ("get", "post", "put", "patch", "delete")
AUTH_HEADERS = {"client_id", "authorization", "teamid", "oauth2token"}
ENVELOPES = {("ret", "code", "msg"), ("ret", "rtnCode", "rtnDesc"), ("error", "errorCode", "errorMsg")}
TOOLSETS = ["apps", "releases", "listing", "reviews", "reports", "products", "projects", "provisioning"]
DEFAULT_TOOLSETS = {"apps", "releases", "reviews"}
REFERENCE_PREFIXES = (
    "https://developer.huawei.com/consumer/en/doc/development/AppGallery-connect-References/",
    "https://developer.huawei.com/consumer/en/doc/app/",
)
OVERLAY_KEYS = {
    "": {"groups", "toolsets", "names", "access", "deprecated", "exclude", "secrets"},
    "groups": {"slug", "title", "reference"},
    "toolsets": {"slug", "description", "default", "operations"},
    "access": {"level", "reason", "operations"},
    "deprecated": {"operation", "replacement", "note"},
    "exclude": {"operations", "callable", "reason", "alternative"},
    "secrets": {"kind", "description", "hint", "operations", "fields"},
}
SECRET_KINDS = {"credential", "download-url", "file-content"}


def default_access(method: str) -> str:
    return {"get": "read", "delete": "destructive"}.get(method.lower(), "write")


@unittest.skipIf(tomllib is None, "нужен Python 3.11+ (tomllib)")
class SpecTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.spec = json.loads((SPEC_DIR / "openapi.json").read_text(encoding="utf-8"))
        cls.overlay = tomllib.loads((SPEC_DIR / "overlay.toml").read_text(encoding="utf-8"))
        cls.lock = json.loads((SPEC_DIR / "pages.lock.json").read_text(encoding="utf-8"))
        # operationId -> (метод, путь, операция); у внешних операций пути нет.
        cls.operations = {}
        for path, item in cls.spec["paths"].items():
            for method, operation in item.items():
                if method in HTTP_METHODS:
                    cls.operations[operation["operationId"]] = (method, path, operation)
        cls.external = cls.spec.get("x-agc-external-operations", {})
        cls.known = set(cls.operations) | set(cls.external)
        cls.excluded = {}
        for rule in cls.overlay.get("exclude", []):
            for operation_id in rule["operations"]:
                cls.excluded[operation_id] = rule

    # ----------------------------------------------------------------- снимок

    def test_openapi_version_and_single_server(self):
        self.assertRegex(self.spec["openapi"], r"^3\.0\.\d+$")
        self.assertEqual([server["url"] for server in self.spec["servers"]], ["https://connect-api.cloud.huawei.com"])

    def test_operations_are_described(self):
        count = 0
        for path, item in self.spec["paths"].items():
            self.assertTrue(path.startswith("/api/"), path)
            self.assertNotIn("?", path)
            for method, operation in item.items():
                self.assertIn(method, HTTP_METHODS, f"{path}: неизвестный ключ {method}")
                count += 1
                operation_id = operation["operationId"]
                with self.subTest(operation=operation_id):
                    self.assertRegex(operation_id, r"^[a-z]+\.[a-z][A-Za-z]*\.[a-z][A-Za-z]*$")
                    self.assertEqual(operation_id.split(".")[0], operation["x-agc-group"])
                    self.assertEqual(operation["tags"], [operation["x-agc-group"]])
                    summary = operation["summary"]
                    self.assertTrue(summary.endswith("."), summary)
                    self.assertLessEqual(len(summary), 160)
                    self.assertTrue(operation["description"].startswith(summary))
                    self.assertTrue(operation["x-agc-reference"].startswith(REFERENCE_PREFIXES))
                    self.assertIn("200", operation["responses"])
                    schema = operation["responses"]["200"]["content"]["application/json"]["schema"]
                    self.assertEqual(schema["type"], "object")
        self.assertEqual(count, len(self.operations), "operationId повторяется")

    def test_parameters(self):
        for operation_id, (_, path, operation) in self.operations.items():
            with self.subTest(operation=operation_id):
                names = set()
                declared_path = set()
                for parameter in operation.get("parameters", []):
                    name = parameter["name"]
                    self.assertNotIn(name, names, "имя параметра повторяется")
                    names.add(name)
                    self.assertNotEqual(name, "body", "имя body занято телом запроса")
                    self.assertIn(parameter["in"], ("path", "query", "header"))
                    self.assertNotIn(name.lower(), AUTH_HEADERS, "заголовки авторизации добавляет исполнитель")
                    self.assertIn(parameter["schema"]["type"], ("string", "integer", "number", "boolean", "array"))
                    self.assertIsInstance(parameter["required"], bool)
                    if parameter["in"] == "path":
                        self.assertTrue(parameter["required"])
                        declared_path.add(name)
                    if parameter["schema"]["type"] == "array":
                        self.assertEqual(parameter["in"], "query")
                        self.assertTrue(parameter.get("explode"))
                self.assertEqual(declared_path, set(re.findall(r"\{([^{}]+)\}", path)))

    def test_request_bodies_are_json_objects(self):
        for operation_id, (method, _, operation) in self.operations.items():
            body = operation.get("requestBody")
            if body is None:
                continue
            with self.subTest(operation=operation_id):
                self.assertNotEqual(method, "get")
                self.assertEqual(list(body["content"]), ["application/json"])
                schema = self.resolve(body["content"]["application/json"]["schema"])
                self.assertEqual(schema["type"], "object")
                for name in schema.get("required", []):
                    self.assertIn(name, schema["properties"])

    def test_result_envelopes(self):
        for operation_id, (_, _, operation) in self.operations.items():
            with self.subTest(operation=operation_id):
                result = operation.get("x-agc-result")
                self.assertIsNotNone(result, "у операции нет конверта результата")
                self.assertEqual(set(result), {"field", "codeField", "messageField"})
                self.assertIn((result["field"], result["codeField"], result["messageField"]), ENVELOPES)
                schema = operation["responses"]["200"]["content"]["application/json"]["schema"]
                envelope = self.resolve(schema["properties"][result["field"]])
                self.assertIn(result["codeField"], envelope["properties"])
                self.assertIn(result["messageField"], envelope["properties"])

    def test_auth_modes(self):
        modes = {"api-client", "oauth-client", "service-account"}
        for operation_id, (_, _, operation) in self.operations.items():
            with self.subTest(operation=operation_id):
                self.assertLessEqual(set(operation.get("x-agc-auth-modes", [])), modes)

    def test_references_resolve_and_components_are_used(self):
        used = set()

        def walk(node):
            if isinstance(node, dict):
                ref = node.get("$ref")
                if ref is not None:
                    self.assertTrue(ref.startswith("#/components/schemas/"), ref)
                    name = ref.rsplit("/", 1)[1]
                    self.assertIn(name, self.spec["components"]["schemas"], f"$ref {ref} не разрешается")
                    used.add(name)
                    self.assertLessEqual(set(node), {"$ref", "description"}, "рядом с $ref допустим только description")
                for value in node.values():
                    walk(value)
            elif isinstance(node, list):
                for value in node:
                    walk(value)

        walk(self.spec)
        self.assertEqual(set(self.spec["components"]["schemas"]) - used, set(), "схемы без ссылок")

    def test_external_operations(self):
        for operation_id, operation in self.external.items():
            with self.subTest(operation=operation_id):
                self.assertNotIn(operation_id, self.operations)
                self.assertIn(operation["method"], ("GET", "POST", "PUT", "PATCH", "DELETE"))
                self.assertTrue(operation["description"].startswith(operation["summary"]))
                self.assertIn(operation["x-agc-url-source"], self.operations)
                self.assertTrue(operation["x-agc-reference"].startswith(REFERENCE_PREFIXES))
                self.assertEqual(operation_id.split(".")[0], operation["x-agc-group"])

    def test_reference_pages_are_locked(self):
        locked = {page["id"] for page in self.lock["pages"]}
        self.assertEqual(len(locked), len(self.lock["pages"]), "страница в pages.lock.json повторяется")
        for page in self.lock["pages"]:
            self.assertRegex(page["sha256"], r"^[0-9a-f]{64}$")
        for operation_id, operation in [(key, value[2]) for key, value in self.operations.items()] + list(
            self.external.items()
        ):
            with self.subTest(operation=operation_id):
                self.assertIn(operation["x-agc-reference"].rsplit("/", 1)[1], locked)

    # ------------------------------------------------------------- дополнение

    def test_overlay_has_no_unknown_keys(self):
        self.assertLessEqual(set(self.overlay), OVERLAY_KEYS[""])
        for section in ("groups", "toolsets", "access", "deprecated", "exclude", "secrets"):
            for entry in self.overlay.get(section, []):
                self.assertLessEqual(set(entry), OVERLAY_KEYS[section], f"[[{section}]]")

    def test_groups(self):
        groups = [group["slug"] for group in self.overlay["groups"]]
        self.assertEqual(len(groups), len(set(groups)))
        used = {value[2]["x-agc-group"] for value in self.operations.values()}
        used |= {operation["x-agc-group"] for operation in self.external.values()}
        self.assertEqual(set(groups), used)
        self.assertEqual({tag["name"] for tag in self.spec["tags"]}, used)
        for group in self.overlay["groups"]:
            self.assertTrue(group["title"])
            self.assertTrue(group["reference"].startswith(REFERENCE_PREFIXES))

    def test_toolsets(self):
        self.assertEqual([toolset["slug"] for toolset in self.overlay["toolsets"]], TOOLSETS)
        self.assertEqual({t["slug"] for t in self.overlay["toolsets"] if t.get("default", False)}, DEFAULT_TOOLSETS)
        seen = {}
        for toolset in self.overlay["toolsets"]:
            self.assertTrue(toolset["description"].strip())
            for operation_id in toolset["operations"]:
                self.assertIn(operation_id, self.known, f"набор {toolset['slug']}: нет операции {operation_id}")
                self.assertNotIn(operation_id, seen, f"{operation_id} в двух наборах")
                seen[operation_id] = toolset["slug"]
        for operation_id in self.known - set(seen):
            rule = self.excluded.get(operation_id)
            self.assertIsNotNone(rule, f"{operation_id}: нет набора")
            self.assertFalse(rule.get("callable", True), f"{operation_id}: вне наборов только callable = false")

    def test_tool_names(self):
        names = self.overlay["names"]
        for operation_id in names:
            self.assertIn(operation_id, self.operations, f"[names]: нет операции {operation_id}")
            self.assertNotIn(operation_id, self.excluded, f"[names]: {operation_id} исключена")
        for operation_id in self.operations:
            if operation_id not in self.excluded:
                self.assertIn(operation_id, names, f"{operation_id}: нет имени инструмента")
        values = list(names.values())
        self.assertEqual(len(values), len(set(values)), "имена инструментов повторяются")
        for name in values:
            self.assertRegex(name, r"^[a-z]+(_[a-z0-9]+)+$")
            self.assertLessEqual(len(name), 40, name)

    def test_access_rules(self):
        seen = set()
        for rule in self.overlay.get("access", []):
            self.assertIn(rule["level"], ("read", "write", "destructive"))
            self.assertTrue(rule["reason"].strip())
            for operation_id in rule["operations"]:
                with self.subTest(operation=operation_id):
                    self.assertIn(operation_id, self.operations)
                    self.assertNotIn(operation_id, seen, "операция указана дважды")
                    seen.add(operation_id)
                    self.assertNotEqual(rule["level"], default_access(self.operations[operation_id][0]),
                                        "исключение совпадает с уровнем по умолчанию")

    def test_levels_of_sensitive_operations(self):
        levels = {operation_id: default_access(method) for operation_id, (method, _, _) in self.operations.items()}
        for rule in self.overlay.get("access", []):
            for operation_id in rule["operations"]:
                levels[operation_id] = rule["level"]
        destructive = {
            "publishing.app.submit", "publishing.app.submitWithFile", "publishing.phasedRelease.update",
            "publishing.releaseTime.update", "comments.review.reply", "pms.product.update",
            "pms.product.batchUpdate", "pms.product.batchActivate", "pms.product.batchDeactivate",
            "pms.promotion.create", "pms.promotion.update", "pms.promotion.batchActivate",
            "pms.promotion.batchDeactivate", "provisioning.cert.delete", "provisioning.device.delete",
            "provisioning.profile.delete", "publishing.languageInfo.delete",
        }
        self.assertEqual({key for key, level in levels.items() if level == "destructive"}, destructive)
        for operation_id in ("pms.product.search", "pms.promotion.search", "pms.productGroup.list",
                             "provisioning.cert.list", "comments.review.list"):
            self.assertEqual(levels[operation_id], "read", operation_id)
        for operation_id in ("publishing.appInfo.update", "publishing.languageInfo.update",
                             "publishing.fileInfo.update", "upload.uploadUrl.get"):
            self.assertEqual(levels[operation_id], "write", operation_id)

    def test_excluded_operations(self):
        seen = set()
        for rule in self.overlay.get("exclude", []):
            self.assertTrue(rule["reason"].strip())
            self.assertTrue(rule["alternative"].strip())
            self.assertIsInstance(rule.get("callable", True), bool)
            for operation_id in rule["operations"]:
                self.assertIn(operation_id, self.known, f"[[exclude]]: нет операции {operation_id}")
                self.assertNotIn(operation_id, seen)
                seen.add(operation_id)
        # Получение токена, шаги загрузки (их ответы — одноразовые адреса и заголовки
        # загрузки) и отправка файла в хранилище не вызываются и через api_request.
        token = [key for key, (_, path, _) in self.operations.items() if path == "/api/oauth2/v1/token"]
        self.assertEqual(token, ["auth.token.create"])
        upload_steps = [key for key in self.operations if key.startswith("upload.")]
        self.assertEqual(len(upload_steps), 4, upload_steps)
        for operation_id in token + upload_steps + list(self.external):
            self.assertIn(operation_id, self.excluded)
            self.assertFalse(self.excluded[operation_id].get("callable", True), operation_id)

    def test_deprecated_rules(self):
        for rule in self.overlay.get("deprecated", []):
            self.assertIn(rule["operation"], self.operations)
            if "replacement" in rule:
                self.assertIn(rule["replacement"], self.overlay["names"])

    def test_secret_fields_exist_in_responses(self):
        seen = set()
        for rule in self.overlay.get("secrets", []):
            self.assertIn(rule["kind"], SECRET_KINDS)
            self.assertTrue(rule["description"].strip())
            for operation_id in rule["operations"]:
                self.assertIn(operation_id, self.operations, f"[[secrets]]: нет операции {operation_id}")
                self.assertNotIn(operation_id, self.excluded, f"[[secrets]]: {operation_id} исключена")
                _, _, operation = self.operations[operation_id]
                root = operation["responses"]["200"]["content"]["application/json"]["schema"]
                for field in rule["fields"]:
                    with self.subTest(operation=operation_id, field=field):
                        self.assertNotIn((operation_id, field), seen)
                        seen.add((operation_id, field))
                        node = self.resolve(root)
                        for segment in field.split("."):
                            name = segment.removesuffix("[]")
                            node = self.resolve(node["properties"][name])
                            if segment.endswith("[]"):
                                self.assertEqual(node["type"], "array")
                                node = self.resolve(node["items"])
                        self.assertEqual(node["type"], "string")

    # --------------------------------------------------------------- гигиена

    def test_public_files_have_no_private_references(self):
        tracker_key = re.compile(r"\b[A-Z]{2,10}-\d+\b")
        allowed = re.compile(r"\b(?:ISO|UTF|SHA|HMAC|RFC|AWS4|HTTP|BCP)-\d+\b")
        for path in sorted(SPEC_DIR.iterdir()):
            if path.suffix not in (".json", ".toml", ".md", ".py"):
                continue
            text = path.read_text(encoding="utf-8")
            with self.subTest(file=path.name):
                for match in tracker_key.finditer(text):
                    self.assertTrue(allowed.fullmatch(match.group(0)), f"похоже на ключ задачи: {match.group(0)}")
                # Слова собраны из частей, чтобы сам тест не содержал этих строк.
                for parts in (("AGENTS", ".md"), ("CLAUDE", ".md"), ("source", "craft"),
                              ("tracker", ".yandex"), ("wiki", ".yandex")):
                    self.assertNotIn("".join(parts), text)

    def resolve(self, schema: dict) -> dict:
        while "$ref" in schema:
            schema = self.spec["components"]["schemas"][schema["$ref"].rsplit("/", 1)[1]]
        return schema


if __name__ == "__main__":
    unittest.main()
