# Unit Test Plan: Time-Variant Routing (TVR)

| Document Info | Details |
| ----- | ----- |
| **Functional Area** | Contact Scheduling (Cron, Parser, Scheduler) |
| **Module** | `hardy-tvr` |
| **Requirements Ref** | [REQ-6](../../docs/requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth) |
| **Test Suite ID** | UTP-TVR-01 |
| **Version** | 1.0 |

## 1. Introduction

This document details the unit testing strategy for the `hardy-tvr` functional area. This module is responsible for ingesting contact schedules (from files and gRPC sessions), evaluating cron-based recurrence, and projecting time-variant routes into the BPA.

**Scope:**

* Cron expression parsing, matching, and occurrence finding.

* Contact plan file parsing (actions, schedule types, link properties, validation).

* Scheduler logic: activation, deactivation, event ordering, lazy recurrence, replace diffing, source isolation, and route refcounting.

* **Note:** gRPC session handling and BPA integration are not covered by unit tests. The gRPC service is exercised by the end-to-end integration test (`tests/test_tvr.sh`).

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by this plan:

| REQ ID | Description | Mechanism |
| ----- | ----- | ----- |
| **6.1** | Specify start of contact period | `start` field or cron expression |
| **6.2** | Specify duration of contact period | `end` field or `duration` with cron |
| **6.2a** | Specify expected periodicity | Cron expression |
| **6.3** | Specify expected bandwidth | `bandwidth` field parsed (not enforced; PeerLinkInfo pending) |
| **6.4** | Updatable without restart | File hot-reload + TVR gRPC service |

## 3. Unit Test Cases

### 3.1 Cron Parsing — 5-Field (REQ-6.2a)

*Objective: Verify parsing of standard 5-field cron expressions into correct bitsets.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Every Minute** | Wildcard in all fields. | `src/cron.rs` | `* * * * *` | All minute bits set; second defaults to bit 0. |
| **Specific Values** | Fixed minute and hour. | `src/cron.rs` | `0 8 * * *` | minute = bit 0, hour = bit 8. |
| **Range** | Weekday business hours. | `src/cron.rs` | `0 9-17 * * 1-5` | hour bits 9–17, dow bits 1–5. |
| **Step** | Every 15 minutes. | `src/cron.rs` | `*/15 * * * *` | minute bits 0, 15, 30, 45. |
| **Range with Step** | Even hours 8–18. | `src/cron.rs` | `0 8-18/2 * * *` | hour bits 8, 10, 12, 14, 16, 18. |
| **List** | Two specific minutes. | `src/cron.rs` | `0,30 * * * *` | minute bits 0 and 30. |

### 3.2 Cron Parsing — 6-Field with Seconds (REQ-6.1)

*Objective: Verify parsing of 6-field cron expressions with second granularity.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Seconds Field** | Specific second, minute, hour. | `src/cron.rs` | `30 0 8 * * *` | second = bit 30, minute = bit 0, hour = bit 8. |
| **Every 10 Seconds** | Step in seconds field. | `src/cron.rs` | `*/10 * * * * *` | second bits 0, 10, 20, 30, 40, 50. |

### 3.3 Cron Parsing — Named Values (REQ-6.2a)

*Objective: Verify named weekday and month aliases.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Named Weekdays** | Range of named days. | `src/cron.rs` | `0 9 * * MON-FRI` | dow bits 1–5. |
| **Named Weekday List** | Comma-separated days. | `src/cron.rs` | `0 9 * * MON,WED,FRI` | dow bits 1, 3, 5. |
| **Case Insensitive** | Lowercase vs uppercase. | `src/cron.rs` | `mon-fri` vs `MON-FRI` | Identical dow bits. |
| **Named Sunday** | SUN maps to bit 0. | `src/cron.rs` | `0 0 * * SUN` | dow = bit 0. |
| **Named Months** | Range of named months. | `src/cron.rs` | `0 8 * MAR-OCT *` | month bits 3–10. |
| **Named Month List** | Comma-separated months. | `src/cron.rs` | `0 8 * JAN,JUN,DEC *` | month bits 1, 6, 12. |
| **Sunday Alias (7)** | Numeric 7 folds to bit 0. | `src/cron.rs` | `0 0 * * 7` | Same as `0 0 * * 0`. |

### 3.4 Cron Parsing — Shortcuts (REQ-6.2a)

*Objective: Verify `@` shortcut expansion.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **@daily** | Midnight every day. | `src/cron.rs` | `@daily` | second=0, minute=0, hour=0, all dom/month/dow. |
| **@midnight** | Alias for @daily. | `src/cron.rs` | `@midnight` | Identical to `@daily`. |
| **@hourly** | Top of every hour. | `src/cron.rs` | `@hourly` | minute=0, all hours. |
| **@weekly** | Sunday midnight. | `src/cron.rs` | `@weekly` | dow = Sunday only. |
| **@monthly** | 1st of every month. | `src/cron.rs` | `@monthly` | dom = bit 1, all months. |
| **@yearly** | Jan 1st midnight. | `src/cron.rs` | `@yearly` | dom = bit 1, month = January. |
| **@annually** | Alias for @yearly. | `src/cron.rs` | `@annually` | Identical to `@yearly`. |
| **Unknown Shortcut** | Reject invalid shortcut. | `src/cron.rs` | `@every`, `@secondly` | Error. |

### 3.5 Cron Parsing — Validation

*Objective: Verify rejection of malformed cron expressions.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Invalid Field Count** | Too few or too many fields. | `src/cron.rs` | `* * *`, `* * * * * * *` | Error. |
| **Out of Range** | Values exceeding field bounds. | `src/cron.rs` | `60 * * * *`, `* 24 * * *`, etc. | Error. |
| **Empty Range** | End < start. | `src/cron.rs` | `* * * * 5-3` | Error. |
| **Zero Step** | Step of 0. | `src/cron.rs` | `*/0 * * * *` | Error. |

### 3.6 Cron Matching

*Objective: Verify O(1) bitset matching against specific datetimes.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Matches Specific** | Exact match and non-match. | `src/cron.rs` | `0 8 * * *` vs 08:00, 08:01, 07:00 | true, false, false. |
| **Matches with Seconds** | 6-field match at :30. | `src/cron.rs` | `30 0 8 * * *` vs 08:00:30, 08:00:00 | true, false. |
| **Matches Weekday** | Weekday filter. | `src/cron.rs` | `0 9 * * MON-FRI` vs Friday, Saturday | true, false. |

### 3.7 Cron next_after / prev_before

*Objective: Verify lazy occurrence finding for scheduler integration.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Same Minute** | At exact match time. | `src/cron.rs` | `0 8` at 08:00 | 08:00 (same). |
| **Later Today** | Next match later in day. | `src/cron.rs` | `30 8` at 08:00 | 08:30. |
| **Tomorrow** | Wraps to next day. | `src/cron.rs` | `0 8` at 09:00 | Next day 08:00. |
| **Skips Weekend** | Weekday-only expression. | `src/cron.rs` | `0 9 * * 1-5` on Friday after 09:00 | Monday 09:00. |
| **Month Rollover** | Wraps to next month. | `src/cron.rs` | `0 0 1 * *` on Mar 2 | Apr 1. |
| **With Seconds** | Second-granularity. | `src/cron.rs` | `*/30 * * * * *` at 08:00:01 | 08:00:30. |
| **Prev Same Minute** | At exact match time. | `src/cron.rs` | `0 8` at 08:00 | 08:00 (same). |
| **Prev Earlier Today** | Previous match earlier in day. | `src/cron.rs` | `0 8` at 09:00 | 08:00. |
| **Prev Yesterday** | Wraps to previous day. | `src/cron.rs` | `0 8` at 07:00 | Previous day 08:00. |
| **Prev Skips Weekend** | Weekday-only backwards. | `src/cron.rs` | `0 17 * * MON-FRI` on Sunday | Previous Friday 17:00. |
| **Prev With Seconds** | Second-granularity backwards. | `src/cron.rs` | `*/30 * * * * *` at 08:00:29 | 08:00:00. |

### 3.8 Cron Display

*Objective: Verify original expression is preserved for display.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Preserves Source** | Display round-trips. | `src/cron.rs` | `*/15 8-17 * * MON-FRI` | Same string. |
| **Preserves Shortcut** | Shortcut round-trips. | `src/cron.rs` | `@daily` | `@daily`. |

### 3.9 Contact Plan Parser — Actions (REQ-6.1)

*Objective: Verify parsing of contact actions and basic syntax.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Simple Via** | Forward to next-hop. | `src/parser.rs` | `ipn:2.*.* via ipn:2.1.0` | `Action::Via(ipn:2.1.0)`. |
| **Simple Drop** | Discard with default reason. | `src/parser.rs` | `ipn:2.*.* drop` | `Action::Drop(None)`. |
| **Drop with Reason** | Discard with explicit reason. | `src/parser.rs` | `ipn:2.*.* drop 6` | `Action::Drop(Some(6))`. |
| **Via with Priority** | Action with priority field. | `src/parser.rs` | `ipn:2.*.* via ipn:2.1.0 priority 50` | Priority = 50. |
| **Reflect Not Supported** | Reject reflect action. | `src/parser.rs` | `ipn:2.*.* reflect` | Error. |

### 3.10 Contact Plan Parser — One-Shot Schedule (REQ-6.1, 6.2)

*Objective: Verify parsing of fixed time window contacts.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Start and End** | Full time window. | `src/parser.rs` | `... start <t1> end <t2>` | `Schedule::OneShot { start, end }`. |
| **Start Only** | Open-ended start. | `src/parser.rs` | `... start <t1>` | `Schedule::OneShot { start, end: None }`. |
| **End Only** | Open-ended end. | `src/parser.rs` | `... end <t2>` | `Schedule::OneShot { start: None, end }`. |
| **End Before Start** | Invalid window. | `src/parser.rs` | `... start <t2> end <t1>` | Error. |
| **With Bandwidth** | One-shot with link properties. | `src/parser.rs` | `... start <t> end <t> bandwidth 256K` | Parsed bandwidth. |
| **Scheduled Drop** | Drop action with time window. | `src/parser.rs` | `... drop start <t1> end <t2>` | `Action::Drop` + `Schedule::OneShot`. |
| **Drop with Reason and Schedule** | Full drop specification. | `src/parser.rs` | `... drop 6 start <t1> end <t2>` | Both reason and schedule. |

### 3.11 Contact Plan Parser — Recurring Schedule (REQ-6.1, 6.2, 6.2a)

*Objective: Verify parsing of cron-based recurring contacts.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Cron + Duration** | Basic recurring contact. | `src/parser.rs` | `... cron "0 8 * * *" duration 90m` | `Schedule::Recurring { cron, duration }`. |
| **With Until** | Bounded recurrence. | `src/parser.rs` | `... cron "..." duration 90m until <t>` | `until` field set. |
| **With Bandwidth and Priority** | Full recurring specification. | `src/parser.rs` | `... cron "..." duration 90m bandwidth 1G priority 50` | All fields parsed. |
| **Invalid Cron** | Malformed cron expression. | `src/parser.rs` | `... cron "bad"` | Error. |
| **Cron Without Duration** | Missing required duration. | `src/parser.rs` | `... cron "0 8 * * *"` | Error. |
| **Duration Without Cron** | Orphaned duration field. | `src/parser.rs` | `... duration 90m` | Error. |
| **Mixed Oneshot/Recurring** | Conflicting schedule types. | `src/parser.rs` | `... start <t> cron "..." duration 90m` | Error. |
| **Until Without Cron** | Orphaned until field. | `src/parser.rs` | `... until <t>` | Error. |

### 3.12 Contact Plan Parser — Duration Formats (REQ-6.2)

*Objective: Verify humantime duration parsing.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Minutes** | Plain minutes. | `src/parser.rs` | `duration 90m` | 90 minutes. |
| **Hours** | Plain hours. | `src/parser.rs` | `duration 2h` | 2 hours. |
| **Compound** | Mixed units. | `src/parser.rs` | `duration 1h30m` | 90 minutes. |
| **HMS** | Hours, minutes, seconds. | `src/parser.rs` | `duration 2h30m15s` | 2h 30m 15s. |
| **Invalid (No Unit)** | Bare number. | `src/parser.rs` | `duration 90` | Error. |
| **Zero Duration** | Logically invalid. | `src/parser.rs` | `duration 0s` | Error. |

### 3.13 Contact Plan Parser — Link Properties (REQ-6.3)

*Objective: Verify bandwidth and delay parsing with SI/humantime units.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Bandwidth Bare Number** | Raw bps value. | `src/parser.rs` | `bandwidth 1000000` | 1,000,000 bps. |
| **Bandwidth SI Suffixes** | K, M, G, T. | `src/parser.rs` | `256K`, `1M`, `10G`, `1T` | Correct bps values. |
| **Bandwidth Long Suffixes** | Kbps, Mbps, etc. | `src/parser.rs` | `256Kbps`, `10Gbps` | Same as short form. |
| **Bandwidth Case Insensitive** | Mixed case. | `src/parser.rs` | `256k`, `10g`, `1GBPS` | Correct bps values. |
| **Delay Humantime** | Various delay units. | `src/parser.rs` | `500ms`, `1s`, `250us`, `4m` | Correct microsecond values. |
| **All Link Properties** | Combined bandwidth + delay. | `src/parser.rs` | `... bandwidth 10G delay 500ms` | Both fields set. |

### 3.14 Contact Plan Parser — File Format

*Objective: Verify multi-line file parsing, comments, field ordering.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Fields Any Order** | Priority before action args. | `src/parser.rs` | Various orderings | Identical parsed contact. |
| **Comments** | Lines starting with `#`. | `src/parser.rs` | `# comment\n<contact>` | Comment skipped. |
| **Blank Lines** | Empty and whitespace-only lines. | `src/parser.rs` | Mixed blanks + contacts | Blanks skipped. |
| **Multiple Contacts** | Multi-line file. | `src/parser.rs` | Two contact lines | Two parsed contacts. |
| **Mixed with Comments** | Realistic file layout. | `src/parser.rs` | Comments, blanks, contacts | Only contacts parsed. |
| **Duplicate Priority** | Repeated field. | `src/parser.rs` | `... priority 10 priority 20` | Error. |
| **Duplicate Start** | Repeated field. | `src/parser.rs` | `... start <t1> start <t2>` | Error. |
| **Invalid Inputs** | Various malformed lines. | `src/parser.rs` | Empty pattern, bad EID, etc. | Error. |
| **Useful Error Messages** | Error includes line:column. | `src/parser.rs` | Various invalid inputs | Caret-annotated error. |
| **Multiline Error Location** | Error on line > 1. | `src/parser.rs` | Error on second line | Correct line number. |

### 3.15 Scheduler — Permanent Contacts (REQ-6.1)

*Objective: Verify immediate activation of contacts with no schedule.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Activates Immediately** | Permanent contact → route op. | `src/scheduler.rs` | Permanent contact | `AddRoute` emitted. |
| **Explicit Priority** | Priority override applied. | `src/scheduler.rs` | Permanent, priority 50 | Route uses priority 50. |

### 3.16 Scheduler — One-Shot Contacts (REQ-6.1, 6.2)

*Objective: Verify event scheduling for fixed time windows.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Future Schedules Events** | Start in future → two events. | `src/scheduler.rs` | start > now | Activate + Deactivate events in timeline. |
| **Active Now** | Window spans now → immediate. | `src/scheduler.rs` | start < now < end | `AddRoute` emitted; Deactivate scheduled. |
| **Past Skipped** | Expired window → skipped. | `src/scheduler.rs` | end < now | No events, skipped count incremented. |
| **No Start** | Open-ended start → immediate. | `src/scheduler.rs` | start = None, end set | `AddRoute` emitted; Deactivate at end. |
| **No End** | Open-ended end → no deactivate. | `src/scheduler.rs` | start = None, end = None | `AddRoute` emitted; no Deactivate. |

### 3.17 Scheduler — Event Ordering (REQ-6.1)

*Objective: Verify correct temporal ordering of events.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Events Fire in Order** | Multiple contacts, ordered. | `src/scheduler.rs` | Two one-shot contacts | Events fire by timestamp. |
| **Deactivate Before Activate** | Same-time tie-breaking. | `src/scheduler.rs` | End of A = Start of B | Deactivate A fires before Activate B. |

### 3.18 Scheduler — Recurring Contacts (REQ-6.1, 6.2, 6.2a)

*Objective: Verify lazy recurrence expansion and re-scheduling.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Schedules Next Occurrence** | Future cron match. | `src/scheduler.rs` | Recurring, now before first match | Activate event at next cron match. |
| **Active at Startup** | Currently in active window. | `src/scheduler.rs` | prev_before(now) + duration > now | `AddRoute` emitted; Deactivate for remainder. |
| **Reschedules After Deactivate** | Next pair after window ends. | `src/scheduler.rs` | Process deactivate event | New Activate + Deactivate scheduled. |
| **Respects Until** | No rescheduling past until. | `src/scheduler.rs` | until < next occurrence | No further events after until. |

### 3.19 Scheduler — Replace and Diff (REQ-6.4)

*Objective: Verify content-based diffing for hot-reload.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Replace Computes Diff** | Add new, keep unchanged, remove old. | `src/scheduler.rs` | Replace with overlapping set | Correct added/removed/unchanged counts. |

### 3.20 Scheduler — Source Isolation and Cleanup

*Objective: Verify per-source contact management and crash cleanup.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Withdraw Removes All** | Source withdrawal. | `src/scheduler.rs` | Withdraw source | All contacts from source removed. |
| **Sources Are Isolated** | Cross-source independence. | `src/scheduler.rs` | Withdraw source A | Source B contacts unaffected. |

### 3.21 Scheduler — Route Refcounting

*Objective: Verify deduplication of identical routes from multiple sources.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Dedup Same Route** | Two sources, same route identity. | `src/scheduler.rs` | Two identical permanent contacts | Single `AddRoute`; `RemoveRoute` only after both withdrawn. |
| **Remove Matches by Content** | Content-based (not identity-based) matching. | `src/scheduler.rs` | Remove by contact value | Correct contact removed. |

### 3.22 Proto Conversion — Timestamps

*Objective: Verify conversion from `prost_types::Timestamp` to `time::OffsetDateTime`.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Valid UTC Timestamp** | Standard Unix epoch seconds. | `src/server.rs` | `{ seconds: 1774857600, nanos: 0 }` | `2026-03-27T08:00:00Z`. |
| **With Nanoseconds** | Sub-second precision preserved. | `src/server.rs` | `{ seconds: 0, nanos: 500_000_000 }` | 500ms after epoch. |
| **Negative Seconds** | Pre-epoch timestamp. | `src/server.rs` | `{ seconds: -1, nanos: 0 }` | `1969-12-31T23:59:59Z`. |
| **Out-of-Range** | Beyond representable range. | `src/server.rs` | `{ seconds: i64::MAX, nanos: 0 }` | `Err(InvalidArgument)`. |

### 3.23 Proto Conversion — Durations

*Objective: Verify conversion from `prost_types::Duration` to `std::time::Duration`.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Valid Duration** | Simple positive duration. | `src/server.rs` | `{ seconds: 90, nanos: 0 }` | 90s. |
| **With Nanoseconds** | Sub-second precision. | `src/server.rs` | `{ seconds: 1, nanos: 500_000_000 }` | 1.5s. |
| **Negative Seconds** | Reject negative seconds. | `src/server.rs` | `{ seconds: -1, nanos: 0 }` | `Err(InvalidArgument)`. |
| **Negative Nanos** | Reject negative nanos. | `src/server.rs` | `{ seconds: 0, nanos: -1 }` | `Err(InvalidArgument)`. |
| **Nanos Overflow** | Reject nanos > 999_999_999. | `src/server.rs` | `{ seconds: 0, nanos: 1_000_000_000 }` | `Err(InvalidArgument)`. |

### 3.24 Proto Conversion — Contacts

*Objective: Verify conversion from proto `Contact` messages to internal `Contact` structs.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Valid Via** | Forward action with EID. | `src/server.rs` | `{ eid_pattern: "ipn:2.*.*", via: "ipn:2.1.0" }` | `Action::Via(ipn:2.1.0)`. |
| **Valid Drop** | Drop with reason code. | `src/server.rs` | `{ eid_pattern: "ipn:2.*.*", drop: { reason_code: 6 } }` | `Action::Drop(Some(6))`. |
| **Drop Zero Reason** | Zero reason = no reason. | `src/server.rs` | `{ drop: { reason_code: 0 } }` | `Action::Drop(None)`. |
| **Missing Action** | No action oneof set. | `src/server.rs` | `{ eid_pattern: "ipn:2.*.*" }` | `Err(InvalidArgument)`. |
| **Invalid EID Pattern** | Malformed pattern string. | `src/server.rs` | `{ eid_pattern: "bad" }` | `Err(InvalidArgument)`. |
| **Invalid Next-Hop EID** | Malformed via EID. | `src/server.rs` | `{ eid_pattern: "ipn:2.*.*", via: "bad" }` | `Err(InvalidArgument)`. |
| **Permanent (No Schedule)** | No schedule oneof set. | `src/server.rs` | `{ ..., no schedule }` | `Schedule::Permanent`. |
| **OneShot** | Start and end timestamps. | `src/server.rs` | `{ ..., one_shot: { start, end } }` | `Schedule::OneShot { start, end }`. |
| **OneShot End Before Start** | Invalid window. | `src/server.rs` | `{ ..., one_shot: { start: t2, end: t1 } }` | `Err(InvalidArgument)`. |
| **Recurring** | Cron + duration. | `src/server.rs` | `{ ..., recurring: { cron, duration } }` | `Schedule::Recurring { ... }`. |
| **Recurring Invalid Cron** | Malformed cron expression. | `src/server.rs` | `{ ..., recurring: { cron: "bad" } }` | `Err(InvalidArgument)`. |
| **Recurring Missing Duration** | Cron without duration. | `src/server.rs` | `{ ..., recurring: { cron: "0 8 * * *" } }` | `Err(InvalidArgument)`. |
| **Recurring Zero Duration** | Duration = 0. | `src/server.rs` | `{ ..., recurring: { cron: "...", duration: 0 } }` | `Err(InvalidArgument)`. |
| **With Priority** | Priority override set. | `src/server.rs` | `{ ..., priority: 50 }` | `priority = Some(50)`. |
| **With Link Properties** | Bandwidth and delay. | `src/server.rs` | `{ ..., bandwidth_bps: 10G, delay_us: 500000 }` | Fields set. |

## 4. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-tvr`

* **Pass Criteria:** All tests listed above must return `ok`.

* **Coverage Target:** > 80% line coverage for `src/cron.rs`, `src/parser.rs`, and `src/scheduler.rs`.
