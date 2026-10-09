"""Captura clientes.json: prompts e corpos de pedido do Jev e do LLM, gravados de laco_uia.py.

Rodar da raiz do repo: .venv/bin/python crates/hcc-laco/tests/golden/gerar_clientes.py
"""

import base64
import json
import os
import struct
import sys
import zlib
from pathlib import Path

RAIZ = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(RAIZ))

import laco_uia as uia  # noqa: E402

CHAVE = "KEY"
ESTADO = {"goal": "preencher o cadastro com ação", "data": {"nome": "Ana", "peso": 1.0, "taxa": 1e-05,
                                                             "grande": 1e16, "idade": 42, "ativo": True},
          "window": "Cadastro - TMainForm", "otherWindows": ["Relatório"],
          "elements": [{"id": "e0", "role": "Edit", "name": "Nome:", "value": "a\"b\\c\nç"}],
          "recentActions": [{"action": "invoke Button 'Salvar'", "result": "ok", "screenChanged": None}]}


def janela(id_, nome, classe="TMainForm"):
    return {"id": id_, "name": nome, "process_id": 10, "class_name": classe, "rect": [0, 0, 1920, 1040]}


def el(id_, nome, papel, acoes, rect=None):
    return {"id": id_, "name": nome, "role": papel, "value": None, "enabled": True, "rect": rect,
            "focused": False, "actions": acoes}


def arvore(elementos):
    return {"observation_id": "o1", "connected": True, "session_id": 1, "foreground": "w1",
            "windows": [janela("w1", "Cadastro - TMainForm")], "elements": elementos, "truncated": False,
            "timestamp": 1700000000.5, "screen": {"width": 1920, "height": 1080}, "segredos": []}


NOME = arvore([el("e0", "Nome:", "ComboBox", ["set_value"]), el("e1", "Nome", "Edit", ["set_value"]),
               el("e2", "Salvar", "Button", ["invoke"])])
TREZENTOS = arvore([el(f"e{i}", f"Botão {i}", "Button", ["invoke"], [i, 0, i + 10, 10]) for i in range(300)])


class Gravador:
    def __init__(self, respostas):
        self.respostas = list(respostas)
        self.pedidos = []

    def __call__(self, url, corpo, cabecalhos, timeout):
        self.pedidos.append({"url": url, "headers": cabecalhos, "body": json.loads(json.dumps(corpo))})
        r = self.respostas.pop(0)
        if isinstance(r, Exception):
            raise r
        return r


def gravar(respostas, f, *args):
    g = Gravador(respostas)
    uia._post = g
    return f(*args), g.pedidos


def jev(perguntas):
    return {"answers": perguntas}


def caso_decidir(nome, arv, respostas):
    acoes = uia.candidatos(arv, {"nome": "Ana"})
    resultado, pedidos = gravar(respostas, uia.decidir, ESTADO, acoes, arv, CHAVE, 30)
    if "top" in resultado:
        resultado["top"] = [list(t) for t in resultado["top"]]
    return {"nome": nome, "estado": ESTADO,
            "candidatos": [{"acao": a, "descricao": uia.descrever(a, arv), "intencao": list(uia.intencao(a, arv))}
                           for a in acoes],
            "respostas": respostas, "pedidos": pedidos, "resultado": resultado}


def png_2x2():
    def bloco(tipo, dados):
        return struct.pack(">I", len(dados)) + tipo + dados + struct.pack(">I", zlib.crc32(tipo + dados))
    linhas = b"".join(b"\x00" + b"\xff\x00\x00" * 2 for _ in range(2))
    return (b"\x89PNG\r\n\x1a\n" + bloco(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 2, 0, 0, 0))
            + bloco(b"IDAT", zlib.compress(linhas)) + bloco(b"IEND", b""))


def caso_ajudar(nome, esforco, conteudo):
    uia.LLM_MODELO, uia.LLM_ESFORCO = "gpt-5.6-luna", esforco
    os.environ["LLM_PROXY_KEY"] = CHAVE
    png = png_2x2()
    resposta = {"choices": [{"message": {"content": conteudo}}]}
    ajuda, pedidos = gravar([resposta], uia.ajudar, ESTADO, png, 45)
    return {"nome": nome, "estado": ESTADO, "png": base64.b64encode(png).decode(), "modelo": "gpt-5.6-luna",
            "esforco": esforco, "resposta": resposta, "pedidos": pedidos,
            "resultado": ajuda.model_dump(exclude_none=True)}


def main():
    acao = {"action": {"choice": "2", "probabilities": {"2": .3, "0": .2, "1": .25, "DONE": .125, "WAIT": .1,
                                                        "BLOCKED": .025}}, "risky": {"noul": .1}}
    lote = {"batch_0": {"choice": "5", "probabilities": {"5": .6}}, "batch_1": {"choice": "260",
                                                                                 "probabilities": {"260": .7}},
            "risky": {"noul": .05}}
    final = {"action": {"choice": "260", "probabilities": {"5": .2, "260": .8}}, "risky": {"noul": .2}}
    mouse = ('```json\n{"interpretacao": "Caixa \\"Atenção\\" com botão Não.", "acoes": [{"type": "mouse", "x": 1, '
             '"y": 1, "button": "left", "mode": "click", "rotulo": "botão Não"}, {"type": "keys", "keys": '
             '["Escape"]}]}\n```')
    plano = '{"interpretacao": "Falta o login.", "impedimento": "faltam credenciais"}'
    proposta = "invoke Button 'Excluir'"
    risco, pedidos_risco = gravar([jev({"risky": {"noul": .9}})], uia.arriscado, ESTADO, proposta, CHAVE, 30)
    saida = {
        "_nota": "Respostas do Jev/LLM são roteiros deste arquivo; pedidos e resultados vêm do Python. "
                 "Chave real trocada por \"KEY\".",
        "jev_url": uia.JEV,
        "llm_url": uia.LLM_URL,
        "prompts": {"DESFECHOS": uia.DESFECHOS, "REGRAS": uia.REGRAS, "RISCO_PERGUNTA": uia.RISCO_PERGUNTA,
                    "AJUDA": uia.AJUDA, "AJUDA_SCHEMA": json.dumps(uia.Ajuda.model_json_schema())},
        "decidir": [
            caso_decidir("intencao_dividida", NOME, [jev(acao)]),
            # Empate de somas: o max() do Python fica com o PRIMEIRO grupo máximo.
            caso_decidir("empate_primeiro_grupo", NOME, [jev({"action": {"probabilities": {"2": .4, "0": .2, "1": .2}},
                                                              "risky": {"noul": .3}})]),
            caso_decidir("trezentos_torneio", TREZENTOS, [jev(lote), jev(final)]),
            caso_decidir("trezentos_wait", TREZENTOS, [jev({"batch_0": {"choice": "BLOCKED", "probabilities": {}},
                                                            "batch_1": {"choice": "WAIT",
                                                                        "probabilities": {"WAIT": .4}}})]),
            caso_decidir("trezentos_bloqueado", TREZENTOS, [jev({"batch_0": {"choice": "BLOCKED"},
                                                                 "batch_1": {"choice": "BLOCKED"}})]),
        ],
        "arriscado": {"estado": ESTADO, "proposta": proposta, "pedidos": pedidos_risco, "resultado": risco,
                      "resposta": jev({"risky": {"noul": .9}})},
        "ajudar": [caso_ajudar("cercado_com_esforco", "high", mouse), caso_ajudar("sem_esforco", None, plano)],
    }
    destino = Path(__file__).with_name("clientes.json")
    destino.write_text(json.dumps(saida, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(destino)


if __name__ == "__main__":
    main()
