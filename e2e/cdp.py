"""Just enough of Chrome's DevTools protocol to drive a headless Chrome:
open pages, run scripts, take screenshots, collect script errors. The
profile directory persists (service worker and its caches included)."""

import atexit
import base64
import json
import os
import subprocess
import time
import urllib.request

import websocket

CHROME = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")


class Browser:
    def __init__(self, profile, width=1280, height=900, port=9333, mobile=False, zone=None):
        self.proc = subprocess.Popen(
            [CHROME, "--headless=new", f"--remote-debugging-port={port}", f"--user-data-dir={profile}", "--no-first-run",
             "--no-default-browser-check", f"--window-size={width},{height}", "about:blank"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        # Even when a check fails midway.
        atexit.register(self.proc.terminate)
        for _ in range(100):
            try:
                tabs = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json"))
                page = next(t for t in tabs if t["type"] == "page")
                break
            except Exception:  # noqa: BLE001
                time.sleep(0.1)
        self.ws = websocket.create_connection(page["webSocketDebuggerUrl"], timeout=30, suppress_origin=True)
        self.n = 0
        self.errors = []
        self.call("Page.enable")
        self.call("Runtime.enable")
        self.call("Log.enable")
        if zone:
            self.call("Emulation.setTimezoneOverride", timezoneId=zone)
        if mobile:
            self.call("Emulation.setDeviceMetricsOverride", width=width, height=height, deviceScaleFactor=2, mobile=True)
            self.call("Emulation.setTouchEmulationEnabled", enabled=True)

    def _event(self, msg):
        m = msg.get("method")
        if m == "Runtime.exceptionThrown":
            d = msg["params"]["exceptionDetails"]
            self.errors.append(d.get("exception", {}).get("description") or d.get("text"))
        elif m == "Runtime.consoleAPICalled" and msg["params"]["type"] == "error":
            self.errors.append(" ".join(str(a.get("value", a.get("description"))) for a in msg["params"]["args"]))

    def call(self, method, **params):
        self.n += 1
        me = self.n
        self.ws.send(json.dumps({"id": me, "method": method, "params": params}))
        while True:
            msg = json.loads(self.ws.recv())
            if msg.get("id") == me:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result", {})
            self._event(msg)

    def go(self, url, wait=1.5):
        self.call("Page.navigate", url=url)
        self.settle(wait)

    def settle(self, wait=1.0):
        end = time.time() + wait
        self.ws.settimeout(0.2)
        while time.time() < end:
            try:
                self._event(json.loads(self.ws.recv()))
            except websocket.WebSocketTimeoutException:
                pass
        self.ws.settimeout(30)

    def js(self, expr):
        r = self.call("Runtime.evaluate", expression=expr, awaitPromise=True, returnByValue=True)
        if "exceptionDetails" in r:
            raise RuntimeError(r["exceptionDetails"].get("exception", {}).get("description", r["exceptionDetails"]["text"]))
        return r["result"].get("value")

    def shot(self, path):
        data = self.call("Page.captureScreenshot", format="png")["data"]
        with open(path, "wb") as f:
            f.write(base64.b64decode(data))

    def close(self):
        try:
            self.call("Browser.close")
            self.proc.wait(timeout=5)
        except Exception:  # noqa: BLE001
            self.proc.terminate()
            self.proc.wait(timeout=10)
