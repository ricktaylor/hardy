# Test Plan: IPN Legacy Encoding Filter

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | IPN Legacy Encoding Filter |
| **Module** | `ipn-legacy-filter` |
| **Implements** | `hardy_bpa::filters::WriteFilter` trait |
| **Requirements Ref** | [LLR 1.1.23](../../docs/requirements.md#standards-compliance-11), [LLR 1.1.24](../../docs/requirements.md#standards-compliance-11) (RFC 9758 IPN encoding) |
| **Test Suite ID** | PLAN-IPNF-01 |
| **Version** | 1.0 |

## 1. Introduction

This document details the testing strategy for the `ipn-legacy-filter` crate. This egress `WriteFilter` rewrites IPN 3-element EIDs to the legacy 2-element encoding for peers that require the older format. It is configured with a set of EID patterns identifying which next-hops need the rewrite.

## 2. Requirements Mapping

This crate supports the interoperability aspects of RFC 9758 IPN encoding:

| LLR ID | Description |
| :--- | :--- |
| **1.1.23** | Support 3-element CBOR encoding of 'ipn' scheme EIDs (RFC 9758) — this filter converts FROM 3-element |
| **1.1.24** | Indicate legacy 2-element vs 3-element encoding — this filter produces 2-element (`LegacyIpn`) output |

## 3. Unit Test Cases

*Objective: Verify the filter logic for all code paths through `WriteFilter::filter()`.*

| Test ID | Scenario | Input | Expected Output |
| :--- | :--- | :--- | :--- |
| **IPNF-01** | **Matching next-hop, IPN source and dest** | Bundle with IPN 3-element source and dest; next-hop matches a configured pattern. | `Continue(None, Some(data))` with both EIDs rewritten to `LegacyIpn`. |
| **IPNF-02** | **Non-matching next-hop** | Bundle with IPN 3-element EIDs; next-hop does not match any configured pattern. | `Continue(None, None)` -- no rewrite. |
| **IPNF-03** | **No next-hop** | Bundle with `next_hop = None`. | `Continue(None, None)` -- no rewrite. |
| **IPNF-04** | **DTN destination (not IPN)** | Bundle with DTN-scheme destination; next-hop matches a configured pattern. | `Continue(None, None)` -- no rewrite (neither source nor dest is IPN). |
| **IPNF-05** | **Only source needs rewrite** | Bundle with IPN 3-element source, DTN destination; next-hop matches. | `Continue(None, Some(data))` with only source rewritten to `LegacyIpn`. |
| **IPNF-06** | **Empty config** | `Config` with no peer patterns. | `IpnLegacyFilter::new()` returns `None` (filter not needed). |

## 4. Execution Strategy

* **Unit Tests:** `cargo test -p ipn-legacy-filter`
