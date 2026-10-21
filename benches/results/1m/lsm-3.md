db_bench: 1000000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| lsm-engine | fillseq | 388893 | 45.1 | 2.57 | 1.29 | 3.50 | 12.46 | settle 0.00s; WA 1.97x; L1=5 L2=50 |
| lsm-engine | fillrandom | 280243 | 32.5 | 3.57 | 1.67 | 4.46 | 187.46 | settle 0.26s; WA 5.56x; L1=7 L2=51 L3=1 |
| lsm-engine | overwrite | 224496 | 26.0 | 4.45 | 2.00 | 5.12 | 284.92 | settle 0.15s; WA 7.33x; L0=1 L1=5 L2=51 L3=9 |
| lsm-engine | readrandom | 567966 | 65.9 | 1.76 | 1.58 | 3.42 | 5.46 | 1000000 of 1000000 found |
| lsm-engine | readmissing | 5591852 | 89.5 | 0.18 | 0.17 | 0.17 | 0.21 | 0 found (expect 0) |
| lsm-engine | seekrandom | 73683 | 854.7 | 13.57 | 13.12 | 23.25 | 28.00 | seek + 100 nexts per op (999995 entries) |
| lsm-engine | readseq | 12814441 | 1486.5 | 0.08 | – | – | – | full scan |
