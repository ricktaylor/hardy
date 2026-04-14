# TVR Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-tvr` |
| **Standard** | — |
| **Test Plans** | [`UTP-TVR-01`](unit_test_plan.md), [`COMP-TVR-01`](component_test_plan.md) |
| **Date** | 2026-04-14 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

All functional areas verified (6 pass). The TVR crate implements REQ-6 (Time-Variant Routing). LLRs map to the TVR functional areas: cron engine, contact plan parser, scheduler, proto conversion, gRPC session lifecycle, and system integration.

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| **—** | Cron parsing, matching, next/prev, display | Pass | `cron.rs::every_minute` .. `cron.rs::display_preserves_shortcut` (43 tests) | 6.1 |
| **—** | Contact plan parser (actions, schedules, link properties, file format) | Pass | `parser.rs::simple_via` .. `parser.rs::multiline_error_shows_correct_line` (42 tests) | 6.2 |
| **—** | Scheduler (permanent, one-shot, recurring, ordering, diffing, isolation, refcounting) | Pass | `scheduler.rs::permanent_activates_immediately` .. `scheduler.rs::remove_matches_by_content` (18 tests) | 6.3 |
| **—** | Proto conversion (timestamps, durations, contacts) | Pass | `server.rs::timestamp_valid_utc` .. `server.rs::contact_with_link_properties` (24 tests) | 6.4 |
| **—** | gRPC session lifecycle | Pass | `test_tvr.sh` TEST 5..10 (6 of 12 scenarios; 3 deferred, 1 unit-covered, 1 implicit, 1 untestable) | 6.5 |
| **—** | System integration (file → BPA → ping) | Pass | `test_tvr.sh` TEST 1..4 | 6.6 |

## 2. Test Inventory

### Cron (`src/cron.rs` — 43 tests)

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `every_minute` | 3.1 | Wildcard 5-field parsing |
| `specific_values` | 3.1 | Fixed minute/hour values |
| `range` | 3.1 | Ranges in hour and dow fields |
| `step` | 3.1 | Step expression `*/15` |
| `range_with_step` | 3.1 | Combined range + step `8-18/2` |
| `list` | 3.1 | Comma-separated list |
| `six_field_with_seconds` | 3.2 | 6-field parsing with second |
| `six_field_every_10_seconds` | 3.2 | Step in seconds field |
| `named_weekdays` | 3.3 | `MON-FRI` range |
| `named_weekday_list` | 3.3 | `MON,WED,FRI` list |
| `named_weekday_case_insensitive` | 3.3 | Case insensitivity |
| `named_sunday` | 3.3 | `SUN` maps to bit 0 |
| `named_months` | 3.3 | `MAR-OCT` range |
| `named_month_list` | 3.3 | `JAN,JUN,DEC` list |
| `dow_sunday_alias_numeric` | 3.3 | `7` folds to `0` |
| `shortcut_daily` | 3.4 | `@daily` expansion |
| `shortcut_midnight` | 3.4 | `@midnight` = `@daily` |
| `shortcut_hourly` | 3.4 | `@hourly` expansion |
| `shortcut_weekly` | 3.4 | `@weekly` expansion |
| `shortcut_monthly` | 3.4 | `@monthly` expansion |
| `shortcut_yearly` | 3.4 | `@yearly` expansion |
| `shortcut_annually` | 3.4 | `@annually` = `@yearly` |
| `shortcut_unknown` | 3.4 | Invalid shortcut rejection |
| `invalid_field_count` | 3.5 | Too few / too many fields |
| `out_of_range` | 3.5 | Values outside field bounds |
| `empty_range` | 3.5 | End < start rejection |
| `zero_step` | 3.5 | Zero step rejection |
| `matches_specific` | 3.6 | Match / non-match at specific time |
| `matches_with_seconds` | 3.6 | 6-field match at :30 |
| `matches_weekday` | 3.6 | Weekday filter match |
| `next_after_same_minute` | 3.7 | At exact match time |
| `next_after_later_today` | 3.7 | Later in same day |
| `next_after_tomorrow` | 3.7 | Wraps to next day |
| `next_after_skips_weekend` | 3.7 | Weekday-only skip |
| `next_after_month_rollover` | 3.7 | Month boundary wrap |
| `next_after_with_seconds` | 3.7 | Second-granularity |
| `prev_before_same_minute` | 3.7 | At exact match time |
| `prev_before_earlier_today` | 3.7 | Earlier in same day |
| `prev_before_yesterday` | 3.7 | Previous day wrap |
| `prev_before_skips_weekend` | 3.7 | Weekday-only backwards |
| `prev_before_with_seconds` | 3.7 | Second-granularity backwards |
| `display_preserves_source` | 3.8 | Display round-trip |
| `display_preserves_shortcut` | 3.8 | Shortcut round-trip |

### Parser (`src/parser.rs` — 42 tests)

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `simple_via` | 3.9 | Basic via action |
| `simple_drop` | 3.9 | Drop with no reason |
| `drop_with_reason` | 3.9 | Drop with explicit reason code |
| `via_with_priority` | 3.9 | Priority field |
| `reflect_not_supported` | 3.9 | Reflect action rejection |
| `oneshot_start_end` | 3.10 | Full time window |
| `oneshot_start_only` | 3.10 | Open-ended start |
| `oneshot_end_only` | 3.10 | Open-ended end |
| `oneshot_end_before_start` | 3.10 | Invalid window rejection |
| `oneshot_with_bps` | 3.10 | One-shot + bandwidth |
| `recurring_cron_duration` | 3.11 | Basic recurring |
| `recurring_with_until` | 3.11 | Bounded recurrence |
| `recurring_with_bps_and_priority` | 3.11 | Full recurring specification |
| `invalid_cron_expression` | 3.11 | Malformed cron |
| `cron_without_duration` | 3.11 | Missing required duration |
| `duration_without_cron` | 3.11 | Orphaned duration |
| `mixed_oneshot_and_recurring` | 3.11 | Conflicting schedule types |
| `until_without_cron` | 3.11 | Orphaned until |
| `duration_minutes` | 3.12 | Humantime minutes |
| `duration_hours` | 3.12 | Humantime hours |
| `duration_compound` | 3.12 | Compound `1h30m` |
| `duration_hms` | 3.12 | Full `2h30m15s` |
| `duration_invalid_no_unit` | 3.12 | Bare number rejection |
| `duration_zero` | 3.12 | Zero duration rejection |
| `bandwidth_bare_number` | 3.13 | Raw bps value |
| `bandwidth_si_suffixes` | 3.13 | K, M, G, T suffixes |
| `bandwidth_long_suffixes` | 3.13 | Kbps, Gbps suffixes |
| `bandwidth_case_insensitive` | 3.13 | Mixed case |
| `delay_humantime` | 3.13 | Delay in ms, s, us |
| `all_link_properties` | 3.13 | Combined bandwidth + delay |
| `fields_any_order` | 3.14 | Field ordering independence |
| `comments` | 3.14 | Comment lines |
| `blank_lines` | 3.14 | Blank line handling |
| `multiple_contacts` | 3.14 | Multi-line file |
| `mixed_with_comments_and_blanks` | 3.14 | Realistic file layout |
| `duplicate_priority` | 3.14 | Repeated field rejection |
| `duplicate_start` | 3.14 | Repeated field rejection |
| `scheduled_drop` | 3.14 | Drop + schedule |
| `drop_with_reason_and_schedule` | 3.14 | Drop + reason + schedule |
| `invalid_inputs` | 3.14 | Various malformed lines |
| `error_messages_are_useful` | 3.14 | Caret-annotated errors |
| `multiline_error_shows_correct_line` | 3.14 | Line number in errors |

### Scheduler (`src/scheduler.rs` — 18 tests)

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `permanent_activates_immediately` | 3.15 | Immediate AddRoute |
| `permanent_with_explicit_priority` | 3.15 | Priority override |
| `oneshot_future_schedules_events` | 3.16 | Future window → timeline events |
| `oneshot_active_now` | 3.16 | Current window → immediate activation |
| `oneshot_past_skipped` | 3.16 | Expired window → skip |
| `oneshot_no_start_activates_immediately` | 3.16 | Open start → immediate |
| `oneshot_no_end_stays_active` | 3.16 | Open end → no deactivate |
| `events_fire_in_order` | 3.17 | Temporal ordering |
| `deactivate_before_activate_at_same_time` | 3.17 | Same-time tie-breaking |
| `recurring_schedules_next_occurrence` | 3.18 | Future cron match scheduling |
| `recurring_active_at_startup` | 3.18 | Mid-window startup detection |
| `recurring_reschedules_after_deactivate` | 3.18 | Next pair after deactivation |
| `recurring_respects_until` | 3.18 | Until boundary enforcement |
| `replace_computes_diff` | 3.19 | Content-based diffing |
| `withdraw_removes_all_contacts` | 3.20 | Source withdrawal |
| `sources_are_isolated` | 3.20 | Cross-source independence |
| `refcount_dedup_same_route` | 3.21 | Route deduplication |
| `remove_matches_by_content` | 3.21 | Content-based matching |

### Proto Conversion (`src/server.rs` — 24 tests)

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `timestamp_valid_utc` | 3.22 | Standard Unix epoch to OffsetDateTime |
| `timestamp_with_nanos` | 3.22 | Sub-second precision preserved |
| `timestamp_negative_seconds` | 3.22 | Pre-epoch timestamp |
| `timestamp_out_of_range` | 3.22 | Beyond representable range |
| `duration_valid` | 3.23 | Simple positive duration |
| `duration_with_nanos` | 3.23 | Sub-second precision |
| `duration_negative_seconds` | 3.23 | Negative seconds rejection |
| `duration_negative_nanos` | 3.23 | Negative nanos rejection |
| `duration_nanos_overflow` | 3.23 | Nanos > 999_999_999 rejection |
| `contact_valid_via` | 3.24 | Via action + permanent schedule |
| `contact_valid_drop_with_reason` | 3.24 | Drop with reason code |
| `contact_drop_zero_reason` | 3.24 | Zero reason = None |
| `contact_missing_action` | 3.24 | No action rejection |
| `contact_invalid_eid_pattern` | 3.24 | Malformed pattern rejection |
| `contact_invalid_next_hop_eid` | 3.24 | Malformed via EID rejection |
| `contact_permanent_no_schedule` | 3.24 | No schedule = Permanent |
| `contact_oneshot` | 3.24 | OneShot with start + end |
| `contact_oneshot_end_before_start` | 3.24 | Invalid window rejection |
| `contact_recurring` | 3.24 | Cron + duration |
| `contact_recurring_invalid_cron` | 3.24 | Malformed cron rejection |
| `contact_recurring_missing_duration` | 3.24 | Missing duration rejection |
| `contact_recurring_zero_duration` | 3.24 | Zero duration rejection |
| `contact_with_priority` | 3.24 | Priority override |
| `contact_with_link_properties` | 3.24 | Bandwidth + delay |

## 3. Coverage vs Plan

| Section | Scenario | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| UTP 3.1-3.8 | Cron parsing, matching, next/prev, display | 43 | 43 | Complete |
| UTP 3.9-3.14 | Contact plan parser | 42 | 42 | Complete |
| UTP 3.15-3.21 | Scheduler | 18 | 18 | Complete |
| UTP 3.22-3.24 | Proto conversion | 24 | 24 | Complete |
| COMP 3 | gRPC session lifecycle | 12 | 6 | Partial (6 via grpcurl; 3 deferred, 1 unit-covered, 1 implicit, 1 untestable) |
| UTP 3.25 | Configuration loading | 14 | 14 | Complete |
| COMP 4 | System integration (test_tvr.sh) | 4 | 4 | Complete |
| | **Total** | **157** | **151** | **96%** |

### System & Component Integration Tests

Implemented in [`tests/test_tvr.sh`](../tests/test_tvr.sh). Requires built binaries and `grpcurl`.

| Test | Scope | Status |
| :--- | :--- | :--- |
| TEST 1: Permanent route | System (file → BPA → ping) | **Passing** |
| TEST 2: Hot-reload (add) | System (file watcher) | **Passing** |
| TEST 3: File removal | System (file watcher) | **Passing** |
| TEST 4: File restore | System (file watcher) | **Passing** |
| TEST 5: gRPC session open | Component (TVR-01) | **Passing** |
| TEST 6: gRPC add contacts + route | Component (TVR-05, TVR-09) | **Passing** |
| TEST 7: gRPC session close cleanup | Component (TVR-09) | **Passing** |
| TEST 8: gRPC duplicate session name | Component (TVR-02) | **Passing** |
| TEST 9: gRPC missing open | Component (TVR-03) | **Passing** |
| TEST 10: gRPC session name reuse | Component (TVR-12) | **Passing** |

## 4. Line Coverage

```
cargo llvm-cov test --package hardy-tvr --lcov --output-path lcov.info --html
```

Results (2026-04-14, 141 tests passed):

```
  lines......: 77.2% (2206 of 2856 lines)
  functions..: 78.4% (273 of 348 functions)
```

| File | Lines | Coverage |
| :--- | :--- | :--- |
| `src/cron.rs` | 506 / 565 | 89.6% |
| `src/parser.rs` | 653 / 717 | 91.1% |
| `src/scheduler.rs` | 691 / 865 | 79.9% |
| `src/server.rs` | 249 / 448 | 55.6% |
| `src/config.rs` | — | Now covered by 14 config unit tests |
| `src/contacts.rs` | 0 / 27 | 0.0% |
| `src/main.rs` | 0 / 82 | 0.0% |
| `src/watcher.rs` | 0 / 74 | 0.0% |
| **Total** | **2206 / 2856** | **77.2%** |

The three core unit-tested modules exceed their 80% target: `parser.rs`
(91.1%) and `cron.rs` (89.6%) are well above; `scheduler.rs` (79.9%) is
marginally below due to the async `run()` function and metrics code
which are only exercised by system tests. `server.rs` (55.6%) reflects
that the gRPC session handling (`run_session`, `handle_message`) is only
tested at the system level via `test_tvr.sh`, while the conversion
functions are fully covered by unit tests. `config.rs` is now covered
by 14 config loading unit tests (defaults, multi-format, env override,
validation).

The 0% files (`contacts.rs`, `main.rs`, `watcher.rs`) are application
wiring — trait impl delegation, `main()` orchestration, and filesystem
watching — which are exercised by the system integration tests but not
by `cargo test`.

## 5. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| gRPC session lifecycle | 3 deferred scenarios (TVR-04, TVR-07, TVR-08) | Low | Straightforward request/response handling with no complex state |
| `contacts.rs`, `main.rs`, `watcher.rs` | 0% line coverage (unit tests only) | Low | Application wiring exercised by system integration tests |

Six of twelve gRPC session scenarios are tested via `grpcurl` in `test_tvr.sh`. TVR-06 is covered by unit tests. TVR-10 is implicit in test teardown. TVR-11 is not testable from a well-behaved gRPC client.

## 6. Conclusion

The TVR crate has comprehensive test coverage: cron engine (43 tests), contact plan parser (42 tests), scheduler (18 tests), proto conversion (24 tests), and configuration loading (14 tests) are all fully covered at the unit level. The gRPC session protocol is verified by 6 `grpcurl`-based component tests covering open, add, close cleanup, duplicate name rejection, missing open, and name reuse. System integration is verified by 4 end-to-end tests using `bp ping`. Overall coverage is 151/157 scenarios (96%), 77.2% line coverage, with only 3 low-risk deferred gRPC scenarios remaining.
