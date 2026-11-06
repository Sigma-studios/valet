#!/usr/bin/env bash
# Completing addresses in the composer (the route, then headless Chrome).
# Python needs requests and websocket-client.
#
#   e2e/run_addresses.sh <dir>
DIR="$(cd "${1:?usage: $0 <scratch dir>}" && pwd)"
source "$(dirname "$0")/lib.sh"

rm -rf "$DIR/adata" "$DIR/chrome-addresses"
start_greenmail
"$PYTHON" "$REPO/e2e/address_e2e.py" seed
start_valet "$DIR/adata"
"$PYTHON" "$REPO/e2e/address_e2e.py" "$DIR"
finish
rm -rf "$DIR/chrome-addresses"
