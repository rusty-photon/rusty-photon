"""Summarize guide-replay's replay.csv as a row for each frame: frames.csv.

Usage: python3 summarize.py <replay.csv> <frames.csv>

Each row gives the frame's usable guide stars and whether it is star-lost,
its offset, the median half-flux diameter and signal-to-noise ratio of its
usable stars, the lowest signal-to-noise ratio among them, and the largest
deviation of any of them from the frame's offset on either axis. That
deviation is measured from the frame's offset, not from each star's mean
as the report's is.
"""
import csv
import statistics
import sys


def main():
    source, target = sys.argv[1], sys.argv[2]
    frames = {}
    with open(source, newline="") as f:
        for row in csv.DictReader(f):
            frames.setdefault(int(row["frame"]), []).append(row)
    with open(target, "w", newline="") as f:
        out = csv.writer(f, lineterminator="\n")
        out.writerow(["frame", "start_utc", "usable", "star_lost", "offset_x", "offset_y",
                      "median_hfd", "median_snr", "lowest_snr", "largest_abs_deviation"])
        for number in sorted(frames):
            rows = frames[number]
            usable = [row for row in rows if row["reading"] == "usable"]
            first = rows[0]
            snr = [float(row["snr"]) for row in usable]
            deviation = max(
                (max(abs(float(row["deviation_x"])), abs(float(row["deviation_y"])))
                 for row in usable if row["deviation_x"]),
                default=0.0,
            )
            out.writerow([
                number, first["start_utc"], len(usable), first["star_lost"],
                f"{float(first['frame_offset_x']):.3f}" if first["frame_offset_x"] else "",
                f"{float(first['frame_offset_y']):.3f}" if first["frame_offset_y"] else "",
                f"{statistics.median(float(row['hfd']) for row in usable):.3f}" if usable else "",
                f"{statistics.median(snr):.1f}" if snr else "",
                f"{min(snr):.1f}" if snr else "",
                f"{deviation:.3f}",
            ])


if __name__ == "__main__":
    main()
