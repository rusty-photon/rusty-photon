"""Sustained-cadence measurement of a camera service through the Alpaca Camera API.

Usage (as root, so the client credential can be read from rp.json):

    python3 -I cadence.py --port 11121 --device 0 --label A \
        --bin 2 --frame full --seconds 3600 --exposure 2 --out DIR [--cpu]

Per frame, back to back:
  t_req      monotonic clock just before PUT startexposure (Duration, Light=true)
  t_started  just after its answer
  t_ready    when the first `true` answer to GET imageready arrived (polled
             every 25 ms from t_started)
  t_pixels   when the whole body of GET imagearray (Accept:
             application/imagebytes) has been read
Derived:
  start_call      = t_started - t_req
  ready_after_end = t_ready - (t_req + exposure)
  download        = t_pixels - t_ready
  latency         = t_pixels - (t_req + exposure)   (exposure end to pixels)
  cycle           = t_pixels - t_req

Every frame is a row, failures included (the error text in `error`); after a
failed frame the loop waits 1 s before the next request. The ImageBytes
header of every frame is checked (metadata version 1, error number 0, rank 2,
dimensions NumX x NumY); a mismatch is a failed frame.

--cpu samples the host once a minute into cpu.csv: load average, whole-host
CPU busy share over the minute, the camera service's and this script's CPU
share (of one core), the camera service's RSS, and the SoC temperature.
"""

import argparse
import csv
import datetime
import json
import os
import struct
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from alpaca_client import Client  # noqa: E402
import summarize  # noqa: E402

POLL_S = 0.025
READY_TIMEOUT_S = 60.0
COLUMNS = [
    "frame", "wall_utc", "bin", "startx", "starty", "numx", "numy", "bytes",
    "start_call_ms", "ready_after_end_ms", "download_ms", "latency_ms",
    "cycle_ms", "error",
]


def ns():
    return time.perf_counter_ns()


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


class Cam:
    def __init__(self, port, device):
        self.c = Client(port, timeout=30.0)
        self.dev = device

    def get(self, member):
        return self.c.alpaca("GET", "camera", self.dev, member)

    def put(self, member, **params):
        return self.c.alpaca("PUT", "camera", self.dev, member, **params)


def service_pid(unit):
    try:
        out = os.popen(f"systemctl show -p MainPID --value {unit}").read().strip()
        pid = int(out)
        return pid if pid > 0 else None
    except ValueError:
        return None


def proc_cpu_ticks(pid):
    with open(f"/proc/{pid}/stat") as f:
        parts = f.read().rsplit(")", 1)[1].split()
    return int(parts[11]) + int(parts[12])  # utime + stime


def proc_rss_mb(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024.0
    return float("nan")


def host_cpu():
    with open("/proc/stat") as f:
        vals = [int(v) for v in f.readline().split()[1:]]
    idle = vals[3] + vals[4]
    return sum(vals), idle


def soc_temp_c():
    try:
        with open("/sys/class/thermal/thermal_zone0/temp") as f:
            return int(f.read()) / 1000.0
    except OSError:
        return float("nan")


def cpu_sampler(path, unit, stop, interval=60.0):
    hz = os.sysconf("SC_CLK_TCK")
    me = os.getpid()
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["wall_utc", "load1", "load5", "host_busy_pct", "camera_service_cpu_pct",
                    "script_cpu_pct", "camera_service_rss_mb", "soc_temp_c"])
        f.flush()
        pid = service_pid(unit)
        t0 = time.monotonic()
        tot0, idle0 = host_cpu()
        svc0 = proc_cpu_ticks(pid) if pid else 0
        me0 = proc_cpu_ticks(me)
        while not stop.wait(interval):
            t1 = time.monotonic()
            tot1, idle1 = host_cpu()
            svc1 = proc_cpu_ticks(pid) if pid else 0
            me1 = proc_cpu_ticks(me)
            dt = t1 - t0
            busy = 100.0 * (1 - (idle1 - idle0) / max(1, tot1 - tot0))
            with open("/proc/loadavg") as la:
                l1, l5 = la.read().split()[:2]
            w.writerow([utc_now(), l1, l5, f"{busy:.1f}",
                        f"{100.0 * (svc1 - svc0) / hz / dt:.1f}",
                        f"{100.0 * (me1 - me0) / hz / dt:.1f}",
                        f"{proc_rss_mb(pid):.0f}" if pid else "",
                        f"{soc_temp_c():.1f}"])
            f.flush()
            t0, tot0, idle0, svc0, me0 = t1, tot1, idle1, svc1, me1


def check_imagebytes(body, numx, numy):
    if len(body) < 44:
        return f"short ImageBytes body ({len(body)} bytes)"
    (mver, err, _ctid, _stid, dstart, _iet, _tet, rank, d1, d2, _d3) = struct.unpack_from("<iiIIiiiiiii", body, 0)
    if mver != 1:
        return f"ImageBytes metadata version {mver}"
    if err != 0:
        msg = body[dstart:].decode("utf-8", "replace")[:200] if 0 < dstart <= len(body) else ""
        return f"ImageBytes error {err}: {msg}"
    if rank != 2 or d1 != numx or d2 != numy:
        return f"ImageBytes rank {rank} dims {d1}x{d2}, expected 2 dims {numx}x{numy}"
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=11121)
    ap.add_argument("--device", type=int, default=0)
    ap.add_argument("--unit", default="rusty-photon-qhy-camera")
    ap.add_argument("--label", required=True)
    ap.add_argument("--bin", type=int, required=True)
    ap.add_argument("--frame", choices=["full", "center512"], required=True)
    ap.add_argument("--seconds", type=float, required=True)
    ap.add_argument("--exposure", type=float, default=2.0)
    ap.add_argument("--out", required=True)
    ap.add_argument("--cpu", action="store_true")
    ap.add_argument("--gain", type=int, help="set Gain before the run (restores a value found earlier)")
    ap.add_argument("--offset", type=int, help="set Offset before the run (restores a value found earlier)")
    ap.add_argument("--max-frames", type=int, default=0, help="stop after this many frames (smoke test)")
    a = ap.parse_args()

    os.makedirs(a.out, exist_ok=True)
    cam = Cam(a.port, a.device)
    meta = {"label": a.label, "started_utc": utc_now(), "exposure_s": a.exposure,
            "poll_interval_s": POLL_S, "endpoint": "https://<rig-host>:%d (loopback)" % a.port}

    if not cam.get("connected"):
        cam.put("connected", Connected="true")
        meta["connected_by_script"] = True
    if a.gain is not None:
        cam.put("gain", Gain=a.gain)
        meta["gain_set_by_script"] = a.gain
    if a.offset is not None:
        cam.put("offset", Offset=a.offset)
        meta["offset_set_by_script"] = a.offset
    for m in ("sensorname", "sensortype", "cameraxsize", "cameraysize", "maxbinx", "maxbiny",
              "pixelsizex", "pixelsizey", "gain", "offset", "readoutmode", "readoutmodes",
              "bayeroffsetx", "bayeroffsety", "driverversion", "interfaceversion", "maxadu"):
        try:
            meta[m] = cam.get(m)
        except Exception as e:  # recorded, not fatal
            meta[m] = f"ERR {e}"

    xs, ys, b = meta["cameraxsize"], meta["cameraysize"], a.bin
    try:
        cam.put("binx", BinX=b)
        cam.put("biny", BinY=b)
    except Exception as e:
        meta["bin_refused"] = str(e)
        with open(os.path.join(a.out, "meta.json"), "w") as f:
            json.dump(meta, f, indent=1)
        print(f"bin {b} refused: {e}", file=sys.stderr)
        sys.exit(3)
    if a.frame == "full":
        sx, sy, nx, ny = 0, 0, xs // b, ys // b
    else:
        nx = ny = 512 // b
        sx, sy = (xs // b - nx) // 2, (ys // b - ny) // 2
        sx -= sx % 2  # keep the Bayer phase
        sy -= sy % 2
    cam.put("startx", StartX=sx)
    cam.put("starty", StartY=sy)
    cam.put("numx", NumX=nx)
    cam.put("numy", NumY=ny)
    meta["frame_set"] = {"binx": cam.get("binx"), "biny": cam.get("biny"), "startx": cam.get("startx"),
                         "starty": cam.get("starty"), "numx": cam.get("numx"), "numy": cam.get("numy")}
    with open(os.path.join(a.out, "meta.json"), "w") as f:
        json.dump(meta, f, indent=1)

    stop = threading.Event()
    sampler = None
    if a.cpu:
        sampler = threading.Thread(target=cpu_sampler, args=(os.path.join(a.out, "cpu.csv"), a.unit, stop),
                                   daemon=True)
        sampler.start()

    csv_path = os.path.join(a.out, f"run-{a.label}.csv")
    exp_ns = int(a.exposure * 1e9)
    run_start = ns()
    frame = 0
    with open(csv_path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(COLUMNS)
        while (ns() - run_start) < a.seconds * 1e9 and not (a.max_frames and frame >= a.max_frames):
            frame += 1
            wall = utc_now()
            row = {"frame": frame, "wall_utc": wall, "bin": b, "startx": sx, "starty": sy,
                   "numx": nx, "numy": ny, "bytes": "", "start_call_ms": "", "ready_after_end_ms": "",
                   "download_ms": "", "latency_ms": "", "cycle_ms": "", "error": ""}
            try:
                t_req = ns()
                cam.put("startexposure", Duration=a.exposure, Light="true")
                t_started = ns()
                row["start_call_ms"] = f"{(t_started - t_req) / 1e6:.1f}"
                t_ready = None
                while True:
                    t_poll = ns()
                    ready = cam.get("imageready")
                    t_ans = ns()
                    if ready:
                        t_ready = t_ans
                        break
                    if (t_ans - t_started) > READY_TIMEOUT_S * 1e9:
                        raise RuntimeError(f"imageready not true {READY_TIMEOUT_S:.0f} s after the start")
                    left = POLL_S - (ns() - t_poll) / 1e9
                    if left > 0:
                        time.sleep(left)
                row["ready_after_end_ms"] = f"{(t_ready - t_req - exp_ns) / 1e6:.1f}"
                st, hdrs, body = cam.c.request("GET", f"/api/v1/camera/{a.device}/imagearray",
                                               accept="application/imagebytes")
                t_pixels = ns()
                row["bytes"] = len(body)
                if st != 200:
                    raise RuntimeError(f"imagearray HTTP {st}: {body[:200]!r}")
                ctype = {k.lower(): v for k, v in hdrs.items()}.get("content-type", "")
                if "imagebytes" not in ctype:
                    raise RuntimeError(f"imagearray answered {ctype!r}, not application/imagebytes")
                bad = check_imagebytes(body, nx, ny)
                if bad:
                    raise RuntimeError(bad)
                row["download_ms"] = f"{(t_pixels - t_ready) / 1e6:.1f}"
                row["latency_ms"] = f"{(t_pixels - t_req - exp_ns) / 1e6:.1f}"
                row["cycle_ms"] = f"{(t_pixels - t_req) / 1e6:.1f}"
            except Exception as e:
                row["error"] = f"{type(e).__name__}: {e}"[:300]
                w.writerow([row[c] for c in COLUMNS])
                f.flush()
                cam.c.close()
                time.sleep(1.0)
                continue
            w.writerow([row[c] for c in COLUMNS])
            f.flush()
    stop.set()
    if sampler:
        sampler.join(timeout=5)
    meta["ended_utc"] = utc_now()
    meta["frames"] = frame
    with open(os.path.join(a.out, "meta.json"), "w") as f:
        json.dump(meta, f, indent=1)
    summarize.write(csv_path, os.path.join(a.out, f"summary-{a.label}"), a.exposure)


if __name__ == "__main__":
    main()
