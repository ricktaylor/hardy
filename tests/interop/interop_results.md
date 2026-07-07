# DTN Implementation Ping Benchmark

_Generated 2026-07-07 09:53:28 by `tests/interop/run_all.sh` — do not edit by hand._

| Implementation | Version | Min | Avg | Max | Stddev | Loss | Pings | vs Hardy |
|----------------|---------|-----|-----|-----|--------|------|-------|----------|
| Hardy | v0.1.0-266-g94150fdf-dirty | 1ms 523us 330ns | 2ms 358us | 5ms 631us 392ns | 1ms 113us | 0% | 20/20 | baseline |
| dtn7-rs | v0.21.0-20-g7e2ae44 | 41ms 596us 274ns | 43ms 667us | 46ms 779us 86ns | 1ms 338us | 0% | 20/20 | 1851% (slower) |
| HDTN | v2.0.0 | 4ms 759us 329ns | 41ms 576us | 45ms 310us 945ns | 8ms 528us | 0% | 20/20 | 1763% (slower) |
| DTNME | 7d8311f (declared: 1.3.2-2024-01-03) | 41ms 618us 679ns | 44ms 188us | 48ms 875us 508ns | 1ms 566us | 0% | 20/20 | 1873% (slower) |
| ION | ion-open-source-4.1.4 (declared: 4.1.4) | 2ms 92us 108ns | 4ms 134us | 11ms 517us 302ns | 2ms 342us | 0% | 20/20 | 175% (slower) |
| ud3tn | v0.15.0 | 43ms 311us 461ns | 45ms 701us | 50ms 645us 10ns | 2ms 8us | 0% | 20/20 | 1938% (slower) |
| NASA cFS | cFS=v7.0.1 bpnode=v7.0.5 bplib=v7.0.5 | 2ms 332us 648ns | 6ms 959us | 12ms 784us 431ns | 3ms 62us | 0% | 20/20 | 295% (slower) |
| ESA-BP | 1.2.6.BETA-BPSEC-943-gf59410a90 (declared: 3.0.0.v20260521) | 14ms 937us 274ns | 28ms 3us | 210ms 964us 974ns | 42ms 49us | 0% | 20/20 | 1187% (slower) |

## Notes

- **Pings**: Received/Transmitted count
- **vs Hardy**: Percentage relative to Hardy baseline (100% = same, >100% = slower)
- Hardy, dtn7-rs, HDTN, DTNME use TCPCLv4; ud3tn uses MTCP; ION, ESA-BP, NASA cFS use STCP via client CLA
- All implementations, including the Hardy baseline, run in Docker on the pinned trixie base, so container/bridge overhead is common to every row
- The TCPCLv4 rows are directly comparable; MTCP/STCP rows carry an extra mtcp-cla bridge hop, so 'vs Hardy' reflects transport as well as implementation for those
