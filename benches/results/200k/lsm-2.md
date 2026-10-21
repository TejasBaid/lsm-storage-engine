db_bench: 200000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| lsm-engine | fillseq | 444307 | 51.5 | 2.25 | 1.25 | 3.42 | 10.12 | settle 0.00s; WA 1.79x; L1=5 L2=5 |
| lsm-engine | fillrandom | 448632 | 52.0 | 2.23 | 1.46 | 3.58 | 7.92 | settle 0.04s; WA 2.24x; L1=5 L2=5 |
| lsm-engine | overwrite | 358065 | 41.5 | 2.79 | 1.50 | 3.71 | 11.33 | settle 0.00s; WA 2.72x; L0=1 L1=5 L2=10 |
| lsm-engine | readrandom | 1003349 | 116.4 | 1.00 | 0.92 | 2.75 | 4.83 | 200000 of 200000 found |
| lsm-engine | readmissing | 5856487 | 93.7 | 0.17 | 0.17 | 0.17 | 0.21 | 0 found (expect 0) |
| lsm-engine | seekrandom | 84329 | 978.2 | 11.86 | 11.88 | 15.71 | 20.04 | seek + 100 nexts per op (199927 entries) |
| lsm-engine | readseq | 14561648 | 1689.2 | 0.07 | – | – | – | full scan |
