"""Laço observar -> Jev -> agir testado sem rede nem desktop real."""
import json
import os
import unittest
from unittest.mock import patch

import laco_uia as uia


def arvore(valor="", ref="o1", elementos=None):
    return {"observation_id": ref, "connected": True, "foreground": "w1",
            "windows": [{"id": "w1", "name": "Editor"}], "screen": {"width": 800, "height": 600},
            "elements": elementos if elementos is not None else [
                {"id": "e0", "name": "Nome", "role": "Edit", "value": valor, "enabled": True, "actions": ["set_value"]},
                {"id": "e1", "name": "Salvar", "role": "Button", "enabled": True, "actions": ["invoke"]}]}


def jev(opcao, p=.9, risco=.05, nome="action"):
    return {"answers": {nome: {"choice": opcao, "probabilities": {opcao: p}}, "risky": {"noul": risco}}}


def llm(**campos):
    return {"choices": [{"message": {"content": json.dumps({"interpretacao": "tela vista", "acoes": [], **campos})}}]}


class Sessao:
    def __init__(self, arvores, **_):
        self.arvores, self.acoes, self.prints, self.closed = list(arvores), [], 0, False

    def observe(self):
        return self.arvores.pop(0) if len(self.arvores) > 1 else self.arvores[0]

    def act(self, acao, ref):
        self.acoes.append((acao, ref))
        return {"ok": True}

    def screenshot(self):
        self.prints += 1
        return b"PNG"

    def close(self):
        self.closed = True


def rodar(arvores, respostas, dados=None, **kw):
    sessao = Sessao(arvores)
    estados = []

    def post(url, corpo, *_):
        if url == uia.JEV:
            estados.append(corpo["state"])
        return respostas.pop(0)
    env = {"TYPESAFE_API_KEY": "k", "HCC_AGENT_CONFIG": "cfg.json", "LLM_PROXY_KEY": "k2"}
    with patch.dict(os.environ, env), patch.object(uia, "AgentSession", lambda **_: sessao), \
         patch.object(uia, "_post", side_effect=post):
        resultado = uia.executar("preencher Nome com Ana", max_passos=3, dados=dados or {"nome": "Ana"}, **kw)
    return resultado, sessao, estados


class LacoTest(unittest.TestCase):
    def test_jev_chooses_action_from_tree_then_done(self):
        acoes = uia.candidatos(arvore(), {"nome": "Ana"})
        alvo = str(next(i for i, a in enumerate(acoes) if a["type"] == "set_value"))
        resultado, sessao, estados = rodar([arvore(), arvore("Ana", "o2")], [jev(alvo), jev("DONE", .95)])
        self.assertTrue(resultado.ok, resultado.motivo)
        self.assertEqual(sessao.acoes, [({"type": "set_value", "target": "e0", "value": "Ana"}, "o1")])
        self.assertEqual(estados[1]["recentActions"][0]["screenChanged"], True)
        self.assertEqual(estados[1]["elements"][0]["value"], "Ana")
        self.assertTrue(sessao.closed)

    def test_llm_called_once_when_jev_blocked_and_its_action_is_offered_to_jev(self):
        respostas = [jev("BLOCKED"), llm(acoes=[{"type": "text", "value": "Ana"}]), None, jev("DONE", .95)]
        base = len(uia.candidatos(arvore(elementos=[]), {}))
        respostas[2] = jev(str(base))
        resultado, sessao, estados = rodar([arvore(elementos=[]), arvore("Ana", "o2")], respostas, dados={})
        self.assertTrue(resultado.ok, resultado.motivo)
        self.assertEqual(sessao.prints, 1)
        self.assertEqual(sessao.acoes[0][0], {"type": "text", "value": "Ana"})
        self.assertEqual(estados[1]["hint"], "tela vista")

    def test_low_probability_after_help_stops_without_acting(self):
        resultado, sessao, _ = rodar([arvore()], [jev("0", .2), llm(), jev("0", .2)])
        self.assertFalse(resultado.ok)
        self.assertIn("sem ação segura", resultado.motivo)
        self.assertEqual(sessao.acoes, [])

    def test_risky_action_is_not_executed(self):
        resultado, sessao, _ = rodar([arvore()], [jev("0", .9, risco=.8)])
        self.assertIn("arriscada", resultado.motivo)
        self.assertEqual(sessao.acoes, [])

    def test_batches_above_limit_merge_single_winner(self):
        elementos = [{"id": f"e{i}", "name": f"b{i}", "role": "Button", "enabled": True, "actions": ["invoke"]}
                     for i in range(300)]
        arv = arvore(elementos=elementos)
        acoes = uia.candidatos(arv, {})
        resposta = {"answers": {"batch_0": {"choice": "BLOCKED", "probabilities": {"BLOCKED": .9}},
                                "batch_1": {"choice": "260", "probabilities": {"260": .8}}, "risky": {"noul": 0}}}
        with patch.object(uia, "_post", return_value=resposta) as post:
            decisao = uia.decidir({}, acoes, arv, "k", 10)
        self.assertEqual(decisao, {"choice": "260", "probability": .8, "risky": 0.0})
        perguntas = post.call_args.args[1]["questions"]
        self.assertEqual(len(perguntas["batch_0"]["criteria"]), uia.LOTE + 3)

    def test_llm_effort_sent_only_when_configured(self):
        resposta = {"choices": [{"message": {"content": '{"interpretacao": "x"}'}}]}
        with patch.dict(os.environ, {"LLM_PROXY_KEY": "k"}), patch.object(uia, "_post", return_value=resposta) as post:
            with patch.object(uia, "LLM_ESFORCO", "high"):
                uia.ajudar({}, None, 10)
            self.assertEqual(post.call_args.args[1]["reasoning_effort"], "high")
            with patch.object(uia, "LLM_ESFORCO", None):
                uia.ajudar({}, None, 10)
            self.assertNotIn("reasoning_effort", post.call_args.args[1])


if __name__ == "__main__":
    unittest.main()
