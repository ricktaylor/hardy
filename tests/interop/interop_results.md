# DTN Implementation Ping Benchmark

_Generated 2026-07-03 11:57:28 by `tests/interop/run_all.sh` — do not edit by hand._

| Implementation | Version | Min | Avg | Max | Stddev | Loss | Pings | vs Hardy |
|----------------|---------|-----|-----|-----|--------|------|-------|----------|
| Hardy | v0.1.0-262-g6e31c12e | 1ms 371us 502ns | 1ms 784us | 5ms 424us 124ns | 599us | 0% | 100/100 | baseline |
| dtn7-rs | v0.21.0-20-g7e2ae44 | 41ms 93us 767ns | 43ms 244us | 46ms 430us 171ns | 1ms 196us | 0% | 100/100 | 2423% (slower) |
| HDTN | v2.0.0 | 3ms 730us 35ns | 43ms 114us | 47ms 532us 319ns | 4ms 196us | 0% | 100/100 | 2416% (slower) |
| DTNME | 7d8311f (declared: 1.3.2-2024-01-03) | 41ms 549us 145ns | 44ms 204us | 48ms 647us 402ns | 1ms 442us | 0% | 100/100 | 2477% (slower) |
| ION | ion-open-source-4.1.4 (declared: 4.1.4) | 2ms 111us 648ns | 2ms 840us | 8ms 848us 482ns | 728us | 0% | 100/100 | 159% (slower) |
| ud3tn | v0.15.0 | 43ms 137us 692ns | 45ms 330us | 48ms 228us 242ns | 1ms 259us | 0% | 100/100 | 2540% (slower) |
| NASA cFS | cFS=v7.0.1 bpnode=v7.0.5 bplib=v7.0.5 | 2ms 60us 93ns | 7ms 230us | 12ms 891us 109ns | 2ms 924us | 0% | 100/100 | 405% (slower) |
| ESA-BP | 1.2.6.BETA-BPSEC-943-gf59410a90 (declared: 3.0.0.v20260521) | 9ms 833us 574ns | 16ms 212us | 217ms 124us 819ns | 20ms 461us | 0% | 100/100 | 908% (slower) |

## Notes

- **Pings**: Received/Transmitted count
- **vs Hardy**: Percentage relative to Hardy baseline (100% = same, >100% = slower)
- Hardy, dtn7-rs, HDTN, DTNME use TCPCLv4; ud3tn uses MTCP; ION, ESA-BP, NASA cFS use STCP via client CLA
- All implementations, including the Hardy baseline, run in Docker on the pinned trixie base, so container/bridge overhead is common to every row
- The TCPCLv4 rows are directly comparable; MTCP/STCP rows carry an extra mtcp-cla bridge hop, so 'vs Hardy' reflects transport as well as implementation for those
