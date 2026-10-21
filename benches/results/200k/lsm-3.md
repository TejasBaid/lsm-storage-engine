db_bench: 200000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| lsm-engine | fillseq | 448299 | 52.0 | 2.23 | 1.25 | 3.38 | 9.46 | settle 0.00s; WA 1.79x; L1=5 L2=5 |
| lsm-engine | fillrandom | 441818 | 51.3 | 2.26 | 1.46 | 3.58 | 8.58 | settle 0.03s; WA 2.24x; L1=5 L2=5 |
| lsm-engine | overwrite | 373239 | 43.3 | 2.68 | 1.50 | 3.71 | 12.71 | settle 0.00s; WA 2.72x; L0=1 L1=5 L2=10 |
| lsm-engine | readrandom | 1030402 | 119.5 | 0.97 | 0.92 | 2.71 | 4.88 | 200000 of 200000 found |
| lsm-engine | readmissing | 5881229 | 94.1 | 0.17 | 0.17 | 0.17 | 0.21 | 0 found (expect 0) |
| lsm-engine | seekrandom | 83387 | 967.3 | 11.99 | 12.12 | 15.50 | 19.79 | seek + 100 nexts per op (199927 entries) |
| lsm-engine | readseq | 14467723 | 1678.3 | 0.07 | – | – | – | full scan |
