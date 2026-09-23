"""Cliente do agente UIA: filho local ou sessão Windows remota por SSH."""
from __future__ import annotations

import base64
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import secrets
import subprocess
import sys
import tempfile
import threading
import time
import uuid


REMOTE_BOOTSTRAP = r"""$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$p = [Console]::In.ReadToEnd() | ConvertFrom-Json
if ($p.operation -eq 'prepare') {
    $dir = Join-Path $env:LOCALAPPDATA ('HangarComputerControl\' + $p.digest)
    New-Item -ItemType Directory -Force $dir | Out-Null
    $exe = Join-Path $dir 'windows-agent.exe'
    $valid = (Test-Path -LiteralPath $exe) -and ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLower() -eq $p.digest)
    if (-not $valid -and $p.shared_executable) {
        Copy-Item -LiteralPath $p.shared_executable -Destination $exe -Force
        $valid = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLower() -eq $p.digest
    }
    @{executable=$exe; valid=$valid} | ConvertTo-Json -Compress
} elseif ($p.operation -eq 'start') {
    if ((Get-FileHash -LiteralPath $p.executable -Algorithm SHA256).Hash.ToLower() -ne $p.digest) { throw 'Hash do agente não confere' }
    $config = Join-Path (Split-Path $p.executable) ($p.task + '.json')
    $p.connection | ConvertTo-Json -Compress | Set-Content -LiteralPath $config -Encoding UTF8
    $action = New-ScheduledTaskAction -Execute $p.executable -Argument ('--config "' + $config + '"')
    $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Hours 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $p.task -Action $action -Principal $principal -Settings $settings | Out-Null
    Start-ScheduledTask -TaskName $p.task
    @{started=$true} | ConvertTo-Json -Compress
} elseif ($p.operation -eq 'stop') {
    $task = Get-ScheduledTask -TaskName $p.task -ErrorAction SilentlyContinue
    if ($task) {
        Stop-ScheduledTask -TaskName $p.task
        Unregister-ScheduledTask -TaskName $p.task -Confirm:$false
    }
    if ($p.pid -and $p.executable) {
        $child = Get-CimInstance Win32_Process -Filter ('ProcessId = ' + [int]$p.pid)
        if ($child -and $child.ExecutablePath -eq $p.executable -and $child.CommandLine.Contains($p.task + '.json')) {
            Stop-Process -Id $p.pid -Force
        }
    }
    if ($p.executable) {
        $config = Join-Path (Split-Path $p.executable) ($p.task + '.json')
        if (Test-Path -LiteralPath $config) { Remove-Item -LiteralPath $config }
    }
    @{stopped=$true} | ConvertTo-Json -Compress
} else { throw 'Operação inválida' }
"""


class AgentSession:
    def __init__(self, config_path=None, cancelado=None):
        self.cancelado = cancelado or threading.Event()
        self.closed = False
        self.config = json.loads(Path(config_path or os.environ["HCC_AGENT_CONFIG"]).read_text())
        self.token = secrets.token_urlsafe(32)
        self.boot = None
        self.agent_pid = None
        self.last_seen = 0.0
        self.commands = queue.Queue()
        self.pending = {}
        self.lock = threading.Lock()
        self.ready = threading.Event()
        self.process = self.tunnel = None
        self.remote_executable = None
        self.task = "HangarComputerControl-" + uuid.uuid4().hex
        self.directory = tempfile.TemporaryDirectory(prefix="hcc-agent-")
        session = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                self.connection.settimeout(10)
                if self.path != "/next" or not hmac.compare_digest(
                        self.headers.get("Authorization", ""), "Bearer " + session.token):
                    self.send_error(403)
                    return
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if not 0 < size <= 16 * 1024 * 1024:
                        raise ValueError("tamanho inválido")
                    data = json.loads(self.rfile.read(size))
                    if not isinstance(data.get("boot"), str) or data.get("session_id", 0) <= 0:
                        raise ValueError("identidade da sessão inválida")
                    with session.lock:
                        if session.boot is None:
                            if not data.get("hello"):
                                raise ValueError("falta apresentação do agente")
                            session.boot = data["boot"]
                            session.agent_pid = data.get("pid")
                            session.last_seen = time.monotonic()
                            session.ready.set()
                        if data["boot"] != session.boot:
                            raise ValueError("outro processo tentou substituir o agente")
                        session.last_seen = time.monotonic()
                        waiting = session.pending.get(data.get("id"))
                        if waiting:
                            waiting[1].update(data)
                            waiting[0].set()
                    if session.closed or session.cancelado.is_set():
                        command = {"operation": "stop"}
                    else:
                        try:
                            command = session.commands.get(timeout=2)
                        except queue.Empty:
                            command = {"operation": "heartbeat"}
                        if session.closed or session.cancelado.is_set():
                            command = {"operation": "stop"}
                    body = json.dumps(command).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (ValueError, TypeError, KeyError):
                    self.send_error(400)
                except (BrokenPipeError, ConnectionResetError, TimeoutError):
                    return

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        port = self.server.server_port
        connection = {"url": f"http://127.0.0.1:{port}/next", "token": self.token}
        try:
            transport = self.config.get("transport", "local")
            if transport == "local":
                path = Path(self.directory.name) / "connection.json"
                path.write_text(json.dumps(connection))
                path.chmod(0o600)
                command = self.config.get("command", [sys.executable, str(Path(__file__).with_name("windows_uia.py"))])
                self.process = subprocess.Popen([*command, "--config", str(path)], stdout=subprocess.DEVNULL)
            elif transport == "ssh":
                artifact = Path(self.config["agent_path"]).resolve()
                digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
                self.tunnel = subprocess.Popen([*self._ssh(), "-N", "-o", "ExitOnForwardFailure=yes",
                                                "-R", f"127.0.0.1:{port}:127.0.0.1:{port}", self.config["host"]],
                                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                prepared = self._remote({"operation": "prepare", "digest": digest,
                                         "shared_executable": self.config.get("shared_executable")})
                self.remote_executable = prepared["executable"]
                if not prepared["valid"]:
                    options = self._ssh()[1:]
                    destination = self.config["host"] + ":" + self.remote_executable.replace("\\", "/")
                    copied = subprocess.run(["scp", *options, str(artifact), destination],
                                            capture_output=True, text=True, timeout=120)
                    if copied.returncode:
                        raise RuntimeError("falha ao enviar agente: " + copied.stderr[-1000:])
                self._remote({"operation": "start", "executable": self.remote_executable,
                              "digest": digest, "task": self.task, "connection": connection})
            else:
                raise ValueError("transport deve ser local ou ssh")
            deadline = time.monotonic() + 30
            while not self.ready.wait(.1):
                self._check_cancel()
                for process in (self.process, self.tunnel):
                    if process is not None and process.poll() is not None:
                        raise RuntimeError(f"processo de conexão encerrou com código {process.returncode}")
                if time.monotonic() >= deadline:
                    raise TimeoutError("agente não respondeu; confira usuário logado, tarefa e canal de retorno")
        except BaseException:
            self.close()
            raise

    def _ssh(self):
        command = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8"]
        if self.config.get("proxy_command"):
            command += ["-o", "ProxyCommand=" + self.config["proxy_command"]]
        return command

    def _remote(self, payload):
        encoded = base64.b64encode(REMOTE_BOOTSTRAP.encode("utf-16le")).decode()
        result = subprocess.run([*self._ssh(), self.config["host"], "powershell", "-NoProfile",
                                 "-NonInteractive", "-EncodedCommand", encoded], input=json.dumps(payload).encode("utf-8"),
                                capture_output=True, timeout=60)
        if result.returncode:
            try:
                error = result.stderr.decode("utf-8")
            except UnicodeDecodeError:
                error = result.stderr.decode("cp850")
            raise RuntimeError("inicialização Windows falhou: " + error[-1500:])
        return json.loads(result.stdout.decode("utf-8-sig"))

    def _check_cancel(self):
        if self.closed or self.cancelado.is_set():
            raise RuntimeError("controle cancelado ou encerrado")

    def _rpc(self, operation, **params):
        self._check_cancel()
        if time.monotonic() - self.last_seen > 10:
            raise ConnectionError("agente desconectado; nenhum estado antigo será reutilizado")
        ident = uuid.uuid4().hex
        event, result = threading.Event(), {}
        timeout = self.config.get("request_timeout", 15)
        command = {"id": ident, "operation": operation, "expires_at": time.time() + timeout, **params}
        with self.lock:
            self.pending[ident] = (event, result)
        self.commands.put(command)
        deadline = time.monotonic() + timeout
        try:
            while not event.wait(.05):
                self._check_cancel()
                if time.monotonic() >= deadline:
                    self.close()
                    raise TimeoutError(f"agente não respondeu a {operation}; execução encerrada")
            self._check_cancel()
            if "error" in result:
                raise RuntimeError(result["error"])
            return result["result"]
        finally:
            with self.lock:
                self.pending.pop(ident, None)

    def observe(self):
        return self._rpc("observe")

    def act(self, action, observation_id):
        return self._rpc("act", action=action, observation_id=observation_id)

    def screenshot(self):
        return base64.b64decode(self._rpc("screenshot")["png"], validate=True)

    def close(self):
        if self.closed:
            return
        self.closed = True
        self.commands.put({"operation": "stop"})
        if self.config.get("transport") == "ssh" and self.remote_executable:
            try:
                self._remote({"operation": "stop", "task": self.task, "executable": self.remote_executable,
                              "pid": self.agent_pid})
            except Exception as exc:
                print(f"não foi possível remover tarefa {self.task}: {type(exc).__name__}", file=sys.stderr)
        for process in (self.process, self.tunnel):
            if process is not None:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
        self.directory.cleanup()
