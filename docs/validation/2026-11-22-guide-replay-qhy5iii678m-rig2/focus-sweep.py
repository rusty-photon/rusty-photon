"""Measure the focus sweep's frames: the median half-flux diameter of the
hour's eight guide stars in each.

Usage: python3 focus-sweep.py <sweep recording> <set recording> <focus-sweep.csv>

The sweep recording holds one frame at each of 29,230 to 29,830 in steps of
60, the set recording two frames at 29,440, both written as record.ps1
writes frames. The stars are measured about the hour's guide stars'
positions in its first frame, which followed the sweep by a few minutes:
the aperture's centroid takes up the field's drift over them. Each star's half-flux diameter is twice the radius holding half its
light, in a 22 px aperture about its centroid, its background the median of
an annulus 22 to 30 px out.
"""
import array
import csv
import math
import statistics
import struct
import sys
from pathlib import Path

STARS = [(1032.7, 938.5), (1566.8, 932.4), (1725.9, 406.6), (259.8, 704.7),
         (1648.7, 887.0), (272.2, 410.2), (818.6, 295.0), (1392.0, 163.2)]
SWEEP = [29230 + 60 * step for step in range(11)]
SET = [29440, 29440]


def frame(path):
    body = Path(path).read_bytes()
    _, _, _, _, start, _, _, _, width, height, _ = struct.unpack("<11i", body[:44])
    pixels = array.array("H")
    pixels.frombytes(body[start:start + 2 * width * height])
    # Width-major: the pixel at (x, y) is at x * height + y.
    return pixels, height


def hfd(pixels, height, cx, cy, radius=22):
    def at(x, y):
        return pixels[x * height + y]
    ix, iy = round(cx), round(cy)
    ring = [at(x, y) for x in range(ix - radius - 8, ix + radius + 9)
            for y in range(iy - radius - 8, iy + radius + 9)
            if radius < math.hypot(x - ix, y - iy) <= radius + 8]
    background = statistics.median(ring)
    light = [(x, y, max(at(x, y) - background, 0))
             for x in range(ix - radius, ix + radius + 1)
             for y in range(iy - radius, iy + radius + 1)
             if math.hypot(x - ix, y - iy) <= radius]
    total = sum(value for _, _, value in light)
    mx = sum(x * value for x, _, value in light) / total
    my = sum(y * value for _, y, value in light) / total
    gathered = 0.0
    for distance, value in sorted((math.hypot(x - mx, y - my), value) for x, y, value in light):
        gathered += value
        if gathered >= total / 2:
            return 2 * distance
    return float("nan")


def main():
    sweep, set_, target = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
    rows = []
    for directory, positions, kind in ((sweep, SWEEP, "sweep"), (set_, SET, "set")):
        with open(directory / "frames.csv", newline="") as f:
            files = [row["file"] for row in csv.DictReader(f)]
        for position, name in zip(positions, files):
            pixels, height = frame(directory / name)
            rows.append((kind, position, statistics.median(hfd(pixels, height, x, y) for x, y in STARS)))
    with open(target, "w", newline="") as f:
        out = csv.writer(f, lineterminator="\n")
        out.writerow(["frames", "position", "median_hfd_px"])
        out.writerows((kind, position, f"{value:.2f}") for kind, position, value in rows)


if __name__ == "__main__":
    main()
