# DTN Implementation Ping Benchmark

_Generated 2026-07-03 11:01:35 by `tests/interop/run_all.sh` — do not edit by hand._

| Implementation | Version | Min | Avg | Max | Stddev | Loss | Pings | vs Hardy |
|----------------|---------|-----|-----|-----|--------|------|-------|----------|
| Hardy | v0.1.0-253-g48800e8c-dirty | 1ms 467us 368ns | 3ms 908us | 11ms 998us 495ns | 2ms 898us | 0% | 20/20 | baseline |
| dtn7-rs | v0.21.0-20-g7e2ae44 | 41ms 642us 731ns | 43ms 237us | 45ms 13us 453ns | 1ms 136us | 0% | 20/20 | 1106% (slower) |
| HDTN | v2.0.0 | 4ms 887us 798ns | 41ms 980us | 47ms 903us 884ns | 8ms 639us | 0% | 20/20 | 1074% (slower) |
| DTNME | 7d8311f (declared: 1.3.2-2024-01-03) | 42ms 68us 242ns | 46ms 602us | 55ms 558us 80ns | 3ms 570us | 0% | 20/20 | 1192% (slower) |
| ION | ion-open-source-4.1.4 (declared: 4.1.4) | 2ms 271us 829ns | 3ms 303us | 7ms 729us 280ns | 1ms 195us | 0% | 20/20 | 84% (faster) |
| ud3tn | v0.15.0 | 44ms 454us 296ns | 46ms 208us | 53ms 35us 535ns | 1ms 965us | 0% | 20/20 | 1182% (slower) |
| NASA cFS | cFS=v7.0.1 bpnode=v7.0.5 bplib=v7.0.5 | 1ms 672us 660ns | 6ms 939us | 11ms 673us 919ns | 3ms 55us | 0% | 20/20 | 177% (slower) |
| ESA-BP | 1.2.6.BETA-BPSEC-943-gf59410a90 (declared: 3.0.0.v20260521) | 14ms 955us 421ns | 31ms 435us | 211ms 99us 188ns | 41ms 708us | 0% | 20/20 | 804% (slower) |

## Notes

- **Pings**: Received/Transmitted count
- **vs Hardy**: Percentage relative to Hardy baseline (100% = same, >100% = slower)
- Hardy, dtn7-rs, HDTN, DTNME use TCPCLv4; ud3tn uses MTCP; ION, ESA-BP, NASA cFS use STCP via client CLA
- All implementations, including the Hardy baseline, run in Docker on the pinned trixie base, so container/bridge overhead is common to every row
- The TCPCLv4 rows are directly comparable; MTCP/STCP rows carry an extra mtcp-cla bridge hop, so 'vs Hardy' reflects transport as well as implementation for those
