#!/usr/bin/env python3
"""CDP site load test: loads the SMOKE_SITES targets over CDP and polls
readyState/element counts, capturing a screenshot per site. Invoked as
    python3 cdp_site_test.py <mode> <port>
Exits non-zero if any site fails the acceptance criteria."""
import base64, hashlib, json, os, socket, struct, subprocess, sys, time, urllib.request
from urllib.parse import urlsplit

mode, port = sys.argv[1], int(sys.argv[2])
HOST = "127.0.0.1"
sites = [s.strip() for s in os.environ.get("SMOKE_SITES", "").split(",") if s.strip()]
results = []

def check(name, ok, detail="", critical=True):
    results.append({"name": name, "ok": bool(ok), "critical": bool(critical), "detail": str(detail)[:300]})
    print(f"SMOKE-CHECK {name}: {'PASS' if ok else 'FAIL'} {str(detail)[:200]}", flush=True)

def http_get(path, timeout=5):
    with urllib.request.urlopen(f"http://{HOST}:{port}{path}", timeout=timeout) as r:
        return json.loads(r.read().decode())

ver = None
deadline = time.time() + 60
while time.time() < deadline:
    try:
        ver = http_get("/json/version")
        break
    except Exception:
        time.sleep(0.5)
if ver is None:
    check(f"{mode} CDP server up", False, "/json/version unreachable")
    sys.exit(1)
check(f"{mode} CDP server up", True)

# Do not connect during startup: wait until the browser has fully
# initialized and registered at least one page target (this is the
# same readiness signal Puppeteer/Playwright wait for).
targets = None
deadline = time.time() + 60
while time.time() < deadline:
    try:
        t = http_get("/json/list")
        if isinstance(t, list) and t:
            targets = t
            break
    except Exception:
        pass
    time.sleep(1)
if not targets:
    check(f"{mode} page target registered", False,
          "/json/list never showed a page target within 60s")
    sys.exit(1)
check(f"{mode} page target registered", True, f"{len(targets)} target(s)")

class WS:
    def __init__(self, url, timeout=70):
        parts = urlsplit(url)
        self.s = socket.create_connection((parts.hostname, parts.port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall((
            f"GET {parts.path} HTTP/1.1\r\nHost: {parts.hostname}:{parts.port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.s.recv(4096)
        head, _, rest = buf.partition(b"\r\n\r\n")
        assert b" 101 " in b" " + head.split(b"\r\n")[0] + b" ", head
        expect = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
        assert expect in head, "bad Sec-WebSocket-Accept"
        self.buf = rest

    def _read(self, n):
        while len(self.buf) < n:
            data = self.s.recv(65536)
            if not data:
                raise EOFError("websocket closed")
            self.buf += data
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def send(self, obj):
        data = json.dumps(obj).encode()
        mask = os.urandom(4)
        n = len(data)
        if n < 126:
            header = b"\x81" + bytes([0x80 | n])
        elif n < 65536:
            header = b"\x81" + bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            header = b"\x81" + bytes([0x80 | 127]) + struct.pack(">Q", n)
        self.s.sendall(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def recv_msg(self):
        frag = bytearray()
        while True:
            b1, b2 = self._read(2)
            fin, op, ln = b2 & 0x80, b1 & 0x0F, b2 & 0x7F
            if ln == 126:
                ln = struct.unpack(">H", self._read(2))[0]
            elif ln == 127:
                ln = struct.unpack(">Q", self._read(8))[0]
            if b2 & 0x80:
                self._read(4)
            payload = self._read(ln)
            frames_seen = getattr(ws, "frames_seen", 0) + 1
            if frames_seen <= 3:
                print(f"RAW-FRAME {mode}: op={op:#04x} fin={bool(fin)} len={ln} head={payload[:60]!r}", flush=True)
            if op == 0x9:
                self.s.sendall(b"\x8a\x80" + os.urandom(4))
                continue
            if op == 0x8:
                raise EOFError("websocket closed by server")
            if op in (0x1, 0x2) and not fin:
                frag += payload
                continue
            if op == 0x0:
                frag += payload
                if fin:
                    msg = bytes(frag)
                    frag = bytearray()
                    return msg.decode("utf-8", "replace")
                continue
            return payload.decode("utf-8", "replace")

ws, last_err = None, None
for attempt in range(3):
    try:
        ws = WS(ver["webSocketDebuggerUrl"])
        break
    except (OSError, EOFError, AssertionError) as e:
        last_err = e
        print(f"WS-RETRY {mode}: attempt {attempt + 1} failed: {e!r}", flush=True)
        time.sleep(3)
if ws is None:
    check(f"{mode} WebSocket connect", False, repr(last_err))
    try:
        ver2 = http_get("/json/version")
        print(f"DIAG {mode}: /json/version still OK: "
              f"{ver2.get('webSocketDebuggerUrl')}", flush=True)
    except Exception as e2:
        print(f"DIAG {mode}: /json/version now fails: {e2!r}", flush=True)
    sys.exit(1)
check(f"{mode} WebSocket connect", True)
next_id, events = [0], []

def cmd(method, params=None, session=None, timeout=30):
    next_id[0] += 1
    msg = {"id": next_id[0], "method": method}
    if params:
        msg["params"] = params
    if session:
        msg["sessionId"] = session
    ws.send(msg)
    deadline = time.time() + timeout
    while time.time() < deadline:
        payload = ws.recv_msg()
        if not payload.strip():
            continue
        reply = json.loads(payload)
        if reply.get("id") == next_id[0]:
            return reply
        events.append(reply)
    raise TimeoutError(method)

# Warm-up ladder: probe the server with increasingly demanding
# commands so an early-startup stall becomes visible per stage.
# Browser.getVersion is answered inline; Target.getTargets reads
# server state; createTarget on about:blank exercises the full
# embedder NewWindow path (the one that needs the event loop).
def probe(method, params, ptimeout, label):
    try:
        pr = cmd(method, params, timeout=ptimeout)
        err = pr.get("error")
        print(f"PROBE {mode}/{label}: {'OK' if not err else err}", flush=True)
        return True
    except TimeoutError:
        print(f"PROBE {mode}/{label}: TIMEOUT after {ptimeout}s", flush=True)
        print(f"DIAG {mode}: server sent {len(events)} unsolicited message(s) so far:",
              [json.dumps(e)[:120] for e in events[-5:]], flush=True)
        return False
# First try a functional-style first message (Target.getTargets,
# which always works there); then the browser-level getVersion.
if not probe("Target.getTargets", None, 15, "getTargets-first"):
    print(f"DIAG {mode}: getTargets-first also failed", flush=True)
if not probe("Browser.getVersion", None, 10, "getVersion"):
    # Wire-level diagnosis: is the receive buffer silently
    # holding bytes (framing desync), and does a WS ping
    # round-trip work on this connection?
    try:
        print(f"DIAG {mode}/residual-buffer: {ws.buf[:120]!r}", flush=True)
    except Exception as e:
        print(f"DIAG {mode}/residual-buffer: {e!r}", flush=True)
    try:
        ss_out = subprocess.run(["ss", "-tanp"], capture_output=True, text=True,
                                timeout=10).stdout
        queues = [l for l in ss_out.splitlines() if "9222" in l or "9223" in l]
        print(f"DIAG {mode}/socket-queues (Send-QRecv-Q):", flush=True)
        for l in queues:
            print(f"  {l.strip()}", flush=True)
    except Exception as e:
        print(f"DIAG {mode}/socket-queues: {e!r}", flush=True)
    try:
        ws.s.settimeout(5)
        ws.s.sendall(b"\x89\x80" + os.urandom(4))
        pong = ws.recv_msg()
        print(f"DIAG {mode}/ws-ping: got {pong[:80]!r}", flush=True)
    except Exception as e:
        print(f"DIAG {mode}/ws-ping: {e!r}", flush=True)
    # Pinpoint the freeze: same connection, new connection, HTTP,
    # then dump the servo thread list from /proc.
    try:
        ws.send({"id": 999901, "method": "Browser.getVersion"})
        deadline = time.time() + 15
        got = []
        while time.time() < deadline:
            try:
                ws.s.settimeout(3)
                raw = ws.recv_msg()
            except (socket.timeout, TimeoutError):
                continue
            except (OSError, EOFError) as e:
                print(f"DIAG {mode}/same-conn retry: socket error {e!r}", flush=True)
                break
            got.append(raw)
            print(f"DIAG {mode}/same-conn RX: {raw[:200]}", flush=True)
            if json.loads(raw).get("id") == 999901:
                print(f"DIAG {mode}/same-conn retry: REPLY ARRIVED", flush=True)
                break
        if not got:
            print(f"DIAG {mode}/same-conn retry: NO message in 15s", flush=True)
    except Exception as e:
        print(f"DIAG {mode}/same-conn retry: {e!r}", flush=True)
    try:
        ws2 = WS(ver["webSocketDebuggerUrl"])
        ws2.send({"id": 999902, "method": "Browser.getVersion"})
        deadline = time.time() + 15
        got = []
        while time.time() < deadline:
            try:
                ws2.s.settimeout(3)
                raw = ws2.recv_msg()
            except (socket.timeout, TimeoutError):
                continue
            except (OSError, EOFError) as e:
                print(f"DIAG {mode}/new-conn probe: socket error {e!r}", flush=True)
                break
            got.append(raw)
            print(f"DIAG {mode}/new-conn RX: {raw[:200]}", flush=True)
            if json.loads(raw).get("id") == 999902:
                print(f"DIAG {mode}/new-conn probe: REPLY ARRIVED", flush=True)
                break
        if not got:
            print(f"DIAG {mode}/new-conn probe: NO message in 15s", flush=True)
    except Exception as e:
        print(f"DIAG {mode}/new-conn probe: {e!r}", flush=True)
    try:
        v = http_get("/json/version")
        print(f"DIAG {mode}/http-after: OK {v.get('webSocketDebuggerUrl')}", flush=True)
    except Exception as e:
        print(f"DIAG {mode}/http-after: {e!r}", flush=True)
    try:
        print(f"SERVER-TAIL {mode}:\n" +
              open(f"smoke-out/site-{mode}-server.log", errors="replace").read()[-1500:],
              flush=True)
    except OSError:
        pass
    sys.exit(1)
if not probe("Target.getTargets", None, 15, "getTargets"):
    sys.exit(1)
if not probe("Target.createTarget", {"url": "about:blank"}, 30, "createTarget-blank"):
    sys.exit(1)
time.sleep(2)

for i, site in enumerate(sites):
    name = f"{mode}/site{i}({site})"
    try:
        r = None
        for attempt in range(2):
            try:
                r = cmd("Target.createTarget", {"url": site}, timeout=45)
                break
            except TimeoutError:
                if attempt == 0:
                    print(f"CREATE-RETRY {mode} {site}", flush=True)
                    time.sleep(5)
        tid = (r or {}).get("result", {}).get("targetId")
        if not tid:
            check(f"{name} createTarget", False, r.get("error") or r)
            continue
        r = cmd("Target.attachToTarget", {"targetId": tid, "flatten": True})
        sid = r.get("result", {}).get("sessionId")
        if not sid:
            for ev_ in events:
                if ev_.get("method") == "Target.attachedToTarget" and \
                        tid in json.dumps(ev_.get("params", {})):
                    sid = ev_["params"].get("sessionId")
                    break
        if not sid:
            check(f"{name} attachToTarget", False, "no sessionId")
            continue
        expr = ("JSON.stringify({rs: document.readyState, title: document.title,"
                " els: document.querySelectorAll('*').length,"
                " txt: (document.body && document.body.innerText ?"
                " document.body.innerText.slice(0, 240) : '')})")
        metrics = None
        fallback = [False, ""]
        deadline = time.time() + 120
        while time.time() < deadline:
            if not fallback[0] and time.time() > deadline - 90:
                # If the embedder LoadUrl path did not land within
                # 30s, drive the navigation through
                # Runtime.evaluate and log the difference loudly.
                fallback[0] = True
                try:
                    cmd("Runtime.evaluate",
                        {"expression": f"location.href = {json.dumps(site)}",
                         "returnByValue": True}, session=sid, timeout=25)
                    print(f"LOADURL-PATH ({site}): navigation did not land, "
                          "using evaluate fallback", flush=True)
                except (TimeoutError, EOFError, OSError):
                    pass
            try:
                r = cmd("Runtime.evaluate", {"expression": expr, "returnByValue": True},
                        session=sid, timeout=25)
            except TimeoutError:
                time.sleep(2)
                continue
            val = r.get("result", {}).get("result", {}).get("value")
            if val:
                m = json.loads(val)
                if m.get("rs") == "complete":
                    metrics = m
                    break
            time.sleep(2)
        if fallback[0]:
            try:
                r = cmd("Runtime.evaluate", {"expression": "location.href",
                                             "returnByValue": True}, session=sid, timeout=25)
                print(f"POST-FALLBACK-LOCATION ({site}):",
                      r.get("result", {}).get("result", {}).get("value"), flush=True)
            except (TimeoutError, EOFError, OSError):
                pass
        check(f"{name} readyState complete", bool(metrics),
              json.dumps(metrics, ensure_ascii=False)[:280] if metrics else "timeout waiting for load")
        els = (metrics or {}).get("els", 0)
        check(f"{name} DOM elements >= 20", els >= 20, str(els))
        r = cmd("Page.captureScreenshot", {"format": "png"}, session=sid, timeout=60)
        data = r.get("result", {}).get("data")
        size = 0
        if data:
            png = base64.b64decode(data)
            size = len(png)
            if png[:8] == b"\x89PNG\r\n\x1a\n" and size > 10000:
                open(f"smoke-out/site-{mode}-{i}.png", "wb").write(png)
        check(f"{name} screenshot PNG > 10KB", size > 10000, f"{size} bytes")
        if metrics:
            print(f"SITE-INFO {name}: title={metrics.get('title', '')[:90]!r} "
                  f"text={metrics.get('txt', '')[:200]!r}", flush=True)
        try:
            cmd("Target.closeTarget", {"targetId": tid}, timeout=15)
        except (TimeoutError, EOFError, OSError):
            pass
    except (EOFError, OSError, TimeoutError, json.JSONDecodeError) as e:
        check(f"{name} crashed/protocol error", False, repr(e))
        try:
            print(f"SERVER-TAIL {mode}:\n" +
                  open(f"smoke-out/site-{mode}-server.log", errors="replace").read()[-1200:],
                  flush=True)
        except OSError:
            pass
json.dump({"results": results}, open(f"smoke-out/site-{mode}-results.json", "w"), indent=1)
bad = [x for x in results if x["critical"] and not x["ok"]]
print(f"SMOKE SUMMARY ({mode}): {len(results) - len(bad)}/{len(results)} checks passed", flush=True)
if bad:
    print("Failed checks:", [x["name"] for x in bad])
    sys.exit(1)
