"""2x2 phase means of a 16-bit FITS frame's central 512x512 region.

A raw Bayer frame under coloured light shows four different phase means; a
frame PHD2 reconstructed to luminance shows four nearly equal ones.

Usage: python3 phases.py <fits>
"""
import struct
import sys

path = sys.argv[1]
with open(path, "rb") as f:
    hdr = b""
    while True:
        block = f.read(2880)
        if not block:
            sys.exit("no END card before the end of the file")
        hdr += block
        if any(block[i:i + 8].rstrip(b" ") == b"END" for i in range(0, len(block), 80)):
            break
    cards = {}
    for i in range(0, len(hdr), 80):
        card = hdr[i:i + 80].decode("ascii", "replace")
        if card[8:10] == "= ":
            cards[card[:8].strip()] = card[10:].split("/")[0].strip()
    w, h = int(cards["NAXIS1"]), int(cards["NAXIS2"])
    bzero = int(float(cards.get("BZERO", "0")))
    data_start = len(hdr)
    size = 512
    x0, y0 = (w - size) // 2 & ~1, (h - size) // 2 & ~1
    sums = [[0, 0], [0, 0]]
    n = [[0, 0], [0, 0]]
    lo, hi = 1 << 20, -(1 << 20)
    for y in range(y0, y0 + size):
        f.seek(data_start + (y * w + x0) * 2)
        row = struct.unpack(f">{size}h", f.read(size * 2))
        for i, v in enumerate(row):
            v += bzero
            sums[y & 1][(x0 + i) & 1] += v
            n[y & 1][(x0 + i) & 1] += 1
            lo, hi = min(lo, v), max(hi, v)
print({
    "region": [x0, y0, size, size],
    "min": lo,
    "max": hi,
    "phase_means": [[round(sums[r][c] / n[r][c], 1) for c in (0, 1)] for r in (0, 1)],
})
