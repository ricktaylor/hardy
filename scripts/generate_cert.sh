#!/bin/bash

# Generate and self-sign a Certificate Authority (CA) certificate
openssl req -x509 -newkey rsa:4096 -nodes -days 365 \
  -keyout ../bpa-server/ca/ca.key -out ../bpa-server/ca/ca.crt -subj "/CN=Test Hardy CA"

# Generate a server private key and a Certificate Signing Request (CSR) for the server
openssl req -newkey rsa:4096 -nodes -keyout ../bpa-server/certs/server.key \
  -out ../bpa-server/certs/server.csr -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost"

# Sign the server CSR with the CA to issue the server certificate (including the SAN)
openssl x509 -req -in ../bpa-server/certs/server.csr -CA ../bpa-server/ca/ca.crt -CAkey ../bpa-server/ca/ca.key \
  -CAcreateserial -out ../bpa-server/certs/server.crt -days 365 -extfile <(printf "subjectAltName=DNS:localhost")