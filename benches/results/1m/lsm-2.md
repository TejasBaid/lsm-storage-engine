db_bench: 1000000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| lsm-engine | fillseq | 388128 | 45.0 | 2.58 | 1.29 | 3.71 | 20.25 | settle 0.00s; WA 1.97x; L1=5 L2=50 |
| lsm-engine | fillrandom | 276013 | 32.0 | 3.62 | 1.71 | 4.54 | 221.25 | settle 0.26s; WA 5.56x; L1=7 L2=51 L3=1 |
| lsm-engine | overwrite | 226534 | 26.3 | 4.41 | 1.96 | 5.12 | 289.33 | settle 0.15s; WA 7.33x; L0=1 L1=5 L2=51 L3=9 |
| lsm-engine | readrandom | 573555 | 66.5 | 1.74 | 1.58 | 3.33 | 5.25 | 1000000 of 1000000 found |
| lsm-engine | readmissing | 5807580 | 92.9 | 0.17 | 0.17 | 0.17 | 0.21 | 0 found (expect 0) |
| lsm-engine | seekrandom | 72529 | 841.3 | 13.79 | 13.38 | 23.46 | 27.00 | seek + 100 nexts per op (999995 entries) |
| lsm-engine | readseq | 12690563 | 1472.1 | 0.08 | – | – | – | full scan |
