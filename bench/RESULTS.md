# Benchmark Results

Hardware:
- CPU: AMD Ryzen 5 7530U with Radeon Graphics (6 Cores, 12 Threads)
- RAM: 24 GB
- OS: Windows 11 (Microsoft Windows NT 10.0.26200.0)
- Disk: NVMe SSD (SOLIDIGM SSDPFKNU512GZH, NTFS)
- Date: 2026-10-10T02:32:57+05:30
- Commit: `88d07c3`

## Adversarial Crash & Recovery Test Suite

Command: `./target/release/crashtest.exe --runs 300 --seed 12345`

| Metric | Result | Target Invariant |
|---|---|---|
| Crash runs (SIGKILL kill -9) | 300 | 300 runs |
| Acknowledged transactions verified | 2,812 | Atomicity & durability |
| Lost committed transactions | 0 | 0 |
| Partial transactions visible | 0 | 0 |
| Tree invariant checks passed | 300 / 300 | 100% |
| CRC errors leaked to caller | 0 | 0 |

Command: `./target/release/crashtest.exe --tear --runs 20 --seed 42`

| Metric | Result | Target Invariant |
|---|---|---|
| Torn write runs | 20 | 20 runs |
| Acked transactions verified | 118 | All preserved |
| Torn write repairs (WAL tail & DWB) | 20 | Successful recovery |

Command: `./target/release/crashtest.exe --features unsafe-skip-dwb --tear --runs 20 --seed 42`

| Metric | Result | Target Invariant |
|---|---|---|
| Negative control corruptions detected | 10 | Proves tear injection actively tears pages |

---

## Transaction Isolation & Concurrency Suite

Test: `cargo test --release --test isolation_test`

| Test / Invariant | Result | Target Invariant |
|---|---|---|
| Bank accounts sum invariant | > 50 checks / invariant held | Consistent read-only snapshot sums |
| Lost update 8-thread increment (8,000) | 8,000 | First-committer-wins prevents lost update |
| Negative control (`unsafe-no-conflict-check`) | Anomaly detected (< 8,000) | Proves conflict checker catches conflicts |
| Write skew (two doctors on call) | Observed under SI | Expected behavior under Snapshot Isolation |
| Snapshot stability (100 scans vs writers) | 100% stable | Snapshot isolation visibility immutability |

---

## YCSB-style Workloads (8 Threads, Release)

Command: `./target/release/pagedb-bench.exe --workload <X> --keys 20000 --threads 8 --duration 10s --seed 1`

| Workload | Access Pattern | Throughput (ops/s) | p50 Latency | p99 Latency | p99.9 Latency | Abort Rate |
|---|---|---|---|---|---|---|
| **A** | 50% Read / 50% Update | 1,447.4 ops/s | 0.08 ms | 31.74 ms | 123.78 ms | 0.01% |
| **B** | 95% Read / 5% Update | 24,414.1 ops/s | 0.00 ms | 6.17 ms | 12.14 ms | 0.00% |
| **C** | 100% Read | 339,933.6 ops/s | 0.01 ms | 0.10 ms | 0.20 ms | 0.00% |
| **E** | 95% Short Scan (100k) / 5% Insert | 744.1 ops/s | 10.46 ms | 22.05 ms | 82.56 ms | 0.00% |

- Commit latency with `sync=Always` (1 thread, Workload A): p50 = 0.02 ms, p99 = 1.28 ms, 2,701.3 ops/s
- Recovery time on WAL reopen: 224 ms (0.224 s)
