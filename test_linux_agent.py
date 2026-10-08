"""Partes puras do agente Linux, sem Hyprland, AT-SPI nem dispositivos reais."""
import re
import unittest
from unittest.mock import patch

import linux_agent as la


def no(role, name="", states=("showing", "enabled"), actions=(), extents=(0, 0, 10, 10), text=None, filhos=()):
    return {"role": role, "name": name, "states": set(states), "actions": list(actions),
            "extents": extents, "text": text, "filhos": list(filhos)}


class Sumiu(Exception):
    pass


def ler(n):
    if n.get("sumiu"):
        raise Sumiu
    return n


class CoordenadasTest(unittest.TestCase):
    def test_monitor_com_escala_e_rotacao_em_coordenada_logica(self):
        self.assertEqual(la.monitor_rect({"x": 0, "y": 0, "width": 1920, "height": 1080, "scale": 1.25,
                                          "transform": 0}), [0, 0, 1536, 864])
        self.assertEqual(la.monitor_rect({"x": 1536, "y": 0, "width": 1920, "height": 1080, "scale": 1,
                                          "transform": 1}), [1536, 0, 2616, 1920])

    def test_monitor_de_referencia_e_o_da_janela_ativa(self):
        monitores = [{"id": 0, "focused": True}, {"id": 1, "focused": False}]
        self.assertEqual(la.reference_monitor(monitores, {"address": "0x1", "monitor": 1})["id"], 1)
        self.assertEqual(la.reference_monitor(monitores, {})["id"], 0)

    def test_janelas_relativas_ao_monitor_sem_ocultas(self):
        clientes = [{"address": "0xa", "title": "Editor", "pid": 7, "class": "gedit", "at": [1542, 51],
                     "size": [800, 600], "mapped": True, "hidden": False},
                    {"address": "0xb", "title": "x", "pid": 8, "class": "y", "at": [0, 0], "size": [1, 1],
                     "mapped": True, "hidden": True}]
        self.assertEqual(la.windows_from(clientes, (1536, 0)),
                         [{"id": "w0xa", "name": "Editor", "process_id": 7, "class_name": "gedit",
                           "rect": [6, 51, 806, 651]}])

    def test_controle_soma_posicao_da_janela(self):
        self.assertEqual(la.element_rect((10, 20, 30, 40), [6, 51, 806, 651]), [16, 71, 46, 111])
        self.assertIsNone(la.element_rect((-1, -1, -1, -1), [0, 0, 9, 9]))


class ArvoreTest(unittest.TestCase):
    janela = [100, 100, 600, 500]

    def montar(self, raizes):
        return la.walk(raizes, ler, lambda n: n["filhos"], self.janela, (Sumiu,))

    def test_monta_controles_no_vocabulario_da_uia(self):
        campo = no("entry", "Nome", ("showing", "enabled", "editable", "focusable"), text="Ana")
        senha = no("password text", "Senha", ("showing", "enabled", "editable"), text="•••")
        botao = no("push button", "Salvar", actions=["click"])
        caixa = no("check box", "Lembrar", ("showing", "enabled", "checkable", "checked"), actions=["Toggle", "Press"])
        item = no("list item", "Downloads", ("showing", "enabled", "selectable"), actions=["Toggle"])
        controles, nos, cortou = self.montar([no("panel", filhos=[campo, senha, botao, caixa, item])])
        por_nome = {c["name"]: c for c in controles}
        self.assertEqual([c["id"] for c in controles], ["e0", "e1", "e2", "e3", "e4"])
        self.assertEqual(nos[0], campo)
        self.assertFalse(cortou)
        self.assertEqual((por_nome["Nome"]["role"], por_nome["Nome"]["value"], por_nome["Nome"]["actions"]),
                         ("Edit", "Ana", ["set_value", "focus"]))
        self.assertEqual(por_nome["Nome"]["rect"], [100, 100, 110, 110])
        self.assertTrue(por_nome["Senha"]["password"])
        self.assertIsNone(por_nome["Senha"]["value"])
        self.assertEqual(por_nome["Salvar"]["actions"], ["invoke"])
        self.assertEqual((por_nome["Lembrar"]["actions"], por_nome["Lembrar"]["value"]), (["toggle"], "on"))
        self.assertEqual((por_nome["Downloads"]["actions"], por_nome["Downloads"]["value"]),
                         (["select"], "not selected"))

    def test_subarvore_oculta_fora_da_janela_e_no_que_sumiu_ficam_de_fora(self):
        oculto = no("panel", states=("enabled",), filhos=[no("push button", "Escondido", actions=["click"])])
        rolado = no("push button", "Rolado", actions=["click"], extents=(0, -200, 50, 20))
        sumiu = {**no("push button", "Fantasma"), "sumiu": True}
        controles, _, _ = self.montar([oculto, rolado, sumiu, no("label", "Pronto")])
        self.assertEqual([c["name"] for c in controles], ["Pronto"])

    def test_no_cujos_filhos_somem_continua_valendo(self):
        def filhos(n):
            if n["name"] == "Lista":
                raise Sumiu
            return n["filhos"]
        controles, _, _ = la.walk([no("list", "Lista", actions=["click"])], ler, filhos, self.janela, (Sumiu,))
        self.assertEqual([c["name"] for c in controles], ["Lista"])

    def test_corte_por_limite_e_informado(self):
        _, _, cortou = la.walk([no("label", str(i)) for i in range(5)], ler, lambda n: [], self.janela, limit=3)
        self.assertTrue(cortou)

    def test_frame_pelo_titulo_senao_o_ativo(self):
        frames = [("Outra", {"showing"}), ("Editor", {"showing"})]
        self.assertEqual(la.choose_frame(frames, "Editor"), 1)
        self.assertEqual(la.choose_frame([("a", set()), ("b", {"active"})], "x"), 1)
        self.assertIsNone(la.choose_frame([("a", set()), ("b", set())], "x"))


class EntradaTest(unittest.TestCase):
    def test_lua_string_nao_deixa_escapar_do_literal(self):
        texto = 'app"); hl.dsp.exit() --\n\\ çã ]]'
        literal = la.lua_string(texto)
        self.assertRegex(literal, r'^"[A-Za-z0-9 ._/\\-]*"$')
        bytes_ = re.sub(rb"\\(\d{3})", lambda m: bytes([int(m[1])]), literal[1:-1].encode())
        self.assertEqual(bytes_.decode(), texto)

    def test_teclas_viram_codigos_evdev(self):
        self.assertEqual(la.key_codes(["Ctrl", "a"]), [29, 30])
        self.assertEqual(la.key_codes(["F13", "Enter", "0"]), [183, 28, 11])
        with self.assertRaisesRegex(ValueError, "desconhecida"):
            la.key_codes(["ç"])

    def test_keys_pressiona_e_solta_em_ordem_inversa(self):
        with patch.object(la, "run_command") as run:
            la.LinuxDesktop.keys(["ctrl", "shift", "t"])
        run.assert_called_once_with(["ydotool", "key", "29:1", "42:1", "20:1", "20:0", "42:0", "29:0"])

    def test_texto_com_alvo_desconhecido_e_programa_inexistente_falham(self):
        desktop = la.LinuxDesktop.__new__(la.LinuxDesktop)
        desktop.elements, desktop.windows = {}, {}
        with patch.object(desktop, "validate_observation"), patch.object(la, "run_command") as run, \
                patch.object(la, "dispatch") as dispatch:
            desktop.observation_id = "o"
            with self.assertRaisesRegex(ValueError, "não pertence"):
                desktop.act({"type": "text", "target": "e9", "value": "x"}, "o")
            with self.assertRaisesRegex(ValueError, "não encontrado"):
                desktop.act({"type": "launch", "application": "programa-que-nao-existe-hcc"}, "o")
        run.assert_not_called()
        dispatch.assert_not_called()

    def test_mouse_soma_origem_do_monitor_e_rolagem_desce_com_delta_positivo(self):
        desktop = la.LinuxDesktop.__new__(la.LinuxDesktop)
        desktop.area = [1536, 0, 3456, 1080]
        with patch.object(la, "dispatch") as dispatch, patch.object(la, "run_command") as run:
            desktop.mouse({"x": 10, "y": 20, "button": "left", "mode": "scroll", "delta": 3})
            with self.assertRaisesRegex(ValueError, "fora da tela"):
                desktop.mouse({"x": 1920, "y": 0, "button": "left", "mode": "click"})
        dispatch.assert_called_once_with("hl.dsp.cursor.move({ x = 1546, y = 20 })")
        run.assert_called_once_with(["ydotool", "mousemove", "--wheel", "-x", "0", "-y", "-3"])


if __name__ == "__main__":
    unittest.main()
