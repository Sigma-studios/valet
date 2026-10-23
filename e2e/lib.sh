# Shared by the e2e scripts: GreenMail and valet, started in the background
# and stopped on exit. Needs $DIR (the scratch directory, with
# gm/greenmail-standalone-2.1.14.jar in it) and Java; valet built first
# (cargo build). Python with requests: $PYTHON, or python3.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PYTHON="${PYTHON:-python3}"
JAR="$DIR/gm/greenmail-standalone-2.1.14.jar"
[ -f "$JAR" ] || { echo "missing $JAR"; exit 1; }
[ -x "$REPO/target/debug/valet" ] || { echo "build valet first: cargo build"; exit 1; }

# A throwaway password for this run's valet only.
export VALET_PASSWORD="e2e-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
export VALET_USER=valet
export E2E_WEB=http://127.0.0.1:18080 E2E_IMAP_PORT=11143 E2E_GM_IMAP_PORT=3143

PIDS=()
cleanup() {
    for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
}
trap cleanup EXIT

wait_port() {
    for _ in $(seq 1 100); do
        (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null && return 0
        sleep 0.2
    done
    echo "nothing listens on port $1"; return 1
}

# GreenMail, empty: test@local.test and org@local.test (password pw).
start_greenmail() {
    java -Dgreenmail.setup.test.smtp -Dgreenmail.setup.test.imap \
        -Dgreenmail.users=test:pw@local.test,org:pw@local.test -Dgreenmail.users.login=email \
        -Dgreenmail.hostname=127.0.0.1 -jar "$JAR" > "$DIR/greenmail.log" 2>&1 &
    PIDS+=($!)
    wait_port 3143
    wait_port 3025
}

# valet on data directory $1, with the two GreenMail accounts (imported
# from VALET_ACCOUNTS when the directory is new).
start_valet() {
    mkdir -p "$1"
    DATA_DIR="$1" LISTEN=127.0.0.1:18080 IMAP_LISTEN=127.0.0.1:11143 SMTP_LISTEN=127.0.0.1:11587 \
    RUST_LOG="${RUST_LOG:-info}" NO_COLOR=1 \
    VALET_ACCOUNTS='[{"id":"test","name":"Test","address":"test@local.test","imap":"127.0.0.1:3143","smtp":"127.0.0.1:3025","password":"pw","tls":false},
                     {"id":"org","name":"Org","address":"org@local.test","imap":"127.0.0.1:3143","smtp":"127.0.0.1:3025","password":"pw","tls":false}]' \
        "$REPO/target/debug/valet" > "$DIR/valet.log" 2>&1 &
    PIDS+=($!)
    wait_port 18080
    wait_port 11143
}

finish() {
    if grep -E "WARN|ERROR" "$DIR/valet.log" | grep -v "sync: " ; then
        echo "(warnings above, from $DIR/valet.log)"
    fi
}
