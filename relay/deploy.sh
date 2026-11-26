#!/usr/bin/env bash
# Deploy the relay to Cloudflare via the API — no wrangler, no MCP tooling.
#
#   deploy.sh                upload worker.js (non-secret bindings declared,
#                            existing secrets survive untouched)
#   deploy.sh secret NAME    set one secret; the value is read from stdin
#
# Credentials come from the repo root .env (gitignored): CLOUDFLARE_ACCOUNT_ID,
# CLOUDFLARE_API_TOKEN. Secret bindings are never part of the upload metadata —
# the API rejects valueless secrets (10021), and ones that already exist
# survive a deployment. The cron trigger is independent. The D1 database is
# resolved by name via the API — no hardcoded ids anywhere.
set -euo pipefail
cd "$(dirname "$0")"

DB_NAME="herdr-relay"

set -a
# read at runtime; the file is gitignored credentials, so shellcheck can't follow it
# shellcheck source=/dev/null
. ../.env
set +a

: "${CLOUDFLARE_ACCOUNT_ID:?missing in ../.env}" "${CLOUDFLARE_API_TOKEN:?missing in ../.env}"

API="https://api.cloudflare.com/client/v4/accounts/$CLOUDFLARE_ACCOUNT_ID"
AUTH="Authorization: Bearer $CLOUDFLARE_API_TOKEN"

# CF answers are pretty-printed ("success": true), so parse JSON instead of grepping.
check() { python3 -c 'import json, sys; d = json.load(sys.stdin); sys.exit(0 if d.get("success") else 1)'; }

db_id() { # resolve the D1 database uuid by name, once per run
    curl -sS "$API/d1/database/$DB_NAME" -H "$AUTH" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d.get("success"), d.get("errors")
print(d["result"]["uuid"])' || {
        echo "cannot resolve D1 database \"$DB_NAME\"" >&2
        exit 1
  }
}

case "${1:-deploy}" in
  deploy)
    DB_ID=$(db_id)
    META=$(
           cat << JSON
{
  "main_module": "worker.js",
  "compatibility_date": "2025-11-01",
  "bindings": [
    { "type": "d1", "name": "DB", "id": "$DB_ID" }
  ]
}
JSON
    )
    RESP=$(curl -sS -X PUT "$API/workers/scripts/herdr-relay" \
        -H "$AUTH" \
        -F "metadata=$META;type=application/json" \
        -F "script=@worker.js;type=application/javascript+module")
    echo "$RESP" >&2
    check <<< "$RESP"
    ;;
  secret)
    NAME="${2:?usage: relay/deploy.sh secret NAME < value}"
    # strip the trailing newline every shell pipeline hands us — a secret ending
    # in \n can never match the clean value devices put in their URL paths
    TEXT=$(python3 -c 'import json, sys; print(json.dumps(sys.stdin.read().rstrip("\r\n")))')
    RESP=$(curl -sS -X PUT "$API/workers/scripts/herdr-relay/secrets" \
        -H "$AUTH" -H "content-type: application/json" \
        -d "{\"name\":\"$NAME\",\"text\":$TEXT,\"type\":\"secret_text\"}")
    echo "$RESP" >&2
    check <<< "$RESP"
    ;;
  *)
    echo "usage: relay/deploy.sh [deploy|secret NAME < value]" >&2
    exit 2
    ;;
esac
