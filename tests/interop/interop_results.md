# DTN Implementation Ping Benchmark

_Generated 2026-07-02 17:11:05 by `tests/interop/run_all.sh` — do not edit by hand._

| Implementation | Version | Min | Avg | Max | Stddev | Loss | Pings | vs Hardy |
|----------------|---------|-----|-----|-----|--------|------|-------|----------|
| Hardy | v0.1.0-250-g611d3ff5-dirty | 1ms 463us 10ns | 2ms 430us | 5ms 606us 247ns | 1ms 64us | 0% | 20/20 | baseline |
| dtn7-rs | v0.21.0-20-g7e2ae44 | 42ms 4us 650ns | 44ms 94us | 48ms 507us 146ns | 1ms 628us | 0% | 20/20 | 1814% (slower) |
| HDTN | v2.0.0 | 5ms 814us 22ns | 41ms 895us | 48ms 560us 645ns | 8ms 511us | 0% | 20/20 | 1724% (slower) |
| DTNME | 7d8311f (declared: 1.3.2-2024-01-03) | 41ms 824us 547ns | 44ms 740us | 49ms 546us 901ns | 2ms 465us | 0% | 20/20 | 1841% (slower) |
| ION | ion-open-source-4.1.4 (declared: 4.1.4) | 2ms 227us 532ns | 3ms 144us | 8ms 534us 910ns | 1ms 323us | 0% | 20/20 | 129% (slower) |
| ud3tn | v0.15.0 | 43ms 716us 92ns | 46ms 117us | 48ms 543us 579ns | 1ms 398us | 0% | 20/20 | 1897% (slower) |
| NASA cFS | cFS=v7.0.1 bpnode=v7.0.5 bplib=v7.0.5 | 1ms 770us 988ns | 6ms 936us | 11ms 629us 545ns | 2ms 879us | 0% | 20/20 | 285% (slower) |
| ESA-BP | 1.2.6.BETA-BPSEC-943-gf59410a90 (declared: 3.0.0.v20260521) | 13ms 363us 30ns | 26ms 302us | 188ms 85us 865ns | 37ms 208us | 0% | 20/20 | 1082% (slower) |

## Notes

- **Pings**: Received/Transmitted count
- **vs Hardy**: Percentage relative to Hardy baseline (100% = same, >100% = slower)
- Hardy, dtn7-rs, HDTN, DTNME use TCPCLv4; ud3tn uses MTCP; ION, ESA-BP, NASA cFS use STCP via client CLA
- All implementations, including the Hardy baseline, run in Docker on the pinned trixie base, so container/bridge overhead is common to every row
- The TCPCLv4 rows are directly comparable; MTCP/STCP rows carry an extra mtcp-cla bridge hop, so 'vs Hardy' reflects transport as well as implementation for those
