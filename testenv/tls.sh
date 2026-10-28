#!/usr/bin/env bash
# PostgreSQL with TLS for the TLS tests. Generates a CA, a server
# certificate for the host name "localhost" signed by it, and a second CA
# that signed nothing, then runs PostgreSQL with ssl=on.
#
#   ./tls.sh up     generate certificates, start the container, print the
#                   environment variables for the tests
#   ./tls.sh down   stop the container (it removes itself)
set -euo pipefail

name="${TLS_PG_NAME:-v12n-tls}"
port="${TLS_PG_PORT:-55434}"
image="${TLS_PG_IMAGE:-postgres:17}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dir="$root/target/tls"

generate() {
  rm -rf "$dir"
  mkdir -p "$dir"
  # The image's OpenSSL, so the result does not depend on the host's.
  docker run --rm --user "$(id -u):$(id -g)" -v "$dir:/out" -w /out \
    --entrypoint bash "$image" -euo pipefail -c '
      ca() {
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
          -days 7 -subj "/CN=$2" -keyout "$1.key" -out "$1.pem" \
          -addext "basicConstraints=critical,CA:TRUE" \
          -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
      }
      ca ca "v12n test CA"
      ca wrong-ca "v12n unrelated CA"
      openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
        -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
      printf "%s\n" "subjectAltName=DNS:localhost" "extendedKeyUsage=serverAuth" \
        "basicConstraints=critical,CA:FALSE" > server.ext
      openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -set_serial 2 \
        -days 7 -extfile server.ext -out server.pem 2>/dev/null
      rm server.csr server.ext ca.key wrong-ca.key
    '
}

up() {
  generate
  # PostgreSQL refuses a key file that others can read, and the mounted
  # one belongs to the host user, so the entrypoint copies it first.
  docker run -d --rm --name "$name" \
    -e POSTGRES_USER=v12n -e POSTGRES_PASSWORD=v12n-dev -e POSTGRES_DB=shop \
    -p "127.0.0.1:$port:5432" \
    -v "$dir:/tls:ro" \
    -v "$root/examples/shop/schema.sql:/docker-entrypoint-initdb.d/01-schema.sql:ro" \
    -v "$root/examples/shop/data.sql:/docker-entrypoint-initdb.d/02-data.sql:ro" \
    --entrypoint bash "$image" -c '
      install -o postgres -m 600 /tls/server.key /tmp/server.key
      install -o postgres -m 644 /tls/server.pem /tmp/server.pem
      exec docker-entrypoint.sh postgres -c ssl=on \
        -c ssl_cert_file=/tmp/server.pem -c ssl_key_file=/tmp/server.key
    ' >/dev/null
  # TCP, not the socket: the init-time server listens on the socket only.
  for _ in $(seq 60); do
    if docker exec "$name" pg_isready -q -h 127.0.0.1 -U v12n -d shop; then
      echo "export V12N_TEST_TLS_DATABASE_URL='postgresql://v12n:v12n-dev@localhost:$port/shop?hostaddr=127.0.0.1'"
      echo "export V12N_TEST_TLS_CERTS='$dir'"
      return
    fi
    sleep 1
  done
  echo "PostgreSQL in $name not ready after 60s" >&2
  docker logs "$name" >&2 || true
  exit 1
}

case "${1:-}" in
  up) up ;;
  down) docker stop "$name" >/dev/null ;;
  *)
    echo "usage: $0 up | down" >&2
    exit 1
    ;;
esac
