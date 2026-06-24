# Test Coverage Report Style Guide

This guide defines the style and content expectations for per-crate `test_coverage_report.md` documents in Hardy.

## Purpose

Coverage reports capture the **current state of test verification** against requirements. They answer: "what is tested, how, and what remains?" They are for reviewers assessing compliance and engineers planning test work.

Coverage reports are **living documents** — updated when tests are added, not snapshots frozen in time; git history is the record of changes.

Since the V-model review gates (TRR/VRR) concluded at v0.1.0, Hardy is in continuous-improvement mode: these reports are maintained alongside the code (git is the history — there is no freeze or "uprev, don't rewrite" ceremony), coverage figures are **generated** by `scripts/run_lcov.sh` rather than hand-embedded, and each report anchors to the crate version it reflects rather than to a review date.

## What Belongs in Coverage Reports

- **LLR verification status** — which requirements are verified and by which tests
- **Test inventory** — what tests exist, where they are, what they cover
- **Coverage metrics** — line coverage from `cargo llvm-cov` / `lcov`
- **Gap analysis** — what is not yet tested and why
- **Cross-references** — tests in other crates that verify requirements assigned to this crate

## What Does NOT Belong in Coverage Reports

- **Test plan content** — don't duplicate test descriptions from the test plans; link to them
- **Bug reports or change history** — coverage reports track current state, not how we got here
- **Implementation details** — don't explain how the code works; the design doc does that
- **Aspirational targets** — report what IS covered, not what coverage SHOULD be

## Document Structure

Every coverage report should follow this structure. Sections may be omitted if genuinely not applicable (e.g., no fuzz targets), but the ordering should be preserved.

```markdown
# <Crate Name> Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `<crate-name>` |
| **Standard** | <RFC or spec reference, if applicable> |
| **Test Plans** | [links to unit, component, fuzz test plans] |
```

### Section 1: LLR Coverage Summary (Requirements Verification Matrix)

This section serves as the **test verification report** referenced by the [Part 4 Requirements Verification Matrix](../requirements.md#part-4-requirements-verification-matrix). It provides end-to-end traceability from top-level requirements through LLRs to individual test results.

A table mapping every LLR assigned to this crate to its verification status and result.

```markdown
| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| **1.1.25** | Valid canonical bundle generation | Pass | `builder.rs::test_builder` + CLI CREATE-01..03 | 1.2 |
| **1.1.33** | Bundle Age for expiry | N/A | Enforced by BPA, not parser | 1.2 |
```

- **Lead with a one-line summary**: "All LLRs verified (N pass, M N/A)" or "N of M LLRs pass"
- **Every assigned LLR must appear** — even if N/A or not tested
- **Result values**: `Pass` (test exists and passes), `Fail` (test exists and fails), `N/A` (not applicable to this crate), `Not tested` (no test exists)
- **Test column**: cite specific test function names, not just file paths
- **Part 4 Ref column**: the mid-level requirement ID from the [Part 4 matrix](../requirements.md#part-4-requirements-verification-matrix) that this LLR traces to (e.g., `1.2` for "demonstrate support via a test verification report of 1.1")
- **Cross-crate coverage**: when an LLR is verified by another crate's tests, say `Pass (bpv7)` and cite the specific test. The Part 4 Ref still traces to the top-level requirement

The Part 4 matrix defines two types of mid-level requirement:
- **Compliance matrix** (e.g., 1.1, 3.1): "deliver a compliance verification matrix" — satisfied by the PICS proforma or RFC compliance matrix documents
- **Test verification** (e.g., 1.2, 3.2): "demonstrate support via a test verification report" — satisfied by this LLR table, where every row with `Result = Pass` is evidence of verification

### Section 2: Test Inventory

List all tests, grouped by type:

- **Unit tests**: table with columns `Test Function | File | Plan Section | Scope`
- **Component/integration tests**: separate subsection with test script path and suite breakdown
- **Fuzz tests**: table with `Target | File | Status`

Include a total count for each group.

When a crate has no unit tests, state "No unit tests are implemented" and reference the test plan for rationale (e.g., "see `PLAN-FOO-01` §4 for rationale"). The test plan should explain *why* — thin wrapper, logic verified end-to-end, or backend-specific scenarios would test third-party library semantics. Don't duplicate the rationale in both documents.

### Section 3: Coverage vs Plan

A table cross-referencing test plan sections against implementation status:

```markdown
| Section | Scenario | Planned | Implemented | Status |
```

- **Planned**: number of scenarios in the test plan
- **Implemented**: number with passing tests
- **Status**: `Complete`, `N/M remaining`, or `Delegated to <crate>`
- **Delegated tests**: when a plan section is covered by another crate, use `—` for Planned/Implemented counts and explain in Status. Adjust totals accordingly
- **Include a total row** with percentage

### Section 4: Line Coverage

Coverage figures are **generated, not hand-embedded.** `scripts/run_lcov.sh` measures every crate and writes the unit and fuzz line/function figures to [`docs/coverage_summary.md`](../coverage_summary.md) (the single source of truth); CI/CFLite publish the live dashboards. Each report's §4 should therefore be a short pointer, not a number dump:

- **Link to the generated summary** (`docs/coverage_summary.md`) and the live dashboards; don't paste figures that will drift.
- **Don't hand-maintain per-file breakdown tables** — they go stale fastest. Link to the live HTML coverage if a per-file view is needed.
- **Note what the numbers exclude**: "unit tests only" or "includes integration tests".
- **Explain anomalies** in prose (this stays by hand): generic monomorphisation inflating function counts, Display impls at 0%, storage backends verified via the harness, etc.

The underlying measurement (for reference) is `cargo llvm-cov test --package <name> --lcov` + `lcov --summary`, which `run_lcov.sh` runs for you.

If line coverage has not yet been measured, include the command block so the reader can run it:

```markdown
```
cargo llvm-cov test --package <name> --lcov --output-path lcov.info --html
lcov --summary lcov.info
```
```

Don't say "has not been run" — provide the command and leave space for results to be added. If line coverage is genuinely not applicable (e.g., thin wrapper with no unit tests and all verification via external harness), state why and omit the section.

**Fuzz coverage**: For crates with fuzz targets, include a separate subsection showing how to generate fuzz coverage from the corpus. This measures which code paths the fuzzer has discovered, complementing the unit test coverage:

```markdown
### Fuzz Coverage

```
cargo +nightly fuzz coverage <target>
cargo +nightly cov -- export --format=lcov ...
lcov --summary ./fuzz/coverage/<target>/lcov.info
```
```

Fuzz coverage is complementary to unit test coverage: unit tests verify correctness against known inputs, fuzz verifies robustness against adversarial input. When both are available, note this and explain what each layer contributes.

### Section 5: Test Infrastructure

Brief description of how tests are structured:

- What test helpers/fixtures exist (e.g., `make_store()`, `Bpa::builder()`)
- What mock types are used and where they live
- Runtime requirements (e.g., `multi_thread` for concurrent tests)
- Patterns that future test authors should follow

This section is optional for simple crates with straightforward tests.

### Section 6: Key Gaps

A table of remaining coverage gaps:

```markdown
| Area | Gap | Severity | Notes |
```

- **Only list genuine gaps** — not items covered by other crates
- **Severity**: High (LLR unverified), Medium (significant code path untested), Low (edge case or defence-in-depth)
- **If no gaps remain**, state that explicitly: "All LLRs verified. No significant gaps remain."

### Section 7: Conclusion

A single paragraph summarising:

- Total test count and plan coverage percentage
- Line coverage percentage
- How many LLRs are verified (the headline metric)
- Key strengths (what's well-tested)
- Primary remaining gaps (if any)
- Other test layers that contribute (fuzz, interop, CLI)

## Cross-Crate References

When a crate delegates verification to another crate:

- **In the LLR table**: use `Verified (other-crate)` status and cite the specific test
- **In Coverage vs Plan**: use `Delegated to <crate>` and adjust totals to show "in-scope" vs "total"
- **Don't double-count**: if a test in crate A verifies an LLR assigned to crate B, only crate B's report should count it as verified. Crate A's report should note it as "Verified (crate B)" without inflating its own counts

## Consistency Rules

- **Header table**: always use `| :--- | :--- |` alignment
- **Date format**: `YYYY-MM-DD`
- **Test function names**: use `file::function_name` format (e.g., `parse.rs::ccsds_compliance`)
- **LLR references**: bold the ID (e.g., `**1.1.30**`)
- **Percentages**: one decimal place for line coverage (e.g., 78.2%), zero for plan coverage (e.g., 92%)
- **File paths**: relative to crate `src/` for source, relative to crate root for test scripts
- **Plan references**: link to the plan file, use the plan's Test Suite ID in brackets (e.g., `[UTP-BPV7-01]`)

## Root-Level Coverage Report

The project-level report at `docs/test_coverage_report.md` summarises all crates. It should:

- List all test plans with their crate and status
- Aggregate test counts (unit, fuzz, component, interop)
- Summarise PICS compliance
- Identify cross-cutting gaps
- Link to per-crate reports rather than duplicating their content

## Requirements Traceability

The coverage reports form part of the deliverable verification chain defined in [requirements.md](../requirements.md):

```
Part 2: Top-level Requirements (REQ-1, REQ-2, ...)
    │
Part 4: Verification Matrix (mid-level: 1.1, 1.2, 3.1, 3.2, ...)
    │
    ├── "compliance verification matrix" (1.1, 3.1, ...) → PICS Proforma / RFC compliance docs
    │
    └── "test verification report" (1.2, 3.2, ...) → Coverage Report §1 LLR table
            │
Part 3: Low-Level Requirements (LLR 1.1.1, 1.1.2, ...)
            │
            └── Individual test functions → Pass/Fail
```

Each coverage report's LLR table closes the loop from Part 4 "test verification report" requirements to individual test results. The `Part 4 Ref` column makes this traceability explicit.

For the root-level coverage report (`docs/test_coverage_report.md`), include a summary mapping Part 4 mid-level requirements to the crate-level reports that satisfy them.

## Questions to Ask When Writing

1. Is every assigned LLR accounted for in the table?
2. Does the test inventory match what `cargo test` actually runs?
3. Are cross-crate references accurate and up to date?
4. Would a reviewer know exactly what is and isn't verified?
5. Are the line coverage numbers current?
