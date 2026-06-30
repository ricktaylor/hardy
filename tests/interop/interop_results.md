# DTN Implementation Ping Benchmark

_Generated 2026-06-29 16:17:21 by `tests/interop/run_all.sh` — do not edit by hand._

| Implementation | Version | Min | Avg | Max | Stddev | Loss | Pings | vs Hardy |
|----------------|---------|-----|-----|-----|--------|------|-------|----------|
| Hardy | v0.1.0-245-ga014ac3c | 1ms 94us 269ns | 1ms 458us | 5ms 289us 336ns | 539us | 0% | 100/100 | baseline |
| dtn7-rs | v0.21.0-20-g7e2ae44 | 3ms 697us 968ns | 43ms 264us | 48ms 667us 931ns | 4ms 257us | 0% | 100/100 | 2967% (slower) |
| HDTN | v2.0.0 | 3ms 19us 727ns | 43ms 5us | 49ms 219us 581ns | 4ms 295us | 0% | 100/100 | 2949% (slower) |
| DTNME | 7d8311f (declared: 1.3.2-2024-01-03) | 41ms 492us 972ns | 43ms 989us | 48ms 911us 306ns | 1ms 705us | 0% | 100/100 | 3017% (slower) |
| ION | ion-open-source-4.1.4 (declared: 4.1.4) | 2ms 186us 705ns | 3ms 248us | 9ms 23us 728ns | 1ms 162us | 0% | 100/100 | 222% (slower) |
| ud3tn | v0.15.0 | 42ms 598us 232ns | 45ms 533us | 49ms 621us 261ns | 1ms 549us | 0% | 100/100 | 3122% (slower) |
| NASA cFS | cFS=v7.0.1 bpnode=v7.0.5 bplib=v7.0.5 | 1ms 686us 146ns | 7ms 132us | 11ms 889us 148ns | 2ms 915us | 0% | 100/100 | 489% (slower) |
| ESA-BP | 1.2.6.BETA-BPSEC-943-gf59410a90-dirty (declared: 3.0.0.v20260521) | 9ms 910us 111ns | 17ms 397us | 308ms 261us 81ns | 29ms 455us | 0% | 100/100 | 1193% (slower) |

## Notes

- **Pings**: Received/Transmitted count
- **vs Hardy**: Percentage relative to Hardy baseline (100% = same, >100% = slower)
- Hardy, dtn7-rs, HDTN, DTNME use TCPCLv4; ud3tn uses MTCP; ION, ESA-BP, NASA cFS use STCP via client CLA
- Hardy baseline runs inline; other tests use existing interop scripts
