db_bench: 200000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| lsm-engine | fillseq | 441076 | 51.2 | 2.27 | 1.25 | 3.42 | 8.54 | settle 0.00s; WA 1.79x; L1=5 L2=5 |
| lsm-engine | fillrandom | 436974 | 50.7 | 2.29 | 1.46 | 3.62 | 8.08 | settle 0.04s; WA 2.24x; L1=5 L2=5 |
| lsm-engine | overwrite | 363542 | 42.2 | 2.75 | 1.50 | 3.79 | 13.58 | settle 0.00s; WA 2.72x; L0=1 L1=5 L2=10 |
| lsm-engine | readrandom | 1005439 | 116.6 | 0.99 | 0.92 | 2.75 | 5.00 | 200000 of 200000 found |
| lsm-engine | readmissing | 5719099 | 91.5 | 0.17 | 0.17 | 0.17 | 0.25 | 0 found (expect 0) |
| lsm-engine | seekrandom | 82064 | 951.9 | 12.19 | 12.29 | 15.83 | 21.08 | seek + 100 nexts per op (199927 entries) |
| lsm-engine | readseq | 14628973 | 1697.0 | 0.07 | – | – | – | full scan |
