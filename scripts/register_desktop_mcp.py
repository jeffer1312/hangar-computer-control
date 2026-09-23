"""Registra o MCP desktop no cliente usado pelo Hangar, sem mudar o Hangar."""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import tomllib

START = "# >>> hangar-computer-control: desktop"
END = "# <<< hangar-computer-control: desktop"
ROOT = Path(__file__).resolve().parent.parent


def codex_document(original, python, server, config):
    parsed = tomllib.loads(original)
    owned = re.compile(re.escape(START) + r".*?" + re.escape(END) + r"\n?", re.S)
    if "desktop" in parsed.get("mcp_servers", {}) and not owned.search(original):
        raise ValueError("já existe MCP desktop não gerenciado por este instalador")
    block = "\n".join([
        START, "[mcp_servers.desktop]", "command = " + json.dumps(str(python)),
        "args = " + json.dumps([str(server)]), 'env_vars = ["TYPESAFE_API_KEY"]',
        "tool_timeout_sec = 300", "env = { HCC_AGENT_CONFIG = " + json.dumps(str(config)) + " }",
        END, "",
    ])
    result = owned.sub(lambda _: block, original) if owned.search(original) else original.rstrip() + "\n\n" + block
    tomllib.loads(result)
    return result


def claude_document(original, python, server, config):
    data = json.loads(original or "{}")
    entry = {"type": "stdio", "command": str(python), "args": [str(server)],
             "env": {"HCC_AGENT_CONFIG": str(config)}}
    current = data.setdefault("mcpServers", {}).get("desktop")
    if current and current != entry:
        raise ValueError("já existe outro MCP desktop; confira a configuração antes de substituir")
    data["mcpServers"]["desktop"] = entry
    return json.dumps(data, indent=2, ensure_ascii=False) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=["codex", "claude"])
    parser.add_argument("--client-config", required=True, type=Path)
    parser.add_argument("--agent-config", required=True, type=Path)
    parser.add_argument("--python", default=sys.executable, type=Path)
    parser.add_argument("--check", action="store_true", help="validar sem gravar")
    args = parser.parse_args()
    config = args.agent_config.resolve(strict=True)
    settings = json.loads(config.read_text(encoding="utf-8"))
    if settings.get("transport", "local") not in ("local", "ssh"):
        parser.error("transporte do agente inválido")
    python = args.python.resolve(strict=True)
    destination = args.client_config.expanduser().resolve()
    original = destination.read_text(encoding="utf-8") if destination.exists() else ""
    result = (codex_document if args.target == "codex" else claude_document)(
        original, python, ROOT / "servidor_mcp.py", config)
    if args.check:
        print(f"Configuração válida para {destination}; nenhum arquivo alterado")
        return
    if result == original:
        print(f"MCP desktop já configurado em {destination}")
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%f")
        backup = destination.with_name(destination.name + ".desktop-" + stamp + ".bak")
        shutil.copy2(destination, backup)
        backup.chmod(0o600)
        print(f"Cópia anterior: {backup}")
    fd, tmp = tempfile.mkstemp(prefix=destination.name + ".", dir=destination.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as out:
            out.write(result)
        os.replace(tmp, destination)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
    print(f"MCP desktop registrado em {destination}; novas sessões carregarão o comando")


if __name__ == "__main__":
    main()
