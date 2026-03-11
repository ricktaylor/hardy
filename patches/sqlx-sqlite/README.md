# patches/sqlx-sqlite — dependency stub

## Problem

`rusqlite 0.38` requires `libsqlite3-sys ^0.36`.
`sqlx 0.8.6` has `sqlx-sqlite` as an optional dependency, and `sqlx-sqlite 0.8.6` requires `libsqlite3-sys ^0.30`.

Both `libsqlite3-sys` versions declare `links = "sqlite3"`, which tells Cargo that they
own the `sqlite3` native symbol namespace. Cargo enforces that only **one** package with a
given `links` value can exist in the resolved dependency graph — even for optional,
unactivated packages.

Because Cargo resolves optional dependencies eagerly (to produce a lock file that is valid
for all possible feature combinations), the two incompatible `libsqlite3-sys` versions
collide at resolution time, before any feature filtering or compilation takes place.

## Solution

This directory contains a **stub** crate that replaces the real `sqlx-sqlite 0.8.6` from
crates.io via the workspace `[patch.crates-io]` table:

```toml
# Cargo.toml (workspace root)
[patch.crates-io]
sqlx-sqlite = { path = "patches/sqlx-sqlite" }
```

The stub:
- Has the same name (`sqlx-sqlite`) and version (`0.8.6`) as the real crate, so it
  satisfies sqlx's exact `= "0.8.6"` requirement.
- Declares the feature flags that sqlx forwards to `sqlx-sqlite` (e.g. `json`, `time`,
  `migrate`) as empty no-ops, so the resolver does not complain about missing features.
- Has **no dependencies** — in particular no `libsqlite3-sys` — which removes the
  `links` conflict entirely.

With the stub in place the resolved graph contains only one `links = "sqlite3"` package
(`libsqlite3-sys 0.36` via rusqlite), and resolution succeeds.

## Why the stub is never compiled

Nothing in this workspace enables the `sqlite` feature on sqlx.  `sqlx-sqlite` is an
optional dep of sqlx that is only activated by that feature.  The resolver picks a version
for it (our stub) to keep the lock file consistent, but the package is never included in
any compilation unit.

## Maintenance

**If sqlx is upgraded**, check whether the new version forwards additional features to
`sqlx-sqlite`.  If it does, add matching empty feature declarations to this stub's
`Cargo.toml`, otherwise the resolver will report a missing-feature error.

**If the `sqlite` feature on sqlx is ever intentionally enabled** in this workspace, this
stub must be removed and the `libsqlite3-sys` conflict resolved by another means (e.g.
porting `sqlite-storage` from rusqlite to sqlx's sqlite backend so that both crates share
a single `libsqlite3-sys` version).
