# BPA Server Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-bpa-server` |
| **Crate version** | `0.2.0` |
| **Standard** | — |
| **Test Plans** | [`PLAN-SERVER-01`](test_plan.md) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

9 of 12 server-specific scenarios pass (3 config + 3 lifecycle implicit + 3 static routes). No formal LLRs are assigned to this crate — configuration requirements (3.1.5, 7.1.x, 7.2.x) are passthrough concerns verified by the respective backend crates. Protocol correctness is verified by `hardy-bpa`, `hardy-proto`, and `hardy-tcpclv4` test suites.

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| — | Configuration defaults | **Pass** | `empty_config_has_defaults` |
| — | Configuration file parsing (YAML, TOML, JSON) | **Pass** | `yaml_overrides_defaults`, `toml_config`, `json_config` |
| — | Environment variable overrides | **Pass** | `env_overrides_file`, `env_overrides_nested_fields` |
| — | Configuration validation | **Pass** | 7 error-case tests (malformed files, invalid values) |
| — | Storage config | **Pass** | `storage_memory_config` |
| — | CLA config | **Pass** | `cla_list_parsing`, `empty_cla_list` |
| — | BPSec key store & bindings configuration | **Pass** | `config/bpsec.rs` (8 tests), `bpsec_config_parses`, `no_bpsec_config` |
| — | BPSec pattern-based key resolution | **Pass** | `bpsec.rs` (16 tests) |
| 3.2 | Process startup | **Pass** | Interop + CI (implicit) |
| 3.2 | BPA gRPC registration | **Pass** | Interop + CI (implicit) |
| 3.2 | Graceful shutdown | **Pass** | Interop + CI (implicit) |

## 2. Test Inventory

### Unit Tests (62 tests)

#### Configuration (`config/mod.rs` — 23 tests)

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `empty_config_has_defaults` | CFG-01 | Empty file → valid defaults |
| `yaml_overrides_defaults` | CFG-02 | YAML overrides default values |
| `toml_config` | CFG-02 | TOML file works identically |
| `json_config` | CFG-02 | JSON file works identically |
| `env_overrides_file` | CFG-03 | Env var overrides file value |
| `env_overrides_nested_fields` | CFG-03 | `__` separator for nested fields |
| `missing_config_file_errors` | CFG-04 | Non-existent file → error |
| `invalid_log_level_errors` | CFG-04 | Bad log level → error |
| `zero_pool_size_errors` | CFG-04 | Zero processing pool → error |
| `zero_poll_channel_depth_errors` | CFG-04 | Zero poll depth → error |
| `negative_value_errors` | CFG-04 | Negative unsigned → error |
| `malformed_yaml_errors` | CFG-04 | Invalid YAML → error |
| `malformed_toml_errors` | CFG-04 | Invalid TOML → error |
| `malformed_json_errors` | CFG-04 | Invalid JSON → error |
| `storage_memory_config` | CFG-05 | Memory storage type selection |
| `cla_list_parsing` | CFG-06 | TCPCLv4 CLA entry |
| `empty_cla_list` | CFG-06 | Empty CLA list valid |
| `echo_service_parsing` | — | Integer + string service IDs |
| `unknown_fields_ignored` | — | Extra fields accepted |
| `single_node_id` | — | Single string node ID |
| `multiple_node_ids` | — | List with IPN + DTN |
| `bpsec_config_parses` | — | BPSec section parsed into typed config |
| `no_bpsec_config` | — | Absent BPSec section is valid |

#### BPSec Key Configuration (`config/bpsec.rs` — 8 tests)

Validation of the key store and pattern bindings at config-build time: a valid key file and bindings build successfully; a missing key file, an unknown `kid` reference, a duplicate `kid`, an empty binding, a key without `key_ops`, and a non-symmetric key are each rejected; a key store with no bindings remains valid.

#### BPSec Key Resolution (`bpsec.rs` — 16 tests)

The `PatternKeySource` resolution logic: EID pattern matching (wildcard, specific-overrides-wildcard, most-specific-role-wins), role-based key release (verifier withholds decrypt keys, source releases only protection keys, integrity-only bindings), operation routing via `key_ops`, missing-`kid` and no-match cases returning `None`, and key-order priority. Provider-level tests cover the empty provider, `set` replacing a previous key source, and snapshot isolation. Two end-to-end tests verify role behaviour on real bundles: a verifier forwards a BCB intact, and an acceptor decrypts the payload at delivery.

#### Static Routes (`static_routes/loader.rs` — 15 tests)

Parser tests for the static routes file format: via/drop/reflect actions, priority, comments, blank lines, CRLF, multi-line, error messages.

#### Static Routes Integration (`tests/test_static_routes.sh` — 5 tests)

| Test | Scope |
| :--- | :--- |
| TEST 1: Startup with routes | BPA starts with routes file |
| TEST 2: Hot-reload | Modify routes file, BPA reloads without error |
| TEST 3: File removal | Delete routes file, BPA handles gracefully |
| TEST 4: File restore | Recreate routes file, BPA reloads without error |
| TEST 5: Ping echo | BPA functional after reload cycle |

### Cross-Coverage from Other Test Suites

Lifecycle scenarios (startup, registration, shutdown) are exercised by:

- **Interop tests** ([`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md)) — all 7 peer implementations
- **CI pipeline** (`compose.ping-tests.yml`) — Docker container lifecycle

## 3. Coverage vs Plan

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| Configuration (CFG-01..06) | Defaults, parsing, env, validation, storage, CLA | 6 | 6 | Complete (23 tests) |
| BPSec key configuration & resolution | Config validation + `PatternKeySource` | — | 24 | Complete |
| Static routes parser | Parser unit tests | — | 15 | Complete |
| Lifecycle (SYS-01..03) | Startup, registration, shutdown | 3 | 3 | Exercised by interop + CI (implicit) |
| Lifecycle (SYS-04) | Crash recovery | — | — | Delegated to bpa + storage harness |
| Lifecycle (SYS-05) | Config reload (static routes) | 1 | 1 | Complete (`test_static_routes.sh`, 5 tests) |
| Observability (OTEL-01..03) | Trace, metric, log export | — | — | Delegated to [`COMP-OTEL-01`](../../otel/docs/component_test_plan.md) |
| Performance, stress, packaging | System-level non-functional | 27 | 0 | Full Activity scope |
| **Total** | | **37** | **27** | **73%** |

## 4. Line Coverage

> Current figures are generated — see the [coverage summary](../../docs/coverage_summary.md) (refreshed by `scripts/run_lcov.sh`) and the live coverage dashboards (CFLite fuzz coverage on gh-pages; CI-published coverage planned). The snapshot below is from the crate version in the header.

```
cargo llvm-cov test --package hardy-bpa-server --lcov --output-path lcov.info
lcov --summary lcov.info
```

```
  lines......: 73.1% (1018 of 1393 lines)
  functions..: 72.0% (134 of 186 functions)
```

Unit tests (62) exercise configuration loading and validation, BPSec key configuration and resolution, and static routes parsing.

## 5. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Performance | System-level throughput/latency not benchmarked | Low | Full Activity scope |

## 6. Conclusion

62 unit tests cover configuration loading (23 tests across YAML, TOML, JSON, env overrides, validation, and error handling), BPSec key configuration and pattern-based key resolution (24 tests), and static routes parsing (15 tests). All 6 configuration test scenarios (CFG-01..06) from the test plan are covered. Lifecycle scenarios (startup, registration, shutdown) are verified implicitly by interop and CI tests. 27 of 37 planned scenarios implemented (73%). Crash recovery (SYS-04) is delegated to the bpa and storage harness. OTEL export (OTEL-01..03) is delegated to the hardy-otel crate — all binaries use the same `hardy_otel::init()`. Remaining gap is performance — Full Activity scope.
