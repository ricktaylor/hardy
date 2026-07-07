# DTN Implementation Ping Benchmark

_Generated 2026-07-07 15:21:38 by `tests/interop/run_all.sh` — do not edit by hand._

| Implementation | Version | Min | Avg | Max | Stddev | Loss | Pings | vs Hardy |
|----------------|---------|-----|-----|-----|--------|------|-------|----------|
| Hardy | v0.2.0 | 1ms 392us 585ns | 1ms 820us | 5ms 564us 130ns | 614us | 0% | 100/100 | baseline |
| dtn7-rs | v0.21.0-20-g7e2ae44 | 41ms 294us 198ns | 43ms 220us | 46ms 506us 776ns | 1ms 202us | 0% | 100/100 | 2374% (slower) |
| HDTN | v2.0.0 | 4ms 996us 968ns | 42ms 763us | 45ms 864us 614ns | 4ms 2us | 0% | 100/100 | 2349% (slower) |
| DTNME | 7d8311f (declared: 1.3.2-2024-01-03) | 41ms 632us 658ns | 44ms 770us | 55ms 711us 889ns | 2ms 448us | 0% | 100/100 | 2459% (slower) |
| ION | ion-open-source-4.1.4 (declared: 4.1.4) | 2ms 106us 311ns | 2ms 829us | 6ms 267us 315ns | 547us | 0% | 100/100 | 155% (slower) |
| ud3tn | v0.15.0 | 42ms 768us 641ns | 45ms 194us | 49ms 700us 829ns | 1ms 317us | 0% | 100/100 | 2483% (slower) |
| NASA cFS | cFS=v7.0.1 bpnode=v7.0.5 bplib=v7.0.5 | 1ms 845us 471ns | 7ms 214us | 12ms 327us 573ns | 2ms 961us | 0% | 100/100 | 396% (slower) |
| ESA-BP | 1.2.6.BETA-BPSEC-943-gf59410a90 (declared: 3.0.0.v20260521) | 9ms 969us 301ns | 16ms 774us | 196ms 146us 56ns | 18ms 529us | 0% | 100/100 | 921% (slower) |

## Notes

- **Pings**: Received/Transmitted count
- **vs Hardy**: Percentage relative to Hardy baseline (100% = same, >100% = slower)
- Hardy, dtn7-rs, HDTN, DTNME use TCPCLv4; ud3tn uses MTCP; ION, ESA-BP, NASA cFS use STCP via client CLA
- All implementations, including the Hardy baseline, run in Docker on the pinned trixie base, so container/bridge overhead is common to every row
- The TCPCLv4 rows are directly comparable; MTCP/STCP rows carry an extra mtcp-cla bridge hop, so 'vs Hardy' reflects transport as well as implementation for those
