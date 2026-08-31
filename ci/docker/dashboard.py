#!/usr/bin/env python3
"""Single-process MetaCubeXD dashboard gateway.

On one port (``--listen``, 9090) it:

1. Serves the bundled MetaCubeXD static UI from ``--ui`` (SPA-friendly).
2. Reverse-proxies the Clash HTTP API from ``--api`` (mihomo local
   ``external-controller``) on the ``/clash-api`` path prefix, injecting
   ``Authorization: Bearer`` so the dashboard auto-connects without prompting
   for the secret (same-origin => no CORS dance).
3. Bridges WebSocket upgrades (``/fwLog``, ``/connections``) to the same API.

The dashboard ``defaultBackendURL`` is rewritten to ``/clash-api`` (relative,
works for both localhost and LAN guests) unless ``--backend-url`` overrides.

Why a path prefix?  MetaCubeXD is an SPA whose client routes
(``/proxies``/``/rules``/...) overlap Clash API paths, so keeping the API under
``/clash-api`` serves the UI at ``/`` without collisions.
"""
from __future__ import annotations

import argparse
import http.client
import json
import mimetypes
import os
import re
import select
import socket
import sys
import threading
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

API_PREFIX = "/clash-api"
# Bare mihomo WebSocket paths — forwarded too, so the dashboard works even when a
# MetaCubeXD build resolves the WS backend to the page origin (empty / relative
# defaultBackendURL) instead of the /clash-api prefix.  None of these collide
# with an SPA client route.
WS_PATHS = {"/connections", "/traffic", "/memory", "/logs"}
HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
}


def log(msg: str) -> None:
    sys.stderr.write(f"[dashboard] {msg}\n")
    sys.stderr.flush()


class DashboardHandler(BaseHTTPRequestHandler):
    server_version = "MetaCubeXD-Gateway/1.0"

    def log_message(self, fmt, *args):  # noqa: D401
        log('%s - "%s"' % (self.client_address[0], fmt % args))

    # ── request dispatch ──────────────────────────────────────────────────
    def do_GET(self):
        self._route("GET")

    def do_POST(self):
        self._route("POST")

    def do_PUT(self):
        self._route("PUT")

    def do_DELETE(self):
        self._route("DELETE")

    def do_PATCH(self):
        self._route("PATCH")

    def do_OPTIONS(self):
        self._route("OPTIONS")

    def do_HEAD(self):
        self._route("HEAD")

    def _route(self, method: str) -> None:
        is_ws = self.headers.get("Upgrade", "").lower() == "websocket"
        if is_ws and self.path.startswith(API_PREFIX):
            self._proxy_ws()
            return
        if self.path.startswith(API_PREFIX):
            self._proxy(method)
            return
        # Defensive: if MetaCubeXD resolved the WS backend to the page origin,
        # it opens bare ws://host/<path> for the Clash WS endpoints. Forward
        # those to mihomo as well (same-origin, secret auto-injected).
        if is_ws and self._upstream_path().split("?", 1)[0] in WS_PATHS:
            self._proxy_ws()
            return
        self._serve_static(method)

    def _upstream_path(self) -> str:
        """Clash API path with an optional ``/clash-api`` prefix stripped.

        Bare mihomo WS paths (``/connections`` …) are returned verbatim so they
        map onto mihomo's own endpoints; prefixed calls lose only the prefix.
        """
        path = self.path
        if path.startswith(API_PREFIX):
            path = path[len(API_PREFIX):]
        if not path.startswith("/"):
            path = "/" + path
        return path

    # ── reverse proxy to mihomo ───────────────────────────────────────────
    def _proxy(self, method: str) -> None:
        host, port = self.server.api_target
        path = self._upstream_path()
        # preserve query string on API calls
        spl = urlsplit(path)
        upstream = spl.path
        if spl.query:
            upstream += "?" + spl.query

        length = int(self.headers.get("content-length", 0) or 0)
        body = self.rfile.read(length) if length else None

        try:
            conn = http.client.HTTPConnection(host, port, timeout=30)
            headers = {}
            for k, v in self.headers.items():
                if k.lower() in HOP_HEADERS:
                    continue
                headers[k] = v
            headers["Authorization"] = f"Bearer {self.server.secret}"
            headers["Host"] = f"{host}:{port}"
            headers["Connection"] = "close"
            conn.request(method, upstream, body=body, headers=headers)
            resp = conn.getresponse()
        except Exception as exc:
            sys.stderr.write(f"[dashboard] upstream connect error for {upstream}: {exc}\n")
            traceback.print_exc()
            self.send_error(502, "upstream mihomo unreachable")
            return

        try:
            self.send_response(resp.status)
            for k, v in resp.getheaders():
                if k.lower() in HOP_HEADERS or k.lower() == "access-control-allow-origin":
                    continue
                self.send_header(k, v)
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE, OPTIONS, PATCH")
            self.send_header("Access-Control-Allow-Headers", "Authorization, Content-Type")
            self.end_headers()
            if method == "HEAD":
                return
            while True:
                chunk = resp.read(65536)
                if not chunk:
                    break
                self.wfile.write(chunk)
        except Exception as exc:
            sys.stderr.write(f"[dashboard] _proxy stream error for {upstream}: {exc}\n")
            traceback.print_exc()
        finally:
            try:
                resp.close()
            except Exception:
                pass
            try:
                conn.close()
            except Exception:
                pass

    # ── websocket bridge ──────────────────────────────────────────────────
    def _proxy_ws(self) -> None:
        host, port = self.server.api_target
        path = self._upstream_path()
        try:
            sock = socket.create_connection((host, port), timeout=10)
        except OSError as exc:
            self.send_error(502, f"upstream unreachable: {exc}")
            return

        # Re-issue the Upgrade handshake to mihomo, adding the secret.
        req_line = f"{self.command} {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
        out = [req_line.encode()]
        for k, v in self.headers.items():
            if k.lower() in ("host", "connection", "keep-alive", "transfer-encoding", "content-length"):
                continue
            out.append(f"{k}: {v}\r\n".encode())
        out.append(f"Authorization: Bearer {self.server.secret}\r\n".encode())
        out.append(b"Connection: Upgrade\r\n")
        out.append(b"Upgrade: websocket\r\n")
        out.append(b"\r\n")
        sock.sendall(b"".join(out))

        # Read mihomo's handshake response, relay to client.
        sock.settimeout(10)
        resp = b""
        while b"\r\n\r\n" not in resp:
            data = sock.recv(4096)
            if not data:
                break
            resp += data
        status_line, _, _ = resp.partition(b"\r\n")
        if b" 101 " not in status_line:
            self.send_response(502, "upstream did not upgrade")
            self.end_headers()
            sock.close()
            return
        # Relay mihomo's 101 response verbatim to the client.
        try:
            self.connection.sendall(resp)
        except OSError:
            sock.close()
            return
        sock.settimeout(None)
        self._pump(sock)

    def _pump(self, upstream: socket.socket) -> None:
        client = self.connection
        stop = threading.Event()

        def forward(src, dst):
            try:
                while not stop.is_set():
                    r, _, _ = select.select([src], [], [], 1.0)
                    if src in r:
                        data = src.recv(65536)
                        if not data:
                            break
                        dst.sendall(data)
            except OSError:
                pass
            finally:
                stop.set()

        t1 = threading.Thread(target=forward, args=(client, upstream), daemon=True)
        t2 = threading.Thread(target=forward, args=(upstream, client), daemon=True)
        t1.start()
        t2.start()
        stop.wait(timeout=3600)
        for s in (client, upstream):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    # ── static serving (SPA) ─────────────────────────────────────────────
    def _serve_static(self, method: str) -> None:
        if method not in ("GET", "HEAD"):
            self.send_error(405, "Method Not Allowed")
            return
        ui = self.server.ui_dir
        rel = urlsplit(self.path).path.lstrip("/")
        rel = rel.split("?")[0]
        if rel == "" or rel.endswith("/"):
            rel = rel + "index.html"
        candidate = os.path.normpath(os.path.join(ui, rel))
        abs_ui = os.path.abspath(ui)
        if not (candidate == abs_ui or candidate.startswith(abs_ui + os.sep)):
            self.send_error(403, "Forbidden")
            return
        if os.path.isfile(candidate):
            ctype, _ = mimetypes.guess_type(candidate)
            self._send_file(candidate, ctype or "application/octet-stream")
        else:
            idx = os.path.join(ui, "index.html")
            if os.path.isfile(idx):
                self._send_file(idx, "text/html")
            else:
                self.send_error(404, "Not Found")

    def _send_file(self, path: str, ctype: str) -> None:
        try:
            with open(path, "rb") as fh:
                data = fh.read()
        except OSError:
            self.send_error(404, "Not Found")
            return
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Vary", "*")
        if path.endswith((".html", ".js")):
            # HTML and JS are rewritten at startup (defaultBackendURL,
            # service-worker fix, etc.).  No-cache is not enough — Firefox in
            # normal mode still serves disk-cached copies after "clearing cache".
            # Use no-store to *guarantee* fresh fetches every time, matching
            # private-browsing behaviour.
            self.send_header("Cache-Control", "no-store, no-cache, must-revalidate, proxy-revalidate")
            self.send_header("Pragma", "no-cache")
            self.send_header("Expires", "0")
        else:
            self.send_header("Cache-Control", "public, max-age=86400")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            if self.command != "HEAD":
                self.wfile.write(data)
        except BrokenPipeError:
            pass


def rewrite_config_js(ui_dir: str, backend_url: str, github_token: str) -> None:
    """Force MetaCubeXD to talk to the Clash API through the dashboard's
    ``/clash-api`` reverse-proxy (same origin => no CORS, secret auto-injected).

    We *append* an override after the page's own ``config.js``. An assignment
    always wins over any object literal declared earlier, so this works no matter
    how the shipped ``config.js`` formats ``defaultBackendURL`` (single/double/
    backtick quotes, a non-empty placeholder, an empty ``''`` value, or the field
    being absent entirely) -- and even if ``config.js`` is missing we create one.

    With ``defaultBackendURL`` pinned to ``/clash-api`` the MetaCubeXD SPA
    prefixes every REST call (``/proxies``, ``/configs``, ``/version`` …) and
    every WebSocket (``/connections``, ``/traffic``, ``/memory``, ``/logs``)
    with ``/clash-api``, so they land on our reverse-proxied path instead of the
    bare page origin (which currently yields ``JSON.parse`` errors on REST and
    "can't establish a connection" on WebSocket).
    """
    config_js = os.path.join(ui_dir, "config.js")
    try:
        with open(config_js, "r", encoding="utf-8") as fh:
            src = fh.read()
    except FileNotFoundError:
        log(f"WARNING: config.js not found in {ui_dir}; creating a new one")
        src = ""

    override = (
        "\n// --- injected by mihomo-freebox dashboard gateway ---\n"
        "window.__METACUBEXD_CONFIG__ = window.__METACUBEXD_CONFIG__ || {};\n"
        f"window.__METACUBEXD_CONFIG__.defaultBackendURL = {json.dumps(backend_url)};\n"
        f"window.__METACUBEXD_CONFIG__.githubToken = {json.dumps(github_token)};\n"
    )
    with open(config_js, "w", encoding="utf-8") as fh:
        fh.write(src + override)
    log(f"rewrote config.js: defaultBackendURL={backend_url!r}")


def rewrite_index_html(ui_dir: str, backend_url: str, github_token: str) -> None:
    """Inject the backend URL + GitHub token into the prerendered ``index.html``.

    MetaCubeXD's Nuxt 3 app reads ``defaultBackendURL`` from
    ``window.__NUXT__.config.public`` (embedded in ``index.html``) — this is the
    *primary* source, checked *before* the legacy ``window.__METACUBESD_CONFIG__``
    fallback that ``rewrite_config_js`` patches via ``config.js``.

    If the ``config.js`` override doesn't take effect at runtime (browser
    caching, script-load race, Alpine missing the ``.js`` MIME type, version
    drift in the built JS, etc.) the app silently falls through to
    ``FALLBACK_BACKEND_URL`` (``http://127.0.0.1:9090`` — the dashboard's own
    origin), producing a "Backend Unreachable" loop and ``JSON.parse`` errors.

    Patching ``index.html`` in addition to ``config.js`` guarantees the
    primary config is correct so the SPA auto-probes ``/clash-api`` without
    ever needing the user to manually enter a backend URL.

    The replacement uses a regex that matches ``defaultBackendURL:`` and
    ``githubToken:`` in *all* quoting styles (single/double quotes, with or
    without spaces) and across every location they appear:
      1. the ``<script src=config.js onerror=...>`` onerror handler
      2. the inline ``window.__METACUBESD_CONFIG__ = … || { … }`` guard
      3. the ``window.__NUXT__.config.public`` object literal
    """
    index_html = os.path.join(ui_dir, "index.html")
    try:
        with open(index_html, "r", encoding="utf-8") as fh:
            src = fh.read()
    except FileNotFoundError:
        log(f"WARNING: index.html not found in {ui_dir}; skipping index.html rewrite")
        return

    # Match "key: 'value'" or 'key: "value"' — preserving the original quote
    # char is essential: index.html embeds JS in double-quoted HTML attributes
    # (e.g. onerror="...defaultBackendURL:''..."), so replacing single-quoted
    # JS strings with double-quoted ones would corrupt the attribute.
    _kv_re = re.compile(
        r"""((?:defaultBackendURL|githubToken)\s*:\s*)(["'])(?:[^"'\\]*)(["'])"""
    )

    def _repl(m: re.Match) -> str:
        prefix = m.group(1)                       # "defaultBackendURL: "
        qc = m.group(2)                           # opening quote: ' or "
        key = prefix.split(":")[0].strip()        # "defaultBackendURL"
        val = backend_url if key == "defaultBackendURL" else github_token
        # Escape backslashes/quotes so the value is valid in either quote style
        val = val.replace("\\", "\\\\").replace(qc, "\\" + qc)
        return f"{prefix}{qc}{val}{qc}"

    new_src, count = _kv_re.subn(_repl, src)

    # Inject a runtime script that sets defaultBackendURL to the FULL
    # origin URL (e.g. "http://127.0.0.1:9090/clash-api") instead of a
    # bare relative path like "/clash-api".
    #
    # Why: MetaCubeXD's transformEndpointURL() uses window.location.protocol
    # (which is "http:" WITH colon) concatenated as `${protocol}//${url}`,
    # producing "http:///clash-api" (THREE slashes).  Node's URL parser — and
    # Firefox — interpret the empty-authority triple-slash as hostname=clash-api
    # (path=/version), causing a CORS error and "Backend Unreachable".
    #
    # A full URL starting with "http://" bypasses transformEndpointURL()
    # entirely (it checks startsWith("http://") and returns as-is).
    # This script runs synchronously after window.__NUXT__ is initialised but
    # before the deferred Nuxt module script boots the SPA.
    if not backend_url.startswith(("http://", "https://")):
        origin_expr = f"window.location.origin + {json.dumps(backend_url)}"
    else:
        origin_expr = json.dumps(backend_url)

    runtime_inject = (
        f'<script>(function(){{'
        f'var u={origin_expr};'
        f'window.__NUXT__=window.__NUXT__||{{}};'
        f'window.__NUXT__.config=window.__NUXT__.config||{{}};'
        f'window.__NUXT__.config.public=window.__NUXT__.config.public||{{}};'
        f'window.__NUXT__.config.public.defaultBackendURL=u;'
        f'window.__METACUBESD_CONFIG__=window.__METACUBESD_CONFIG__||{{}};'
        f'window.__METACUBESD_CONFIG__.defaultBackendURL=u;'
        f'}})();'
        f'</script>\n'
    )

    # Inject RIGHT AFTER the window.__NUXT__=... setup script.  This is
    # critical: MetaCubeXD's prerendered index.html places the __NUXT__ config
    # initialisation in <body> (well after <head>), so injecting in <head>
    # would run before __NUXT__ exists and the override would be clobbered.
    # Module scripts are deferred, so they run after HTML parsing — our
    # synchronous script (placed after __NUXT__ setup) will execute first.
    # Idempotency guard: if the runtime script is already present, don't
    # inject a second copy.  This handles restarts where rewrite_index_html
    # was called on an already-modified file.
    if 'var u=' in new_src and 'window.__METACUBESD_CONFIG__=window.__METACUBESD_CONFIG__||' in new_src:
        log("runtime script already present, skipping injection")
        with open(index_html, "w", encoding="utf-8") as fh:
            fh.write(new_src)
        return

    # Match the __NUXT__ SETUP line specifically: `window.__NUXT__={}`
    # (empty-object initialisation), NOT `window.__NUXT__=window.__NUXT__||{}`
    # (which appears inside our own runtime script and would cause double-injection).
    # The DOTALL + ? makes .*? match minimally up to the closing </script>.
    nuxt_match = re.search(r'window\.__NUXT__=\{\}.*?</script>', new_src, re.DOTALL)
    if nuxt_match:
        pos = nuxt_match.end()
        new_src = new_src[:pos] + runtime_inject + new_src[pos:]
        log(f"injected runtime script after __NUXT__ setup: defaultBackendURL = {origin_expr}")
    else:
        # Fallback: before the first <script type="module"> tag
        module_match = re.search(r'<script type="module"', new_src)
        if module_match:
            pos = module_match.start()
            new_src = new_src[:pos] + runtime_inject + new_src[pos:]
            log(f"injected runtime script before <script type=\"module\"> (fallback): "
                f"defaultBackendURL = {origin_expr}")
        else:
            log("WARNING: no injection point found in index.html; "
                "runtime script not injected")

    with open(index_html, "w", encoding="utf-8") as fh:
        fh.write(new_src)
    log(f"rewrote index.html: defaultBackendURL={backend_url!r}, "
        f"githubToken={github_token!r} ({count} value replacement(s))")


def rewrite_sw_js(ui_dir: str) -> None:
    """Fix the PWA service worker so it doesn't serve stale ``index.html``.

    MetaCubeXD ships a Workbox service worker that precaches ``index.html``
    (under the ``"./`` URL) with a content-hash revision.  Once installed, the
    SW serves this cached copy on every navigation — even after we rewrite
    ``index.html`` on disk, browsers keep showing the old page because the SW's
    Cache API holds the pre-fix copy.  Clearing the browser cache does NOT
    clear the SW cache, so users see "Backend Unreachable" indefinitely.

    Two-part fix:
    1. Set the ``"./`` precache revision to ``null`` so Workbox always fetches
       ``index.html`` from the network instead of serving a stale cached copy.
    2. Prepend ``self.skipWaiting()`` so the updated SW installs and activates
       immediately on the next update check, rather than waiting for a future
       navigation cycle.
    """
    sw_js = os.path.join(ui_dir, "sw.js")
    try:
        with open(sw_js, "r", encoding="utf-8") as fh:
            src = fh.read()
    except FileNotFoundError:
        log(f"sw.js not found in {ui_dir}; skipping service-worker rewrite")
        return

    # Idempotency guard: skip if already patched.
    # Note: "self.skipWaiting()" also appears inside MetaCubeXD's message
    # handler, so we check for our *prepended* marker specifically.
    if 'url:"./",revision:null' in src and src.startswith("self.skipWaiting();"):
        log("sw.js: already patched (index.html precache null + skipWaiting)")
        return

    # 1. Set the './' (index.html) precache revision to null
    new_src, count = re.subn(
        r'\{url:"\./",revision:"[^"]*"\}',
        '{url:"./",revision:null}',
        src,
    )

    # 2. Prepend self.skipWaiting() so the new SW activates immediately.
    #    Check startswith — we can't just search for "self.skipWaiting()"
    #    because the message handler already contains it.
    if not new_src.startswith("self.skipWaiting();"):
        new_src = "self.skipWaiting();\n" + new_src

    with open(sw_js, "w", encoding="utf-8") as fh:
        fh.write(new_src)
    log(f"rewrote sw.js: index.html precache revision set to null "
        f"({count} replacement(s)), skipWaiting() added")


class DashboardServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr, handler, ui_dir, api_url, secret, backend_url, github_token):
        super().__init__(addr, handler)
        self.ui_dir = ui_dir
        spl = urlsplit(api_url)
        self.api_target = (spl.hostname or "127.0.0.1", spl.port or 19090)
        self.secret = secret
        self.backend_url = backend_url
        self.github_token = github_token
        rewrite_config_js(ui_dir, backend_url, github_token)
        rewrite_index_html(ui_dir, backend_url, github_token)
        rewrite_sw_js(ui_dir)


def parse_args(argv=None):
    ap = argparse.ArgumentParser(description="MetaCubeXD dashboard gateway")
    ap.add_argument("--listen", default=os.environ.get("DASHBOARD_LISTEN", "0.0.0.0:9090"))
    ap.add_argument("--ui", default=os.environ.get("EXTERNAL_UI", "/etc/mihomo/ui"))
    ap.add_argument("--api", default=os.environ.get("MIHOMO_API", "http://127.0.0.1:19090"))
    ap.add_argument("--secret", default=os.environ.get("MIHOMO_SECRET", "freebox"))
    ap.add_argument("--backend-url", default=os.environ.get("DASHBOARD_BACKEND_URL", "/clash-api"))
    ap.add_argument("--github-token", default=os.environ.get("GITHUB_TOKEN", ""))
    return ap.parse_args(argv)


def main(argv=None) -> int:
    args = parse_args(argv)
    host, _, port = args.listen.partition(":")
    port = int(port)
    if not os.path.isdir(args.ui):
        log(f"FATAL: ui dir not found: {args.ui}")
        return 1
    srv = DashboardServer(
        (host, port), DashboardHandler, args.ui, args.api,
        args.secret, args.backend_url, args.github_token,
    )
    log(f"serving MetaCubeXD UI from {args.ui}")
    log(f"proxying /clash-api -> {args.api} (secret injected)")
    log(f"backendURL={args.backend_url}")
    log(f"listening on {host}:{port}")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        log("shutting down")
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
