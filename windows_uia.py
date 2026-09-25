"""Agente Windows independente: observa UIA e executa uma ação por observação."""
from __future__ import annotations

import argparse
import base64
import ctypes
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.request
import uuid


class WindowsDesktop:
    def __init__(self):
        from pywinauto import Desktop
        import win32gui
        self.desktop = Desktop(backend="uia")
        self.gui = win32gui
        self.elements = {}
        self.windows = {}
        self.observation_id = None
        self.observed_at = 0.0
        self.foreground = None
        session = ctypes.c_uint()
        if not ctypes.windll.kernel32.ProcessIdToSessionId(os.getpid(), ctypes.byref(session)):
            raise ctypes.WinError()
        self.session_id = session.value
        if not self.session_id:
            raise RuntimeError("o agente deve rodar na sessão gráfica, não na sessão 0")
        kernel = ctypes.windll.kernel32
        kernel.CreateMutexW.restype = ctypes.c_void_p
        kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint]
        self.mutex = kernel.CreateMutexW(None, False, f"Local\\HangarComputerControl-{self.session_id}")
        if not self.mutex or kernel.WaitForSingleObject(self.mutex, 0) not in (0, 0x80):
            raise RuntimeError("outro agente já controla esta sessão Windows")

    def available(self):
        state = ctypes.c_void_p()
        size = ctypes.c_uint()
        wts = ctypes.windll.wtsapi32
        if not wts.WTSQuerySessionInformationW(None, self.session_id, 8, ctypes.byref(state), ctypes.byref(size)):
            raise ctypes.WinError()
        try:
            if ctypes.cast(state, ctypes.POINTER(ctypes.c_int)).contents.value != 0:
                raise RuntimeError("sessão Windows desconectada")
        finally:
            wts.WTSFreeMemory.argtypes = [ctypes.c_void_p]
            wts.WTSFreeMemory(state)
        user = ctypes.windll.user32
        user.OpenInputDesktop.restype = ctypes.c_void_p
        user.CloseDesktop.argtypes = [ctypes.c_void_p]
        handle = user.OpenInputDesktop(0, False, 0x0001)
        if not handle:
            raise RuntimeError("desktop bloqueado, desconectado ou protegido por UAC")
        user.CloseDesktop(handle)

    @staticmethod
    def rect(element):
        r = element.rectangle()
        return [r.left, r.top, r.right, r.bottom]

    @staticmethod
    def pattern(element, name):
        try:
            return getattr(element, "iface_" + name)
        except Exception as exc:
            if type(exc).__name__ == "NoPatternInterfaceError":
                return None
            raise

    def observe(self):
        self.available()
        self.observation_id = None
        self.elements.clear()
        self.windows.clear()
        # Área de trabalho vazia não tem janela ativa; o shell (Program Manager) faz o papel.
        foreground = self.gui.GetForegroundWindow() or ctypes.windll.user32.GetShellWindow()
        windows = []
        enumerated = list(self.desktop.windows())
        # Diálogos pertencentes a outra janela podem faltar na enumeração do desktop.
        if not any(window.handle == foreground for window in enumerated):
            enumerated.append(self.desktop.window(handle=foreground).wrapper_object())
        for window in enumerated:
            if window.is_visible():
                wid = "w" + str(window.handle)
                self.windows[wid] = window
                windows.append({"id": wid, "name": window.window_text(),
                                "process_id": window.process_id(), "class_name": window.class_name(),
                                "rect": self.rect(window)})
        self.foreground = "w" + str(foreground)
        window = self.windows.get(self.foreground)
        if window is None:
            raise RuntimeError("a janela ativa mudou durante a observação")
        controls = []
        # Limite de leitura, não de aplicativos: informa explicitamente se houve corte.
        pending = list(window.children())
        # Menus, dropdowns e tooltips são janelas próprias do mesmo processo, fora da janela ativa.
        import win32process
        pid = window.process_id()
        popups = []

        def collect(handle, _):
            # Só janelas PERTENCENTES à ativa (owner): outra janela principal do mesmo
            # processo não entra, senão a árvore dobra e o alvo fica ambíguo.
            # Popup, menu e diálogo têm dono (no VCL é a janela oculta do TApplication, não a principal);
            # janela principal solta tem dono 0 e fica fora, senão a árvore dobra. Menu Win32 (#32768) não tem dono.
            # Desabilitada = está atrás de um diálogo modal; seus controles não respondem e confundem o alvo.
            if handle != foreground and self.gui.IsWindowVisible(handle) and self.gui.IsWindowEnabled(handle) \
                    and win32process.GetWindowThreadProcessId(handle)[1] == pid \
                    and (self.gui.GetWindow(handle, 4) or self.gui.GetClassName(handle) == "#32768"):
                popups.append(handle)
            return True
        self.gui.EnumWindows(collect, None)
        for handle in popups:
            try:
                pending.extend(self.desktop.window(handle=handle).wrapper_object().children())
            except Exception:
                continue
        # Botões da barra de tarefas entram sempre: app minimizado (VCL esconde a janela) só volta por ali.
        barra = self.gui.FindWindow("Shell_TrayWnd", None)
        if barra and barra != foreground:
            try:
                pending.extend(self.desktop.window(handle=barra).wrapper_object().children())
            except Exception:
                pass
        visited = 0
        # Orçamento de tempo, não de nós: Chrome entrega 800 nós em 2 s, IDE Delphi ~150 em 4 s.
        prazo = time.monotonic() + 4
        while pending and visited < 800 and time.monotonic() < prazo:
            element = pending.pop(0)
            visited += 1
            info = element.element_info
            if info.element.CurrentIsPassword:
                continue
            # Contêiner "invisível" (popup host, pane sem retângulo) ainda tem filhos visíveis.
            pending.extend(element.children()[:40])
            if not element.is_visible():
                continue
            role = info.control_type
            actions = []
            value = None
            for pattern, action in (("invoke", "invoke"), ("selection_item", "select"),
                                    ("toggle", "toggle"), ("scroll", "scroll")):
                if self.pattern(element, pattern) is not None:
                    actions.append(action)
            expansion = self.pattern(element, "expand_collapse")
            if expansion is not None:
                actions.append("expand" if expansion.CurrentExpandCollapseState == 0 else "collapse")
            edit = self.pattern(element, "value")
            if edit is not None:
                value = edit.CurrentValue
                if not edit.CurrentIsReadOnly:
                    actions.append("set_value")
            elif role in ("Document", "Edit") or (not actions and not info.name):
                # Console (Windows Terminal) e visores sem nome expõem o texto só por TextPattern.
                text = self.pattern(element, "text")
                if text is not None:
                    value = text.DocumentRange.GetText(4000)
            if info.element.CurrentIsKeyboardFocusable:
                actions.append("focus")
            if not actions and not info.name and value is None:
                continue
            eid = "e" + str(len(controls))
            self.elements[eid] = (element, info.name, role)
            controls.append({"id": eid, "name": info.name, "role": role,
                             "value": value[:4000] if isinstance(value, str) else value,
                             "enabled": element.is_enabled(), "rect": self.rect(element),
                             "focused": bool(info.element.CurrentHasKeyboardFocus), "actions": actions})
        if (self.gui.GetForegroundWindow() or ctypes.windll.user32.GetShellWindow()) != foreground:
            raise RuntimeError("a janela ativa mudou durante a observação; tente novamente")
        self.observation_id = uuid.uuid4().hex
        self.observed_at = time.monotonic()
        return {"observation_id": self.observation_id, "connected": True,
                "session_id": self.session_id, "foreground": self.foreground,
                "windows": windows, "elements": controls, "truncated": bool(pending),
                "timestamp": time.time(),
                "screen": {"width": ctypes.windll.user32.GetSystemMetrics(0),
                           "height": ctypes.windll.user32.GetSystemMetrics(1)}}

    def validate_observation(self, observation_id):
        self.available()
        if not observation_id or observation_id != self.observation_id or time.monotonic()-self.observed_at > 120:
            raise RuntimeError("observação expirada; observe novamente")
        if "w" + str(self.gui.GetForegroundWindow() or ctypes.windll.user32.GetShellWindow()) != self.foreground:
            raise RuntimeError("o foco mudou; observe novamente antes de agir")

    @staticmethod
    def keys(names):
        from pywinauto.keyboard import parse_keys, send_keys
        if not isinstance(names, list) or not 1 <= len(names) <= 8:
            raise ValueError("keys precisa conter de 1 a 8 teclas")
        aliases = {"ctrl": "VK_CONTROL", "control": "VK_CONTROL", "alt": "VK_MENU",
                   "shift": "VK_SHIFT", "win": "VK_LWIN", "meta": "VK_LWIN",
                   "enter": "ENTER", "escape": "ESC", "tab": "TAB", "space": "SPACE",
                   "backspace": "BACKSPACE", "delete": "DELETE", "home": "HOME", "end": "END",
                   "pageup": "PGUP", "pagedown": "PGDN", "arrowleft": "LEFT", "arrowright": "RIGHT",
                   "arrowup": "UP", "arrowdown": "DOWN", "insert": "INSERT"}
        codes = []
        for key in names:
            if not isinstance(key, str):
                raise ValueError("tecla deve ser string")
            lower = key.lower()
            if lower in aliases:
                codes.append(aliases[lower])
            elif re.fullmatch(r"F(?:[1-9]|1[0-9]|2[0-4])", key.upper()):
                codes.append(key.upper())
            elif len(key) == 1 and key.isascii() and key.isalnum():
                codes.append(key.lower())
            else:
                raise ValueError("tecla desconhecida: " + key)
        # Valide tudo antes de pressionar modificadores.
        for key in codes:
            parse_keys("{" + key + " down}")
            parse_keys("{" + key + " up}")
        pressed = []
        try:
            for key in codes:
                pressed.append(key)
                send_keys("{" + key + " down}", pause=0)
        finally:
            errors = []
            for key in reversed(pressed):
                try:
                    send_keys("{" + key + " up}", pause=0)
                except Exception as exc:
                    errors.append(f"{key}: {exc}")
            if errors:
                raise RuntimeError("falha ao liberar teclas: " + "; ".join(errors))

    def act(self, action, observation_id):
        self.validate_observation(observation_id)
        # Uma observação só autoriza uma ação, mesmo se a chamada falhar pela metade.
        self.observation_id = None
        if not isinstance(action, dict):
            raise ValueError("ação deve ser objeto")
        kind = action.get("type")
        target = action.get("target")
        if kind == "activate":
            if target not in self.windows:
                raise ValueError("janela não pertence à observação")
            self.windows[target].set_focus()
        elif kind == "launch":
            app, args = action.get("application"), action.get("args", [])
            if not isinstance(app, str) or not app or not isinstance(args, list) or not all(isinstance(a, str) for a in args):
                raise ValueError("application e args inválidos")
            subprocess.Popen([app, *args], shell=False)
        elif kind == "keys":
            self.keys(action.get("keys"))
        elif kind == "text":
            from pywinauto.keyboard import send_keys
            value = action.get("value")
            if not isinstance(value, str) or len(value) > 20000:
                raise ValueError("texto inválido")
            if target in self.elements:  # digitar "no campo X" começa focando X
                self.elements[target][0].set_focus()
                time.sleep(.15)
            escaped = "".join("{" + ch + "}" if ch in "+^%~(){}" else ch for ch in value)
            send_keys(escaped, with_spaces=True, with_newlines=True, vk_packet=True, pause=0)
        elif kind == "mouse":
            from pywinauto import mouse
            x, y = action.get("x"), action.get("y")
            w, h = ctypes.windll.user32.GetSystemMetrics(0), ctypes.windll.user32.GetSystemMetrics(1)
            if type(x) is not int or type(y) is not int or not 0 <= x < w or not 0 <= y < h:
                raise ValueError("coordenada fora da tela")
            button, mode = action.get("button", "left"), action.get("mode", "click")
            if button not in ("left", "right", "middle"):
                raise ValueError("botão inválido")
            if mode in ("click", "double"):
                (mouse.click if mode == "click" else mouse.double_click)(button=button, coords=(x, y))
            elif mode == "move":
                mouse.move(coords=(x, y))
            elif mode == "scroll":
                delta = action.get("delta", 0)
                if type(delta) is not int or abs(delta) > 50:
                    raise ValueError("delta inválido")
                mouse.scroll(coords=(x, y), wheel_dist=-delta)
            elif mode == "drag":
                x2, y2 = action.get("x2"), action.get("y2")
                if type(x2) is not int or type(y2) is not int or not 0 <= x2 < w or not 0 <= y2 < h:
                    raise ValueError("destino fora da tela")
                mouse.press(button=button, coords=(x, y))
                try:
                    mouse.move(coords=(x2, y2), duration=.2)
                finally:
                    mouse.release(button=button, coords=(x2, y2))
            else:
                raise ValueError("modo de mouse inválido")
        else:
            if target not in self.elements:
                raise ValueError("controle não pertence à observação")
            element, name, role = self.elements[target]
            if element.element_info.name != name or element.element_info.control_type != role or not element.is_enabled():
                raise RuntimeError("controle mudou; observe novamente")
            if element.element_info.element.CurrentIsPassword:
                raise RuntimeError("campo protegido não foi exposto ao controlador")
            if kind == "invoke":
                # Invoke por UIA "dá ok" e não faz nada em botão da barra de tarefas e menu VCL;
                # clique real no controle é o que uma pessoa faz. Invoke só sem posição na tela.
                # Em item de lista/árvore um clique só seleciona; ali Invoke é a ação padrão (abrir).
                r = element.rectangle()
                if role not in ("ListItem", "TreeItem", "DataItem") and r.width() > 0 and r.height() > 0 \
                        and not element.element_info.element.CurrentIsOffscreen:
                    element.click_input()
                else:
                    element.iface_invoke.Invoke()
            elif kind == "set_value":
                value = action.get("value")
                if not isinstance(value, str) or len(value) > 20000:
                    raise ValueError("valor inválido")
                # SetValue troca o texto sem avisar o dono do campo: o diálogo Salvar como
                # continuou usando o nome antigo. Digitar como pessoa dispara as notificações.
                from pywinauto.keyboard import send_keys
                element.set_focus()
                time.sleep(.15)
                send_keys("^a{DELETE}", pause=.05)
                send_keys("".join("{" + ch + "}" if ch in "+^%~(){}" else ch for ch in value),
                          with_spaces=True, with_newlines=True, vk_packet=True, pause=.01)
                time.sleep(.15)
                lido = element.iface_value.CurrentValue or ""
                if lido.replace("\r\n", "\n").strip() != value.replace("\r\n", "\n").strip():
                    raise RuntimeError(f"valor digitado não confirmado; campo ficou com {lido[:80]!r}")
            elif kind == "select":
                element.iface_selection_item.Select()
            elif kind == "toggle":
                element.iface_toggle.Toggle()
            elif kind in ("expand", "collapse"):
                getattr(element.iface_expand_collapse, "Expand" if kind == "expand" else "Collapse")()
            elif kind == "focus":
                element.set_focus()
            elif kind == "scroll":
                direction = action.get("direction", "down")
                if direction not in ("up", "down", "left", "right"):
                    raise ValueError("direção inválida")
                amount = action.get("amount", "small")
                if amount not in ("small", "large"):
                    raise ValueError("amount deve ser small ou large")
                # UIA ScrollAmount: large/small decrement=0/1, no=2, large/small increment=3/4.
                value = (0 if amount == "large" else 1) if direction in ("up", "left") else (3 if amount == "large" else 4)
                element.iface_scroll.Scroll(value if direction in ("left", "right") else 2,
                                            value if direction in ("up", "down") else 2)
            else:
                raise ValueError("tipo de ação desconhecido: " + str(kind))
        return {"ok": True}

    def screenshot(self):
        from PIL import ImageGrab
        self.available()
        output = io.BytesIO()
        imagem = ImageGrab.grab()
        if imagem.width > 1280:  # o modelo de visão não precisa de 1080p e a chamada cai de ~40 s pra ~10 s
            imagem = imagem.resize((1280, round(imagem.height * 1280 / imagem.width)))
        imagem.save(output, format="PNG")
        return base64.b64encode(output.getvalue()).decode()

    def request(self, request):
        operation = request.get("operation")
        if operation == "observe":
            return self.observe()
        if operation == "act":
            return self.act(request["action"], request["observation_id"])
        if operation == "screenshot":
            return {"png": self.screenshot()}
        raise ValueError("operação inválida")


def run(config):
    desktop = WindowsDesktop()
    boot = uuid.uuid4().hex
    response = {"hello": True, "boot": boot, "session_id": desktop.session_id, "pid": os.getpid()}
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    while True:
        request = urllib.request.Request(config["url"], data=json.dumps(response).encode(),
                                         headers={"Authorization": "Bearer " + config["token"],
                                                  "Content-Type": "application/json"})
        with opener.open(request, timeout=20) as stream:
            command = json.load(stream)
        if command.get("operation") == "stop":
            return
        response = {"boot": boot, "session_id": desktop.session_id, "pid": os.getpid()}
        if command.get("operation") == "heartbeat":
            continue
        response["id"] = command["id"]
        try:
            if time.time() > command["expires_at"]:
                raise TimeoutError("comando expirou antes de chegar ao desktop")
            response["result"] = desktop.request(command)
        except Exception as exc:
            response["error"] = f"{type(exc).__name__}: {exc}"


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    args = parser.parse_args()
    path = Path(args.config)
    try:
        config = json.loads(path.read_text(encoding="utf-8-sig"))
        path.unlink()  # A credencial de partida não fica no disco depois de carregada.
        run(config)
    except Exception:
        import traceback
        path.with_suffix(".error.log").write_text(traceback.format_exc(), encoding="utf-8")
        sys.exit(1)
