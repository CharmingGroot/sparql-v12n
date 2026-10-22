#!/usr/bin/env bash
# Smoke check: the shop VIP query against Ontop returns orders 1001, 1003, 1004.
set -euo pipefail

endpoint="${ONTOP_ENDPOINT:-http://127.0.0.1:58080/sparql}"
timeout="${SMOKE_TIMEOUT:-120}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
query="$root/examples/shop/queries/vip-orders.rq"

# SPARQL 1.1 Protocol §2.1.2: query via POST with a URL-encoded body.
run_query() {
  curl -fsS -X POST \
    -H 'Accept: application/sparql-results+json' \
    --data-urlencode "query@$query" \
    "$endpoint"
}

deadline=$((SECONDS + timeout))
until result="$(run_query 2>/dev/null)"; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "Ontop endpoint $endpoint not ready after ${timeout}s" >&2
    run_query >&2 || true
    exit 1
  fi
  sleep 2
done

orders="$(printf '%s' "$result" | grep -oE 'http://example\.org/order/[0-9]+' | sort -u)"
expected="$(printf 'http://example.org/order/%s\n' 1001 1003 1004)"

if [ "$orders" != "$expected" ]; then
  echo "unexpected orders from $endpoint" >&2
  echo "expected:" >&2
  echo "$expected" >&2
  echo "got:" >&2
  echo "${orders:-<none>}" >&2
  echo "raw result:" >&2
  echo "$result" >&2
  exit 1
fi

echo "smoke ok: VIP orders 1001, 1003, 1004"
