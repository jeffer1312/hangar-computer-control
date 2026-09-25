"""Cancelamento do cliente não deixa o laço enviando entrada remota."""
import asyncio
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from threading import Event
from types import SimpleNamespace
from unittest.mock import Mock, patch

import servidor_mcp
from laco_uia import Resultado


class Contexto:
    request_context = SimpleNamespace(meta=None)

    async def report_progress(self, *args, **kwargs):
        pass


class MCPTest(unittest.IsolatedAsyncioTestCase):
    async def test_caller_duration_limit_reaches_worker(self):
        with patch.object(servidor_mcp, "executar", return_value=Resultado()) as executar:
            await servidor_mcp.objetivo("teste", Contexto(), limite_segundos=600)
        self.assertEqual(executar.call_args.kwargs["limite_segundos"], 600)

    async def test_diagnostics_use_explicit_agent_configuration(self):
        desktop = Mock()
        desktop.observe.return_value = {"connected": True, "screen": {"width": 800, "height": 600}}
        desktop.screenshot.return_value = b"fake PNG"
        factory = Mock(return_value=desktop)
        with patch.dict(os.environ, HCC_AGENT_CONFIG="mock.json"), \
             patch.object(servidor_mcp.desktop_agent, "AgentSession", factory):
            self.assertIn("UIA, 800x600", servidor_mcp.estado())
            caminho = servidor_mcp.ver_tela().split(";", 1)[0]
        self.assertEqual(Path(caminho).read_bytes(), b"fake PNG")
        self.assertEqual(desktop.close.call_count, 2)
        factory.assert_called_with(config_path="mock.json")

    async def test_target_resolves_from_agents_dir(self):
        with tempfile.TemporaryDirectory() as pasta:
            for nome in ("delphi-02", "tardis"):
                Path(pasta, f"{nome}-agent.json").write_text("{}")
            padrao = str(Path(pasta, "delphi-02-agent.json"))
            with patch.dict(os.environ, HCC_AGENT_CONFIG=padrao), patch.dict(os.environ, {"HCC_AGENTS_DIR": ""}), \
                 patch.object(servidor_mcp, "executar", return_value=Resultado()) as executar:
                await servidor_mcp.objetivo("teste", Contexto(), alvo="tardis")
                self.assertEqual(executar.call_args.kwargs["config"], str(Path(pasta, "tardis-agent.json")))
                await servidor_mcp.objetivo("teste", Contexto())
                self.assertEqual(executar.call_args.kwargs["config"], padrao)
                with self.assertRaisesRegex(ValueError, "disponíveis: delphi-02, tardis"):
                    await servidor_mcp.objetivo("teste", Contexto(), alvo="winboat")
                with self.assertRaisesRegex(ValueError, "alvo desconhecido"):
                    servidor_mcp.estado(alvo="winboat")

    async def test_steps_written_to_hangar_progress_file(self):
        def executar(texto, passos, dados, cancelado, progresso, limite_segundos=240, config=None):
            progresso("observacao")
            return Resultado()

        ctx = Contexto()
        ctx.request_context = SimpleNamespace(meta={"claudecode/toolUseId": "toolu_abc"})
        with tempfile.TemporaryDirectory() as casa, patch.dict(os.environ, HOME=casa), \
             patch.object(servidor_mcp, "executar", side_effect=executar):
            await servidor_mcp.objetivo("teste", ctx)
            linhas = Path(casa, ".hangar/tool-progress/toolu_abc.jsonl").read_text().splitlines()
        self.assertEqual(json.loads(linhas[0])["message"], "lendo a tela")

    async def test_client_cancellation_reaches_worker(self):
        iniciado, encerrou = Event(), Event()

        def executar(texto, passos, dados, cancelado, progresso, limite_segundos=240, config=None):
            iniciado.set()
            cancelado.wait(2)
            if cancelado.is_set():
                encerrou.set()
            return Resultado()

        with patch.object(servidor_mcp, "executar", side_effect=executar):
            tarefa = asyncio.create_task(servidor_mcp.objetivo("teste", Contexto()))
            self.assertTrue(await asyncio.to_thread(iniciado.wait, 1))
            tarefa.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await tarefa
            self.assertTrue(await asyncio.to_thread(encerrou.wait, 1))


if __name__ == "__main__":
    unittest.main()
