#!/usr/bin/env bash
# Spelling and grammar checking in the composer (the API, then headless
# Chrome: underlines, corrections). Python needs requests and
# websocket-client.
#
#   e2e/run_spell.sh <dir>
DIR="$(cd "${1:?usage: $0 <scratch dir>}" && pwd)"
source "$(dirname "$0")/lib.sh"

rm -rf "$DIR/sdata" "$DIR/chrome-spell"
start_greenmail
start_valet "$DIR/sdata"
"$PYTHON" "$REPO/e2e/spell_e2e.py" "$DIR"
grep "spelling:" "$DIR/valet.log"
finish
