"""Agente Linux (Hyprland/Wayland) no mesmo protocolo do windows_uia.

Janelas pelo hyprctl, controles pelo AT-SPI (pyatspi), print pelo grim, mouse e teclas pelo
ydotool e texto pelo wtype. Roda com o Python do sistema, onde o pyatspi está instalado.
"""
from __future__ import annotations

import base64
import fcntl
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import time
import uuid

import windows_uia

# Papéis AT-SPI no vocabulário da UIA, que o laço já conhece (Button, Edit, MenuItem...).
ROLES = {"push button": "Button", "button": "Button", "toggle button": "Button", "push button menu": "Button",
         "check box": "CheckBox", "radio button": "RadioButton", "entry": "Edit", "password text": "Edit",
         "spin button": "Spinner", "combo box": "ComboBox", "document web": "Document",
         "document frame": "Document", "document text": "Document", "label": "Text", "static": "Text",
         "heading": "Text", "paragraph": "Text", "menu item": "MenuItem", "check menu item": "MenuItem",
         "radio menu item": "MenuItem", "menu": "MenuItem", "list item": "ListItem", "tree item": "TreeItem",
         "table cell": "DataItem", "link": "Hyperlink", "page tab": "TabItem", "image": "Image", "icon": "Image",
         "menu bar": "MenuBar", "tool bar": "ToolBar", "scroll bar": "ScrollBar", "slider": "Slider",
         "list": "List", "list box": "List", "tree": "Tree", "tree table": "Tree", "table": "Table",
         "page tab list": "Tab", "frame": "Window", "dialog": "Window", "window": "Window", "panel": "Pane",
         "filler": "Pane", "section": "Group", "status bar": "StatusBar", "progress bar": "ProgressBar"}
ITEMS = ("ListItem", "TreeItem", "TabItem", "DataItem")
# Nomes de ação variam por toolkit: GTK "click"/"activate", Qt "Press", Chromium "doDefault".
INVOKE = ("click", "press", "activate", "dodefault", "jump", "open", "showmenu")
POPUPS = ("window", "popup menu", "menu", "tool tip")

# Códigos evdev (linux/input-event-codes.h): independem do layout, e letras e números
# ficam na mesma posição no ABNT2 e no US.
KEYCODES = {"ctrl": 29, "control": 29, "alt": 56, "shift": 42, "win": 125, "meta": 125, "enter": 28,
            "escape": 1, "tab": 15, "space": 57, "backspace": 14, "delete": 111, "home": 102, "end": 107,
            "pageup": 104, "pagedown": 109, "arrowleft": 105, "arrowright": 106, "arrowup": 103,
            "arrowdown": 108, "insert": 110,
            **{f"f{n}": 58 + n for n in range(1, 11)}, "f11": 87, "f12": 88,
            **{f"f{n}": 170 + n for n in range(13, 25)},
            **{str(n): 1 + n for n in range(1, 10)}, "0": 11,
            **{c: 16 + i for i, c in enumerate("qwertyuiop")}, **{c: 30 + i for i, c in enumerate("asdfghjkl")},
            **{c: 44 + i for i, c in enumerate("zxcvbnm")}}
BUTTONS = {"left": 0x00, "right": 0x01, "middle": 0x02}


def monitor_rect(monitor):
    """Retângulo lógico do monitor: hyprctl dá o modo em pixels físicos; janelas e cursor usam o lógico."""
    w, h = monitor["width"], monitor["height"]
    if monitor.get("transform", 0) % 2:
        w, h = h, w
    x, y = monitor["x"], monitor["y"]
    return [x, y, x + round(w / monitor["scale"]), y + round(h / monitor["scale"])]


def reference_monitor(monitors, active):
    """Monitor da janela ativa (ou o focado): o print e as coordenadas da observação são dele."""
    for m in monitors:
        if (m["id"] == active["monitor"]) if active.get("address") else m.get("focused"):
            return m
    raise RuntimeError("nenhum monitor focado no hyprctl")


def windows_from(clients, origin):
    """Janelas no formato do protocolo, em coordenadas relativas ao monitor de referência."""
    ox, oy = origin
    return [{"id": "w" + c["address"], "name": c["title"], "process_id": c["pid"], "class_name": c["class"],
             "rect": [c["at"][0] - ox, c["at"][1] - oy, c["at"][0] + c["size"][0] - ox, c["at"][1] + c["size"][1] - oy]}
            for c in clients if c.get("mapped") and not c.get("hidden")]


def element_rect(extents, window_rect):
    """No Wayland o AT-SPI só sabe a posição dentro da janela; a da janela vem do hyprctl."""
    x, y, w, h = extents
    if w <= 0 or h <= 0:
        return None
    return [window_rect[0] + x, window_rect[1] + y, window_rect[0] + x + w, window_rect[1] + y + h]


def role_name(role, states):
    if role == "text":
        return "Edit" if "editable" in states else "Text"
    return ROLES.get(role, role.title().replace(" ", ""))


def element_from(info, window_rect):
    """Nó AT-SPI já lido (dict) -> controle do protocolo, ou None se não serve para o laço."""
    states = info["states"]
    role = role_name(info["role"], states)
    password = info["role"] == "password text"
    names = [n.casefold() for n in info["actions"]]
    actions = []
    if "toggle" in names:  # no Qt o item de lista seleciona por "Toggle"; Press do checkbox é o mesmo toggle
        actions.append("select" if role in ITEMS else "toggle")
    elif any(n in INVOKE for n in names):
        actions.append("invoke")
    if "expandable" in states:
        actions.append("collapse" if "expanded" in states else "expand")
    if "editable" in states:  # o valor entra pelo teclado; EditableText não é exigido (WebKitGTK não expõe)
        actions.append("set_value")
    if "focusable" in states:
        actions.append("focus")
    value = None
    if role in ("CheckBox", "RadioButton") or "checkable" in states or info["role"] == "toggle button":
        value = "on" if "checked" in states else "off"
    elif "selectable" in states and role in ITEMS:
        value = "selected" if "selected" in states else "not selected"
    elif not password and info["text"] is not None and (role in ("Edit", "Document") or (not actions and not info["name"])):
        value = info["text"][:4000]
    if not actions and not info["name"] and value is None:
        return None
    return {"name": info["name"], "role": role, "value": value,
            "enabled": "enabled" in states or "sensitive" in states,
            "rect": element_rect(info["extents"], window_rect) if info["extents"] else None,
            "focused": "focused" in states, "actions": actions, **({"password": True} if password else {})}


def intersects(a, b):
    return a[0] < b[2] and b[0] < a[2] and a[1] < b[3] and b[1] < a[3]


def walk(roots, read, children, window_rect, vanished=(), budget=4.0, limit=800):
    """Busca em largura com orçamento de tempo, como no Windows; devolve controles, nós e se cortou."""
    pending, controls, nodes = list(roots), [], []
    deadline = time.monotonic() + budget
    visited = 0
    while pending and visited < limit and time.monotonic() < deadline:
        node = pending.pop(0)
        visited += 1
        try:
            info = read(node)
        except vanished:
            continue  # nó destruído durante a leitura (lista atualizando) já não está na tela
        if "showing" not in info["states"]:
            continue  # no AT-SPI filho de nó oculto também é oculto: a subárvore inteira sai
        try:
            pending.extend(children(node))
        except vanished:
            pass  # filhos sumiram; o próprio nó foi lido e continua valendo
        element = element_from(info, window_rect)
        # Rolado para fora da janela (lista longa): o laço não deve mirar nele.
        if element is None or (element["rect"] and not intersects(element["rect"], window_rect)):
            continue
        element["id"] = "e" + str(len(controls))
        controls.append(element)
        nodes.append(node)
    return controls, nodes, bool(pending)


def choose_frame(frames, title):
    """Índice do frame AT-SPI da janela ativa: pelo título, senão o ativo, senão o único."""
    for i, (name, _) in enumerate(frames):
        if name == title:
            return i
    active = [i for i, (_, states) in enumerate(frames) if "active" in states]
    if len(active) == 1:
        return active[0]
    return 0 if len(frames) == 1 else None


def lua_string(text):
    """Literal Lua com tudo fora de [A-Za-z0-9 ._/-] escapado em bytes: sem injeção no dispatch."""
    return '"' + "".join(chr(b) if chr(b).isascii() and (chr(b).isalnum() or chr(b) in " ._/-") else f"\\{b:03d}"
                         for b in text.encode()) + '"'


def key_codes(names):
    if not isinstance(names, list) or not 1 <= len(names) <= 8:
        raise ValueError("keys precisa conter de 1 a 8 teclas")
    codes = []
    for key in names:
        if not isinstance(key, str) or key.casefold() not in KEYCODES:
            raise ValueError(f"tecla desconhecida: {key}")
        codes.append(KEYCODES[key.casefold()])
    return codes


def run_command(command, input=None):
    result = subprocess.run(command, input=input, capture_output=True, timeout=15)
    if result.returncode:
        detail = (result.stderr or result.stdout).decode(errors="replace").strip()[-500:]
        raise RuntimeError(f"{command[0]} falhou ({result.returncode}): {detail}")
    return result.stdout


def hypr(query):
    return json.loads(run_command(["hyprctl", "-j", query]))


def dispatch(lua):
    # hyprctl dispatch sai com 0 mesmo quando o dispatcher falha; só "ok" é sucesso.
    output = run_command(["hyprctl", "dispatch", lua]).decode(errors="replace").strip()
    if output != "ok":
        raise RuntimeError(f"hyprctl dispatch falhou: {output[:300]}")


class LinuxDesktop:
    def __init__(self):
        import pyatspi
        from gi.repository import GLib
        self.atspi = pyatspi
        self.vanished = (GLib.Error,)
        if not os.environ.get("HYPRLAND_INSTANCE_SIGNATURE") or not os.environ.get("WAYLAND_DISPLAY"):
            raise RuntimeError("o agente deve rodar dentro da sessão Hyprland do usuário")
        self.session_id = os.getuid()
        if not self.session_id:
            raise RuntimeError("o agente não deve rodar como root")
        runtime = Path(os.environ.get("XDG_RUNTIME_DIR") or "/tmp")
        self.lock = open(runtime / "hangar-computer-control-agent.lock", "a")
        try:
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("outro agente já controla esta sessão") from None
        self.elements = {}
        self.windows = {}
        self.observation_id = None
        self.observed_at = 0.0
        self.foreground = None
        self.area = [0, 0, 0, 0]

    @staticmethod
    def available():
        locked = subprocess.run(["pgrep", "-x", "hyprlock"], capture_output=True)
        if locked.returncode == 0:
            raise RuntimeError("tela bloqueada")
        if locked.returncode != 1:
            raise RuntimeError(f"pgrep falhou ({locked.returncode})")

    def read(self, node):
        interfaces = set(node.get_interfaces())
        names, extents, text = [], None, None
        if "Action" in interfaces:
            action = node.queryAction()
            names = [action.getName(i) for i in range(action.nActions)]
        if "Component" in interfaces:
            e = node.queryComponent().getExtents(self.atspi.WINDOW_COORDS)
            extents = (e.x, e.y, e.width, e.height)
        if "Text" in interfaces:
            text = node.queryText().getText(0, 4000)
        return {"role": node.getRoleName(), "name": node.name or "",
                "states": {s.value_nick for s in node.getState().getStates()},
                "actions": names, "extents": extents, "text": text}

    @staticmethod
    def children(node):
        # Sem teto por nó: o limite de nós e de tempo do walk é que corta, e aí marca truncated.
        return [c for c in (node.getChildAtIndex(i) for i in range(node.childCount)) if c is not None]

    def pid(self, app):
        try:
            return app.get_process_id()
        except self.vanished:
            return None  # aplicativo encerrando enquanto a lista era lida

    def roots(self, client):
        """Filhos do frame da janela ativa e dos menus/popups abertos do mesmo aplicativo."""
        apps = [a for a in self.atspi.Registry.getDesktop(0) if a is not None]
        # Sem app com o pid da janela, não há árvore confiável: o laço cai no print.
        own = [a for a in apps if self.pid(a) == client["pid"]]
        tops = [t for app in own for t in self.children(app)]
        frames = [t for t in tops if t.getRoleName() in ("frame", "dialog")]
        chosen = choose_frame([(f.name or "", {s.value_nick for s in f.getState().getStates()}) for f in frames],
                              client["title"])
        if chosen is None:
            return []
        popups = [t for t in tops if t.getRoleName() in POPUPS
                  and t.getState().contains(self.atspi.STATE_SHOWING)]
        return [c for top in [frames[chosen], *popups] for c in self.children(top)]

    def current(self):
        active = hypr("activewindow")
        monitor = reference_monitor(hypr("monitors"), active)
        return active, monitor_rect(monitor), monitor

    def observe(self):
        self.available()
        self.observation_id = None
        self.elements.clear()
        self.windows.clear()
        active, area, _ = self.current()
        clients = hypr("clients")
        windows = windows_from(clients, area[:2])
        self.windows = {"w" + c["address"]: c for c in clients if c.get("mapped") and not c.get("hidden")}
        self.area = area
        size = [area[2] - area[0], area[3] - area[1]]
        controls, truncated = [], False
        if active.get("address"):
            self.foreground = "w" + active["address"]
            window = next((w for w in windows if w["id"] == self.foreground), None)
            if window is None:
                raise RuntimeError("a janela ativa mudou durante a observação")
            controls, nodes, truncated = walk(self.roots(active), self.read, self.children, window["rect"],
                                              self.vanished)
            self.elements = {e["id"]: (n, e["name"], e["role"], e["rect"]) for e, n in zip(controls, nodes)}
        else:
            # Área de trabalho vazia não tem janela ativa; uma janela fictícia faz o papel do shell.
            self.foreground = "desktop"
            windows.append({"id": "desktop", "name": "Área de trabalho", "process_id": 0,
                            "class_name": "desktop", "rect": [0, 0, *size]})
        after = hypr("activewindow").get("address")
        if ("w" + after if after else "desktop") != self.foreground:
            raise RuntimeError("a janela ativa mudou durante a observação; tente novamente")
        self.observation_id = uuid.uuid4().hex
        self.observed_at = time.monotonic()
        return {"observation_id": self.observation_id, "connected": True, "session_id": self.session_id,
                "foreground": self.foreground, "windows": windows, "elements": controls, "truncated": truncated,
                "timestamp": time.time(), "screen": {"width": size[0], "height": size[1]}}

    def validate_observation(self, observation_id):
        self.available()
        if not observation_id or observation_id != self.observation_id or time.monotonic() - self.observed_at > 120:
            raise RuntimeError("observação expirada; observe novamente")
        address = hypr("activewindow").get("address")
        if ("w" + address if address else "desktop") != self.foreground:
            raise RuntimeError("o foco mudou; observe novamente antes de agir")

    @staticmethod
    def keys(names):
        codes = key_codes(names)
        # Um comando só pressiona e solta tudo; se falhar, solta de novo para não prender modificador.
        events = [f"{c}:1" for c in codes] + [f"{c}:0" for c in reversed(codes)]
        try:
            run_command(["ydotool", "key", *events])
        except BaseException:
            try:
                subprocess.run(["ydotool", "key", *[f"{c}:0" for c in reversed(codes)]], capture_output=True, timeout=15)
            except (OSError, subprocess.SubprocessError):
                pass  # o erro que importa é o original, relançado abaixo
            raise

    @staticmethod
    def type_text(value):
        if not isinstance(value, str) or len(value) > 20000:
            raise ValueError("texto inválido")
        # wtype usa keymap próprio: acento e símbolo saem certos em qualquer layout (ydotool type supõe US).
        run_command(["wtype", "-"], input=value.encode())

    def point(self, x, y):
        w, h = self.area[2] - self.area[0], self.area[3] - self.area[1]
        if type(x) is not int or type(y) is not int or not 0 <= x < w or not 0 <= y < h:
            raise ValueError("coordenada fora da tela")
        # cursor.move posiciona em coordenada lógica global, sem a aceleração do ydotool mousemove.
        dispatch(f"hl.dsp.cursor.move({{ x = {self.area[0] + x}, y = {self.area[1] + y} }})")

    def mouse(self, action):
        button, mode = action.get("button", "left"), action.get("mode", "click")
        if button not in BUTTONS:
            raise ValueError("botão inválido")
        code = BUTTONS[button]
        if mode not in ("click", "double", "move", "scroll", "drag"):
            raise ValueError("modo de mouse inválido")
        delta = action.get("delta", 0)
        if mode == "scroll" and (type(delta) is not int or abs(delta) > 50):
            raise ValueError("delta inválido")
        self.point(action.get("x"), action.get("y"))
        if mode == "click":
            run_command(["ydotool", "click", hex(0xC0 | code)])
        elif mode == "double":
            run_command(["ydotool", "click", "--repeat", "2", "--next-delay", "60", hex(0xC0 | code)])
        elif mode == "scroll":
            # delta positivo desce, como no Windows; REL_WHEEL positivo sobe.
            run_command(["ydotool", "mousemove", "--wheel", "-x", "0", "-y", str(-delta)])
        elif mode == "drag":
            x2, y2 = action.get("x2"), action.get("y2")
            run_command(["ydotool", "click", hex(0x40 | code)])
            try:
                time.sleep(.1)
                self.point(x2, y2)
                time.sleep(.1)
            finally:
                run_command(["ydotool", "click", hex(0x80 | code)])

    def focus_field(self, target):
        node, _, _, rect = self.elements[target]
        try:
            if node.queryComponent().grabFocus():
                return
        except self.vanished:
            pass  # GTK4 não implementa grabFocus pelo AT-SPI
        if not rect:
            raise RuntimeError("aplicativo recusou o foco e o campo não tem posição na tela")
        # Clicar no campo foca como uma pessoa faria.
        self.point((rect[0] + rect[2]) // 2, (rect[1] + rect[3]) // 2)
        run_command(["ydotool", "click", hex(0xC0 | BUTTONS["left"])])

    def do_action(self, node, wanted):
        action = node.queryAction()
        names = [action.getName(i).casefold() for i in range(action.nActions)]
        index = next((i for i, n in enumerate(names) if n in wanted), None)
        if index is None:
            raise RuntimeError(f"controle sem ação {wanted[0]}: {names}")
        if not action.doAction(index):
            raise RuntimeError(f"aplicativo recusou a ação {names[index]}")

    def act(self, action, observation_id):
        self.validate_observation(observation_id)
        # Uma observação só autoriza uma ação, mesmo se a chamada falhar pela metade.
        self.observation_id = None
        if not isinstance(action, dict):
            raise ValueError("ação deve ser objeto")
        kind, target = action.get("type"), action.get("target")
        if kind == "activate":
            if target not in self.windows:
                raise ValueError("janela não pertence à observação")
            dispatch(f"hl.dsp.focus({{ window = {lua_string('address:' + self.windows[target]['address'])} }})")
        elif kind == "launch":
            app, args = action.get("application"), action.get("args", [])
            if not isinstance(app, str) or not app or not isinstance(args, list) or not all(isinstance(a, str) for a in args):
                raise ValueError("application e args inválidos")
            if not shutil.which(app):  # exec_cmd responde "ok" mesmo sem o programa
                raise ValueError(f"programa não encontrado: {app}")
            # Pelo Hyprland o programa nasce na sessão, não como filho do agente que encerra junto.
            dispatch(f"hl.dsp.exec_cmd({lua_string(shlex.join([app, *args]))})")
        elif kind == "keys":
            self.keys(action.get("keys"))
        elif kind == "text":
            if target is not None and target not in self.elements:
                raise ValueError("controle não pertence à observação")
            if target in self.elements:  # digitar "no campo X" começa focando X
                self.focus_field(target)
                time.sleep(.15)
            self.type_text(action.get("value"))
        elif kind == "mouse":
            self.mouse(action)
        else:
            if target not in self.elements:
                raise ValueError("controle não pertence à observação")
            node, name, role, _ = self.elements[target]
            info = self.read(node)
            if (info["name"], role_name(info["role"], info["states"])) != (name, role) \
                    or not {"enabled", "sensitive"} & info["states"]:
                raise RuntimeError("controle mudou; observe novamente")
            # Posição relida agora: lista rolada ou janela redimensionada depois da observação
            # faria o centro antigo cair em outro controle.
            at, size = (window := hypr("activewindow"))["at"], window["size"]
            window_rect = [at[0] - self.area[0], at[1] - self.area[1],
                           at[0] - self.area[0] + size[0], at[1] - self.area[1] + size[1]]
            rect = info["extents"] and element_rect(info["extents"], window_rect)
            center = rect and ((rect[0] + rect[2]) // 2, (rect[1] + rect[3]) // 2)
            on_screen = bool(center) and window_rect[0] <= center[0] < window_rect[2] \
                and window_rect[1] <= center[1] < window_rect[3]
            # O "click" do AT-SPI no WebKitGTK responde ok e só mostra a dica do botão; clique real é o que
            # uma pessoa faz. Ação AT-SPI só sem posição na tela; em item de lista o clique só seleciona.
            if kind == "invoke" and role not in ITEMS and on_screen or kind in ("select", "toggle") and on_screen:
                self.mouse({"x": center[0], "y": center[1], "button": "left", "mode": "click"})
            elif kind == "invoke":
                self.do_action(node, INVOKE)
            elif kind in ("select", "toggle"):
                self.do_action(node, ("toggle", *INVOKE))
            elif kind in ("expand", "collapse"):
                self.do_action(node, ("expand or contract", kind, "expand or collapse", *INVOKE))
            elif kind == "focus":
                if not node.queryComponent().grabFocus():
                    raise RuntimeError("aplicativo recusou o foco")
            elif kind == "set_value":
                value = action.get("value")
                if not isinstance(value, str) or len(value) > 20000:
                    raise ValueError("valor inválido")
                # Digitar como pessoa dispara as notificações do campo, como no agente Windows.
                self.focus_field(target)
                time.sleep(.15)
                # Sem leitura de volta, o foco confirmado é a única garantia de que Ctrl+A cai no campo certo.
                if info["text"] is None and "focused" not in self.read(node)["states"]:
                    raise RuntimeError("campo não recebeu o foco; nada foi digitado")
                self.keys(["ctrl", "a"])
                self.keys(["delete"])
                self.type_text(value)
                time.sleep(.15)
                # Sem interface Text (WebKitGTK) não há como ler de volta; a tela observada é a prova.
                if info["role"] != "password text" and info["text"] is not None:
                    lido = node.queryText().getText(0, -1) or ""
                    if lido.replace("\r\n", "\n").strip() != value.replace("\r\n", "\n").strip():
                        raise RuntimeError(f"valor digitado não confirmado; campo ficou com {lido[:80]!r}")
            else:
                raise ValueError("tipo de ação desconhecido: " + str(kind))
        return {"ok": True}

    def screenshot(self):
        self.available()
        _, area, monitor = self.current()
        width = area[2] - area[0]
        # grim escala sobre o tamanho lógico: o print sai com no máximo 1280 px, como no Windows.
        scale = min(1280, width) / width
        png = run_command(["grim", "-o", monitor["name"], "-s", repr(scale), "-"])
        return base64.b64encode(png).decode()

    def request(self, request):
        operation = request.get("operation")
        if operation == "observe":
            return self.observe()
        if operation == "act":
            return self.act(request["action"], request["observation_id"])
        if operation == "screenshot":
            return {"png": self.screenshot()}
        raise ValueError("operação inválida")


if __name__ == "__main__":
    windows_uia.main(LinuxDesktop)
