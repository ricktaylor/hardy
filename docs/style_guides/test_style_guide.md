# Test Style Guide

Conventions for writing tests in the Hardy workspace: how they synchronize, where they live, and what makes them trustworthy. This is the project-specific complement to the [test strategy](../test_strategy.md), which describes the overall approach across unit, integration, fuzz, and interop levels, and to the [Tests section](code_style_guide.md#tests) of the code style guide, which this guide expands.

These rules are mandatory, not aspirational. A test that breaks one is a defect even while it is passing: it will either flake on a loaded CI runner or pass while the behaviour it names is broken. Both cost more than the test saves.

## Applying These Conventions

Apply these to test code you are already writing or changing. Do not sweep the suite to reformat existing tests for their own sake; when you touch a test file for another reason, bring what you touch up to these rules.

## Determinism: never synchronize with time

A test must never use a sleep or a timing margin to order two operations. `tokio::time::sleep`, `std::thread::sleep`, or any "give it a moment to get there" delay is a race: it fails when the machine is slow and wastes wall-clock when it is not. There is no acceptable duration. `sleep(Duration::from_millis(10))` is exactly as wrong as `sleep(Duration::from_secs(2))`, because both encode a *guess* about how long another task takes instead of *observing* that it finished.

Synchronize on the event itself. The toolbox, in rough order of preference:

1. **A signal the code under test raises**: a channel, `watch`, `Notify`, or oneshot the production code (or a mock it drives) sends at the moment of interest. Wait on that.
2. **A capacity-1 channel rendezvous**: a bounded-1 channel where a second send blocks until the consumer has taken the first item, proving the consumer is inside the section under test.
3. **A `Barrier`** for concurrent fan-in, when N tasks must all reach a point before any proceeds.
4. **Bind-before-spawn** for servers: bind the listener, then hand the already-bound socket to the serving task, so there is no window in which the client can connect before the server is listening.

```rust
// WRONG: a fixed delay standing in for "the consumer has parked on the next pull"
let handle = tokio::spawn(async move { sink.send(&mut rx).await });
tokio::time::sleep(Duration::from_millis(100)).await; // hope it parked
registration.unregister().await;

// RIGHT: a capacity-1 rendezvous, the second send returns only once the
// consumer has taken the first item, so it is provably inside sink.send
let (tx, mut rx) = channel::bounded(1);
tx.send(Segment::Next(head)).await.unwrap();
let handle = tokio::spawn(async move { sink.send(&mut rx).await });
tx.send(Segment::Next(tail)).await.unwrap();
registration.unregister().await;
```

When a private state transition is the thing you need to wait for and nothing observable marks it, add a minimal `#[cfg(test)]` hook (an accessor or a signal channel) to the type under test, rather than sleeping until the transition "must" have happened. `Sender::state()` in `bpa/src/storage/channel.rs` is the pattern: a `#[cfg(test)]` method exposing the atomic state so a test can wait for it. A decorator that signals on a state write (a metadata-store wrapper firing a channel when a bundle is parked) is the same idea one level up.

## `timeout()` bounds regressions; it does not order or prove absence

The only sanctioned use of `tokio::time::timeout` or a deadline is to wrap an event-driven wait so a *regression* fails the test loudly instead of hanging the whole suite forever. When you use one, make it generous: seconds, sized so it can never fire on correct code on a slow machine, and mark it with a comment stating that intent:

```rust
// The timeout only bounds a regression; correct code completes at once.
let event = tokio::time::timeout(Duration::from_secs(5), events_rx.recv_async())
    .await
    .expect("delivery event never arrived")
    .unwrap();
```

Two uses are forbidden:

- **A timeout sized so a step "should finish in time."** That is a `sleep` wearing a different type. If the timeout duration is load-bearing for correctness rather than just a hang bound, the test is timing-dependent.
- **Proving something did *not* happen by waiting a window and checking nothing arrived.** A quiet window proves only that the window was too short. To assert absence, drive a real barrier first and then check: `bpa.shutdown().await` joins the worker pool, so anything it was going to do has happened by the time it returns; a drained-to-disconnect channel is likewise a barrier. Assert the receiver is empty *after* the barrier, not after a nap.

## Time-dependent behaviour runs on the paused clock

For code whose behaviour depends on elapsed time (keepalives, idle timeouts, retry backoff, bundle expiry), drive the clock explicitly with tokio's test time instead of waiting for real time to pass:

```rust
#[tokio::test(start_paused = true)]
async fn keepalive_fires_at_the_interval() {
    // ... set up ...
    tokio::time::advance(KEEPALIVE_INTERVAL).await;
    // assert the keepalive was sent, exactly, instantly
}
```

`start_paused` freezes the clock and auto-advances it only when every task is idle, so timers fire at an exact virtual instant and the test costs no real time. Never test a timeout by letting it elapse.

## No shared ambient state between tests

Tests run concurrently and in-process. Nothing one test writes to a shared namespace may leak into another.

- **Ports:** bind with `:0` and read back the OS-assigned port; never hardcode a port number. A fixed port collides with a concurrent test or a leftover process, and it sits in the ephemeral range where the collision is silent.
- **Filesystem:** use a temp directory suffixed with `std::process::id()` (or a `tempfile::tempdir` handle); never a fixed path under the system temp dir. A fixed path lets a crashed prior run's state replay into this one, which is especially dangerous with storage backends that recover on startup.
- **Process-global env vars:** a `#[serial]` test that sets an env var must set and clear it through an RAII guard whose `Drop` removes it, so a panicking assertion cannot leak the variable into the next `#[serial]` test.

```rust
struct EnvGuard(&'static str);
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: single-threaded #[serial] test.
        unsafe { std::env::remove_var(self.0) };
    }
}
```

## A test must be able to fail for the behaviour it names

Before writing the assertions, ask: *what single-line mutation of the production code would make this behaviour wrong, and does this test then fail?* If no mutation fails it, the test asserts nothing and should not be written.

- **Exercise the real production path.** Never re-implement the algorithm in the test body and assert on the copy. A cache-eviction test must call the real cache, not a `BTreeSet` that mimics it; a router test must call the real router. A test that mirrors the code passes in lockstep with a bug.
- **Assert the specific outcome, not its shape.** `matches!(err, Error::InvalidCrc(_))`, not `result.is_err()`. The exact value or the exact typed error variant. A bare `is_err()` / `is_some()` passes for the wrong error; a `to_string().contains("...")` on an error message couples the test to prose and passes for any error that happens to share a word. If the typed variant is buried too deep to match (flattened into an opaque wrapper), that is a signal to give the error type a reachable variant, not to fall back to string matching.
- **Assert the field under test.** When a bundle, message, or struct can differ in more than one way, assert the specific field the test is about, not merely that "something arrived" or that a count is non-zero.
- **A test with no assertion is not a test.** "It did not panic" is not a behaviour. Neither is `assert!(expected >= 1)` on a value that is a `NonZero`.

## Placement follows the source

- **Integration tests, the default.** A test that exercises only a crate's public API lives in the crate's `tests/` directory (a sibling of `src/`). These compile as separate crates and can see only the public surface, which keeps them honest about what the crate exposes.
- **In-file unit tests, only for private access.** A `#[cfg(test)] mod tests { use super::*; ... }` at the bottom of a source file is for tests that need private module or file internals. This is the one expected `use super::*;`.
- **No dedicated test or fixture file under `src/`.** Never create `src/tests.rs`, `src/test_util.rs`, or a sibling `*_tests.rs`. A test module that has grown large is not a reason to hive it into `src/tests.rs`; if its tests only touch the public API, move them to `tests/` instead, and leave the genuinely internal ones inline.
- **Shared fixtures live in an inline `#[cfg(test)] pub mod tests`, cross-imported.** A fixture used across the crate (an ephemeral-loopback helper, say) lives in `lib.rs`'s tests module. A fixture specific to one behaviour (a mock sink, a recording double) lives in the tests module of the file that owns that behaviour, and other test modules import it by path: `use crate::session::tests::MockSink;`. This keeps the fixture beside the code it doubles without a standalone `src/` test file.
- **Fixtures shared between integration-test binaries live in `tests/common/mod.rs`.** Each file directly under `tests/` compiles as its own crate, so inline fixtures cannot be cross-imported there; the cargo-conventional `tests/common/` subdirectory is not treated as a test binary, and each test file that needs it declares `mod common;`. Head the module with `#![allow(dead_code)]` and a comment saying why: different binaries use different subsets of the fixtures, so per-binary unused helpers are expected.

A dedicated test-harness *crate* (for example a `tests/` workspace member that exists solely to provide a reusable suite across backends) is exempt from the "no file under `src/`" rule: being a test library is that crate's whole purpose.

## Conventions for all tests

- Test functions are `snake_case` and name the scenario under test.
- Do not add rustdoc to test functions or test helpers.
- Parsers and protocol stream handlers also have fuzz targets under the crate's `fuzz/` directory; new wire-facing parsing code should come with one.
- **No hard-coded cryptographic values, even in tests.** Literal keys, IVs, and salts are forbidden everywhere, `#[cfg(test)]` modules included. A value that is immaterial to the assertion (a round trip with whatever key was used, a rejection that fires before the key is read) MUST be generated — through the crate's own randomness helper — at the length the algorithm requires.
- **A key MAY be hard-coded only when its value is part of the fixture's contract.** The test is binary: were the expected bytes produced *outside the test* with that exact key — a spec appendix, a conformance/PICS vector, an interop capture? Then the key is pinned data, stays verbatim, and regenerating it would break the very correspondence under verification. If the test itself produces everything it checks, nothing pins the key: it MUST be generated. There is no third case.
- **Agreement on a generated value is explicit data flow, never ambient.** When two paths must use the same key (sign, then verify), the test binds it once and passes it to both; helpers take the key as a parameter. Never memoize a generating fixture into a process-global to make ambient calls agree — that is the shared ambient state this guide forbids, and it hides the dependency. Conversely, when a test *needs* two distinct values (a wrong-key rejection), generate both and `assert_ne!` them — distinctness must be structural, never encoded in two magic literals.
