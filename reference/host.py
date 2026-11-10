"""Reference host: run one plugin process over the 0.1 protocol.

Transport: newline-delimited JSON-RPC 2.0 on the plugin's stdin/stdout.
stderr is the plugin's log. One request in flight at a time.
"""
import json
import subprocess

from . import core

PROTOCOL_ERRORS = {-32601: "method not found", -32001: "unsupported contract", -32010: "input rejected"}


class PluginError(Exception):
    pass


class Plugin:
    # no time budget yet: a plugin that never answers blocks the host (RFC open item)
    def __init__(self, command, cwd=None):
        self.proc = subprocess.Popen(command, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True, encoding="utf-8")
        self.next_id = 0
        self.info = self.call("initialize", {"contract": ["0.1"]})
        if self.info.get("contract") != "0.1":
            raise PluginError(f"plugin chose contract {self.info.get('contract')!r}")

    def call(self, method, params):
        self.next_id += 1
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}
        self.proc.stdin.write(json.dumps(msg, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        if not line:
            raise PluginError(f"plugin exited: {self.proc.stderr.read()[-2000:]}")
        reply = json.loads(line)
        if reply.get("id") != self.next_id:
            raise PluginError(f"reply id {reply.get('id')} != {self.next_id}")
        if "error" in reply:
            raise PluginError(f"{reply['error'].get('code')}: {reply['error'].get('message')}")
        return reply["result"]

    def run(self, md):
        out = self.call("run", {"input": md})
        errs = core.output_errors(md, out)
        if errs:
            raise PluginError("rejected output: " + "; ".join(errs[:10]))
        return out

    def close(self):
        try:
            self.call("shutdown", {})
        finally:
            self.proc.stdin.close()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
