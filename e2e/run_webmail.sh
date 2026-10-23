#!/usr/bin/env bash
# The webmail with the tree: sidebar, moving into nodes (and into a new one,
# which makes the folder at the provider), deleting to the account's Trash,
# drafts, "Make it in every account", "Add to the tree".
#
#   e2e/run_webmail.sh <dir>
DIR="$(cd "${1:?usage: $0 <scratch dir>}" && pwd)"
source "$(dirname "$0")/lib.sh"

rm -rf "$DIR/wdata"
start_greenmail
"$PYTHON" "$REPO/e2e/mail_e2e.py" seed
start_valet "$DIR/wdata"
"$PYTHON" "$REPO/e2e/webmail_e2e.py"
finish
echo "webmail e2e: all good"
