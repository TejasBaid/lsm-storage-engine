db_bench: 1000000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| sled 0.34 | fillseq | 269028 | 31.2 | 3.72 | 0.58 | 21.75 | 61.71 | settle 0.47s |
| sled 0.34 | fillrandom | 249554 | 28.9 | 4.01 | 1.46 | 24.96 | 113.00 | settle 0.00s |
| sled 0.34 | overwrite | 177492 | 20.6 | 5.63 | 4.04 | 24.96 | 140.92 | settle 0.00s |
| sled 0.34 | readrandom | 228921 | 26.6 | 4.37 | 3.54 | 15.25 | 114.04 | 1000000 of 1000000 found |
| sled 0.34 | readmissing | 4601543 | 73.6 | 0.22 | 0.17 | 0.38 | 0.96 | 0 found (expect 0) |
| sled 0.34 | seekrandom | 20362 | 236.2 | 49.11 | 49.42 | 90.00 | 103.08 | seek + 100 nexts per op (999995 entries) |
| sled 0.34 | readseq | 1768105 | 205.1 | 0.57 | – | – | – | full scan |
