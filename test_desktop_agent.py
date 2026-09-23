import json
from pathlib import Path
import sys
import tempfile
from threading import Event
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
import urllib.error
import urllib.request

from desktop_agent import AgentSession, REMOTE_BOOTSTRAP
from windows_uia import WindowsDesktop


class AgentTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        config = Path(self.directory.name) / "config.json"
        config.write_text(json.dumps({"transport": "local", "request_timeout": .5,
                                     "command": [sys.executable, str(Path(__file__).parent / "tests/fake_windows_agent.py")]}))
        self.cancel = Event()
        self.agent = AgentSession(config, self.cancel)

    def tearDown(self):
        self.agent.close()
        self.directory.cleanup()

    def test_roundtrip_and_old_reference_rejected(self):
        state = self.agent.observe()
        self.assertTrue(self.agent.act({"type": "focus", "target": "e1"}, state["observation_id"])["ok"])
        with self.assertRaisesRegex(RuntimeError, "expirada"):
            self.agent.act({"type": "focus", "target": "e1"}, state["observation_id"])

    def test_bad_credential_cannot_control(self):
        url = f"http://127.0.0.1:{self.agent.server.server_port}/next"
        request = urllib.request.Request(url, data=b'{}', headers={"Authorization": "Bearer wrong"})
        with self.assertRaises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(request)
        self.assertEqual(caught.exception.code, 403)

    def test_process_cannot_replace_live_agent(self):
        url = f"http://127.0.0.1:{self.agent.server.server_port}/next"
        request = urllib.request.Request(url, data=json.dumps({"hello": True, "boot": "other", "session_id": 1}).encode(),
                                         headers={"Authorization": "Bearer " + self.agent.token})
        with self.assertRaises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(request)
        self.assertEqual(caught.exception.code, 400)

    def test_disconnection_never_returns_cached_screenshot(self):
        self.agent.observe()
        with self.assertRaisesRegex(TimeoutError, "screenshot"):
            self.agent.screenshot()
        self.assertTrue(self.agent.closed)
        self.assertIsNotNone(self.agent.process.poll())

    def test_cancel_prevents_sending_action(self):
        state = self.agent.observe()
        self.cancel.set()
        with self.assertRaisesRegex(RuntimeError, "cancelado"):
            self.agent.act({"type": "keys", "keys": ["Enter"]}, state["observation_id"])


class ObservationTest(unittest.TestCase):
    def setUp(self):
        self.desktop = object.__new__(WindowsDesktop)
        self.desktop.available = Mock()
        self.desktop.gui = Mock()
        self.desktop.gui.GetForegroundWindow.return_value = 123
        self.desktop.foreground = "w123"
        self.desktop.observation_id = "fresh"
        self.desktop.observed_at = time.monotonic()
        self.desktop.elements = {}

    def test_changed_focus_rejects_input(self):
        self.desktop.gui.GetForegroundWindow.return_value = 456
        with self.assertRaisesRegex(RuntimeError, "foco mudou"):
            self.desktop.validate_observation("fresh")


    def test_expired_observation(self):
        self.desktop.observed_at -= 121
        with self.assertRaisesRegex(RuntimeError, "expirada"):
            self.desktop.validate_observation("fresh")

    def test_failed_action_also_consumes_observation(self):
        with self.assertRaisesRegex(ValueError, "não pertence"):
            self.desktop.act({"type": "invoke", "target": "missing"}, "fresh")
        with self.assertRaisesRegex(RuntimeError, "expirada"):
            self.desktop.validate_observation("fresh")


class BootstrapTest(unittest.TestCase):
    def test_powershell_error_is_not_masked_by_encoding(self):
        agent = object.__new__(AgentSession)
        agent.config = {"host": "test"}
        result = Mock(returncode=1, stderr="A expressão de atribuição não é válida".encode("cp850"))
        with patch("desktop_agent.subprocess.run", return_value=result):
            with self.assertRaisesRegex(RuntimeError, "expressão de atribuição"):
                agent._remote({"operation": "prepare"})

    def test_powershell_bootstrap_starts_with_assignment(self):
        self.assertTrue(REMOTE_BOOTSTRAP.startswith("$ErrorActionPreference ="))


class KeyboardTest(unittest.TestCase):
    def keyboard(self, send=None):
        return SimpleNamespace(send_keys=send or Mock(), parse_keys=Mock())

    def test_letters_and_digits_use_supported_syntax(self):
        keyboard = self.keyboard()
        with patch.dict(sys.modules, {"pywinauto.keyboard": keyboard}):
            WindowsDesktop.keys(["CTRL", "SHIFT", "S"])
            WindowsDesktop.keys(["Ctrl", "1"])
        sent = [call.args[0] for call in keyboard.send_keys.call_args_list]
        self.assertIn("{s down}", sent)
        self.assertIn("{1 down}", sent)
        self.assertNotIn("{VK_S down}", sent)

    def test_keyup_failure_does_not_leave_modifiers_pressed(self):
        def send(sequence, **kwargs):
            if sequence in ("{s up}", "{VK_S up}"):
                raise RuntimeError("falha de envio")
        keyboard = self.keyboard(Mock(side_effect=send))
        with patch.dict(sys.modules, {"pywinauto.keyboard": keyboard}):
            with self.assertRaises(RuntimeError):
                WindowsDesktop.keys(["Ctrl", "Shift", "S"])
        sent = [call.args[0] for call in keyboard.send_keys.call_args_list]
        self.assertIn("{VK_SHIFT up}", sent)
        self.assertIn("{VK_CONTROL up}", sent)

    def test_invalid_sequence_is_rejected_before_any_input(self):
        keyboard = self.keyboard()
        keyboard.parse_keys.side_effect = ValueError("invalid sequence")
        with patch.dict(sys.modules, {"pywinauto.keyboard": keyboard}):
            with self.assertRaises(ValueError):
                WindowsDesktop.keys(["Ctrl", "S"])
        keyboard.send_keys.assert_not_called()


class ModalObservationTest(unittest.TestCase):
    def test_foreground_dialog_missing_from_enumeration_is_observed(self):
        agent = object.__new__(WindowsDesktop)
        agent.available = Mock()
        agent.elements, agent.windows = {}, {}
        agent.session_id = 1
        agent.gui = Mock()
        agent.gui.GetForegroundWindow.return_value = 197794
        agent.desktop = Mock()
        agent.desktop.windows.return_value = []
        dialog = Mock(handle=197794)
        dialog.is_visible.return_value = True
        dialog.window_text.return_value = "Salvar como"
        dialog.process_id.return_value = 1234
        dialog.class_name.return_value = "#32770"
        dialog.children.return_value = []
        agent.desktop.window.return_value.wrapper_object.return_value = dialog
        agent.rect = Mock(return_value=[0, 0, 800, 600])
        with patch("windows_uia.ctypes.windll", create=True) as dll, \
             patch.dict(sys.modules, {"win32process": Mock()}):
            dll.user32.GetSystemMetrics.return_value = 800
            state = agent.observe()
        self.assertEqual(state["foreground"], "w197794")
        self.assertEqual(state["windows"][0]["name"], "Salvar como")
        self.assertIs(agent.windows["w197794"], dialog)
        agent.desktop.window.assert_any_call(handle=197794)


if __name__ == "__main__":
    unittest.main()
