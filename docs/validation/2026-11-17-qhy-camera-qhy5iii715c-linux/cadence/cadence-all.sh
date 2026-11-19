#!/bin/sh
# Runs A, B and C back to back on the QHY5III715C (device 0 of qhy-camera),
# as root under systemd-run. $1 = the directory holding the scripts and the
# output root.
set -u
D=$1
cd "$D" || exit 1
OUT="$D/cadence"
mkdir -p "$OUT"
exec > "$OUT/run.out" 2>&1
mark() { echo "== $(date -u +%FT%T.%3NZ) $*"; }
mark "start; rp=$(systemctl is-active rusty-photon-rp); qhy-camera $(dpkg-query -W -f='${Version}' rusty-photon-qhy-camera)"
mark "A: full frame, bin 2, 3600 s"
python3 -I cadence.py --label A --bin 2 --frame full --seconds 3600 --cpu --out "$OUT/A"
mark "A exit=$?"
mark "B: 512 x 512 at the centre, bin 1, 600 s"
python3 -I cadence.py --label B --bin 1 --frame center512 --seconds 600 --cpu --out "$OUT/B"
mark "B exit=$?"
mark "C: full frame, bin 1, 600 s"
python3 -I cadence.py --label C --bin 1 --frame full --seconds 600 --cpu --out "$OUT/C"
mark "C exit=$?"
journalctl -u rusty-photon-qhy-camera --since "-2h" -o short-iso-precise > "$OUT/qhy-camera-journal.txt" 2>&1
chown -R "$(stat -c %u "$D"):$(stat -c %g "$D")" "$OUT"
mark "done"
touch "$OUT/DONE"
