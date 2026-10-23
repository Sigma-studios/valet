#!/usr/bin/env bash
# The calendar page, its feeds and the installable app: routes (always),
# then the pages in a headless Chrome, online and with valet stopped
# (offline reading), when Chrome is there ($CHROME, or the usual place).
# Python needs requests and, for Chrome, websocket-client.
#
#   e2e/run_calendar.sh <dir>
DIR="$(cd "${1:?usage: $0 <scratch dir>}" && pwd)"
source "$(dirname "$0")/lib.sh"

rm -rf "$DIR/cdata"
start_valet "$DIR/cdata"
VALET_PID="${PIDS[${#PIDS[@]}-1]}"
"$PYTHON" "$REPO/e2e/calendar_e2e.py"
if [ -x "${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}" ]; then
    out="$DIR/browser-$(date +%s)"
    mkdir -p "$out"
    "$PYTHON" "$REPO/e2e/browser_e2e.py" online "$out"
    kill "$VALET_PID"
    "$PYTHON" "$REPO/e2e/browser_e2e.py" offline "$out"
    echo "screenshots in $out/shots"
else
    echo "(no Chrome: browser checks skipped)"
fi
finish
echo "calendar e2e: all good"
