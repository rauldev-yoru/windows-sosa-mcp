"""Minimal stdio MCP client used by the acceptance scripts (stdlib only)."""
from __future__ import annotations

import json
import shlex
import subprocess
import threading
import time
from typing import Any

PROTOCOL = "2025-06-18"


class McpProcess:
    """Minimal newline-delimited JSON-RPC client over a child's stdio."""

    def __init__(self, label: str, command: str, timeout: float) -> None:
        self.label = label
        self.timeout = timeout
        self.proc = subprocess.Popen(
            shlex.split(command),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self.next_id = 1
        self.stderr: list[str] = []
        threading.Thread(target=self._drain_stderr, daemon=True).start()

    def _drain_stderr(self) -> None:
        assert self.proc.stderr is not None
        for line in iter(self.proc.stderr.readline, b""):
            self.stderr.append(line.decode("utf-8", "replace").rstrip())

    def _send(self, message: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write((json.dumps(message) + "\n").encode())
        self.proc.stdin.flush()

    def notify(self, method: str, params: dict | None = None) -> None:
        message: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self._send(message)

    def request(self, method: str, params: dict | None = None) -> tuple[dict, float]:
        rid = self.next_id
        self.next_id += 1
        message: dict[str, Any] = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            message["params"] = params
        start = time.perf_counter()
        self._send(message)
        result: dict[str, Any] = {}
        done = threading.Event()

        def read() -> None:
            assert self.proc.stdout is not None
            while True:
                line = self.proc.stdout.readline()
                if not line:
                    result["eof"] = True
                    break
                try:
                    msg = json.loads(line)
                except ValueError:
                    continue  # not JSON-RPC (should not happen on stdout)
                if msg.get("id") == rid:
                    result["msg"] = msg
                    break
            done.set()

        threading.Thread(target=read, daemon=True).start()
        if not done.wait(self.timeout):
            raise TimeoutError(f"{self.label}: {method} timed out after {self.timeout}s")
        elapsed = (time.perf_counter() - start) * 1000
        if "msg" not in result:
            raise RuntimeError(f"{self.label}: EOF before response to {method}; stderr={self.stderr[-5:]}")
        return result["msg"], elapsed

    def close(self) -> None:
        try:
            assert self.proc.stdin is not None
            self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except Exception:  # noqa: BLE001 - best effort shutdown
            self.proc.kill()
