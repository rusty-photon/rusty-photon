#!/usr/bin/env python3
"""Measure how PHD2 completes capture_single_frame, against a live PHD2.

Usage: python3 phd2probe.py <workdir> [scenario ...]

Every event and RPC exchange is printed as one JSON line with a time in
seconds since the probe started. Scenarios run in the order given.
"""
import json
import os
import queue
import socket
import struct
import sys
import threading
import time

T0 = time.monotonic()


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
                if msg["Event"] == "SingleFrameComplete" and msg.get("Path"):
                    p = msg["Path"]
                    msg["_stat_at_event"] = (
                        os.stat(p).st_size if os.path.exists(p) else None
                    )
                log("event", msg=msg)
                self.events.put((now(), msg))
            else:
                with self.cv:
                    self.replies[msg.get("id")] = msg
                    self.cv.notify_all()

    def call(self, method, params=None, quiet=False):
        rid = self.next_id
        self.next_id += 1
        req = {"method": method, "id": rid}
        if params is not None:
            req["params"] = params
        t_sent = now()
        self.sock.sendall((json.dumps(req) + "\r\n").encode())
        with self.cv:
            self.cv.wait_for(lambda: rid in self.replies, timeout=30)
            reply = self.replies.pop(rid, None)
        if not quiet:
            log("rpc", req=req, reply=reply, sent=t_sent)
        return reply

    def wait_event(self, name, timeout):
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                return None
            try:
                t, msg = self.events.get(timeout=left)
            except queue.Empty:
                return None
            if msg.get("Event") == name:
                return t, msg

    def drain(self):
        while not self.events.empty():
            self.events.get_nowait()


def fits_header(path):
    cards = []
    with open(path, "rb") as f:
        while True:
            block = f.read(2880)
            if len(block) < 2880:
                break
            for i in range(0, 2880, 80):
                card = block[i : i + 80].decode("ascii", "replace").rstrip()
                cards.append(card)
                if card.startswith("END") and card.strip() == "END":
                    return cards
    return cards


def poll_states_until(p, name, timeout, interval=0.05):
    """Poll get_app_state until the named event arrives; log each change."""
    states = []
    last = None
    deadline = time.monotonic() + timeout
    got = None
    while time.monotonic() < deadline:
        r = p.call("get_app_state", quiet=True)
        st = r.get("result") if r else None
        if st != last:
            log("state", state=st)
            states.append((now(), st))
            last = st
        try:
            t, msg = p.events.get(timeout=interval)
            if msg.get("Event") == name:
                got = (t, msg)
                break
        except queue.Empty:
            pass
    # a few more polls after completion
    for _ in range(6):
        r = p.call("get_app_state", quiet=True)
        st = r.get("result") if r else None
        if st != last:
            log("state", state=st)
            last = st
        time.sleep(interval)
    return got


def ensure_ready(p):
    p.call("stop_capture")
    time.sleep(0.5)
    r = p.call("get_connected")
    if not (r and r.get("result")):
        p.call("set_connected", [True])
        time.sleep(2)
    p.drain()


def sc_baseline(p, wd):
    for m in [
        "get_app_state",
        "get_profile",
        "get_connected",
        "get_camera_frame_size",
        "get_exposure",
        "get_exposure_durations",
        "get_camera_binning",
        "get_pixel_scale",
        "get_use_subframes",
        "get_paused",
    ]:
        p.call(m)


def capture_and_wait(p, params, timeout):
    t_req = now()
    r = p.call("capture_single_frame", params)
    got = poll_states_until(p, "SingleFrameComplete", timeout)
    if got:
        log("complete", after_request_s=round(got[0] - t_req, 4), msg=got[1])
    else:
        log("complete", missing=True, waited_s=timeout)
    return r, got


def sc_path(p, wd):
    ensure_ready(p)
    path = os.path.join(wd, "path-2000.fits")
    _, got = capture_and_wait(p, {"exposure": 2000, "save": True, "path": path}, 15)
    if os.path.exists(path):
        log("file", path=path, size=os.path.getsize(path), header=fits_header(path))


def sc_default_dir(p, wd):
    ensure_ready(p)
    _, got = capture_and_wait(p, {"exposure": 1000, "save": True}, 15)
    if got and got[1].get("Path"):
        path = got[1]["Path"]
        log("file", path=path, size=os.path.getsize(path))
        os.remove(path)


def sc_nosave(p, wd):
    ensure_ready(p)
    capture_and_wait(p, {"exposure": 1000}, 15)


def sc_second_capture(p, wd):
    ensure_ready(p)
    path = os.path.join(wd, "second-a.fits")
    p.call("capture_single_frame", {"exposure": 3000, "save": True, "path": path})
    time.sleep(0.5)
    p.call(
        "capture_single_frame",
        {"exposure": 1000, "save": True, "path": os.path.join(wd, "second-b.fits")},
    )
    got = poll_states_until(p, "SingleFrameComplete", 15)
    log("complete", msg=got[1] if got else None)
    time.sleep(1.5)
    log("files", a=os.path.exists(path), b=os.path.exists(os.path.join(wd, "second-b.fits")))


def sc_loop_during(p, wd):
    ensure_ready(p)
    path = os.path.join(wd, "loop-during.fits")
    p.call("capture_single_frame", {"exposure": 3000, "save": True, "path": path})
    time.sleep(0.5)
    p.call("loop")
    got = poll_states_until(p, "SingleFrameComplete", 15)
    log("complete", msg=got[1] if got else None)
    time.sleep(3)
    p.call("get_app_state")
    p.call("stop_capture")
    time.sleep(1)
    p.call("get_app_state")


def sc_stop_during(p, wd):
    ensure_ready(p)
    path = os.path.join(wd, "stop-during.fits")
    t_req = now()
    p.call("capture_single_frame", {"exposure": 8000, "save": True, "path": path})
    time.sleep(1.0)
    t_stop = now()
    p.call("stop_capture")
    got = poll_states_until(p, "SingleFrameComplete", 15)
    if got:
        log("complete", after_request_s=round(got[0] - t_req, 4),
            after_stop_s=round(got[0] - t_stop, 4), msg=got[1])
    else:
        log("complete", missing=True)
    log("file", exists=os.path.exists(path))


def sc_pause_during(p, wd):
    ensure_ready(p)
    path = os.path.join(wd, "pause-during.fits")
    p.call("capture_single_frame", {"exposure": 3000, "save": True, "path": path})
    time.sleep(0.5)
    p.call("set_paused", [True, "full"])
    got = poll_states_until(p, "SingleFrameComplete", 8)
    log("complete", msg=got[1] if got else None)
    p.call("get_paused")
    p.call("set_paused", [False])
    got = poll_states_until(p, "SingleFrameComplete", 8)
    log("complete-after-resume", msg=got[1] if got else None)
    log("file", exists=os.path.exists(path))
    p.call("stop_capture")


def sc_guide_during(p, wd):
    ensure_ready(p)
    path = os.path.join(wd, "guide-during.fits")
    p.call("capture_single_frame", {"exposure": 3000, "save": True, "path": path})
    time.sleep(0.5)
    p.call("guide", {"settle": {"pixels": 1.5, "time": 2, "timeout": 30}})
    got = poll_states_until(p, "SingleFrameComplete", 15)
    log("complete", msg=got[1] if got else None)
    for _ in range(10):
        time.sleep(1)
        p.call("get_app_state")
    p.call("stop_capture")
    time.sleep(1)
    log("file", exists=os.path.exists(path))


def sc_bad_params(p, wd):
    ensure_ready(p)
    existing = os.path.join(wd, "exists.fits")
    open(existing, "wb").close()
    p.call("capture_single_frame", {"exposure": 1000, "save": True, "path": existing})
    p.call("capture_single_frame", {"exposure": 1000, "save": True, "path": "rel.fits"})
    p.call("capture_single_frame", {"exposure": 0})
    p.call("capture_single_frame", {"exposure": 600001})
    p.call("capture_single_frame", {"exposure": 1000, "save": False, "path": os.path.join(wd, "x.fits")})
    p.call("capture_single_frame", {"exposure": 1500.0})
    p.drain()


def sc_bad_dir(p, wd):
    ensure_ready(p)
    capture_and_wait(
        p, {"exposure": 1000, "save": True, "path": os.path.join(wd, "no-such-dir", "f.fits")}, 10
    )


def sc_disconnected(p, wd):
    p.call("stop_capture")
    time.sleep(0.5)
    p.call("set_connected", [False])
    time.sleep(1)
    p.call("capture_single_frame", {"exposure": 1000, "save": True, "path": os.path.join(wd, "disc.fits")})
    time.sleep(2)
    p.call("set_connected", [True])
    time.sleep(2)


def sc_looping_refusal(p, wd):
    ensure_ready(p)
    p.call("loop")
    time.sleep(2)
    p.call("get_app_state")
    p.call("capture_single_frame", {"exposure": 1000, "save": True, "path": os.path.join(wd, "while-looping.fits")})
    p.call("stop_capture")
    time.sleep(1)


def sc_short(p, wd):
    """A short exposure: is the state change ever visible to a 50 ms poll?"""
    ensure_ready(p)
    for i, ms in enumerate([1, 10, 100]):
        path = os.path.join(wd, f"short-{ms}.fits")
        capture_and_wait(p, {"exposure": ms, "save": True, "path": path}, 10)


SCENARIOS = {
    "baseline": sc_baseline,
    "path": sc_path,
    "default_dir": sc_default_dir,
    "nosave": sc_nosave,
    "second_capture": sc_second_capture,
    "loop_during": sc_loop_during,
    "stop_during": sc_stop_during,
    "pause_during": sc_pause_during,
    "guide_during": sc_guide_during,
    "bad_params": sc_bad_params,
    "bad_dir": sc_bad_dir,
    "disconnected": sc_disconnected,
    "looping_refusal": sc_looping_refusal,
    "short": sc_short,
}


def main():
    wd = sys.argv[1]
    os.makedirs(wd, exist_ok=True)
    names = sys.argv[2:] or list(SCENARIOS)
    p = Phd2()
    time.sleep(1.0)  # let the Version/AppState greeting arrive
    for name in names:
        log("scenario", name=name)
        SCENARIOS[name](p, wd)
    log("done")


if __name__ == "__main__":
    main()
