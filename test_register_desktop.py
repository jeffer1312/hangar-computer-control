import importlib.util
import json
from pathlib import Path
import tomllib
import unittest

spec = importlib.util.spec_from_file_location("register_desktop", Path(__file__).parent / "scripts/register_desktop_mcp.py")
register = importlib.util.module_from_spec(spec)
spec.loader.exec_module(register)


class RegistrationTest(unittest.TestCase):
    def test_codex_preserves_other_servers_and_is_idempotent(self):
        original = '[mcp_servers.other]\ncommand = "keep"\n'
        args = (r"C:\Program Files\Python\python.exe", r"C:\agent\servidor_mcp.py", r"C:\agent\local.json")
        result = register.codex_document(original, *args)
        self.assertTrue(result.startswith(original))
        self.assertEqual(register.codex_document(result, *args), result)
        data = tomllib.loads(result)["mcp_servers"]["desktop"]
        self.assertEqual(data["command"], args[0])
        self.assertEqual(data["env_vars"], ["TYPESAFE_API_KEY"])

    def test_existing_user_configuration_is_not_overwritten(self):
        with self.assertRaises(ValueError):
            register.codex_document('[mcp_servers.desktop]\ncommand="custom"', "py", "server", "config")
        with self.assertRaises(ValueError):
            register.claude_document('{"mcpServers":{"desktop":{"command":"custom"}}}', "py", "server", "config")

    def test_claude_preserves_unrelated_settings(self):
        result = register.claude_document('{"projects":{"a":{}},"mcpServers":{"other":{}}}', "py", "server", "config")
        self.assertEqual(json.loads(result)["projects"], {"a": {}})
        self.assertIn("other", json.loads(result)["mcpServers"])
        self.assertEqual(register.claude_document(result, "py", "server", "config"), result)


if __name__ == "__main__":
    unittest.main()
