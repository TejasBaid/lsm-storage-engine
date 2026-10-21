db_bench: 200000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| sled 0.34 | fillseq | 715348 | 83.0 | 1.40 | 0.54 | 10.42 | 33.29 | settle 0.35s |
| sled 0.34 | fillrandom | 506595 | 58.8 | 1.97 | 1.00 | 14.12 | 40.83 | settle 0.08s |
| sled 0.34 | overwrite | 749064 | 86.9 | 1.33 | 1.04 | 11.12 | 30.17 | settle 0.00s |
| sled 0.34 | readrandom | 1858819 | 215.6 | 0.54 | 0.50 | 0.96 | 1.50 | 200000 of 200000 found |
| sled 0.34 | readmissing | 4947107 | 79.2 | 0.20 | 0.17 | 0.33 | 0.46 | 0 found (expect 0) |
| sled 0.34 | seekrandom | 61195 | 709.9 | 16.34 | 16.29 | 21.08 | 26.12 | seek + 100 nexts per op (199927 entries) |
| sled 0.34 | readseq | 6537924 | 758.4 | 0.15 | – | – | – | full scan |
