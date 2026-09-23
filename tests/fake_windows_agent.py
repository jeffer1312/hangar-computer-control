"""Agente de teste do transporte; não opera o desktop."""
import argparse
import base64
import json
from pathlib import Path
import urllib.request
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("--config")
config = json.loads(Path(parser.parse_args().config).read_text())
boot = uuid.uuid4().hex
reply = {"hello": True, "boot": boot, "session_id": 1}
current = None
while True:
    request = urllib.request.Request(config["url"], data=json.dumps(reply).encode(),
                                     headers={"Authorization": "Bearer " + config["token"]})
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            command = json.load(response)
    except Exception:
        break
    op = command["operation"]
    if op == "stop":
        break
    reply = {"boot": boot, "session_id": 1}
    if op == "heartbeat":
        continue
    reply["id"] = command["id"]
    if op == "observe":
        current = uuid.uuid4().hex
        reply["result"] = {"connected": True, "observation_id": current, "elements": []}
    elif op == "act":
        if command["observation_id"] != current:
            reply["error"] = "observação expirada"
        else:
            current = None
            reply["result"] = {"ok": True}
    elif op == "screenshot":
        # Desconexão durante RPC: o cliente não pode devolver captura anterior.
        break
