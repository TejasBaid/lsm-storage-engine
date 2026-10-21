db_bench: 1000000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| lsm-engine | fillseq | 391441 | 45.4 | 2.55 | 1.29 | 3.50 | 11.58 | settle 0.00s; WA 1.97x; L1=5 L2=50 |
| lsm-engine | fillrandom | 277807 | 32.2 | 3.60 | 1.67 | 4.46 | 195.04 | settle 0.26s; WA 5.56x; L1=7 L2=51 L3=1 |
| lsm-engine | overwrite | 226900 | 26.3 | 4.41 | 1.96 | 5.00 | 286.42 | settle 0.14s; WA 7.33x; L0=1 L1=5 L2=51 L3=9 |
| lsm-engine | readrandom | 561753 | 65.2 | 1.78 | 1.67 | 3.54 | 5.92 | 1000000 of 1000000 found |
| lsm-engine | readmissing | 5864183 | 93.8 | 0.17 | 0.17 | 0.17 | 0.21 | 0 found (expect 0) |
| lsm-engine | seekrandom | 73307 | 850.4 | 13.64 | 13.21 | 23.33 | 28.33 | seek + 100 nexts per op (999995 entries) |
| lsm-engine | readseq | 12960048 | 1503.4 | 0.08 | – | – | – | full scan |
