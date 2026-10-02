#!/usr/bin/env bash
# Capture frame-timing / thermal data from a paired Steam Frame and summarise.
#
#   tools/perf-capture.sh [--duration S] [--out DIR] [--launch] [--video PATH-ON-HEADSET]
#
# Protocol with the app (crates/app): when the file
#   ${XDG_DATA_HOME:-~/.local/share}/frameplayer/perf/ENABLE
# exists (or FP_PERF_LOG=1 is set), FramePlayer appends one CSV row per
# presented frame to perf/frame_timing-<unix>.csv with the header
#   t_ns,display_period_ns,cpu_ms,gpu_ms,decode_queue,dropped
# This script creates ENABLE, optionally launches the app, samples SoC
# temperatures and GPU clock once per second, then pulls everything into
# $OUT and prints a summary (mean / p50 / p99 frame time, dropped frames,
# max temperature).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DUR=60
OUT=perf/$(date +%Y%m%d-%H%M%S)
LAUNCH=0
VIDEO=""
while [ $# -gt 0 ]; do
  case "$1" in
    --duration) DUR=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --launch) LAUNCH=1; shift ;;
    --video) VIDEO=$2; shift 2 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
mkdir -p "$OUT"
FPI=${FPI:-"cargo run -q --release --manifest-path $ROOT/Cargo.toml -p fp-installer --"}
CFG=$(mktemp "${TMPDIR:-/tmp}/frame-ssh.XXXXXX")
trap 'rm -f "$CFG"' EXIT
$FPI ${FRAME_DEVICE:+--device "$FRAME_DEVICE"} ssh-config --alias frame >"$CFG"
rsh() { ssh -F "$CFG" frame "$@"; }

PERF='${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer/perf'
rsh "mkdir -p $PERF && touch $PERF/ENABLE && date +%s > $PERF/.capture-start"
if [ -n "$VIDEO" ]; then
  # The app opens this file at start when FP_OPEN is present.  [coordinate with crates/app]
  rsh "printf '%s\n' $(printf %q "$VIDEO") > $PERF/OPEN"
fi
if [ "$LAUNCH" = 1 ]; then $FPI ${FRAME_DEVICE:+--device "$FRAME_DEVICE"} launch; fi

echo "==> sampling for ${DUR}s"
# [verify] sysfs paths on the Frame (SM8650): thermal zone names and the
# Adreno devfreq node. The loop tolerates missing files.
rsh "for i in \$(seq $DUR); do
  ts=\$(date +%s)
  for z in /sys/class/thermal/thermal_zone*; do
    [ -r \$z/temp ] && printf '%s,%s,%s\n' \$ts \"\$(cat \$z/type 2>/dev/null)\" \"\$(cat \$z/temp)\"
  done
  for g in /sys/class/kgsl/kgsl-3d0/devfreq/cur_freq /sys/class/drm/card0/device/devfreq/*/cur_freq; do
    [ -r \"\$g\" ] && printf '%s,gpu_freq_hz,%s\n' \$ts \"\$(cat \$g)\" && break
  done
  [ -r /sys/class/power_supply/battery/capacity ] && printf '%s,battery_pct,%s\n' \$ts \"\$(cat /sys/class/power_supply/battery/capacity)\"
  sleep 1
done" > "$OUT/sensors.csv" || true

echo "==> pulling logs"
rsh "rm -f $PERF/ENABLE; start=\$(cat $PERF/.capture-start); cd $PERF && find . -name 'frame_timing-*.csv' -newermt @\$start -print0 | xargs -0r tar -cf - " | tar -xf - -C "$OUT" 2>/dev/null || true
# SteamVR's own logs for compositor-side timing. [verify] location on the Frame.
for f in vrcompositor.txt vrserver.txt vrmonitor.txt; do
  rsh "cat \$HOME/.local/share/Steam/logs/$f 2>/dev/null | tail -n 5000" > "$OUT/$f" || true
  [ -s "$OUT/$f" ] || rm -f "$OUT/$f"
done
rsh 'tail -n 2000 ${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer/logs/launcher.log 2>/dev/null' > "$OUT/launcher.log" || true

echo "==> summary ($OUT)"
shopt -s nullglob
csvs=("$OUT"/frame_timing-*.csv)
if [ ${#csvs[@]} -eq 0 ]; then
  echo "  no frame_timing CSVs (was FramePlayer running with perf logging?)"
else
  for c in "${csvs[@]}"; do
    # Percentiles via sort(1) so this works with any awk (macOS included).
    p=$(awk -F, 'NR>1 && $2>0 {print $2/1e6}' "$c" | sort -n | awk '{a[NR]=$1} END {if (NR) printf "%.2f %.2f", a[int(NR*0.5)+1], a[int(NR*0.99)>0?int(NR*0.99):1]}')
    awk -F, -v pct="$p" 'NR>1 && $2>0 { n++; s+=$2/1e6; d+=$6; g+=$4; if ($4>gmax) gmax=$4 }
      END {
        if (n==0) { print "  " FILENAME ": empty"; exit }
        split(pct, q, " ")
        printf "  %s: %d frames, period mean %.2f ms (%.1f Hz), p50 %s, p99 %s ms; GPU mean %.2f ms max %.2f ms; dropped %d (%.2f%%)\n",
          FILENAME, n, s/n, 1000/(s/n), q[1], q[2], g/n, gmax, d, 100*d/n
      }' "$c"
  done
fi
if [ -s "$OUT/sensors.csv" ]; then
  awk -F, '$2 !~ /gpu_freq|battery/ { if ($3>m[$2]) m[$2]=$3 } $2=="gpu_freq_hz" { if ($3>gf) gf=$3; if (gmin==""||$3<gmin) gmin=$3 }
    END { for (k in m) if (m[k] > 1000) printf "  max %-24s %.1f C\n", k, m[k]/1000; if (gf) printf "  GPU clock %d-%d MHz\n", gmin/1e6, gf/1e6 }' "$OUT/sensors.csv" | sort -k2
fi
