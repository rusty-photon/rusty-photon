"""A minimal Alpaca client for loopback calls on the rig.

The endpoint (host name the certificate carries, port) and the Basic-auth
credential are read from rp's own configuration, the client side of the
observatory credential, so neither appears on a command line or in this
file. The connection goes to 127.0.0.1 and verifies the server's
certificate against the system trust store under the host name rp uses.
Run as root (rp.json is readable by its service user only).
"""

import base64
import http.client
import json
import socket
import ssl
import urllib.parse

RP_JSON = "/var/lib/rusty-photon/.config/rusty-photon/rp.json"


def _walk(o):
    if isinstance(o, dict):
        yield o
        for v in o.values():
            yield from _walk(v)
    elif isinstance(o, list):
        for v in o:
            yield from _walk(v)


def endpoint_for_port(port):
    """(host, port, user, password) of the rp entry whose alpaca_url uses `port`."""
    with open(RP_JSON) as f:
        cfg = json.load(f)
    for d in _walk(cfg):
        url = d.get("alpaca_url")
        if not url:
            continue
        u = urllib.parse.urlsplit(url)
        if u.port == port and isinstance(d.get("auth"), dict):
            return u.hostname, u.port, d["auth"]["username"], d["auth"]["password"]
    raise SystemExit(f"no rp.json entry with an alpaca_url on port {port}")


class Client:
    def __init__(self, port, timeout=30.0):
        host, port, user, password = endpoint_for_port(port)
        self.server_name = host
        self.port = port
        self.timeout = timeout
        self.ctx = ssl.create_default_context()  # system store, hostname checked
        tok = base64.b64encode(f"{user}:{password}".encode()).decode()
        self.headers = {"Authorization": f"Basic {tok}"}
        self.client_id = 4242
        self.txn = 0
        self.conn = None

    def _connect(self):
        server_name, ctx, timeout = self.server_name, self.ctx, self.timeout

        class _Loopback(http.client.HTTPSConnection):
            # TCP to loopback; SNI and certificate verification under the
            # name rp uses for the service.
            def connect(self):
                raw = socket.create_connection(("127.0.0.1", self.port), timeout)
                raw.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                self.sock = ctx.wrap_socket(raw, server_hostname=server_name)

        self.conn = _Loopback("127.0.0.1", self.port, timeout=timeout, context=ctx)

    def _conn(self):
        if self.conn is None:
            self._connect()
        return self.conn

    def close(self):
        if self.conn is not None:
            try:
                self.conn.close()
            finally:
                self.conn = None

    def request(self, method, path, params=None, accept=None, body_bytes=False):
        """Returns (status, headers, body). Retries once on a dropped keep-alive."""
        self.txn += 1
        params = dict(params or {})
        params.setdefault("ClientID", self.client_id)
        params.setdefault("ClientTransactionID", self.txn)
        headers = dict(self.headers)
        headers["Host"] = f"{self.server_name}:{self.port}"
        if accept:
            headers["Accept"] = accept
        enc = urllib.parse.urlencode(params)
        for attempt in (1, 2):
            conn = self._conn()
            try:
                if method == "GET":
                    conn.request("GET", f"{path}?{enc}", headers=headers)
                else:
                    headers["Content-Type"] = "application/x-www-form-urlencoded"
                    conn.request(method, path, body=enc, headers=headers)
                resp = conn.getresponse()
                body = resp.read()
                return resp.status, dict(resp.getheaders()), body
            except (http.client.RemoteDisconnected, BrokenPipeError, ConnectionResetError):
                self.close()
                if attempt == 2:
                    raise

    def alpaca(self, method, device_type, number, member, **params):
        """Standard Alpaca JSON call; returns Value, raises on an Alpaca error."""
        st, _, body = self.request(
            method, f"/api/v1/{device_type}/{number}/{member}", params
        )
        if st != 200:
            raise RuntimeError(f"HTTP {st} on {method} {member}: {body[:300]!r}")
        j = json.loads(body)
        if j.get("ErrorNumber", 0) != 0:
            raise RuntimeError(f"Alpaca error {j['ErrorNumber']} on {method} {member}: {j.get('ErrorMessage')}")
        return j.get("Value")
