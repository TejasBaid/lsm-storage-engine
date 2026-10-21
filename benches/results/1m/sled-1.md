db_bench: 1000000 keys, 16-byte keys, 100-byte values, cache 64 MiB, 10 CPUs

| engine | benchmark | ops/s | MB/s | avg µs/op | p50 µs | p99 µs | p99.9 µs | notes |
|---|---|---:|---:|---:|---:|---:|---:|---|
| sled 0.34 | fillseq | 260880 | 30.3 | 3.83 | 0.58 | 21.88 | 85.54 | settle 0.32s |
| sled 0.34 | fillrandom | 234018 | 27.1 | 4.27 | 1.46 | 25.33 | 116.42 | settle 0.00s |
| sled 0.34 | overwrite | 170663 | 19.8 | 5.86 | 4.08 | 27.54 | 153.04 | settle 0.00s |
| sled 0.34 | readrandom | 218643 | 25.4 | 4.57 | 3.58 | 16.42 | 118.96 | 1000000 of 1000000 found |
| sled 0.34 | readmissing | 4519155 | 72.3 | 0.22 | 0.21 | 0.38 | 1.00 | 0 found (expect 0) |
| sled 0.34 | seekrandom | 19987 | 231.8 | 50.03 | 49.50 | 92.79 | 112.58 | seek + 100 nexts per op (999995 entries) |
| sled 0.34 | readseq | 1561891 | 181.2 | 0.64 | – | – | – | full scan |
