# DTN Implementation Ping Benchmark

_Generated 2026-06-29 15:31:16 by `tests/interop/run_all.sh` — do not edit by hand._

| Implementation | Version | Min | Avg | Max | Stddev | Loss | Pings | vs Hardy |
|----------------|---------|-----|-----|-----|--------|------|-------|----------|
| Hardy | v0.1.0-245-g81e1a420-dirty | 1ms 210us 953ns | 2ms 110us | 4ms 367us 565ns | 899us | 0% | 20/20 | baseline |
| dtn7-rs | v0.21.0-20-g7e2ae44 | 41ms 592us 630ns | 43ms 431us | 46ms 245us 687ns | 1ms 372us | 0% | 20/20 | 2058% (slower) |
| HDTN | v2.0.0 | 3ms 181us 802ns | 41ms 477us | 45ms 331us 601ns | 8ms 861us | 0% | 20/20 | 1965% (slower) |
| DTNME | 7d8311f (declared: 1.3.2-2024-01-03) | 41ms 519us 365ns | 44ms 614us | 51ms 882us 761ns | 2ms 560us | 0% | 20/20 | 2114% (slower) |
| ION | ion-open-source-4.1.4 (declared: 4.1.4) | 1ms 895us 207ns | 2ms 999us | 7ms 539us 463ns | 1ms 147us | 0% | 20/20 | 142% (slower) |
| ud3tn | v0.15.0 | 43ms 632us 86ns | 45ms 709us | 47ms 628us 730ns | 1ms 185us | 0% | 20/20 | 2166% (slower) |
| NASA cFS | cFS=v7.0.1 bpnode=v7.0.5 bplib=v7.0.5 | 2ms 402us 15ns | 6ms 594us | 12ms 124us 661ns | 2ms 863us | 0% | 20/20 | 312% (slower) |
| ESA-BP | 1.2.6.BETA-BPSEC-943-gf59410a90-dirty (declared: 3.0.0.v20260521) | 14ms 3us 617ns | 32ms 194us | 281ms 650us 424ns | 57ms 325us | 0% | 20/20 | 1525% (slower) |

## Notes

- **Pings**: Received/Transmitted count
- **vs Hardy**: Percentage relative to Hardy baseline (100% = same, >100% = slower)
- Hardy, dtn7-rs, HDTN, DTNME use TCPCLv4; ud3tn uses MTCP; ION, ESA-BP, NASA cFS use STCP via client CLA
- Hardy baseline runs inline; other tests use existing interop scripts
