# Bundle Transition Refactoring Design

This document describes the design goals for centralizing bundle state transitions, the problems with the current approach, and proposed architectural options.

## Related Documents

- **[Bundle State Machine Design](bundle_state_machine_design.md)**: Current state machine diagram and crash safety model
- **[Storage Subsystem Design](storage_subsystem_design.md)**: Persistence model for bundle state
- **[Filter Subsystem Design](filter_subsystem_design.md)**: Filter hooks and checkpoint ordering

## Motivation

The bundle state machine is the core design of the BPA. Every bundle follows a
lifecycle from reception to final disposition, and the state determines what
operations are valid at each step. Getting this wrong causes data loss, crash
recovery failures, or bundles stuck in invalid states.

Currently, state transitions are scattered across the codebase. Any code with
access to the bundle can set `metadata.status` to any value, skip persistence,
or apply an invalid transition. The state machine exists only as documentation
and developer discipline - the code does not enforce it.

## Problems

### 1. No transition validation

Any code can assign any state:

```rust
bundle.metadata.status = BundleStatus::Dispatching;
```

There is no check that the bundle was in a valid source state. An `AduFragment`
bundle could be set to `Dispatching` without going through reassembly.

### 2. Persistence is a separate call

After changing the status field, the caller must remember to persist:

```rust
bundle.metadata.status = BundleStatus::Waiting;
self.store.update_status(&mut bundle, &BundleStatus::Waiting).await;
```

If the persistence call is forgotten, the in-memory state and storage diverge.
A crash at this point loses the transition.

### 3. State transitions in the wrong layer

The storage layer (`Store`, `channel::Sender`) performs state transitions.
`channel::Sender::send()` auto-updates the bundle status to the channel's
target state. `adu_reassembly.rs` transitions bundles to `AduFragment`.
State machine logic belongs in the dispatcher, not in storage plumbing.

### 4. Inconsistent naming

The enum is `BundleStatus` but represents processing state, not protocol
status. Variant names mix tenses (`New`, `Dispatching`, `Waiting`,
`ForwardPending`). The field is `metadata.status` but conceptually it is
the bundle's processing state.

## Common Ground

Regardless of which option is chosen, the following should apply:

### State enum

Rename to `BundleState` with consistent naming:

```
BundleState::Received         Bundle arrived, not yet processed
BundleState::Ingested         Ingress filter done, ready for routing
BundleState::ForwardPending   Queued for CLA peer
BundleState::FragmentPending  Waiting for sibling fragments
BundleState::RoutePending     No route available, waiting
BundleState::ServicePending   Status report waiting for service registration
```

### State transition diagram

```
                       ingest()
 Received ───────────────────────────> Ingested
 RoutePending ────────────────────────>    |
 FragmentPending ─────────────────────>    |
 ServicePending ──────────────────────>    |
                                           |
              ┌──── route() ──────── RoutePending ──── (watch + expire)
              |
 Ingested ────┼──── forward() ────── ForwardPending ── (watch + expire)
              |
              ├──── fragment() ───── FragmentPending ─ (watch + expire)
              |
              ├──── service() ────── ServicePending ── (watch + expire)
              |
              ├──── deliver ───────── (tombstone)
              |
              └──── drop ──────────── (tombstone)

 ForwardPending ─── route() ───────── RoutePending ── (peer died)

 Any ───────────── delete() ────────── (data + tombstone, consumed)
```

### Private state field

The `metadata.state` field is not public. External code reads through
`bundle.state()` which returns `&BundleState`.

### Controlled construction

| Constructor | Purpose | Persists |
|-------------|---------|----------|
| `Bundle::new(bpv7, data, ingress, store)` | CLA reception | Yes |
| `Bundle::draft(bpv7)` | Pre-filter path (originate, reports) | No |
| `Bundle::recover(bpv7, metadata)` | Crash recovery from storage | No |

### Channel behavior

The channel does not transition bundles. It asserts the bundle is already
in the expected state. The caller transitions before sending.

## Option A: Transitions on Bundle

Transition methods live on `Bundle` and take `&Store`. Each method validates,
applies, and persists in one call.

```rust
// Dispatcher:
bundle.route(&store).await?;
bundle.forward(&store, peer, queue).await?;
```

```
┌──────────────┐         ┌──────────────────────┐         ┌───────┐
│  Dispatcher  │──call──>│  Bundle::route(store) │──call──>│ Store │
│              │         │  - validate           │         │       │
│              │         │  - set state           │         │ persist
│              │         │  - persist             │         │       │
│              │         │  - watch               │         │       │
└──────────────┘         └──────────────────────┘         └───────┘
```

**Pros:**
- One call per transition, impossible to forget persistence
- State machine is centralized in `transitions.rs`
- Simple call sites in the Dispatcher

**Cons:**
- Bundle (a data type) depends on Store (infrastructure)
- Transition methods are async because of persistence
- Harder to unit test transitions without a Store

**Module layout:**
```
bundle/
  mod.rs           Bundle struct, state() accessor
  state.rs         BundleState enum
  metadata.rs      BundleMetadata
  transitions.rs   Constructors + transition methods (takes &Store)
```

## Option B: Pure state machine + Store applies

Transition validation lives on `BundleState` as pure functions (no I/O).
The Store applies the validated transition and persists.

```rust
// Dispatcher:
let next = bundle.state().route()?;
store.apply(&mut bundle, next).await;
```

```
┌──────────────┐         ┌────────────────────┐
│  Dispatcher  │──call──>│ BundleState::route()│  pure, no I/O
│              │         │ - validate           │
│              │         │ - return next state  │
│              │         └────────────────────┘
│              │
│              │──call──>┌──────────────────────┐
│              │         │ Store::apply(bundle)  │
│              │         │ - set state            │
│              │         │ - persist              │
│              │         │ - watch                │
│              │         └──────────────────────┘
└──────────────┘
```

**Pros:**
- Bundle is a plain data type, no storage dependency
- State machine is pure logic, fully testable with unit tests
- Clear separation: validation vs persistence

**Cons:**
- Two calls per transition (validate + apply), caller can forget the second
- Store must trust that the caller validated (no double-check)
- More ceremony at each call site

**Module layout:**
```
bundle/
  mod.rs           Bundle struct, state() accessor
  state.rs         BundleState enum + pure transition methods
  metadata.rs      BundleMetadata
```

## Option C: Dispatcher owns transitions

A `transitions.rs` in the dispatcher module holds methods on `Dispatcher`
that validate, apply, and persist. Bundle and Store stay decoupled.

```rust
// Dispatcher:
self.transition_route(&mut bundle).await?;
self.transition_forward(&mut bundle, peer, queue).await?;
```

```
┌──────────────────────────────────┐
│  Dispatcher::transition_route()  │
│  - validate bundle.state()       │
│  - set state                      │
│  - store.update_state(bundle)    │
│  - store.watch_bundle(bundle)    │
└──────────────────────────────────┘
```

**Pros:**
- Bundle is a plain data type
- Store is a plain persistence layer
- All transition logic + persistence in one place
- Dispatcher already has access to Store

**Cons:**
- Transition methods scattered on Dispatcher, not on the data they modify
- Other code (storage backends, recovery) may need transitions too
- `Dispatcher` grows larger

**Module layout:**
```
bundle/
  mod.rs           Bundle struct, state() accessor
  state.rs         BundleState enum
  metadata.rs      BundleMetadata

dispatcher/
  transitions.rs   Transition methods on Dispatcher
```

## Naming

| Old | New |
|-----|-----|
| `BundleStatus` | `BundleState` |
| `BundleStatus::New` | `BundleState::Received` |
| `BundleStatus::Dispatching` | `BundleState::Ingested` |
| `BundleStatus::Waiting` | `BundleState::RoutePending` |
| `BundleStatus::ForwardPending` | `BundleState::ForwardPending` |
| `BundleStatus::AduFragment` | `BundleState::FragmentPending` |
| `BundleStatus::WaitingForService` | `BundleState::ServicePending` |
| `metadata.status` | `metadata.state` (via accessor) |

## Migration

Regardless of the chosen option, migration can be done incrementally:

1. Add the new `BundleState` enum and `state.rs`
2. Add backward-compatible alias `pub type BundleStatus = BundleState`
3. Migrate callers file by file (each PR compiles independently)
4. Remove the alias once all callers are migrated
5. Update storage backends (sqlite, postgres) enum names
6. Update test fixtures
