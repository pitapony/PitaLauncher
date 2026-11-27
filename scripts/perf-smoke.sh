#!/usr/bin/env bash
#
# perf-smoke.sh — measure the launcher's three performance budgets.
#
#   cold start to the first painted library  < 2000 ms  (with a 500-game library)
#   scroll frame interval                     < 16.7 ms  (60 fps, 95th percentile)
#   peak resident memory with the webview     < 300 MiB  (the whole process group)
#
# The numbers are judged by `pitagames-cli perf check` against
# `pitagames_core::perf`'s budgets, so the container and the two target machines
# cannot disagree about what "pass" means. The launcher writes its own half of
# the measurement (cold start + scroll frames) to `PITAGAMES_PERF_FILE`; this
# script seeds the library, drives the scroll with xdotool, measures the
# process group's resident memory and hands everything to the CLI.
#
# Usage:
#   scripts/perf-smoke.sh [--require <metrics>] [--games <n>] [--display <:N>]
#                         [--target <dir>] [--json] [--keep]
#
#   --require   Metrics to enforce (default: memory). The container can only
#               trust memory — a software renderer cannot show 60 fps and its
#               cold start is not the machine's — so it records the other two.
#               A hardware run passes `--require cold-start,frame-time,memory`.
#   --games     Library size to seed (default: 500, the budget's library).
#   --display   Use an existing X display instead of starting Xvfb (hardware).
#   --target    Cargo target dir holding the built binaries
#               (default: $CARGO_TARGET_DIR, else `target`).
#   --json      Print the CLI report as JSON.
#   --keep      Keep the work directory (fixtures, logs) for inspection.
#
# Env:
#   PITAGAMES_PERF_OUT  Also copy the final measurement JSON here.
#
# Requires: a built `pitagames` and `pitagames-cli`, xdotool, dbus-run-session
# (optional), and either $DISPLAY or Xvfb.

set -euo pipefail

GAMES=500
REQUIRE="memory"
DISPLAY_ARG=""
TARGET="${CARGO_TARGET_DIR:-target}"
JSON=0
KEEP=0

while [ $# -gt 0 ]; do
    case "$1" in
        --require) REQUIRE="${2:?--require needs a metric list}"; shift 2 ;;
        --require=*) REQUIRE="${1#--require=}"; shift ;;
        --games) GAMES="${2:?--games needs a count}"; shift 2 ;;
        --games=*) GAMES="${1#--games=}"; shift ;;
        --display) DISPLAY_ARG="${2:?--display needs a display}"; shift 2 ;;
        --display=*) DISPLAY_ARG="${1#--display=}"; shift ;;
        --target) TARGET="${2:?--target needs a dir}"; shift 2 ;;
        --target=*) TARGET="${1#--target=}"; shift ;;
        --json) JSON=1; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "perf-smoke: unknown argument $1" >&2; exit 2 ;;
    esac
done

BIN="$TARGET/debug/pitagames"
CLI="$TARGET/debug/pitagames-cli"
for binary in "$BIN" "$CLI"; do
    if [ ! -x "$binary" ]; then
        echo "perf-smoke: $binary is missing; build it first" >&2
        exit 2
    fi
done

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pitagames-perf.XXXXXX")"
XVFB_PID=""
SAMPLER_PID=""
APP_ROOT=""

cleanup() {
    [ -n "$SAMPLER_PID" ] && kill "$SAMPLER_PID" 2>/dev/null || true
    if [ -n "$APP_ROOT" ]; then
        kill -TERM -- "-$APP_ROOT" 2>/dev/null || true
        sleep 1
        kill -KILL -- "-$APP_ROOT" 2>/dev/null || true
    fi
    if [ -n "$XVFB_PID" ]; then
        kill "$XVFB_PID" 2>/dev/null || true
    fi
    if [ "$KEEP" -eq 1 ]; then
        echo "perf-smoke: kept $WORK" >&2
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# --- the fixture: a Steam library of $GAMES installed games -------------------
STEAM="$WORK/steam"
mkdir -p "$STEAM/steamapps"
for i in $(seq 1 "$GAMES"); do
    appid=$((1000000 + i))
    printf '"AppState"\n{\n\t"appid"\t\t"%s"\n\t"name"\t\t"Perf Game %03d"\n\t"installdir"\t"Perf Game %03d"\n\t"StateFlags"\t"4"\n\t"SizeOnDisk"\t"1073741824"\n}\n' \
        "$appid" "$i" "$i" > "$STEAM/steamapps/appmanifest_$appid.acf"
done

export XDG_DATA_HOME="$WORK/data"
export XDG_CONFIG_HOME="$WORK/config"
export PITAGAMES_STEAM_ROOT="$STEAM"
export PITAGAMES_NO_DESKTOP_ENTRY=1
export PITAGAMES_PERF_FILE="$WORK/perf.json"
mkdir -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME"

# --- a display ----------------------------------------------------------------
if [ -n "$DISPLAY_ARG" ]; then
    export DISPLAY="$DISPLAY_ARG"
elif [ -n "${DISPLAY:-}" ] && xdpyinfo >/dev/null 2>&1; then
    : # an existing display (hardware)
else
    export DISPLAY=":99"
    Xvfb "$DISPLAY" -screen 0 1920x1080x24 >"$WORK/xvfb.log" 2>&1 &
    XVFB_PID=$!
    for _ in $(seq 1 50); do
        xdpyinfo >/dev/null 2>&1 && break
        sleep 0.1
    done
fi

# --- launch -------------------------------------------------------------------
# `setsid` puts the launcher, the webview and WebKit's helpers in one session
# whose id is the pid, so the memory sample and the teardown are both the whole
# process group and nothing of this script.
if command -v dbus-run-session >/dev/null 2>&1; then
    setsid dbus-run-session -- "$BIN" >"$WORK/app.log" 2>&1 &
else
    setsid "$BIN" >"$WORK/app.log" 2>&1 &
fi
APP_ROOT=$!

# Wait for the launcher to appear, then for the first painted library.
for _ in $(seq 1 100); do
    pgrep -f "$BIN" >/dev/null 2>&1 && break
    kill -0 "$APP_ROOT" 2>/dev/null || { echo "perf-smoke: the launcher exited early" >&2; cat "$WORK/app.log" >&2; exit 1; }
    sleep 0.1
done

for _ in $(seq 1 600); do
    if [ -f "$PITAGAMES_PERF_FILE" ] && grep -q '"cold_start_ms": *[0-9]' "$PITAGAMES_PERF_FILE"; then
        break
    fi
    kill -0 "$APP_ROOT" 2>/dev/null || { echo "perf-smoke: the launcher died before its first paint" >&2; cat "$WORK/app.log" >&2; exit 1; }
    sleep 0.1
done
if ! grep -q '"cold_start_ms": *[0-9]' "$PITAGAMES_PERF_FILE" 2>/dev/null; then
    echo "perf-smoke: the launcher never painted a library" >&2
    cat "$WORK/app.log" >&2
    exit 1
fi

# --- resident memory of the process group, sampled while it scrolls -----------
sample_memory() {
    local pgid="$1" out="$2" max=0 kb
    while :; do
        kb="$(ps -eo pgid=,rss= | awk -v g="$pgid" '$1==g {t+=$2} END{print t+0}')"
        [ "$kb" -gt "$max" ] && max="$kb"
        printf '%s\n' "$max" > "$out"
        sleep 0.2
    done
}
sample_memory "$APP_ROOT" "$WORK/rss_kb" &
SAMPLER_PID=$!

# --- drive a scroll burst -----------------------------------------------------
WINDOW=""
for _ in $(seq 1 100); do
    WINDOW="$(xdotool search --onlyvisible --name '^PitaGames$' 2>/dev/null | head -1 || true)"
    [ -n "$WINDOW" ] && break
    sleep 0.1
done
if [ -z "$WINDOW" ]; then
    echo "perf-smoke: could not find the launcher window" >&2
    cat "$WORK/app.log" >&2
    exit 1
fi
xdotool windowfocus --sync "$WINDOW" 2>/dev/null || true
# One tight burst: the launcher records the wall time between the scroll steps
# it processes, which is what the 60 fps budget is measured on.
xdotool key --clearmodifiers --repeat 150 --delay 2 Down 2>/dev/null || true
# Let the throttled measurement file take its last write.
sleep 1

kill "$SAMPLER_PID" 2>/dev/null || true
SAMPLER_PID=""
MEM_KB="$(cat "$WORK/rss_kb" 2>/dev/null || echo 0)"
MEM_BYTES=$(( MEM_KB * 1024 ))

if [ -n "${PITAGAMES_PERF_OUT:-}" ]; then
    cp "$PITAGAMES_PERF_FILE" "$PITAGAMES_PERF_OUT"
fi

# --- judge --------------------------------------------------------------------
ARGS=(perf check --file "$PITAGAMES_PERF_FILE" --memory "$MEM_BYTES" --require "$REQUIRE")
[ "$JSON" -eq 1 ] && ARGS+=(--json)
set +e
"$CLI" "${ARGS[@]}"
STATUS=$?
set -e

echo "perf-smoke: games=$GAMES memory=$MEM_BYTES bytes (sum of the process group's RSS)" >&2
exit "$STATUS"
