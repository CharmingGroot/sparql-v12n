#!/usr/bin/env bash
# PostgreSQL with TLS for the TLS tests. Generates a CA, a server
# certificate for the host name "localhost" and a client certificate for
# the role v12n_cert, both signed by it, a second CA that signed nothing,
# and certificate revocation lists, then runs PostgreSQL with ssl=on.
# The role v12n signs in with a password, over TLS or not; v12n_cert needs
# TLS and a client certificate.
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
      # The server maps a client certificate to the role named by its CN.
      openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
        -subj "/CN=v12n_cert" -keyout client.key -out client.csr 2>/dev/null
      printf "%s\n" "extendedKeyUsage=clientAuth" "basicConstraints=critical,CA:FALSE" \
        > client.ext
      openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -set_serial 3 \
        -days 7 -extfile client.ext -out client.pem 2>/dev/null
      # libpq and v12n refuse a client key that others can read.
      chmod 600 client.key
      # CRLs: none revoked, the server certificate revoked, and one from the
      # unrelated CA, which says nothing about the server certificate.
      printf "%s\n" "[ca]" "default_ca = test" "[test]" "database = index.txt" \
        "crlnumber = crlnumber" "default_md = sha256" "default_crl_days = 7" > ca.cnf
      crl() {
        openssl ca -config ca.cnf -keyfile "$1.key" -cert "$1.pem" -gencrl -out "$2" 2>/dev/null
      }
      : > index.txt; echo 01 > crlnumber
      crl ca empty.crl
      crl wrong-ca wrong-ca.crl
      openssl ca -config ca.cnf -keyfile ca.key -cert ca.pem -revoke server.pem 2>/dev/null
      crl ca revoked.crl
      # sslcrldir is read as an OpenSSL hashed directory: <issuer hash>.r<n>.
      mkdir revoked-crls
      cp revoked.crl "revoked-crls/$(openssl crl -hash -noout -in revoked.crl).r0"
      rm server.csr server.ext client.csr client.ext ca.cnf index.txt* crlnumber* \
        ca.key wrong-ca.key
    '
}

up() {
  generate
  # PostgreSQL refuses a key file that others can read, and the mounted
  # one belongs to the host user, so the entrypoint copies it first.
  # ssl_ca_file makes the server ask for a client certificate; a client
  # without one can still use the password entry.
  docker run -d --rm --name "$name" \
    -e POSTGRES_USER=v12n -e POSTGRES_PASSWORD=v12n-dev -e POSTGRES_DB=shop \
    -p "127.0.0.1:$port:5432" \
    -v "$dir:/tls:ro" \
    -v "$root/examples/shop/schema.sql:/docker-entrypoint-initdb.d/01-schema.sql:ro" \
    -v "$root/examples/shop/data.sql:/docker-entrypoint-initdb.d/02-data.sql:ro" \
    --entrypoint bash "$image" -c '
      install -o postgres -m 600 /tls/server.key /tmp/server.key
      install -o postgres -m 644 /tls/server.pem /tmp/server.pem
      install -o postgres -m 644 /tls/ca.pem /tmp/ca.pem
      printf "%s\n" "local all all trust" \
        "hostssl all v12n_cert all cert" \
        "host all v12n all scram-sha-256" > /tmp/pg_hba.conf
      echo "CREATE ROLE v12n_cert LOGIN;" > /docker-entrypoint-initdb.d/03-roles.sql
      exec docker-entrypoint.sh postgres -c ssl=on \
        -c ssl_cert_file=/tmp/server.pem -c ssl_key_file=/tmp/server.key \
        -c ssl_ca_file=/tmp/ca.pem -c hba_file=/tmp/pg_hba.conf
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
