# Design Document: pagedb

## Overview
`pagedb` is an embedded B+tree storage engine engineered in Rust (safe Rust with `#![forbid(unsafe_code)]`), featuring:
- Slotted page architecture (8 KiB pages)
- Buffer pool with CLOCK eviction and no-steal policy
- Multi-Version Concurrency Control (MVCC) with Snapshot Isolation (first-committer-wins)
- Write-Ahead Log (WAL) with logical redo-only recovery
- Double-Write Buffer (DWB) protecting against torn page writes during checkpoints
- Crash resiliency verified with adversarial crash injection and torn-write harnesses

## Crash-Point Walkthrough
(Detailed step-by-step crash recovery mechanics and analysis will be documented here as components are implemented).
