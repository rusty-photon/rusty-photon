#!/bin/sh
# Run D on the QHY5III715C (device 0 of qhy-camera): full frame, bin 1,
# 3600 s, the frame the guider reads from the colour sensor. rp is stopped
# for the run and started again however the run ends. As root under
# systemd-run; $1 = the directory holding the scripts and the output root.
set -u
D=$1
cd "$D" || exit 1
OUT="$D/cadence-d"
mkdir -p "$OUT"
exec > "$OUT/run.out" 2>&1
mark() { echo "== $(date -u +%FT%T.%3NZ) $*"; }
finish() {
  systemctl start rusty-photon-rp
  mark "rp=$(systemctl is-active rusty-photon-rp)"
  chown -R "$(stat -c %u "$D"):$(stat -c %g "$D")" "$OUT"
  touch "$OUT/DONE"
}
trap finish EXIT
mark "start; rp=$(systemctl is-active rusty-photon-rp); qhy-camera $(dpkg-query -W -f='${Version}' rusty-photon-qhy-camera)"
systemctl stop rusty-photon-rp
mark "rp stopped: $(systemctl is-active rusty-photon-rp)"
mark "D: full frame, bin 1, 3600 s"
python3 -I cadence.py --label D --bin 1 --frame full --seconds 3600 --cpu --out "$OUT/D"
mark "D exit=$?"
journalctl -u rusty-photon-qhy-camera --since "-70min" -o short-iso-precise > "$OUT/qhy-camera-journal.txt" 2>&1
mark "done"
