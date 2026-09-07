# Hardy BPA gRPC Client Authentication — Design Document

**Status:** Draft for iteration
**Scope:** `hardy-bpa-server` (and by extension `hardy-bpa`), `hardy-proto` (docs only — no schema change), all gRPC clients of the BPA (CLAs, services, routing agents)
**Related:** `test-cla-design.md` (first exerciser; consumes this, does not define it)

---

## 1. Problem statement

The BPA's gRPC surface currently accepts registrations from any client that
can reach the port. For the CLA interface this is worse than a bundle-injection
hole: a client that registers as a CLA **claiming peers** is offered bundles
for transmission. A rogue CLA advertising attractive next-hops is a
traffic-interception primitive — availability and metadata are lost outright,
and confidentiality/integrity survive only for BPSec-protected traffic, which
not all traffic is.

The same exposure exists on the application/service registration interface:
a rogue service registering for an EID is a *delivery*-interception primitive,
arguably worse.

The BPA therefore needs to establish, at stream establishment, that the client
is an **expected** one — and which one.

## 2. Goals and non-goals

### Goals

1. The BPA rejects gRPC clients that do not present a valid credential.
2. A credential is bound to a **named client identity**, not merely to
   admission ("may connect" is the weak version; "is `test-cla-1`" is the
   useful one).
3. One mechanism across the whole BPA gRPC surface — CLA, service, and
   routing/management interfaces — not a CLA-only bolt-on.
4. No change to the protobuf schema. Authentication is a transport-layer
   concern; the proto describes protocol semantics.
5. The validation mechanism is pluggable, so static tokens can later be
   supplanted by mTLS / SPIFFE / JWT without redesign.
6. Rotation and revocation are possible without unnecessary disruption.

### Non-goals

- **Not bundle-layer security.** BPSec remains the end-to-end story; this
  authenticates the local management/CLA plane only.
- **Not authorisation policy beyond identity binding** (v1). Constraining
  *what* an authenticated CLA may claim (peer EIDs, etc.) is anticipated and
  the config shape leaves room for it, but v1 binds token → name only. See Q8.
- **Not transport encryption.** TLS is configured orthogonally on the
  listener; this design states the interaction (§5) but does not implement it.

## 3. Design

### 3.1 Mechanism: bearer token in gRPC metadata

Clients present a static secret on every connection as standard gRPC metadata:

```
authorization: Bearer <token>
```

Checked once, at stream establishment, by a tonic interceptor on the BPA
server. **Not** a field in the Register message — welding one auth scheme into
the wire format forecloses every future scheme and pollutes protocol semantics
with trust machinery. The metadata approach is idiomatic gRPC, understood by
standard proxies and middleware, and swappable underneath the proto.

### 3.2 Identity binding

BPA-side configuration maps each token to an expected-client entry:

```yaml
grpc:
  auth:
    required: true            # false ⇒ open (dev/loopback only; logs a warning)
    clients:
      - name: test-cla-1
        kind: cla
        tokens:               # a set, to permit rotation (§3.5)
          - env: TEST_CLA_1_TOKEN
      - name: tvr-agent
        kind: routing
        tokens:
          - file: /etc/hardy/secrets/tvr.token
      - name: echo-service
        kind: service
        tokens:
          - value: "..."      # literal permitted, discouraged; env/file preferred
```

Validation at stream open:

1. Token present and matches (constant-time comparison) some client entry.
2. The interface being used matches the entry's `kind` (a CLA token is not
   valid on the service interface, and vice versa).
3. The identity claimed in registration (CLA name / service identity) matches
   the entry's `name`. A leaked low-value token must not impersonate an
   arbitrary client — authentication without this binding is not
   authorisation.

The authenticated `name` is attached to the connection context and recorded on
every subsequent event/trace from that stream.

### 3.3 Failure behaviour

- Missing/invalid token, kind mismatch, or name mismatch ⇒ stream refused
  with gRPC `UNAUTHENTICATED` (name mismatch may reasonably be
  `PERMISSION_DENIED`; pick one and document it).
- **Clients must not hot-retry** on `UNAUTHENTICATED`: exponential backoff
  with a generous floor (seconds, not milliseconds). A misconfigured client in
  a tight reconnect loop is a self-inflicted log-spam DoS. This is a stated
  requirement on all Hardy-provided clients (CLAs, TVR, tools).
- Auth failures are first-class observability events (§6). A burst of them is
  precisely what an operator needs to see.

### 3.4 Revocation

Removing a token (or a client entry) from BPA config takes effect on config
reload and **terminates any live stream authenticated by it**. By the
established stream-semantics doctrine, stream teardown is a link flap — the
BPA treats outstanding transfers per the outcome-unknown rules — so revocation
is self-consistent with existing behaviour rather than a special case.

### 3.5 Rotation

Each client entry holds a *set* of currently-valid tokens. Rotation is:
add new token → roll clients over at leisure → remove old token. No forced
flap. A single-token schema would make every rotation a service interruption;
the set costs nothing and avoids it.

### 3.6 Pluggable validation

The interceptor is structured as a chain of validators; v1 ships exactly one
(`StaticTokenValidator` over the config above). Anticipated future validators,
requiring no redesign:

- **mTLS / client certificates:** identity from the presented cert (SAN),
  mapped to the same client-entry table; the bearer token becomes optional or
  absent.
- **SPIFFE/SPIRE:** SVID-based identity for cloud deployments — given Hardy's
  cloud framing, someone will want this; the metadata-plus-interceptor shape
  accommodates it for free.
- **JWT:** signed, expiring tokens where a central issuer exists.

The contract each validator satisfies: *(connection, metadata, registration
claim) → authenticated client name, or refusal*. Everything downstream keys on
the name.

## 4. Scope across the gRPC surface

Applies uniformly to every BPA-exposed gRPC service:

| Interface | Rogue-client risk | Kind |
|---|---|---|
| CLA registration/transfer | Traffic interception via claimed peers; bundle injection | `cla` |
| Service/application registration | Delivery interception via claimed EIDs; bundle injection | `service` |
| Routing/management (route install etc.) | Route manipulation — full traffic-steering control | `routing` |

One interceptor, one config table, one event vocabulary. A CLA-only mechanism
would leave the two arguably-worse doors open.

## 5. Interaction with transport security

A bearer token is only as strong as the channel it crosses. Doctrine:

- **Unix domain socket / loopback:** token serves as a misconfiguration guard
  and light local access control. Acceptable without TLS.
- **Network listener:** tokens are sniffable and replayable in plaintext;
  `auth.required: true` on a non-loopback listener without TLS elicits a loud
  startup warning (candidate for hard refusal — see Q9). Server-auth TLS +
  bearer token is the intended baseline network posture; full mTLS arrives as
  a validator (§3.6), not a fork in the design.

## 6. Observability

- Events: `auth_success {client, kind}`, `auth_failure {reason, peer_addr}`
  (token value never logged, not even hashed-prefixed by default),
  `stream_revoked {client, cause: token_removed | config_reload}`.
- The authenticated client name is attached to all spans originating from the
  stream, so per-client behaviour is queryable in the existing OTel pipeline.
- Metric: auth failures counter, labelled by reason — the burst detector.

## 7. Client-side requirements (all Hardy gRPC clients)

- Config accepts `token`, `token_env`, or `token_file` per BPA endpoint —
  indirection so secrets stay out of committed config. The TestCla's
  `[[bpa]]` entries grow exactly this.
- Send the metadata header on every connection; treat `UNAUTHENTICATED` per
  §3.3 (backoff, no hot-retry, clear log line naming the endpoint).
- `hardy-tvr`, `hardy-tcpclv4-server`, `hardy-file-cla`, the echo/file
  services, and the CLI tools all gain the same three config keys and the
  same client-side interceptor — one shared helper in a common crate, not
  five implementations.

## 8. Testing

The TestCla is the natural first exerciser (see `test-cla-design.md`); these
join its Phase 0/1 suite as cheap scenarios:

1. **Wrong token** ⇒ refused, `UNAUTHENTICATED`, client backs off, auth
   failure event emitted.
2. **Right token, wrong claimed name** ⇒ refused (the impersonation case).
3. **Right token, wrong interface kind** ⇒ refused.
4. **Revocation mid-scenario** ⇒ stream terminated; BPA-side behaviour
   identical to a link flap; outstanding transfers → outcome-unknown.
5. **Rotation** ⇒ add token B, reconnect client with B, remove A: no flap,
   no transfer disruption.
6. **Open mode** (`required: false`) ⇒ everything admitted, warning logged.

Plus unit coverage: constant-time comparison, config reload semantics, and
the interceptor refusing before any registration handler runs.

## 9. Migration and compatibility

- `auth.required` defaults to **false** for one release cycle with a startup
  warning when unset, then flips to true-by-default. Existing single-machine
  deployments keep working; the warning tells them what is coming.
- No proto change ⇒ no wire-compatibility question. Old clients against an
  auth-required BPA fail with `UNAUTHENTICATED` and a self-explanatory error.

## 10. Open questions

| # | Question | Notes | Status |
|---|---|---|---|
| Q8 | Authorisation scope beyond identity: should a `cla` entry constrain the peer EIDs it may announce (and a `service` entry the EIDs it may register)? | Anticipated tightening; config shape reserves room (`allowed_peers` / `allowed_eids` lists). Decide after the identity binding has been exercised. | Open |
| Q9 | Should `auth.required: true` on a non-loopback plaintext listener be a hard startup refusal rather than a warning? | Safer; may annoy lab setups. Perhaps refusal with an explicit `i_understand_plaintext_tokens: true` escape hatch. | Open |
| Q10 | Failure code taxonomy: `UNAUTHENTICATED` for all refusals, or `PERMISSION_DENIED` for name/kind mismatch? | Cosmetic but worth consistency across the surface. | Open |
| Q11 | Does config reload (for revocation/rotation, §3.4–3.5) already exist in `hardy-bpa-server`, or does this design force the reload mechanism into existence? | If forced: SIGHUP vs watch vs management RPC. | Open |
| Q12 | Do the CLI tools (`bp`, etc.) authenticate with the same per-client entries, or is a shared `tools` identity acceptable? | Per-client is cleaner; shared is operationally lighter for ephemeral tools. | Open |
