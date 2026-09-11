# bpa-server TODO

## Missing storage default: report as a `Config::load` error instead of panicking

### Background

`MetadataStorageConfig::default()` and `BundleStorageConfig::default()` (`src/config/storage.rs`) panic in builds compiled without the feature that provides their default backend (`sqlite-storage` for metadata, `localdisk-storage` for bundle data). The stance is deliberate: an unconfigured node must never silently degrade to a non-persistent memory backend, and the panic is pinned by `empty_config_has_defaults` in `src/config/mod.rs`, which flips to `should_panic(expected = "built without the")` under those feature combinations.

`StorageConfig` now carries per-field `#[serde(default)]` rather than a container-level one, so a selector default is only constructed for a selector genuinely absent from the config. A feature-reduced build that configures both backends explicitly no longer panics at startup; the panic is confined to the case it is actually about.

### What is still wrong

A panic is the wrong report for a user error in a config file. The operator gets a backtrace instead of a diagnostic, and the failure is indistinguishable from a bug. It also keeps most of the config tests from running under `--no-default-features`: any test that loads a config without an explicit `storage.metadata`/`storage.bundle` aborts the process rather than returning an error the test can assert on.

### What is needed

- A typed error variant on the `Config::load` error surface for "no default backend in this build", carrying which selector (`storage.metadata` or `storage.bundle`) and which feature would have provided it, so the message stays as actionable as today's panic text.
- The selector `Default` impls can then stop panicking; the missing-default decision moves to the point where the config is validated, which means threading fallibility through the serde path (a custom deserializer for the two fields, or a post-load validation pass over an `Option`-shaped intermediate).
- Every `Config::load` caller updated for the widened error taxonomy: this is a load-API contract change, which is why it was deferred from the per-field-defaults fix by review ruling (2026-09-11) rather than folded into it.
- `empty_config_has_defaults` converts from `should_panic` to asserting the typed error variant, and the config tests become runnable under `--no-default-features`.
