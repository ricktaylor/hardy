# hardy-file-service: Usage Guide

## Running

### From source

```bash
cargo run -p hardy-file-service -- \
  --service-id 42 \
  --destination "ipn:1.42" \
  -c file-service/config.yaml
```

### Docker (native Linux only)

inotify events do not propagate across VM boundaries.
Docker Desktop on macOS/Windows is not supported.

```bash
docker run \
  -e HARDY_FILE_SERVICE_BPA_ADDRESS="http://host.docker.internal:50051" \
  -e HARDY_FILE_SERVICE_OUTBOX=/data/outbox \
  -e HARDY_FILE_SERVICE_ERRORS=/data/errors \
  -e HARDY_FILE_SERVICE_INBOX=/data/inbox \
  -v ./data/outbox:/data/outbox \
  -v ./data/errors:/data/errors \
  -v ./data/inbox:/data/inbox \
  hardy-file-service \
  --service-id 42 \
  --destination "ipn:1.42"
```

### Docker Compose

```yaml
file-service:
  image: hardy-file-service:latest
  command: ["--service-id", "42", "--destination", "ipn:1.42"]
  environment:
    HARDY_FILE_SERVICE_BPA_ADDRESS: "http://hardy:50051"
    HARDY_FILE_SERVICE_OUTBOX: /data/outbox
    HARDY_FILE_SERVICE_ERRORS: /data/errors
    HARDY_FILE_SERVICE_INBOX: /data/inbox
  volumes:
    - ./data/outbox:/data/outbox
    - ./data/errors:/data/errors
    - ./data/inbox:/data/inbox
```

## Sending a bundle

Drop a file in the outbox directory:

```bash
echo "hello" > outbox/message.txt
cp payload.bin outbox/
mv /tmp/data.bin outbox/
```

The file is detected, sent as a bundle to the configured destination,
then deleted. Use unique filenames to avoid overwrites.

For atomic writes (large files), use the dotfile pattern:

```bash
cp bigfile.bin outbox/.bigfile.tmp
mv outbox/.bigfile.tmp outbox/bigfile.bin
```

## Receiving a bundle

Incoming bundles appear as files in the inbox directory:

```bash
ls inbox/
# ipn_1.42_1234567890_0
# ipn_1.42_1234567891_1

cat inbox/ipn_1.42_1234567890_0
# (bundle payload)
```

Filenames encode `{source}_{timestamp}_{sequence}`.

## Configuration

Configuration is layered (highest priority first):

1. CLI flags (`--service-id`, `--destination`, `--config`)
2. Environment variables (`HARDY_FILE_SERVICE_` prefix)
3. Config file (YAML/TOML/JSON)
4. Built-in defaults

### Config file example

```yaml
# bpa-address: "http://[::1]:50051"
# log-level: "info"
# lifetime: "24h"
service-id: 42
destination: "ipn:1.42"
# outbox: /tmp/hardy/outbox
# errors: /tmp/hardy/errors
# inbox: /tmp/hardy/inbox
```

### Environment variables

```bash
HARDY_FILE_SERVICE_BPA_ADDRESS="http://bpa:50051"
HARDY_FILE_SERVICE_SERVICE_ID=42
HARDY_FILE_SERVICE_DESTINATION="ipn:1.42"
HARDY_FILE_SERVICE_LIFETIME="1h"
HARDY_FILE_SERVICE_OUTBOX=/data/outbox
HARDY_FILE_SERVICE_ERRORS=/data/errors
HARDY_FILE_SERVICE_INBOX=/data/inbox
```

### Reference

| Field | Default | Required | Description |
|---|---|---|---|
| log-level | info | no | trace, debug, info, warn, error |
| bpa-address | http://[::1]:50051 | no | gRPC endpoint of the BPA |
| service-id | - | yes | IPN service number |
| destination | - | yes | Destination EID for outgoing bundles |
| lifetime | 24h | no | Bundle lifetime (human-readable) |
| outbox | /tmp/hardy/outbox | no | Directory to watch for outgoing files |
| errors | /tmp/hardy/errors | no | Directory for failed send attempts |
| inbox | /tmp/hardy/inbox | no | Directory to write incoming payloads |

## Error handling

### Outbox errors

Failed files are moved to the errors directory. Inspect and re-submit
by moving them back to the outbox:

```bash
ls /tmp/hardy/errors/
mv /tmp/hardy/errors/failed_file.bin /tmp/hardy/outbox/
```

### Inbox errors

Write failures are logged at `error!` level. The BPA considers the
bundle delivered regardless. Monitor logs for data loss.
