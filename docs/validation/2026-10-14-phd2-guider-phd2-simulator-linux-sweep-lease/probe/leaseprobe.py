#!/usr/bin/env python3
"""Check the camera facade's sweep lease against a live PHD2.

Usage: python3 leaseprobe.py

Drives phd2-guider's camera facade (127.0.0.1:11228) and its REST API
(127.0.0.1:11230), and reads PHD2 (127.0.0.1:4400) as a second
event-server client. Every step is printed as one JSON line with a time in
seconds since the probe started. Every expected outcome is asserted: the
probe stops at the first one that does not hold and exits 1, so a run that
reaches "done" passed every check.
"""
import json
import socket
import struct
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

T0 = time.monotonic()
FACADE = "http://127.0.0.1:11228/api/v1/camera/0"
REST = "http://127.0.0.1:11230/api/v1"
TXN = [0]
ACQUIRE = "rustyphoton:sweep-lease-acquire"
RENEW = "rustyphoton:sweep-lease-renew"
RELEASE = "rustyphoton:sweep-lease-release"


def now():
    return round(time.monotonic() - T0, 4)


def log(kind, **kw):
    print(json.dumps({"t": now(), "kind": kind, **kw}), flush=True)


def check(condition, what, **kw):
    log("check", what=what, ok=bool(condition), **kw)
    if not condition:
        log("failed", what=what)
        sys.exit(1)


def token(letter):
    return letter * 32


class Phd2:
    """A second client of PHD2's event server."""

    def __init__(self):
        self.sock = socket.create_connection(("127.0.0.1", 4400))
        self.rfile = self.sock.makefile("r", encoding="utf-8", newline="\r\n")
        self.next_id = 1
        self.replies = {}
        self.version = None
        self.cv = threading.Condition()
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        for line in self.rfile:
            line = line.strip()
            if not line:
                continue
            msg = json.loads(line)
            with self.cv:
                if msg.get("Event") == "Version":
                    self.version = msg.get("PHDVersion")
                elif "Event" not in msg:
                    self.replies[msg.get("id")] = msg
                self.cv.notify_all()

    def call(self, method, params=None):
        rid = self.next_id
        self.next_id += 1
        req = {"method": method, "id": rid}
        if params is not None:
            req["params"] = params
        self.sock.sendall((json.dumps(req) + "\r\n").encode())
        with self.cv:
            self.cv.wait_for(lambda: rid in self.replies, timeout=30)
            reply = self.replies.pop(rid, None)
        log("rpc", req=req, reply=reply)
        return reply

    def state(self):
        return self.call("get_app_state")["result"]

    def wait_state(self, want, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.state() == want:
                return True
            time.sleep(0.25)
        return False


def alpaca(method, member, form=None, accept=None):
    """One Alpaca call. Returns (Value, ErrorNumber, ErrorMessage) for JSON,
    or the ImageBytes header and body."""
    TXN[0] += 1
    url = f"{FACADE}/{member}"
    if method == "GET":
        q = urllib.parse.urlencode({"ClientID": 7, "ClientTransactionID": TXN[0]})
        req = urllib.request.Request(f"{url}?{q}")
        if accept:
            req.add_header("Accept", accept)
    else:
        body = dict(form or {})
        body.update({"ClientID": 7, "ClientTransactionID": TXN[0]})
        req = urllib.request.Request(
            url, data=urllib.parse.urlencode(body).encode(), method="PUT"
        )
    sent = time.monotonic()
    with urllib.request.urlopen(req, timeout=60) as r:
        raw = r.read()
        ctype = r.headers.get("Content-Type", "")
    took = round(time.monotonic() - sent, 4)
    if "imagebytes" in ctype:
        header = struct.unpack("<11i", raw[:44])
        log("alpaca", method=method, member=member, took=took, imagebytes=header,
            bytes=len(raw))
        return header, raw
    reply = json.loads(raw)
    log("alpaca", method=method, member=member, form=form, took=took,
        value=reply.get("Value"), error=reply.get("ErrorNumber"),
        message=reply.get("ErrorMessage"))
    return reply.get("Value"), reply.get("ErrorNumber"), reply.get("ErrorMessage")


def action(name, parameters):
    return alpaca("PUT", "action", {"Action": name, "Parameters": parameters})


def lease(name, letter, ttl_s=None):
    parameters = {"token": token(letter)}
    if ttl_s is not None:
        parameters["ttl_s"] = ttl_s
    return action(name, json.dumps(parameters))


def rest(path, body=None):
    req = urllib.request.Request(
        f"{REST}/{path}", data=json.dumps(body or {}).encode(), method="POST",
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            status, reply = r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        status, reply = e.code, json.loads(e.read() or b"null")
    log("rest", path=path, status=status, reply=reply)
    return status, reply


def refused(result, code, needle):
    _value, error, message = result
    return error == code and needle in (message or "")


def main():
    phd2 = Phd2()
    with phd2.cv:
        phd2.cv.wait_for(lambda: phd2.version is not None, timeout=5)
    check(phd2.version is not None, "PHD2 reported its version", version=phd2.version)
    check(phd2.state() == "Stopped", "PHD2 starts Stopped")
    size = phd2.call("get_camera_frame_size").get("result")
    check(isinstance(size, list) and len(size) == 2, "PHD2's camera is connected",
          size=size)
    width, height = size

    # Not connected: an acquire is NotConnected (0x407 = 1031).
    alpaca("PUT", "connected", {"Connected": "false"})
    check(refused(lease(ACQUIRE, "0"), 1031, "not connected"),
          "an acquire on a disconnected camera is NotConnected")

    alpaca("PUT", "connected", {"Connected": "true"})
    check(alpaca("GET", "connected")[0] is True, "the facade camera connects")
    actions = alpaca("GET", "supportedactions")[0]
    check(actions[:3] == [ACQUIRE, RENEW, RELEASE],
          "SupportedActions lists the three lease Actions first", actions=actions)

    # Granted, and guiding refused while it is held.
    value, error, _ = lease(ACQUIRE, "a", 30)
    check(error == 0 and json.loads(value) == {"token": token("a"), "ttl_s": 30},
          "a lease is granted under the caller's token")
    for path, body in (("guiding/start", {}), ("guiding/pause", {"full": True}),
                       ("dither", {"amount_px": 3.0}), ("calibration/clear", {})):
        status, reply = rest(path, body)
        check(status == 409 and reply["error"] == "camera_busy"
              and "sweep lease" in reply["message"],
              f"{path} is refused with camera_busy while the lease is held")
    check(phd2.state() == "Stopped", "PHD2 is still Stopped: no guiding reached it")

    # A frame under the lease, sent as UInt16.
    alpaca("PUT", "startexposure", {"Duration": "1", "Light": "true"})
    started = time.monotonic()
    while alpaca("GET", "imageready")[0] is not True:
        check(time.monotonic() - started < 15, "the frame is ready within 15 s")
        time.sleep(0.05)
    log("ready", after=round(time.monotonic() - started, 4))
    header, raw = alpaca("GET", "imagearray", accept="application/imagebytes")
    check(header[6] == 8, "ImageBytes sends the frame as UInt16 (type 8)",
          transmission_element_type=header[6])
    check(header[8:10] == (width, height), "the image is PHD2's full frame",
          dimensions=header[8:10])
    check(len(raw) - header[4] == width * height * 2, "two bytes a pixel",
          data_bytes=len(raw) - header[4])

    value, error, _ = lease(RENEW, "a")
    check(error == 0 and json.loads(value)["ttl_s"] == 30, "the held lease renews")
    check(refused(lease(ACQUIRE, "b"), 1035, "already held"),
          "a second lease is refused while one is held")
    value, error, _ = lease(RELEASE, "a")
    check(error == 0 and json.loads(value) == {"released": True}, "the lease releases")
    value, error, _ = lease(RELEASE, "a")
    check(error == 0 and json.loads(value) == {"released": False},
          "a second release answers released false")
    check(refused(lease(ACQUIRE, "a"), 1035, "used before"),
          "a token is never granted twice")

    # PHD2 looping (set by another client): refused, naming the state.
    phd2.call("loop")
    check(phd2.wait_state("Looping"), "PHD2 loops when another client asks")
    check(refused(lease(ACQUIRE, "c"), 1035, "Looping"),
          "a lease is refused while PHD2 loops, naming the state")
    phd2.call("stop_capture")
    check(phd2.wait_state("Stopped"), "PHD2 stops again")

    # Ends: a guiding stop, a disconnect, the TTL.
    check(lease(ACQUIRE, "d", 30)[1] == 0, "a lease is granted")
    check(rest("guiding/stop")[0] == 200, "guiding/stop is never refused")
    check(refused(lease(RENEW, "d"), 1035, "guiding stop"),
          "a guiding stop ends the lease")
    check(lease(ACQUIRE, "e", 30)[1] == 0, "a lease is granted")
    alpaca("PUT", "connected", {"Connected": "false"})
    check(refused(lease(RENEW, "e"), 1035, "disconnected"),
          "a disconnect ends the lease")
    alpaca("PUT", "connected", {"Connected": "true"})
    check(lease(ACQUIRE, "f", 10)[1] == 0, "a 10 s lease is granted")
    time.sleep(11)
    check(refused(lease(RENEW, "f"), 1035, "expired"), "the lease lapses at its TTL")
    status, _reply = rest("calibration/clear")
    check(status == 200, "guiding operations are accepted once the lease has lapsed")

    # A frame in flight: refused, naming when it ends.
    alpaca("PUT", "startexposure", {"Duration": "3", "Light": "true"})
    check(refused(lease(ACQUIRE, "1"), 1035, "at the latest"),
          "a lease is refused while a frame is in flight")
    started = time.monotonic()
    while alpaca("GET", "imageready")[0] is not True:
        check(time.monotonic() - started < 15, "the frame is ready within 15 s")
        time.sleep(0.1)

    alpaca("PUT", "connected", {"Connected": "false"})
    log("done")


if __name__ == "__main__":
    main()
