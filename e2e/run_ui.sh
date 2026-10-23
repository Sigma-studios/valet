#!/usr/bin/env bash
# The tree editor's web routes (create, nest, rename, move, members, special
# use, drag and drop layout, delete, reset), on a copy of the data
# run_mail_e2e.sh left in <dir>/mdata. GreenMail isn't needed.
#
#   e2e/run_ui.sh <dir>
DIR="$(cd "${1:?usage: $0 <scratch dir>}" && pwd)"
source "$(dirname "$0")/lib.sh"

[ -d "$DIR/mdata" ] || { echo "run run_mail_e2e.sh first"; exit 1; }
rm -rf "$DIR/udata" && cp -R "$DIR/mdata" "$DIR/udata"
start_valet "$DIR/udata"
"$PYTHON" "$REPO/e2e/ui_e2e.py"
echo "ui e2e: all good"
