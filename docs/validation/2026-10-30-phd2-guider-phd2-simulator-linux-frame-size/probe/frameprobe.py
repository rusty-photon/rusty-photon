#!/usr/bin/env python3
"""Check the camera facade's connect against a PHD2 that reports 0 x 0.

Usage: python3 frameprobe.py <image_dir>

Drives phd2-guider's camera facade (127.0.0.1:11228) and reads PHD2
(127.0.0.1:4400) as a second event-server client. PHD2's equipment must be
connected and no frame taken since, so that PHD2 does not know the frame
size yet; the probe asserts that before it connects the facade. Every step
is printed as one JSON line with a time in seconds since the probe started.
Every expected outcome is asserted: the probe stops at the first one that
does not hold and exits 1, so a run that reaches "done" passed every check.

Counting PHD2's frames needs no wait. PHD2 sends an event to every client
before it reads its next request, and the facade answers a connect that
takes a frame only after that frame's SingleFrameComplete. So when the
probe's next request to PHD2 is answered, any SingleFrameComplete the
connect caused has already reached the probe.
"""
import json
import os
import socket
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


def check(condition, what, **kw):
    log("check", what=what, ok=bool(condition), **kw)
    if not condition:
        log("failed", what=what)
        sys.exit(1)


class Phd2:
    """A second client of PHD2's event server."""

    def __init__(self):
        self.sock = socket.create_connection(("127.0.0.1", 4400))
        self.rfile = self.sock.makefile("r", encoding="utf-8", newline="\r\n")
        self.next_id = 1
        self.replies = {}
        self.events = []
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
                if "Event" in msg:
                    if msg["Event"] == "Version":
                        self.version = msg.get("PHDVersion")
                    self.events.append(msg)
                    log("event", event={k: v for k, v in msg.items()
                                        if k not in ("Timestamp", "Host", "Inst")})
                else:
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

    def frames_since(self, mark):
        with self.cv:
            return [e for e in self.events[mark:] if e["Event"] == "SingleFrameComplete"]


def alpaca(method, member, form=None):
    """One Alpaca call. Returns (Value, ErrorNumber, ErrorMessage, seconds)."""
    TXN[0] += 1
    url = f"{FACADE}/{member}"
    if method == "GET":
        q = urllib.parse.urlencode({"ClientID": 7, "ClientTransactionID": TXN[0]})
        req = urllib.request.Request(f"{url}?{q}")
    else:
        body = dict(form or {})
        body.update({"ClientID": 7, "ClientTransactionID": TXN[0]})
        req = urllib.request.Request(
            url, data=urllib.parse.urlencode(body).encode(), method="PUT"
        )
    sent = time.monotonic()
    with urllib.request.urlopen(req, timeout=60) as r:
        reply = json.loads(r.read())
    took = round(time.monotonic() - sent, 4)
    log("alpaca", method=method, member=member, form=form, took=took,
        value=reply.get("Value"), error=reply.get("ErrorNumber"),
        message=reply.get("ErrorMessage"))
    return reply.get("Value"), reply.get("ErrorNumber"), reply.get("ErrorMessage"), took


def connect_and_read(phd2, what):
    """Connect the facade, then read what it serves and what PHD2 reports."""
    with phd2.cv:
        mark = len(phd2.events)
    _, error, message, took = alpaca("PUT", "connected", {"Connected": "true"})
    check(error == 0, f"{what}: Connected = true succeeds", message=message, took=took)
    size = phd2.call("get_camera_frame_size").get("result")
    frames = phd2.frames_since(mark)
    served = {m: alpaca("GET", m)[0]
              for m in ("connected", "cameraxsize", "cameraysize", "sensorname")}
    check(served["connected"] is True, f"{what}: the facade reads Connected")
    return size, frames, served


def main():
    image_dir = sys.argv[1]
    phd2 = Phd2()
    with phd2.cv:
        phd2.cv.wait_for(lambda: phd2.version is not None, timeout=5)
    check(phd2.version == "2.6.14", "PHD2 reported its version", version=phd2.version)
    profile = phd2.call("get_profile").get("result") or {}
    check(profile.get("name") == "Simulator", "PHD2's profile is the Simulator",
          profile=profile)
    camera = (phd2.call("get_current_equipment").get("result") or {}).get("camera") or {}
    check(camera.get("name") == "Simulator" and camera.get("connected") is True,
          "PHD2's Simulator camera is connected", camera=camera)
    check(phd2.call("get_app_state").get("result") == "Stopped", "PHD2 is Stopped")
    size = phd2.call("get_camera_frame_size").get("result")
    check(size == [0, 0], "PHD2 does not know the frame size yet", size=size)
    check(os.listdir(image_dir) == [], "image_dir starts empty",
          entries=os.listdir(image_dir))
    check(alpaca("GET", "connected")[0] is False, "the facade starts disconnected")

    # First connect: PHD2 reports 0 x 0, so the facade takes one frame.
    size, frames, served = connect_and_read(phd2, "first connect")
    check(len(frames) == 1, "first connect: PHD2 took exactly one frame",
          frames=len(frames))
    frame = frames[0]
    path = frame.get("Path") or ""
    check(frame.get("Success") is True, "first connect: the frame succeeded")
    check(os.path.dirname(path) == image_dir
          and os.path.basename(path).startswith("phd2-guider-"),
          "first connect: the frame was the facade's, in image_dir", path=path)
    check(not os.path.exists(path), "first connect: the frame's file was deleted",
          path=path)
    check(size == [752, 580], "first connect: PHD2 now reports the Simulator's size",
          size=size)
    check([served["cameraxsize"], served["cameraysize"]] == size,
          "first connect: the facade serves PHD2's size", served=served)
    check(served["sensorname"] == "Simulator",
          "first connect: SensorName is PHD2's camera name", served=served)
    check(alpaca("PUT", "connected", {"Connected": "false"})[1] == 0,
          "first connect: Connected = false succeeds")

    # Second connect: PHD2 knows the size now, so no frame is taken.
    size, frames, served = connect_and_read(phd2, "second connect")
    check(frames == [], "second connect: PHD2 took no frame", frames=len(frames))
    check([served["cameraxsize"], served["cameraysize"]] == size == [752, 580],
          "second connect: the facade serves PHD2's size", served=served)
    check(alpaca("PUT", "connected", {"Connected": "false"})[1] == 0,
          "second connect: Connected = false succeeds")

    check(os.listdir(image_dir) == [],
          "image_dir is empty after: no frame file, no restart marker",
          entries=os.listdir(image_dir))
    log("done")


if __name__ == "__main__":
    main()
