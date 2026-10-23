#!/usr/bin/env bash
# Mail apps' view, over IMAP: valet with two GreenMail accounts, the tree of
# folders gathering both (LIST, special-use, MOVE, APPEND, lazy folders,
# CREATE/RENAME/DELETE of nodes). Leaves valet's data in <dir>/mdata for
# run_ui.sh.
#
#   e2e/run_mail_e2e.sh <dir>
DIR="$(cd "${1:?usage: $0 <scratch dir>}" && pwd)"
source "$(dirname "$0")/lib.sh"

rm -rf "$DIR/mdata"
start_greenmail
"$PYTHON" "$REPO/e2e/mail_e2e.py" seed
start_valet "$DIR/mdata"
"$PYTHON" "$REPO/e2e/mail_e2e.py" check
finish
echo "mail e2e: all good"
