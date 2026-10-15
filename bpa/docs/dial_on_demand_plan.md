# Dial-on-Demand Plan (DRAFT)

> **Status: draft for review.** Moving name resolution and dial-on-demand out of CLA registration and the forward path into a self-contained subsystem of the BPA: one resolver turns a name into a node's EIDs and the convergence-layer endpoints it offers, and each CLA that can dial registers that ability for its own CL protocol. **Nothing here is implemented yet.** Once implemented, this folds into [`design.md`](design.md) and the CLA design documents, and the draft retires.

## Motivation

### `ClaAddress` does four jobs

- **Adjacency key.** A CLA reports each adjacency with `Sink::add_peer(ClaAddress, node IDs)` and withdraws it with `remove_peer`. Every CLA keeps its own address map ([`routing_subsystem_design.md`](routing_subsystem_design.md#cla-registry-mapping)), so an address means something only to the CLA that reported it.
- **Forward target.** `Cla::forward` names the adjacency by its `ClaAddress`.
- **Ingress provenance.** The CLA that delivers a bundle names the sending adjacency (`peer_addr`), and the BPA persists it in the bundle's metadata.
- **Dial-on-demand handler key.** `ClaInit::address_type` (over gRPC, `RegisterClaRequest.address_type`) registers a CLA as the handler for addresses of a `ClaAddressType`, in a map the RIB keeps.

The fourth job conflates two kinds of address: one a CLA can resolve and dial (an IP address and port, or a DNS name), and one that is local to a CLA and cannot be dialled (`ClaAddress::Private`: a `file-cla` peer's inbox directory, a `bibe` decapsulation endpoint). Keyed by type, two CLAs that both registered `Private` would contend for one slot, and the last to register would win. Nothing consults the map: no route can name an address (route actions are drop, via and reflect, and the RIB's vocabulary is EIDs throughout, [`routing_table_redesign.md`](routing_table_redesign.md)), so the handler role is a seam with no caller.

### `ClaAddressType` exists for that job alone

`ClaAddress` is an enum over `ClaAddressType`'s families, `Tcp(SocketAddr)` and `Private(Bytes)`, and the family is what the handler map is keyed by. Nothing else in the BPA reads it. The BPA uses an adjacency's address only as a key in the reporting CLA's own map, as provenance stored beside that CLA's name, and as text in logs; no BPA code matches on the variants. The typed `Tcp` variant serves only the CLAs that build and parse it, and its name implies a dialability the BPA never acts on. The family's other uses, the `(ClaAddressType, Bytes)` wire encoding and the `tcp:`/`private:` display prefix, follow from the type rather than from any need for it. A closed enum of families also cannot describe what discovery returns: one variant per convergence layer (TCP, UDP, QUIC, and whatever follows) in the BPA's core interface and wire format, for a distinction only the dialling CLA acts on.

### Dialling happens in the wrong places

- **Inside `forward`.** `tcpclv4` dials any TCP address it holds no session for: to grow a busy pool, and for an in-flight transfer whose pool has died. A pool of inbound sessions is keyed by the peer's ephemeral source port, so both cases can dial an address that accepts nothing ([tcpclv4 Peer identity](../../tcpclv4/docs/design.md#peer-identity)). Reachability is probed per bundle: the deferred-`Failed` loop in [`design.md`](design.md#deferred-cla-transfer-outcomes) is, by design, the only probe a plan-less static route has, and it runs un-damped.
- **In an application, once.** `tcpclv4-server` resolves its configured `peers` by DNS at startup, dials the first address only, and does not redial after the session ends. `bpa-server` has no dial targets: its TCPCLv4 sessions are all inbound. `bp ping` calls `Tcpclv4::connect` directly and then sleeps, because nothing reports when the session's adjacency exists.
- **Nowhere, for EID resolution.** Requirement 6.1.4 ([`requirements.md`](../../docs/requirements.md)) asks for an API that resolves EIDs to available CLA addresses, for example by DNS for TCPCLv4. None exists.

### One DNS answer describes every convergence layer a node offers

DNS can return, for one name, both who the node is and every way to reach it:

- **SVCB (RFC 9460).** One RRset under the node's name carries its identity and its topology. A targetless record (priority 1, target `.`) carries the node's EID in an `eid` parameter, an `ipn:` node ID for a node also named by a `dtn:` authority. Service-form records carry, one per convergence layer, the CL protocol (ALPN, for example `qbcl` for QUBICLE), the port, the target host and address hints, ordered by priority. A single query therefore yields TCP, QUIC and UDP endpoints and the node's alternate EID together.
- **SRV.** RFC 9174 Section 8.1 registers the `dtn-bundle` service name for TCP and UDP, so `_dtn-bundle._tcp` and `_dtn-bundle._udp` SRV records name TCPCL and UDPCL endpoints; this is the legacy form beside SVCB.
- **Local names.** A single-label or private-use name (`rover-alpha`, `habitat.local`) resolves by local means (mDNS, a hosts table, a pre-shared manifest) and must not fall back to global DNS.
- **Without DNS.** The same bindings can come from configuration, a manifest loaded before a contact, or a control-plane exchange (the DTN Peering Protocol), so the resolver is not DNS-only.

The answer is one resolution with many CL endpoints; dialling any one of them is the business of the CLA that speaks that CL protocol.

## Target shape

### One resolver

The BPA holds at most one registered resolver. It answers a query for an EID, or for the name a `dtn:` authority carries, with a **resolution**:

- **The node's EIDs**: every EID the answer binds to the node, its alternate names (the queried `dtn:` authority and the `ipn:` node ID of the SVCB `eid` parameter, for example).
- **Dial targets**: an ordered list, each a CL protocol identifier, an endpoint (target host and port, with any address hints) and a priority.
- **A lifetime**: the records' TTL, after which the resolution is stale.

Composition is the resolver's job, not the BPA's: the BPA has no basis for ranking or merging answers from several sources, so a deployment that wants DNS, mDNS and configured bindings composes them into one resolver, which owns the precedence (SVCB over SRV, local names never to global DNS). The resolver is pulled: the BPA asks when it needs an answer, and it registers and unregisters through the same registration-and-Sink lifecycle as the other components. `bpa` defines the trait and stays `no_std`; `bpa-server` supplies the DNS implementation.

### Dial capabilities, per CL protocol

A CLA that can establish adjacencies on request registers a dial capability with the subsystem, naming the CL protocol identifiers it serves (`tcpcl` for `tcpclv4`, `qbcl` for a QUBICLE CLA). This registration stands beside the CLA's own and is separate from it: a CLA with nothing to dial (`file-cla`, `bibe`) registers none, and no CLA registration declares an address type.

Dialling a resolution walks its targets in priority order, hands each to the capability for its CL protocol, skips a protocol with no capability, and stops at the first adjacency. One lookup can thus offer TCP, QUIC and UDP endpoints, and the CLAs this node runs decide which are usable. A dial target is not a `ClaAddress`: a target is something to dial, while a `ClaAddress` is what a CLA reports for an adjacency that exists. CL protocol identifiers belong to dial targets and capabilities, and nowhere else in the BPA.

**A dial completes with an adjacency.** A capability's dial returns once the CLA has reported the resulting adjacency through its ordinary `add_peer`, or once it has failed, so a caller can rely on the adjacency existing when the dial succeeds (`bp ping` then needs no sleep). The node the adjacency names is whatever the convergence layer learns. For TCPCLv4 that is SESS_INIT's node ID: RFC 9174 Section 4.6 lets a session whose node ID differs from the one the dial intended be established, and if it is, associates the session with the node ID it announced.

### Alternate EIDs

A node may carry one node ID per URI scheme, an `ipn:` and a `dtn:` name for the same node. A resolution that binds several EIDs to one node tells the BPA that demand for any of them is met by an adjacency announcing any of them: a bundle for `dtn://rover.mars.dtn/` waits for the adjacency that announces `ipn:1.42.0`. The same resolver serves origination-time binding, where an application resolves a name to the EID it places in a bundle; the BPA never rewrites a bundle's destination in flight.

**DNS locates; it does not authenticate.** A resolution's targets locate candidate peers, and its EIDs are zone data: DNSSEC attests that the zone published them, not that the zone is entitled to them. Which node a session has reached is proven only by the convergence layer's own authentication (a NODE-ID certificate, RFC 9174 Section 4.4.4.3). Until NODE-ID certificates can be issued and validated (the planned ACME support), Hardy routes on unauthenticated node IDs ([tcpclv4 Peer identity](../../tcpclv4/docs/design.md#peer-identity)).

### Triggers

- **Keep-up targets** from configuration, the successor of `tcpclv4-server`'s `peers` and new for `bpa-server`: resolved and dialled at start, and redialled with backoff whenever no adjacency runs through them.
- **Demand.** A RIB lookup that finds no adjacency for a bundle's destination, or for a Via route's target, raises demand for that EID; the bundle waits as today, and the subsystem resolves and dials. When the adjacency appears, its `add_peer` wakes the waiting bundles. Demand is debounced per EID: one resolution in flight at a time, a successful one cached for its lifetime, and a failed one retried after a back-off interval, so many bundles for one unreachable EID cost one lookup.
- **Explicit requests**, from an administration API or the tools.

### `forward` never dials

It sends over an adjacency that exists, and refuses an address with no live session. Growing an existing adjacency's session pool stays inside the CLA, and only for a pool whose address was dialled: a pool of inbound sessions is never dialled ([tcpclv4 Peer identity](../../tcpclv4/docs/design.md#peer-identity)). A transfer whose session dies mid-flight reports `Failed`, and the BPA routes the bundle again.

### `ClaAddress` becomes an opaque handle, and `ClaAddressType` goes

The BPA needs three things from an adjacency's address: equality within one CLA's map, a stable encoding for provenance and the wire, and a readable form for logs and inspection. It needs no family. The handle is therefore the CLA's own canonical rendering of the address, compared exactly: a socket address for `tcpclv4`, an inbox path for `file-cla`, an EID for `bibe`. Each CLA maps it to its internal key (`tcpclv4`'s pools stay keyed by `SocketAddr`), and `forward` refuses a handle the CLA cannot interpret, as `tcpclv4` refuses a `Private` address today. The proto `ClaAddress` message changes with it. Stored provenance written in today's form still decodes: it is never consulted for a decision, so the legacy `Tcp` and `Private` forms map to the handle's form, losslessly for `Tcp`.

### Reachability probing moves to the subsystem

Once `forward` stops dialling, the peer of a plan-less static route is probed by the subsystem's keep-up redial, with backoff, rather than by a dial cycle per bundle (open question 1).

## Relationship to peer identity

- A dialled session makes a dialable pool; an inbound one does not.
- Demand for a node is met only by an adjacency announcing one of its EIDs; a session that announces another node is that node's adjacency, whichever node the dial intended.
- **Neighbour to peer (BP-ARP).** A dial can produce an adjacency whose node ID is not known until the convergence layer learns it, and a node's IDs can change. The `cla::Sink` lacks that seam: `add_peer` with an empty node-ID list records a neighbour, but nothing promotes it, because `add_peer` on an address that is already claimed is refused and an adjacency's node IDs cannot be updated.

## Alternatives rejected

- **The address-type handler map** (today's `ClaInit::address_type`). A CLA registration keyed by address type conflates "reports adjacencies at addresses of this kind" with "can dial addresses of this kind", and `Private` addresses, which only their own CLA can interpret, cannot be keyed by type at all.
- **One `ClaAddressType` variant per convergence layer** (`Udp`, `Quic` beside `Tcp`), made mandatory at CLA registration, with resolved `ClaAddress` values matched to CLAs by type. It grows the BPA's core interface and wire format by one variant per CL protocol, though the set of CL protocols is open-ended and named by ALPN and service names the BPA need not enumerate, and it keeps CLA registration doubling as dial dispatch.
- **`connect()` on the `Cla` trait**, with a default that fails. Dialling is a capability some CLAs have and others do not; a separate registration keeps CLA registration to adjacency reporting and names the CL protocols a dialler serves.
- **Several resolvers ranked by the BPA.** The BPA has no basis for merging their answers; the one resolver composes its sources with the context to order them.
- **Keeping the typed `ClaAddress` for display.** It would leave a family the BPA never acts on in every CLA's interface and on the wire, with `Tcp` still suggesting the BPA can dial it; a CLA's own rendering reads as well in logs.
- **Dialling inside `forward`** (today's `tcpclv4`). It couples reachability probing to bundle flow, one un-damped probe per bundle, and dials whatever address the adjacency is keyed by, inbound ephemeral ports included.
- **Routes that name CL addresses** (`via tcp://host:port`). They would put CLA addresses into the RIB, whose vocabulary is EIDs throughout; bindings from EIDs to dial targets belong to the resolver instead.

## Implementation programme

- **P1, `ClaAddressType` and the dead registration role.** Remove `ClaInit::address_type`, the CLA registry's address-type registration, `Rib`'s `address_types` map and `ClaAddressType`; reshape `ClaAddress` into the opaque handle, with stored provenance in today's form still decoding; reserve `RegisterClaRequest.address_type` and reshape the proto `ClaAddress` message. Forwarding behaviour does not change (open question 2).
- **P2, the subsystem (`bpa`).** The resolver trait and its single-slot registration; the dial-capability registry keyed by CL protocol identifier; resolutions with their EIDs, dial targets and lifetime; the per-EID debounce cache; keep-up targets with redial and backoff.
- **P3, `tcpclv4`.** The `tcpcl` dial capability (today's `Tcpclv4::connect`), completing once the adjacency is reported; `forward` refuses an address with no session; pool growth only for dialled pools. `bp ping` dials through the subsystem and drops its sleep.
- **P4, the servers.** In `bpa-server`, a DNS resolver (SVCB, with SRV as the legacy form; local names never to global DNS) and keep-up target configuration; `tcpclv4-server`'s `peers` moved onto the subsystem.
- **P5, gRPC.** A resolver proxy, and carrying a remote CLA's dial capability (open question 4).
- **P6, demand.** The RIB's demand signal on a lookup that finds no adjacency, and alternate EIDs meeting demand (open questions 7 and 8).
- **Documentation, as each step lands:** `design.md`'s rationale for the un-damped deferred-`Failed` loop; `tcpclv4`'s Deferred forward outcomes; the proto component test plan's CLA-CLI-01; requirement 6.1.4's row in the requirements coverage report.

## Open questions

1. **Who probes a plan-less static route's peer.** The deferred-`Failed` loop is un-damped because it is the only probe such a route has ([`design.md`](design.md#deferred-cla-transfer-outcomes)). Once `forward` stops dialling, the subsystem's keep-up redial is that probe, with backoff; whether the loop's rationale then retires, leaving the loop to cover only transfers whose session died mid-flight, needs a ruling.
2. **When P1 lands.** It changes no forwarding behaviour but breaks the `bpa` CLA API and the CLA proto. Landing it before `cla.proto` is frozen as v1 keeps a family the BPA never acts on out of the frozen API, at the cost of a breaking change ahead of the subsystem.
3. **The handle's representation.** Text, as above, or bytes with a separate display form. Text reads directly in logs and inspection, and every in-tree CLA's address has a canonical text form; bytes suit a CLA whose addresses have no natural text, at the price of a second field or hex in logs.
4. **The gRPC shape of a dial capability.** Messages on the existing CLA stream, or a service of its own.
5. **The CL protocol identifiers.** ALPN where one exists (`qbcl`); TCPCL has none, and SRV names it by service and transport (`_dtn-bundle._tcp`). The identifiers must match the DNS discovery conventions for DTN, which are not yet specified: whether TCPCL gets an ALPN or stays SRV-only, and whether one SVCB RRset may mix CL protocols over different transports.
6. **Trying targets in order, or racing them.** Strict priority order with the first adjacency winning, or racing the leading targets in the manner of Happy Eyeballs (RFC 8305), across protocols as well as addresses.
7. **Trust in alternate EIDs.** Whether demand for one name may be met by an adjacency announcing another on the resolver's word (zone data, DNSSEC where available), or only once the correspondence is proven both ways (the forward `eid` binding matched by a reverse `ipn.arpa` mapping).
8. **The demand signal.** What the RIB emits for a lookup that finds no adjacency, how the debounce intervals are set, and how demand treats a resolution whose targets all fail.

## Related documents

- [`design.md`](design.md#deferred-cla-transfer-outcomes): deferred CLA transfer outcomes, and the loop open question 1 concerns
- [`routing_subsystem_design.md`](routing_subsystem_design.md#peer-table): the peer table and the CLA's address map
- [`routing_table_redesign.md`](routing_table_redesign.md): the EID-only routing vocabulary
- [`../../tcpclv4/docs/design.md`](../../tcpclv4/docs/design.md#peer-identity): pools, node identity and the dialable-pool rule
