# Multi-Replica BPA Architecture

## Overview

In a multi-replica deployment, BPA instances share persistent state (PostgreSQL for metadata, S3/disk for bundle data) and forward bundles through CLA services. Each BPA replica is stateless for forwarding — peer management belongs to the CLAs.

## Deployment Topology

```mermaid
graph TB
    subgraph "Shared State"
        PG[(PostgreSQL<br/>Metadata Storage)]
        S3[(S3 / Object Store<br/>Bundle Storage)]
    end

    subgraph "BPA Replicas"
        BPA1[BPA Replica 1]
        BPA2[BPA Replica 2]
        BPA3[BPA Replica 3]
    end

    subgraph "CLA Services"
        TCPCLV4[TCPCLv4 Server<br/>external, gRPC]
        UDPCL[UDP CL<br/>external, gRPC]
    end

    subgraph "Local CLAs"
        FILE1[File CLA<br/>in-process, BPA 1 only]
    end

    subgraph "Remote Nodes"
        N1[Node A]
        N2[Node B]
        N3[Node C]
    end

    BPA1 --> PG
    BPA2 --> PG
    BPA3 --> PG

    BPA1 --> S3
    BPA2 --> S3
    BPA3 --> S3

    BPA1 --> TCPCLV4
    BPA2 --> TCPCLV4
    BPA3 --> TCPCLV4

    BPA1 --> UDPCL
    BPA2 --> UDPCL

    BPA1 --> FILE1

    TCPCLV4 --> N1
    TCPCLV4 --> N2
    UDPCL --> N3
    FILE1 --> N1
```

## Bundle Lifecycle

Only two persistent statuses:

- **New**: persisted, not yet forwarded (or crashed mid-dispatch)
- **Waiting**: forwarding failed, waiting for retry (route unavailable, CLA down, reassembly pending)

Both are claimable by any replica.

## Hot Path

The common case. Bundle arrives, gets processed in memory, persisted once for crash safety, forwarded immediately.

```mermaid
sequenceDiagram
    participant CLA_IN as CLA (Ingress)
    participant BPA as BPA Replica
    participant DB as PostgreSQL
    participant Store as S3
    participant CLA_OUT as CLA (Egress)

    CLA_IN->>BPA: receive bundle (raw bytes)
    BPA->>BPA: parse, validate, filter (Ingress)
    BPA->>Store: persist bundle data
    BPA->>DB: insert metadata (status: New)
    BPA->>BPA: RIB lookup
    BPA->>CLA_OUT: forward bundle to node X

    alt Success
        CLA_OUT-->>BPA: Sent
        BPA->>DB: tombstone
        BPA->>Store: delete bundle data
    else Failure
        CLA_OUT-->>BPA: NoNeighbour / CLA down
        BPA->>DB: update status: Waiting
    end
```

One storage write on the success path (persist + tombstone). No intermediate status transitions.

## Cold Path

Edge cases: route not yet available, reassembly in progress, CLA temporarily down. The bundle sits in storage as `Waiting` until conditions change.

```mermaid
sequenceDiagram
    participant BPA as Any BPA Replica
    participant DB as PostgreSQL

    BPA->>DB: SELECT ... WHERE status IN ('New', 'Waiting')<br/>FOR UPDATE SKIP LOCKED LIMIT N
    DB-->>BPA: claimed bundles

    loop Each claimed bundle
        BPA->>BPA: RIB lookup
        alt Route available
            BPA->>BPA: forward via CLA
        else No route
            BPA->>DB: keep as Waiting
        end
    end
```

PostgreSQL `FOR UPDATE SKIP LOCKED` is the work distribution mechanism. No external broker. If a replica crashes, its connection drops, the transaction rolls back, and the rows become claimable by other replicas automatically.

## Crash Recovery

No special recovery protocol. A crashed replica's bundles are either:

- **New**: persisted but never forwarded. Any replica claims and routes them on the next poll cycle.
- **Waiting**: already in the cold path. Any replica can claim them.

No stale peer references, no orphaned `ForwardPending` status, no replica-specific state in storage.

## CLA Ownership

```mermaid
graph LR
    subgraph "BPA Replica"
        RIB[RIB<br/>node X reachable via CLA Y]
        D[Dispatcher]
        RIB --> D
    end

    subgraph "CLA Service"
        PT[Peer Table<br/>connections, sessions]
        FWD[Forwarder]
        PT --> FWD
    end

    D -- "forward bundle<br/>to node X" --> FWD
    FWD -- "Sent / NoNeighbour" --> D
```

- **BPA knows which CLA** (from RIB), not which peer or address.
- **CLA owns peer state**: TCP sessions, addresses, connection lifecycle.
- **BPA is stateless for forwarding**: no peer_id, no CLA address, no queue assignment in storage.

## Key Principles

- **Hot path is fast**: in-memory processing, one persist for crash safety, forward immediately.
- **Cold path is distributed**: PostgreSQL row locking distributes work across replicas. No broker.
- **Crash is invisible**: connection drop releases locks. Other replicas pick up the work.
- **Two statuses**: `New` (persisted, awaiting first dispatch) and `Waiting` (retry later). No `ForwardPending`, no `Dispatching`.
- **CLA is a service**: manages its own peers and connections. BPA delegates forwarding, doesn't micromanage.
- **Replicas don't coordinate**: same config, same CLAs, same RIB. Each processes what it receives. Shared storage handles the rest.
