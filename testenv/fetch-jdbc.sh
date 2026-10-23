#!/usr/bin/env bash
# Download the PostgreSQL JDBC driver that Ontop loads from testenv/jdbc/.
set -euo pipefail

version=42.7.14
jar="postgresql-${version}.jar"
url="https://repo1.maven.org/maven2/org/postgresql/postgresql/${version}/${jar}"
# Maven Central publishes only SHA-1 for this artifact. The SHA-256 is the
# digest GitHub reports for the same asset on the pgjdbc REL${version} release.
sha256=73914527305a40cce504b0d3d90b23caf565136912d607ae8ae7c5895512332c
sha1=0e7c94ce80cb1dd9875ff44ffe6a40a8aaa3eefd

dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/jdbc"
mkdir -p "$dir"

digest() {
  local algo=$1 file=$2
  if command -v shasum >/dev/null 2>&1; then
    shasum -a "$algo" "$file" | cut -d' ' -f1
  else
    "sha${algo}sum" "$file" | cut -d' ' -f1
  fi
}

verify() {
  local file=$1
  [ "$(digest 256 "$file")" = "$sha256" ] && [ "$(digest 1 "$file")" = "$sha1" ]
}

# Ontop runs as its own user in the container; mktemp creates files 0600.
if [ -f "$dir/$jar" ] && verify "$dir/$jar"; then
  chmod 0644 "$dir/$jar"
  echo "$jar already present and verified"
  exit 0
fi

tmp="$(mktemp "$dir/.${jar}.XXXXXX")"
trap 'rm -f "$tmp"' EXIT

curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp" "$url"

if ! verify "$tmp"; then
  echo "checksum mismatch for $jar" >&2
  echo "  sha256 expected $sha256, got $(digest 256 "$tmp")" >&2
  echo "  sha1   expected $sha1, got $(digest 1 "$tmp")" >&2
  exit 1
fi

chmod 0644 "$tmp"
mv "$tmp" "$dir/$jar"
trap - EXIT
echo "fetched and verified $dir/$jar"
