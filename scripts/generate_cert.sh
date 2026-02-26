#!/bin/bash

PROJECT_ROOT="$(git rev-parse --show-toplevel)"
BPA_SERVER="$PROJECT_ROOT/bpa-server"

# Ensure required directories exist
mkdir -p "$BPA_SERVER/ca" "$BPA_SERVER/certs"

# Generate and self-sign a Certificate Authority (CA) certificate
openssl req -x509 -newkey rsa:4096 -nodes -days 365 \
  -keyout "$BPA_SERVER/ca/ca.key" -out "$BPA_SERVER/ca/ca.crt" -subj "/CN=Test Hardy CA"

# Generate a server private key and a Certificate Signing Request (CSR) for the server
openssl req -newkey rsa:4096 -nodes -keyout "$BPA_SERVER/certs/server.key" \
  -out "$BPA_SERVER/certs/server.csr" -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost"

# Sign the server CSR with the CA to issue the server certificate (including the SAN)
openssl x509 -req -in "$BPA_SERVER/certs/server.csr" -CA "$BPA_SERVER/ca/ca.crt" -CAkey "$BPA_SERVER/ca/ca.key" \
  -CAcreateserial -out "$BPA_SERVER/certs/server.crt" -days 365 -extfile <(printf "subjectAltName=DNS:localhost")
