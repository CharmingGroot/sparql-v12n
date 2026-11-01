#!/bin/sh
# Copies the queries and mappings of the test cases into the seed corpora
# (fuzz/corpus/<target>/, not committed). Run from the repository root.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
corpus="$root/fuzz/corpus"
mkdir -p "$corpus/query_parse" "$corpus/query_plan" "$corpus/mapping_parse"
n=0
for f in $(find "$root/tests/diff" "$root/examples" -name '*.rq'); do
  n=$((n + 1))
  cp "$f" "$corpus/query_parse/seed-$n.rq"
  cp "$f" "$corpus/query_plan/seed-$n.rq"
done
m=0
for f in $(find "$root/tests/diff" "$root/examples" -name 'mapping.ttl') \
  $(find "$root/tests/w3c/rdb2rdf" -name 'r2rml*.ttl'); do
  m=$((m + 1))
  cp "$f" "$corpus/mapping_parse/seed-$m.ttl"
done
echo "seeded $n queries and $m mappings"
