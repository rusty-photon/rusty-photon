"""Summarise a cadence CSV: summarize.py RUN.csv OUT_PREFIX [EXPOSURE_S]

Writes OUT_PREFIX.json and OUT_PREFIX.md: frames, failures, frames per hour
(successful frames over the run's wall time, first request to last pixels),
and the median, 95th percentile and maximum of latency, ready_after_end,
download and cycle, with the share of frames whose latency exceeds 500 ms
and 1000 ms. Percentiles interpolate linearly between ranks (numpy's
default).
"""

import csv
import datetime
import json
import sys

METRICS = ["latency_ms", "ready_after_end_ms", "download_ms", "cycle_ms", "start_call_ms"]


def pct(sorted_vals, p):
    if not sorted_vals:
        return float("nan")
    k = (len(sorted_vals) - 1) * p / 100.0
    lo = int(k)
    hi = min(lo + 1, len(sorted_vals) - 1)
    return sorted_vals[lo] + (sorted_vals[hi] - sorted_vals[lo]) * (k - lo)


def parse_utc(s):
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))


def summarize(csv_path, exposure_s=2.0):
    rows = list(csv.DictReader(open(csv_path)))
    ok = [r for r in rows if not r["error"]]
    bad = [r for r in rows if r["error"]]
    out = {"frames": len(rows), "ok": len(ok), "failures": len(bad)}
    if rows:
        t0 = parse_utc(rows[0]["wall_utc"])
        last = rows[-1]
        span = (parse_utc(last["wall_utc"]) - t0).total_seconds()
        if last["cycle_ms"]:
            span += float(last["cycle_ms"]) / 1000.0
        out["span_s"] = round(span, 1)
        out["frames_per_hour"] = round(len(ok) / (span / 3600.0), 1) if span > 0 else None
        out["first_utc"], out["last_utc"] = rows[0]["wall_utc"], last["wall_utc"]
        out["bin"], out["frame"] = rows[0]["bin"], f'{rows[0]["numx"]}x{rows[0]["numy"]} at ({rows[0]["startx"]},{rows[0]["starty"]})'
        out["bytes"] = sorted({int(r["bytes"]) for r in ok}) if ok else []
    for m in METRICS:
        v = sorted(float(r[m]) for r in ok if r[m] != "")
        out[m] = {"median": round(pct(v, 50), 1), "p95": round(pct(v, 95), 1),
                  "max": round(v[-1], 1) if v else None, "min": round(v[0], 1) if v else None}
    lat = [float(r["latency_ms"]) for r in ok]
    out["latency_over_500ms_pct"] = round(100.0 * sum(x > 500 for x in lat) / len(lat), 2) if lat else None
    out["latency_over_1000ms_pct"] = round(100.0 * sum(x > 1000 for x in lat) / len(lat), 2) if lat else None
    out["errors"] = {}
    for r in bad:
        out["errors"][r["error"]] = out["errors"].get(r["error"], 0) + 1
    out["exposure_s"] = exposure_s
    return out


def markdown(s, label):
    lines = [f"Run {label}: bin {s.get('bin')}, frame {s.get('frame')}, {s['frames']} frames "
             f"({s['ok']} good, {s['failures']} failed) over {s.get('span_s')} s, "
             f"{s.get('frames_per_hour')} good frames per hour.", "",
             "| ms | median | p95 | max |", "|---|---|---|---|"]
    for m in METRICS:
        lines.append(f"| `{m}` | {s[m]['median']} | {s[m]['p95']} | {s[m]['max']} |")
    lines += ["", f"Latency over 500 ms: {s['latency_over_500ms_pct']} % of good frames; "
              f"over 1000 ms: {s['latency_over_1000ms_pct']} %."]
    if s["errors"]:
        lines += ["", "Failures:"] + [f"- {n} x {e}" for e, n in s["errors"].items()]
    return "\n".join(lines) + "\n"


def write(csv_path, prefix, exposure_s=2.0):
    s = summarize(csv_path, exposure_s)
    label = prefix.rsplit("summary-", 1)[-1]
    with open(prefix + ".json", "w") as f:
        json.dump(s, f, indent=1)
    with open(prefix + ".md", "w") as f:
        f.write(markdown(s, label))
    return s


if __name__ == "__main__":
    exp = float(sys.argv[3]) if len(sys.argv) > 3 else 2.0
    s = write(sys.argv[1], sys.argv[2], exp)
    print(markdown(s, sys.argv[2].rsplit("summary-", 1)[-1]))
