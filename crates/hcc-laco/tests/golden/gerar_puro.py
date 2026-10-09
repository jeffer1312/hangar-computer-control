"""Captura puro.json a partir das funções puras de laco_uia.py (referência Python).

Rodar da raiz do repo: uv run python crates/hcc-laco/tests/golden/gerar_puro.py
"""

import json
import re
import sys
from pathlib import Path

RAIZ = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(RAIZ))

import laco_uia as uia  # noqa: E402

DADOS = {"nome": "José d'Ávila", "senha": "s3gr3d0", "ativo": True, "inativo": False, "idade": 42,
         "peso": 1.0, "taxa": 0.1, "grande": 1e16, "pequeno": 1e-05, "token": 1234, "pin": "",
         "vazio": None, "lista": [1]}
SEGREDOS = [str(v) for k, v in DADOS.items() if re.search(r"senha|password|passwd|pin|token", str(k), re.I)]


def janela(id_, nome, classe, rect, pid=10):
    return {"id": id_, "name": nome, "process_id": pid, "class_name": classe, "rect": rect}


def el(id_, nome, papel, acoes=(), valor=None, rect=None, enabled=True, focused=False, password=False):
    e = {"id": id_, "name": nome, "role": papel, "value": valor, "enabled": enabled, "rect": rect,
         "focused": focused, "actions": list(acoes)}
    if password:
        e["password"] = True
    return e


def obs(nome, janelas, elementos, foreground="w1", largura=1920, truncated=False):
    return nome, {"observation_id": f"o-{nome}", "connected": True, "session_id": 1, "foreground": foreground,
                  "windows": janelas, "elements": elementos, "truncated": truncated, "timestamp": 1700000000.5,
                  "screen": {"width": largura, "height": 1080}}


FIXTURES = [
    obs("menu_vcl", [janela("w1", "Sistema - TMainForm", "TMainForm", [0, 0, 1920, 1040]),
                     janela("w2", "Bloco de notas", "Notepad", [100, 100, 900, 700], 20)], [
        el("e0", "Cadastro", "MenuItem", ["expand", "invoke"], rect=[180, 23, 240, 42]),
        el("e1", "&Nome:", "Edit", ["set_value", "focus"], valor="a\"b'c\\d\ne\tf", rect=[10, 60, 300, 80], focused=True),
        el("e2", "Salvar", "Button", ["invoke"], rect=[10, 90, 90, 110]),
        el("e3", "Lista", "List", ["scroll", "select"], rect=[10, 120, 300, 400]),
        el("e4", "Ajuda", "Hyperlink", ["focus"], rect=[400, 60, 450, 75]),
        el("e5", "Desabilitado", "Button", ["invoke"], rect=[10, 410, 90, 430], enabled=False),
        el("e6", "Opção", "CheckBox", ["toggle"], valor="", rect=None),
        el("e7", "Árvore", "TreeItem", ["expand", "collapse"], rect=[500, 100, 600, 120]),
        el("e8", "Imagem", "Image", [], rect=[700, 100, 701, 101]),
        el("e9", "só foco", "Edit", ["focus"], valor="valor com nbsp"),
    ]),
    obs("dialogo_desenhado", [janela("w1", "Atenção", "TMessageForm", [600, 400, 1300, 650])], [
        el("e0", "Não foi possível gravar o registro solicitado.", "Text", [], rect=[620, 430, 1280, 460]),
        el("e1", "", "Pane", [], rect=[600, 400, 1300, 650]),
    ]),
    obs("salvar_como", [janela("w1", "Salvar como", "#32770", [200, 150, 1100, 750]),
                        janela("w2", "Relatório - TMainForm", "TMainForm", [0, 0, 1920, 1040])], [
        el("e0", "Nome do arquivo:", "Edit", ["set_value"], valor="relatorio.pdf", rect=[300, 600, 900, 625]),
        el("e1", "Senha:", "Edit", ["set_value"], rect=[300, 640, 900, 665], password=True),
        el("e2", "&Salvar", "Button", ["invoke"], rect=[800, 700, 880, 725]),
        el("e3", "Tipo:", "ComboBox", ["expand", "select"], valor="PDF (*.pdf)", rect=[300, 670, 900, 690]),
        el("e4", "Eco", "Edit", ["set_value"], valor="s3gr3d0", rect=[300, 500, 900, 520]),
        el("e5", "Eco parcial", "Text", [], valor="digitado: s3gr3d0 ok", rect=[300, 530, 900, 550]),
        el("e6", "Cancelar", "Button", ["invoke"], rect=[890, 700, 970, 725], focused=True),
    ], largura=1280),
    obs("trezentos_botoes", [janela("w1", "Grade", "TMainForm", [0, 0, 1920, 1040])],
        [el(f"e{i}", f"Botão {i}", "Button", ["invoke"] if i % 3 else ["focus"],
            rect=[i, 0, i + 10, 10] if i % 2 else None) for i in range(300)], truncated=True),
    obs("barra_tarefas", [janela("w1", "Barra", "Shell_TrayWnd", [0, 1040, 1920, 1080]),
                          janela("w2", "Program Manager", "Progman", [0, 0, 1920, 1080]),
                          janela("w3", "Planilha", "XLMAIN", [0, 0, 1920, 1040])], [
        el("e0", "Iniciar", "Button", ["invoke"], rect=[0, 1040, 48, 1080]),
        el("e1", "Planilha", "Button", ["invoke", "focus"], rect=[60, 1040, 108, 1080]),
        el("e2", "Relógio", "Text", ["focus"], valor="12:00", rect=[1800, 1040, 1920, 1080]),
    ]),
    obs("barra_titulo", [janela("w1", "Editor", "TMainForm", [0, 0, 1920, 1040])], [
        el("e0", "", "TitleBar", [], rect=[16, 0, 1920, 23]),
        el("e1", "Close", "Button", ["invoke"], rect=[1872, 0, 1920, 22]),
        el("e2", "Fechar", "Button", ["invoke"], rect=[1872, 30, 1920, 52]),
        el("e3", "Minimizar", "Button", ["invoke"], rect=[1776, 0, 1824, 22]),
        el("e4", "fechar", "Button", ["invoke"], rect=[1873, 1, 1921, 24]),
        el("e5", "Relatório", "TabItem", ["select"], rect=[0, 40, 120, 60]),
    ]),
]

RECENTES = [
    {"action": f"invoke Button 'Passo {i}'", "result": "ok", "screenChanged": i % 2 == 0,
     "target_rect": [i, i, i + 5, i + 5], "clique": ["click", i, i]} for i in range(9)
] + [
    {"action": "WAIT", "result": "esperou 0,5 s", "screenChanged": None},
    {"action": "set_value Edit 'Nome' = 'Ana'", "result": "NOT executed: comando expirou", "screenChanged": False},
    {"action": "mouse click (10,20)", "result": "ok", "screenChanged": None, "clique": ["click", 10, 20]},
]

EXTRAS = [
    {"type": "text", "value": "s3gr3d0"},
    {"type": "text", "value": "José d'Ávila"},
    {"type": "text", "value": "aspas \"duplas\" e 'simples'"},
    {"type": "set_value", "target": "inexistente", "value": "x"},
    {"type": "text", "value": "nbsp\u00a0ctl\x01zw\u200bemoji\U0001F600 \\ fim\u2028"},
    {"type": "keys", "keys": ["Alt", "F4"]},
    {"type": "keys", "keys": ["ctrl", "s"]},
    {"type": "launch", "application": "notepad.exe", "args": ["a b.txt", "-x"]},
    {"type": "launch", "application": "calc.exe"},
    {"type": "mouse", "x": 10, "y": 20, "button": "left", "mode": "click", "rotulo": "link Mais"},
    {"type": "mouse", "x": 10, "y": 20, "button": "right", "mode": "double"},
    {"type": "activate", "target": "w2"},
    {"type": "scroll", "target": "e3", "direction": "down", "amount": "large"},
]

PARA_TELA = [
    ("clique em 696,366", [{"type": "mouse", "x": 696, "y": 366, "button": "left", "mode": "click"},
                           {"type": "mouse", "x": 695, "y": 10, "button": "left", "mode": "click"},
                           {"type": "mouse", "x": 1, "y": 3, "button": "left", "mode": "drag", "x2": 696, "y2": 366},
                           {"type": "mouse", "x": 696, "y": 366, "button": "left", "mode": "drag", "x2": 3, "y2": 5},
                           {"type": "keys", "keys": ["Enter"]}]),
    ("arrastar de 10 , 20 até 30,40", [{"type": "mouse", "x": 10, "y": 20, "button": "left", "mode": "drag",
                                        "x2": 30, "y2": 41},
                                       {"type": "mouse", "x": 30, "y": 40, "button": "left", "mode": "move"}]),
]

DENTRO = [
    {"type": "mouse", "x": 0, "y": 0, "button": "left", "mode": "click"},
    {"type": "mouse", "x": 1919, "y": 1039, "button": "left", "mode": "click"},
    {"type": "mouse", "x": 1920, "y": 500, "button": "left", "mode": "click"},
    {"type": "mouse", "x": 500, "y": 1040, "button": "left", "mode": "click"},
    {"type": "mouse", "x": 650, "y": 450, "button": "left", "mode": "click"},
    {"type": "mouse", "x": -1, "y": 5, "button": "left", "mode": "click"},
    {"type": "keys", "keys": ["Enter"]},
]


def mascarar(estado):
    """Decisão 4 e C11: segredo nunca vai ao Jev, em nenhum campo; chave secreta de `data` vira "<senha>"."""
    sec = sorted({s for s in SEGREDOS if s}, key=lambda s: (-len(s), s))
    padrao = re.compile("|".join(map(re.escape, sec)))

    def andar(v):
        if isinstance(v, str):
            return padrao.sub("<senha>", v)
        if isinstance(v, list):
            return [andar(x) for x in v]
        if isinstance(v, dict):
            return {k: andar(x) for k, x in v.items()}
        return v
    estado = andar(estado)
    estado["data"] = {k: "<senha>" if re.search(r"senha|password|passwd|pin|token", k, re.I) else v
                      for k, v in estado["data"].items()}
    return estado


def capturar(nome, arvore):
    arv = {**arvore, "segredos": SEGREDOS}
    acoes = uia.candidatos(arv, DADOS)
    dica = "dica de teste" if nome != "trezentos_botoes" else ""
    para_tela = []
    for goal, lista in PARA_TELA:
        objs = [uia.Acao(**a) for a in lista]
        uia.para_tela(objs, arv, goal)
        para_tela.append({"goal": goal, "antes": lista, "depois": [a.model_dump(exclude_none=True) for a in objs]})
    fg, cont = uia.assinatura(arv)
    return {
        "nome": nome,
        "obs": arvore,
        "dica": dica,
        "candidatos": [{"acao": a, "descricao": uia.descrever(a, arv), "sem_valor": uia.descrever(a, arv, com_valor=False)}
                       for a in acoes],
        "intencoes": [list(uia.intencao(a, arv)) for a in acoes],
        "fecha_janela": [uia.fecha_janela(a, arv) for a in acoes],
        "extras": [{"acao": a, "descricao": uia.descrever(a, arv), "sem_valor": uia.descrever(a, arv, com_valor=False),
                    "intencao": list(uia.intencao(a, arv)), "fecha_janela": uia.fecha_janela(a, arv)} for a in EXTRAS],
        "assinatura": [fg, sorted(list(k) for k, n in cont.items() for _ in range(n))],
        "estado_compacto": mascarar(uia.estado_compacto("preencher o cadastro", DADOS, arv, RECENTES, dica)),
        "sem_controles": uia.sem_controles(arv),
        "para_tela": para_tela,
        "dentro": [{"acao": a, "dentro": uia.dentro(a, arv)} for a in DENTRO],
    }


def main():
    saida = {
        "_mudanca": "estado_compacto mascarado depois da captura (Decisão 4 e C11): valor de chave secreta em `data` "
                    "vira \"<senha>\", e valor secreto ecoado em qualquer texto do estado vira <senha>. O Python mandava cru.",
        "dados": DADOS,
        "segredos": SEGREDOS,
        "recentes": RECENTES,
        "fixtures": [capturar(n, a) for n, a in FIXTURES],
    }
    destino = Path(__file__).with_name("puro.json")
    destino.write_text(json.dumps(saida, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(destino)


if __name__ == "__main__":
    main()
