# Extensions Maintained Outside the Workspace

Hardy is extended without changes to this workspace, in one of two ways:

- **Over gRPC.** A convergence layer adapter, routing agent, service or application runs as its own process and attaches to a stock `hardy-bpa-server` through the gRPC APIs defined in [`hardy-proto`](../proto/): the `Cla`, `RoutingAgent`, `Service` and `Application` services. `hardy-tcpclv4-server`, `hardy-tvr` and the `bp` tool work this way.
- **In process.** Anything builds against the public traits of the unmodified `hardy-bpa` crate: `cla`, `routing`, `services`, `storage`, and filter packs registered through `BpaBuilder::add_filters`. For filters and storage backends, which run inside the BPA's pipeline, this is the only route.

This page lists extensions of both kinds that are maintained outside the workspace.

An entry here is a pointer, not an endorsement. Hardy does not build, test, review or release the extensions listed; each is maintained by its author, who keeps the entry current, including which Hardy version or git revision it builds against. An extension that implements a specification without a standards body behind it (an individual Internet-Draft, an experimental block type) is listed with that status so that users can judge the risk for themselves.

## Registry

| Extension | Repository | Hooks | What it does | Builds against | Status | Maintainer |
|-----------|------------|-------|--------------|----------------|--------|------------|
| `hardy-custody-receipt` | [PR #769 branch](https://github.com/ricktaylor/hardy/pull/769); demo and results in [BSVKey/dtn-custody-demo](https://github.com/BSVKey/dtn-custody-demo/blob/main/ssi/hardy/RESULTS.md) | filter (Egress `Rewriter`, in process) | Signs a COSE_Sign1 (Ed25519) custody receipt for each bundle the node forwards and appends it to the Custody Receipt Block, experimental block type 224, per [draft-bsvkey-dtn-custody-receipt-block](https://datatracker.ietf.org/doc/draft-bsvkey-dtn-custody-receipt-block/). Nodes without the filter forward the block unchanged. | `main` (the filter API is newer than `hardy-bpa` 0.2.0) | Experimental: individual Internet-Draft, no code point assigned | [EmbryoSpace](https://github.com/EmbryoSpace) (BSVKey) |

## Getting listed

Open a pull request adding one row to the table above. Keep to the columns shown, link the repository, and for a specification link its canonical reference. In Hooks, say what the extension is (filter, CLA, routing agent, service, application, storage) and whether it attaches over gRPC or builds in process. A row is removed when its repository disappears or stops building against a current Hardy.

## Running an extension

- **Over gRPC:** a running `hardy-bpa-server` with its gRPC listener enabled (see the server configuration in the [user guide](https://ricktaylor.github.io/hardy/)) and the address to connect to. The extension's README says which of the four services it registers with.
- **In process:** a binary that constructs the BPA, since `hardy-bpa-server` has no plugin loading. The extension's README says which of these it expects:
  - a binary of your own built around `BpaBuilder`;
  - a branch of `hardy-bpa-server` that adds a feature and configuration section for the extension.
