db_bench: 200000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| sled 0.34 | fillseq | 707387 | 82.1 | 1.41 | 0.54 | 10.62 | 33.12 | settle 0.28s |
| sled 0.34 | fillrandom | 514292 | 59.7 | 1.94 | 1.00 | 14.08 | 39.46 | settle 0.09s |
| sled 0.34 | overwrite | 789316 | 91.6 | 1.27 | 0.96 | 9.96 | 29.92 | settle 0.00s |
| sled 0.34 | readrandom | 1880719 | 218.2 | 0.53 | 0.50 | 0.96 | 1.50 | 200000 of 200000 found |
| sled 0.34 | readmissing | 5112856 | 81.8 | 0.20 | 0.17 | 0.33 | 0.46 | 0 found (expect 0) |
| sled 0.34 | seekrandom | 62883 | 729.4 | 15.90 | 15.83 | 21.29 | 25.04 | seek + 100 nexts per op (199927 entries) |
| sled 0.34 | readseq | 6539394 | 758.6 | 0.15 | – | – | – | full scan |
