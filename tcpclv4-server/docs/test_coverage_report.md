# TCPCLv4 Server Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-tcpclv4-server` |
| **Test Plans** | [`PLAN-TCPCL-SERVER-01`](test_plan.md) |
| **Date** | 2026-04-14 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The tcpclv4-server wraps the `hardy-tcpclv4` library. Protocol-level verification is covered by the library's [coverage report](../../tcpclv4/docs/test_coverage_report.md). This report covers server-specific concerns only.

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 3.2 | Process startup | **Pass** | Interop + CI (implicit) |
| 3.2 | BPA gRPC registration | **Pass** | Interop + CI (implicit) |
| 3.2 | Graceful shutdown (SIGTERM) | **Pass** | Interop + CI (implicit) |
| 3.2 | Configuration defaults | **Pass** | `empty_config_has_defaults` |
| 3.2 | Configuration file parsing (TOML, YAML, JSON) | **Pass** | `toml_overrides_defaults`, `yaml_config`, `json_config` |
| 3.2 | Environment variable overrides | **Pass** | `env_overrides_file`, `env_overrides_nested_fields` |
| 3.2 | Configuration validation | **Pass** | 7 error-case tests |
| 3.2 | Performance (> 1Gbps) | **Not tested** | PERF-SRV-01 |
| 3.2 | OCI image structure | **Not tested** | PKG-OCI-01 |
| 3.2 | Helm chart | **Not tested** | PKG-HELM-01 |

## 2. Test Inventory

### Unit Tests (16 tests in `config.rs`)

| Test Function | Scope |
| :--- | :--- |
| `empty_config_has_defaults` | Empty file → valid defaults (bpa_address, cla_name, port 4556) |
| `toml_overrides_defaults` | TOML overrides all fields |
| `yaml_config` | YAML file works identically |
| `json_config` | JSON file works identically |
| `env_overrides_file` | Env var overrides file value |
| `env_overrides_nested_fields` | `__` separator for nested TCPCLv4 fields |
| `missing_config_file_errors` | Non-existent file → error |
| `invalid_log_level_errors` | Bad log level → error |
| `negative_segment_mru_errors` | Negative MRU → error |
| `invalid_address_errors` | Bad listen address → error |
| `tls_partial_config` | TLS cert + key without CA |
| `malformed_toml_errors` | Invalid TOML → error |
| `malformed_yaml_errors` | Invalid YAML → error |
| `unknown_fields_ignored` | Extra fields accepted |
| `large_segment_mru` | Large MRU value accepted |
| `keepalive_zero` | Zero keepalive = disabled |

### Cross-Coverage from Other Test Suites

Lifecycle scenarios (startup, registration, shutdown) are exercised by:

- **Interop tests** ([`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md)) — 4 TCPCLv4 peer implementations
- **CI pipeline** (`compose.ping-tests.yml`) — Docker container lifecycle

## 3. Coverage vs Plan

| Source | Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| Configuration (CFG-01..03) | Defaults, multi-format parsing, env overrides, validation | 3 | 3 | Complete (16 tests) |
| Server lifecycle (SYS-01..03) | Startup, registration, shutdown | 3 | 3 | Exercised by interop + CI (implicit) |
| Performance (PERF-SRV-01) | Throughput | 1 | 0 | Full Activity scope |
| Packaging (PKG-OCI-01, PKG-HELM-01) | OCI image, Helm chart | 2 | 0 | Full Activity scope |

## 4. Line Coverage

```
cargo llvm-cov test --package hardy-tcpclv4-server --lcov --output-path lcov.info
lcov --summary lcov.info
```

Unit tests (16) exercise config loading, multi-format parsing, env override, validation, and error handling.

## 5. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Packaging | No OCI/Helm verification | Low | Full Activity scope |
| Performance | No throughput test | Low | Full Activity scope |

## 6. Conclusion

16 configuration unit tests cover all 3 planned config scenarios (CFG-01..03) including multi-format parsing (TOML, YAML, JSON), env overrides with nested field support, and comprehensive validation/error handling. Lifecycle scenarios are verified implicitly by interop and CI. Performance and packaging tests remain for Full Activity.
