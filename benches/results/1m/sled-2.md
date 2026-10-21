db_bench: 1000000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| sled 0.34 | fillseq | 246766 | 28.6 | 4.05 | 0.58 | 21.54 | 65.79 | settle 0.28s |
| sled 0.34 | fillrandom | 250485 | 29.1 | 3.99 | 1.42 | 24.92 | 114.25 | settle 0.00s |
| sled 0.34 | overwrite | 176594 | 20.5 | 5.66 | 4.04 | 26.83 | 143.88 | settle 0.00s |
| sled 0.34 | readrandom | 224857 | 26.1 | 4.45 | 3.62 | 16.12 | 113.46 | 1000000 of 1000000 found |
| sled 0.34 | readmissing | 4469800 | 71.5 | 0.22 | 0.21 | 0.38 | 1.04 | 0 found (expect 0) |
| sled 0.34 | seekrandom | 19814 | 229.8 | 50.47 | 49.71 | 93.62 | 130.62 | seek + 100 nexts per op (999995 entries) |
| sled 0.34 | readseq | 1626547 | 188.7 | 0.61 | – | – | – | full scan |
