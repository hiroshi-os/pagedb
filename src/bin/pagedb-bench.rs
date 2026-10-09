use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use pagedb::db::{Db, Options, SyncMode};

#[derive(Debug, Clone, Copy)]
enum Workload {
    A, // 50/50 read/update
    B, // 95/5 read/update
    C, // 100% read
    E, // 95% scan (100 keys) / 5% insert
}

fn parse_workload(s: &str) -> Workload {
    match s.to_uppercase().as_str() {
        "A" => Workload::A,
        "B" => Workload::B,
        "C" => Workload::C,
        "E" => Workload::E,
        _ => Workload::A,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut workload = Workload::A;
    let mut keys = 100_000usize;
    let mut key_size = 16usize;
    let mut value_size = 100usize;
    let mut threads = vec![1, 4, 8];
    let mut duration_secs = 10u64;
    let mut seed = 1u64;
    let mut dir = PathBuf::from("target/bench_data");

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--workload" => {
                if i + 1 < args.len() {
                    workload = parse_workload(&args[i + 1]);
                    i += 1;
                }
            }
            "--keys" => {
                if i + 1 < args.len() {
                    keys = args[i + 1].parse().unwrap_or(keys);
                    i += 1;
                }
            }
            "--key-size" => {
                if i + 1 < args.len() {
                    key_size = args[i + 1].parse().unwrap_or(key_size);
                    i += 1;
                }
            }
            "--value-size" => {
                if i + 1 < args.len() {
                    value_size = args[i + 1].parse().unwrap_or(value_size);
                    i += 1;
                }
            }
            "--threads" => {
                if i + 1 < args.len() {
                    threads = args[i + 1]
                        .split(',')
                        .filter_map(|s| s.parse().ok())
                        .collect();
                    i += 1;
                }
            }
            "--duration" => {
                if i + 1 < args.len() {
                    let s = args[i + 1].trim_end_matches('s');
                    duration_secs = s.parse().unwrap_or(duration_secs);
                    i += 1;
                }
            }
            "--seed" => {
                if i + 1 < args.len() {
                    seed = args[i + 1].parse().unwrap_or(seed);
                    i += 1;
                }
            }
            "--dir" if i + 1 < args.len() => {
                dir = PathBuf::from(&args[i + 1]);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);

    println!(
        "=== pagedb Benchmark ===\nWorkload: {:?}, Keys: {}, Key Size: {}, Val Size: {}, Duration: {}s, Seed: {}",
        workload, keys, key_size, value_size, duration_secs, seed
    );

    let db = Arc::new(
        Db::open(
            &dir,
            Options {
                page_cache_frames: 16384,
                wal_segment_bytes: 64 * 1024 * 1024,
                sync: SyncMode::Always,
            },
        )
        .expect("Open DB for bench"),
    );

    // 1. Load phase
    print!("Loading {} keys...", keys);
    let load_start = Instant::now();
    let val_template = vec![b'v'; value_size];
    for k_idx in 0..keys {
        let key_str = format!("{:0width$}", k_idx, width = key_size);
        let mut tx = db.begin();
        tx.put(key_str.as_bytes(), &val_template).expect("load put");
        tx.commit().expect("load commit");
    }
    let load_duration = load_start.elapsed();
    println!(" Completed in {:.2?}", load_duration);

    // 2. Run workload for each thread count
    for num_threads in threads {
        println!("\n--- Running with {} threads ---", num_threads);
        let running = Arc::new(AtomicBool::new(true));
        let total_ops = Arc::new(AtomicU64::new(0));
        let abort_ops = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::new();
        let mut histograms = Vec::new();

        for tid in 0..num_threads {
            let db_cl = db.clone();
            let running_cl = running.clone();
            let total_ops_cl = total_ops.clone();
            let abort_ops_cl = abort_ops.clone();

            let (tx_hist, rx_hist) = std::sync::mpsc::channel();
            let val_template_cl = val_template.clone();

            handles.push(thread::spawn(move || {
                let mut hist: Histogram<u64> =
                    Histogram::new_with_bounds(1, 10_000_000, 3).expect("histogram");
                let mut rng = seed ^ ((tid as u64 + 1) * 1000003);

                while running_cl.load(Ordering::Relaxed) {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;

                    let op_roll = rng % 100;
                    let target_key_num = (rng >> 8) % (keys as u64);
                    let target_key =
                        format!("{:0width$}", target_key_num, width = key_size).into_bytes();

                    let op_start = Instant::now();

                    match workload {
                        Workload::A => {
                            if op_roll < 50 {
                                // 50% Read
                                let tx = db_cl.begin();
                                let _ = tx.get(&target_key);
                            } else {
                                // 50% Update
                                let mut tx = db_cl.begin();
                                let _ = tx.put(&target_key, &val_template_cl);
                                match tx.commit() {
                                    Ok(_) => {}
                                    Err(_) => {
                                        abort_ops_cl.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                        Workload::B => {
                            if op_roll < 95 {
                                // 95% Read
                                let tx = db_cl.begin();
                                let _ = tx.get(&target_key);
                            } else {
                                // 5% Update
                                let mut tx = db_cl.begin();
                                let _ = tx.put(&target_key, &val_template_cl);
                                match tx.commit() {
                                    Ok(_) => {}
                                    Err(_) => {
                                        abort_ops_cl.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                        Workload::C => {
                            // 100% Read
                            let tx = db_cl.begin();
                            let _ = tx.get(&target_key);
                        }
                        Workload::E => {
                            if op_roll < 95 {
                                // 95% short scan 100 keys
                                let tx = db_cl.begin();
                                let end_key = format!(
                                    "{:0width$}",
                                    (target_key_num + 100).min(keys as u64),
                                    width = key_size
                                )
                                .into_bytes();
                                let _ = tx.scan(target_key.as_slice()..end_key.as_slice());
                            } else {
                                // 5% insert
                                let mut tx = db_cl.begin();
                                let _ = tx.put(&target_key, &val_template_cl);
                                match tx.commit() {
                                    Ok(_) => {}
                                    Err(_) => {
                                        abort_ops_cl.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                    }

                    let op_micros = op_start.elapsed().as_micros() as u64;
                    let _ = hist.record(op_micros.max(1));
                    total_ops_cl.fetch_add(1, Ordering::Relaxed);
                }

                let _ = tx_hist.send(hist);
            }));

            histograms.push(rx_hist);
        }

        thread::sleep(Duration::from_secs(duration_secs));
        running.store(false, Ordering::Relaxed);

        for h in handles {
            let _ = h.join();
        }

        let mut combined_hist: Histogram<u64> =
            Histogram::new_with_bounds(1, 10_000_000, 3).expect("histogram");
        for rx in histograms {
            if let Ok(h) = rx.recv() {
                combined_hist.add(&h).expect("combine histogram");
            }
        }

        let total = total_ops.load(Ordering::Relaxed);
        let aborts = abort_ops.load(Ordering::Relaxed);
        let ops_per_sec = (total as f64) / (duration_secs as f64);
        let abort_rate = if total > 0 {
            (aborts as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        let p50_ms = combined_hist.value_at_quantile(0.50) as f64 / 1000.0;
        let p99_ms = combined_hist.value_at_quantile(0.99) as f64 / 1000.0;
        let p999_ms = combined_hist.value_at_quantile(0.999) as f64 / 1000.0;

        println!(
            "Result: {:>8.1} ops/s | p50: {:>6.2}ms | p99: {:>6.2}ms | p99.9: {:>6.2}ms | Abort Rate: {:.2}%",
            ops_per_sec, p50_ms, p99_ms, p999_ms, abort_rate
        );
    }

    let stats = db.check_invariants().expect("check invariants");
    println!(
        "\nFinal Tree Stats: Height = {}, Fill Factor = {:.2}%, Total Keys = {}",
        stats.height,
        stats.fill_factor * 100.0,
        stats.total_keys
    );
}
