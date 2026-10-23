#!/usr/bin/env bash
# Runs every examples/shop/queries/*.rq through Ontop and `v12n query` and
# compares the solutions after sorting. Needs `docker compose up -d` and jq.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
endpoint="${ONTOP_ENDPOINT:-http://127.0.0.1:58080/sparql}"
v12n="${V12N_BIN:-$root/target/debug/v12n}"
export V12N_DATABASE_URL="${V12N_DATABASE_URL:-postgresql://v12n:v12n-dev@127.0.0.1:55432/shop}"
mapping="$root/examples/shop/mapping.ttl"

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
  rows="$(printf '%s' "$expected" | grep -c '' || true)"
  if [ "$expected" = "$actual" ]; then
    echo "same     $name ($rows rows)"
  else
    echo "DIFFERS  $name"
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") | sed 's/^/  /' || true
    status=1
  fi
done
exit $status
