"""MCP para operar um desktop Windows pela árvore de acessibilidade.

A ferramenta principal é `objetivo`: o agente chamador descreve o que quer em
português e o laço interno resolve — observar, decidir, agir — sem devolver imagem
para quem chamou. `ver_tela` e `estado` existem para diagnóstico quando o laço
para e alguém precisa entender por quê.
"""

from __future__ import annotations

import asyncio
import os
import tempfile
import time
from pathlib import Path
from threading import Event

from mcp.server.mcpserver import MCPServer, Context

from desktop_agent import AgentSession
from laco_uia import executar

mcp = MCPServer("hangar-computer-control")


def alvos() -> dict[str, Path]:
    pasta = os.environ.get("HCC_AGENTS_DIR") or os.path.dirname(os.environ.get("HCC_AGENT_CONFIG", ""))
    if not pasta:
        return {}
    return {p.name.removesuffix("-agent.json"): p for p in sorted(Path(pasta).glob("*-agent.json"))}


def config_do(alvo: str | None) -> str | None:
    if not alvo:
        return os.environ.get("HCC_AGENT_CONFIG")
    disponiveis = alvos()
    if alvo not in disponiveis:
        raise ValueError(f"alvo desconhecido: {alvo}; disponíveis: {', '.join(disponiveis) or 'nenhum'}")
    return str(disponiveis[alvo])


def _padrao() -> str:
    return Path(os.environ.get("HCC_AGENT_CONFIG", "")).name.removesuffix("-agent.json") or "nenhum"


ALVOS = (f"\n\n`alvo` escolhe o Windows controlado. Disponíveis: {', '.join(alvos()) or 'nenhum'}; "
         f"sem `alvo`, usa o padrão ({_padrao()}).")


@mcp.tool(description=(
    """Realiza um objetivo no desktop Windows, do começo ao fim.

    Descreva o que quer em português, como pediria a uma pessoa ("abrir o
    Bloco de Notas e digitar olá"). O laço interno lê a árvore de acessibilidade,
    o Jev decide cada passo e o agente executa até concluir ou desistir.

    `dados` leva valores conhecidos (nome, texto, caminho) que o laço pode digitar.
    Só conclui com evidência ou devolve um impedimento específico. Ação arriscada
    (apagar, sobrescrever, enviar) não autorizada no texto para e volta pra você.
    limite_segundos é o orçamento total; max_passos limita os ciclos internos.""" + ALVOS))
async def objetivo(texto: str, ctx: Context, max_passos: int = 12,
                   dados: dict | None = None, limite_segundos: float = 240, alvo: str | None = None) -> str:
    config = config_do(alvo)
    cancelado = Event()
    loop = asyncio.get_running_loop()
    etapas = 0

    def progresso(etapa):
        nonlocal etapas
        etapas += 1
        future = asyncio.run_coroutine_threadsafe(ctx.report_progress(etapas, message=etapa), loop)
        try:
            future.result(timeout=2)
        except Exception:
            future.cancel()

    try:
        resultado = await asyncio.to_thread(executar, texto, max_passos, dados, cancelado, progresso,
                                           limite_segundos=limite_segundos, config=config)
        return resultado.resumo()
    finally:
        cancelado.set()


@mcp.tool(description=(
    """Captura a tela do desktop e devolve o caminho do arquivo PNG.

    Para quando o `objetivo` parou e você precisa olhar o que há na tela.
    Cite o caminho devolvido na sua resposta para que o usuário veja a imagem.""" + ALVOS))
def ver_tela(alvo: str | None = None) -> str:
    inicio = time.monotonic()
    sessao = AgentSession(config_path=config_do(alvo))
    try:
        caminho = Path(tempfile.mkdtemp(prefix="hcu-tela-")) / "tela.png"
        caminho.write_bytes(sessao.screenshot())
        return f"{caminho}; {time.monotonic() - inicio:.2f}s"
    finally:
        sessao.close()


@mcp.tool(description="Diz se o desktop está acessível e em qual resolução." + ALVOS)
def estado(alvo: str | None = None) -> str:
    sessao = None
    try:
        sessao = AgentSession(config_path=config_do(alvo))
        observacao = sessao.observe()
        if not observacao.get("connected"):
            return "indisponível: agente desconectado"
        tela = observacao["screen"]
        return f"disponível via UIA, {tela['width']}x{tela['height']}"
    except Exception as e:
        return f"indisponível: {e}"
    finally:
        if sessao is not None:
            sessao.close()


def main():
    mcp.run()


if __name__ == "__main__":
    main()
