db_bench: 200000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| sled 0.34 | fillseq | 709012 | 82.2 | 1.41 | 0.54 | 10.25 | 32.50 | settle 0.34s |
| sled 0.34 | fillrandom | 498254 | 57.8 | 2.01 | 1.04 | 14.29 | 40.25 | settle 0.10s |
| sled 0.34 | overwrite | 813367 | 94.4 | 1.23 | 0.96 | 9.96 | 29.04 | settle 0.01s |
| sled 0.34 | readrandom | 1904571 | 220.9 | 0.53 | 0.50 | 0.92 | 1.38 | 200000 of 200000 found |
| sled 0.34 | readmissing | 4928111 | 78.8 | 0.20 | 0.17 | 0.33 | 0.46 | 0 found (expect 0) |
| sled 0.34 | seekrandom | 64652 | 750.0 | 15.47 | 15.46 | 17.96 | 21.50 | seek + 100 nexts per op (199927 entries) |
| sled 0.34 | readseq | 6644647 | 770.8 | 0.15 | – | – | – | full scan |
