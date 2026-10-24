#!/usr/bin/env bash
# Notifications of new mail: the arrivals valet's Android app asks for, and
# (CHROME_PUSH=1, Chrome there, the internet) real Web Push in a headless
# Chrome. Python needs requests and, for Chrome, websocket-client.
#
#   e2e/run_notify.sh <dir>
DIR="$(cd "${1:?usage: $0 <scratch dir>}" && pwd)"
source "$(dirname "$0")/lib.sh"

rm -rf "$DIR/ndata" "$DIR/chrome-push"
start_greenmail
"$PYTHON" "$REPO/e2e/mail_e2e.py" seed
start_valet "$DIR/ndata"
"$PYTHON" "$REPO/e2e/notify_e2e.py" "$DIR"
finish
echo "notify e2e: all good"
