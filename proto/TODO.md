# Proto TODO

## CLA gRPC Allowlist

### Problem

The current gRPC CLA registration model allows any gRPC client to register any
CLA name. The bpa-server has no config-driven control over which CLAs are
permitted.

### Design

The CLA name is a sysadmin concern — assigned via CLI argument to the CLA
process. The bpa-server config provides an allowlist of permitted CLA names
with their associated address types and egress policies.

**CLA process side** — name assigned by sysadmin:

```
hardy-stcp-cla --bpa localhost:50051 --name cl0 --listen 0.0.0.0:4556
```

**bpa-server config** — allowlist with per-CLA settings:

```toml
[[grpc.clas]]
name = "cl0"
address-type = "private"
# policy = { ... }

[[grpc.clas]]
name = "cl1"
address-type = "tcp"
```

**Protocol flow:**

1. CLA process connects to BPA gRPC, sends `name` in `RegisterClaRequest`
2. bpa-server checks `name` against allowlist
3. If allowed: registers with BPA core using configured address-type and
   policy; returns node IDs to the CLA
4. If not allowed: rejects with `PERMISSION_DENIED`

**Reconnection:** When a gRPC CLA disconnects, it unregisters (existing
lifecycle). When it reconnects, it re-registers — same flow. Peers are
cleaned up on unregister and rediscovered on re-register. The BPA core stays
unchanged.

### Changes Required

- **cla.proto**: No changes — `RegisterClaRequest.name` stays as-is. Remove
  `address_type` from the request (server assigns from config).
- **server/cla.rs**: Check name against allowlist, reject unknown names,
  apply configured address-type/policy on registration.
- **bpa-server config**: Add `[[grpc.clas]]` allowlist sections.
- **BPA core**: No changes.

## bp ping with External CLA (--cla /path/to/binary)

### Problem

With the plugin system removed, `bp ping --cla /path/to/cla-binary` needs a
new way to use external CLAs. The tool doesn't run a persistent gRPC server,
so the CLA can't just connect at leisure — bp ping needs to know when the CLA
has registered before it can start pinging.

### Design

`bp ping` spawns the CLA binary as a subprocess and waits for it to register
via gRPC:

1. bp ping creates its in-process BPA (as today)
2. Starts a gRPC server on an ephemeral port
3. Execs the CLA binary, passing `--bpa <grpc-addr>` automatically plus any
   user-supplied arguments via `--cla-args`
4. Waits for the CLA to connect and register (with timeout)
5. Proceeds with ping

**CLI interface:**

The existing `--cla-config` (JSON blob) is replaced by `--cla-args`, which
passes arguments through to the CLA binary verbatim. The CLA binary owns its
own CLI — bp ping doesn't interpret or restructure the arguments.

```
bp ping ipn:2.7 --cla /usr/bin/hardy-stcp-cla \
    --cla-args "--name cl0 --listen 0.0.0.0:4557 --peer 127.0.0.1:4556 --peer-node ipn:2.0"
```

bp ping automatically injects `--bpa <grpc-addr>` (the CLA doesn't need to
know what ephemeral port was chosen). All other arguments are the user's
responsibility — they know the CLA's CLI better than bp ping does.

### Registration Notification

No BPA core changes needed. The gRPC `server::cla::Service` struct gets an
optional notification channel:

```rust
pub struct Service {
    bpa: Arc<dyn BpaRegistration>,
    channel_size: usize,
    on_register: Option<tokio::sync::mpsc::Sender<String>>,  // NEW
}
```

After a successful `bpa.register_cla()` in the `register()` handler, fire the
channel:

```rust
if let Some(tx) = &self.on_register {
    let _ = tx.send(request.name.clone()).await;
}
```

`bp ping` creates the Service with `Some(tx)` and awaits the receiver.
`bpa-server` creates it with `None`. Same code path, optional notification.

**Waiting with edge cases:**

```rust
tokio::select! {
    name = cla_ready_rx => { /* CLA registered, proceed */ }
    _ = tokio::time::sleep(Duration::from_secs(10)) => {
        return Err("CLA didn't register within 10s");
    }
    status = child.wait() => {
        return Err("CLA process exited unexpectedly");
    }
}
```

### For Built-in CLAs in bp ping

CLAs that bp ping supports natively (tcpclv4, and potentially stcp/mtcp as
crate dependencies behind feature flags) don't need gRPC — they're created
in-process as today. The exec path is only for `--cla /path/to/binary`.

## Remove Plugin ABI

The `plugin-abi` crate and cdylib plugin system should be removed. CLAs are
either:

- **Built-in crate dependencies** (tcpclv4, file-cla, optionally stcp/mtcp) —
  compiled into bpa-server or bp tools, configured via `[[clas]]` in
  bpa-server config.
- **External gRPC processes** — connect to bpa-server's gRPC server, checked
  against the allowlist above.

The MTCP/STCP CLA (`tests/interop/mtcp/`) should be ported from a cdylib
plugin to either a built-in crate dependency or a standalone gRPC binary.
For `bp ping`, making it a crate dependency (behind a feature flag) is
simplest. For `bpa-server`, either approach works.
