#!/usr/bin/env python3
"""Measure the camera facade and PHD2's single frame with a real QHY camera.

Usage: python3 qhyprobe.py <scratch dir> <scenario> [...]

Scenarios: facts, direct, facade, abort, direct_abort.

Every event, RPC exchange and Alpaca call is printed as one JSON line with a
time in seconds since the probe started. PHD2's event stream is read as a
second event-server client, so every SingleFrameComplete is timed here.
"""
import json
import os
import queue
import socket
import struct
import sys
import threading
import time
import urllib.parse
import urllib.request

T0 = time.monotonic()
FACADE = "http://127.0.0.1:11228/api/v1/camera/0"
TXN = [0]


def now():
    return round(time.monotonic() - T0, 4)


def log(kind, **kw):
    print(json.dumps({"t": now(), "kind": kind, **kw}), flush=True)


class Phd2:
    def __init__(self, host="127.0.0.1", port=4400):
        self.sock = socket.create_connection((host, port))
        self.rfile = self.sock.makefile("r", encoding="utf-8", newline="\r\n")
        self.next_id = 1
        self.replies = {}
        self.cv = threading.Condition()
        self.events = queue.Queue()
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        for line in self.rfile:
            line = line.strip()
            if not line:
                continue
            msg = json.loads(line)
            if "Event" in msg:
                if msg["Event"] in ("LoopingExposures", "GuideStep", "StarLost"):
                    log("event", msg={"Event": msg["Event"]})
                else:
                    log("event", msg=msg)
                self.events.put((now(), msg))
            else:
                with self.cv:
                    self.replies[msg.get("id")] = msg
                    self.cv.notify_all()

    def call(self, method, params=None):
        rid = self.next_id
        self.next_id += 1
        req = {"method": method, "id": rid}
        if params is not None:
            req["params"] = params
        sent = now()
        self.sock.sendall((json.dumps(req) + "\r\n").encode())
        with self.cv:
            self.cv.wait_for(lambda: rid in self.replies, timeout=30)
            reply = self.replies.pop(rid, None)
        log("rpc", req=req, reply=reply, sent=sent)
        return reply

    def wait_frame(self, timeout, path_prefix=None):
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                return None, None
            try:
                t, msg = self.events.get(timeout=left)
            except queue.Empty:
                return None, None
            if msg.get("Event") != "SingleFrameComplete":
                continue
            if path_prefix and not str(msg.get("Path", "")).startswith(path_prefix):
                continue
            return t, msg

    def drain(self):
        while True:
            try:
                self.events.get_nowait()
            except queue.Empty:
                return


def alpaca(method, member, form=None, accept=None, quiet=False):
    TXN[0] += 1
    url = f"{FACADE}/{member}"
    sent = now()
    if method == "GET":
        q = urllib.parse.urlencode({"ClientID": 7, "ClientTransactionID": TXN[0]})
        req = urllib.request.Request(f"{url}?{q}")
        if accept:
            req.add_header("Accept", accept)
    else:
        body = dict(form or {})
        body.update({"ClientID": 7, "ClientTransactionID": TXN[0]})
        req = urllib.request.Request(url, data=urllib.parse.urlencode(body).encode(), method="PUT")
    with urllib.request.urlopen(req, timeout=60) as r:
        raw = r.read()
        ctype = r.headers.get("Content-Type", "")
    done = now()
    if "imagebytes" in ctype:
        hdr = struct.unpack("<11i", raw[:44])
        out = {
            "imagebytes": True,
            "bytes": len(raw),
            "error": hdr[1],
            "data_start": hdr[4],
            "element_type": hdr[5],
            "transmission_type": hdr[6],
            "rank": hdr[7],
            "dims": list(hdr[8:11]),
        }
        if hdr[6] == 8:  # UInt16
            n = (len(raw) - hdr[4]) // 2
            data = struct.unpack(f"<{n}H", raw[hdr[4]:])
            out["min"] = min(data)
            out["max"] = max(data)
            out["mean"] = round(sum(data) / n, 2)
    else:
        out = json.loads(raw)
    if not quiet:
        log("alpaca", method=method, member=member, form=form, sent=sent, answered=done, reply=out)
    return out


def fits_header(path):
    cards = {}
    with open(path, "rb") as f:
        while True:
            block = f.read(2880)
            if not block:
                break
            done = False
            for i in range(0, 2880, 80):
                card = block[i:i + 80].decode("ascii", "replace")
                key = card[:8].strip()
                if key == "END":
                    done = True
                    break
                if card[8:10] == "= ":
                    cards[key] = card[10:].split("/")[0].strip()
            if done:
                break
    return cards


def scenario_facts(p, work):
    p.call("get_app_state")
    p.call("get_current_equipment")
    p.call("get_camera_frame_size")
    p.call("get_camera_binning")
    p.call("get_exposure")
    p.call("get_profile")
    p.call("get_connected")
    alpaca("PUT", "connected", {"Connected": "true"})
    for m in ["connected", "cameraxsize", "cameraysize", "sensorname", "pixelsizex",
              "pixelsizey", "sensortype", "maxadu", "exposuremin", "exposuremax",
              "camerastate", "canabortexposure", "name", "description", "driverinfo"]:
        alpaca("GET", m)


def scenario_direct(p, work):
    for ms in (100, 1000, 5000):
        path = os.path.join(work, f"direct-{ms}.fits")
        p.drain()
        p.call("capture_single_frame", {"exposure": ms, "save": True, "path": path})
        t, msg = p.wait_frame(ms / 1000 + 30, path_prefix=path)
        size = os.stat(path).st_size if os.path.exists(path) else None
        hdr = fits_header(path) if size else {}
        log("direct_frame", exposure_ms=ms, event_t=t, event=msg, file_size=size,
            header={k: hdr.get(k) for k in ("BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "BZERO",
                                            "BSCALE", "BAYERPAT", "XBINNING", "YBINNING",
                                            "EXPOSURE", "EXPTIME", "INSTRUME", "GAIN",
                                            "PIXSIZE1", "PIXSIZE2")},
            all_keys=sorted(hdr.keys()))


def scenario_facade(p, work):
    alpaca("PUT", "connected", {"Connected": "true"})
    for dur in ("0.1", "1", "5"):
        p.drain()
        alpaca("PUT", "startexposure", {"Duration": dur, "Light": "true"})
        t, msg = p.wait_frame(float(dur) + 30)
        log("facade_event", duration=dur, event_t=t, event=msg)
        ready_at = None
        for _ in range(3000):
            r = alpaca("GET", "imageready", quiet=True)
            if r.get("Value"):
                ready_at = now()
                break
            time.sleep(0.01)
        log("facade_ready", duration=dur, ready_t=ready_at)
        alpaca("GET", "camerastate")
        alpaca("GET", "lastexposureduration")
        alpaca("GET", "imagearray", accept="application/imagebytes")


def scenario_abort(p, work):
    alpaca("PUT", "connected", {"Connected": "true"})
    p.drain()
    alpaca("PUT", "startexposure", {"Duration": "5", "Light": "true"})
    time.sleep(1.0)
    alpaca("PUT", "abortexposure")
    alpaca("GET", "camerastate")
    t, msg = p.wait_frame(30)
    log("abort_event", event_t=t, event=msg)
    alpaca("GET", "camerastate")
    alpaca("GET", "imageready")
    # The camera should be idle again: a short frame right after the abort.
    p.drain()
    alpaca("PUT", "startexposure", {"Duration": "0.1", "Light": "true"})
    t, msg = p.wait_frame(30)
    log("after_abort_event", event_t=t, event=msg)
    for _ in range(3000):
        if alpaca("GET", "imageready", quiet=True).get("Value"):
            log("after_abort_ready")
            break
        time.sleep(0.01)
    alpaca("GET", "camerastate")


def scenario_abort_then_start(p, work):
    """An abort answered while the QHY frame still runs; a start sent then."""
    alpaca("PUT", "connected", {"Connected": "true"})
    p.drain()
    alpaca("PUT", "startexposure", {"Duration": "5", "Light": "true"})
    time.sleep(1.0)
    alpaca("PUT", "abortexposure")
    alpaca("PUT", "startexposure", {"Duration": "0.1", "Light": "true"})
    alpaca("GET", "camerastate")
    t, msg = p.wait_frame(30)
    log("abort_then_start_event", event_t=t, event=msg)
    time.sleep(0.2)
    alpaca("GET", "camerastate")
    alpaca("GET", "imageready")
    alpaca("PUT", "startexposure", {"Duration": "0.1", "Light": "true"})
    t, msg = p.wait_frame(30)
    log("restart_event", event_t=t, event=msg)
    for _ in range(3000):
        if alpaca("GET", "imageready", quiet=True).get("Value"):
            log("restart_ready")
            break
        time.sleep(0.01)


def scenario_header(p, work):
    path = os.path.join(work, "direct-1000.fits")
    log("header", path=path, cards=fits_header(path))


def scenario_direct_abort(p, work):
    path = os.path.join(work, "direct-abort.fits")
    p.drain()
    p.call("capture_single_frame", {"exposure": 5000, "save": True, "path": path})
    time.sleep(1.0)
    p.call("stop_capture")
    t, msg = p.wait_frame(30, path_prefix=path)
    log("direct_abort_event", event_t=t, event=msg,
        file_size=os.stat(path).st_size if os.path.exists(path) else None)
    p.call("get_app_state")


SCENARIOS = {
    "facts": scenario_facts,
    "direct": scenario_direct,
    "facade": scenario_facade,
    "abort": scenario_abort,
    "direct_abort": scenario_direct_abort,
    "abort_then_start": scenario_abort_then_start,
    "header": scenario_header,
}


def main():
    work = sys.argv[1]
    os.makedirs(work, exist_ok=True)
    p = Phd2()
    time.sleep(0.5)
    for name in sys.argv[2:]:
        log("scenario", name=name)
        try:
            SCENARIOS[name](p, work)
        except Exception as e:  # noqa: BLE001 - a probe logs and moves on
            log("scenario_error", name=name, error=repr(e))


if __name__ == "__main__":
    main()
