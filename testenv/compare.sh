#!/usr/bin/env bash
# Runs every examples/shop/queries/*.rq through Ontop, `v12n query` and the
# `v12n serve` endpoint, and compares the solutions after sorting. Needs
# `docker compose up -d` and jq. Starts `v12n serve` with v12n.toml and
# stops it on exit.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
endpoint="${ONTOP_ENDPOINT:-http://127.0.0.1:58080/sparql}"
v12n="${V12N_BIN:-$root/target/debug/v12n}"
export V12N_DATABASE_URL="${V12N_DATABASE_URL:-postgresql://v12n:v12n-dev@127.0.0.1:55432/shop}"
mapping="$root/examples/shop/mapping.ttl"
server="http://127.0.0.1:57878/sparql" # [server] in v12n.toml

"$v12n" serve --config "$root/testenv/v12n.toml" 2>"${V12N_SERVE_LOG:-/dev/null}" &
serve_pid=$!
trap 'kill "$serve_pid" 2>/dev/null || true; wait "$serve_pid" 2>/dev/null || true' EXIT
for _ in $(seq 50); do
  curl -fsS "${server%/sparql}/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done

# One line per solution, `?var=<kind> <value> <datatype or @lang>` sorted by
# variable. A plain literal and an xsd:string literal are the same term.
normalize() {
  jq -r '
    .results.bindings[]
    | to_entries
    | sort_by(.key)
    | map("?\(.key)=\(.value.type) \(.value.value)"
          + (if .value["xml:lang"] then " @\(.value["xml:lang"])"
             elif .value.datatype and .value.datatype != "http://www.w3.org/2001/XMLSchema#string"
             then " ^^\(.value.datatype)" else "" end))
    | join("  ")' | LC_ALL=C sort
}

status=0
for query in "$root"/examples/shop/queries/*.rq; do
  name="$(basename "$query")"
  expected="$(curl -fsS -X POST -H 'Accept: application/sparql-results+json' \
    --data-urlencode "query@$query" "$endpoint" | normalize)"
  actual="$("$v12n" query --format json "$mapping" "$query" | normalize)"
  served="$(curl -fsS -X POST -H 'Accept: application/sparql-results+json' \
    --data-urlencode "query@$query" "$server" | normalize)"
  rows="$(printf '%s' "$expected" | grep -c '' || true)"
  if [ "$expected" = "$actual" ] && [ "$expected" = "$served" ]; then
    echo "same     $name ($rows rows)"
  else
    echo "DIFFERS  $name"
    echo "  ontop vs v12n query:"
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") | sed 's/^/    /' || true
    echo "  ontop vs v12n serve:"
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$served") | sed 's/^/    /' || true
    status=1
  fi
done
exit $status
