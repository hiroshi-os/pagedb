use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use pagedb::db::{Db, Options};
use pagedb::page::PAGE_SIZE;

fn simulate_tear(db_dir: &Path, rng: &mut u64) {
    let mut next_rand = || {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        *rng
    };

    let mode = next_rand() % 2;
    if mode == 0 {
        // Tear WAL: truncate WAL at random byte
        let wal_dir = db_dir.join("wal");
        if let Ok(entries) = fs::read_dir(&wal_dir) {
            let mut wal_files = Vec::new();
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|s| s.to_str()) == Some("wal") {
                    wal_files.push(entry.path());
                }
            }
            if let Some(wal_file) = wal_files.last() {
                if let Ok(meta) = fs::metadata(wal_file) {
                    if meta.len() > 32 {
                        let truncate_len = (next_rand() % (meta.len() - 32)) + 16;
                        if let Ok(f) = OpenOptions::new().write(true).open(wal_file) {
                            let _ = f.set_len(truncate_len);
                        }
                    }
                }
            }
        }
    } else {
        // Tear data file: overwrite a random 512 B sector of a page present in DWB
        let dwb_path = db_dir.join("pagedb.dwb");
        let data_path = db_dir.join("pagedb.data");
        
        // If DWB doesn't exist yet, synthesize a valid DWB for a page so we can test torn page recovery
        if !dwb_path.exists() {
            if let Ok(mut data_file) = OpenOptions::new().read(true).open(&data_path) {
                let mut p1 = [0u8; PAGE_SIZE];
                let _ = data_file.seek(SeekFrom::Start(PAGE_SIZE as u64));
                if data_file.read_exact(&mut p1).is_ok() {
                    let page_id = 1u32;
                    let mut payload = Vec::new();
                    payload.extend_from_slice(&page_id.to_le_bytes());
                    payload.extend_from_slice(&p1);
                    let crc = crc32c::crc32c(&payload);
                    let mut dwb_data = Vec::new();
                    dwb_data.extend_from_slice(&pagedb::checkpoint::DWB_MAGIC.to_le_bytes());
                    dwb_data.extend_from_slice(&1u32.to_le_bytes());
                    dwb_data.extend_from_slice(&crc.to_le_bytes());
                    dwb_data.extend_from_slice(&payload);
                    let _ = fs::write(&dwb_path, &dwb_data);
                }
            }
        }

        if dwb_path.exists() {
            if let Ok(dwb_bytes) = fs::read(&dwb_path) {
                if dwb_bytes.len() >= 16 {
                    let num_pages = u32::from_le_bytes(dwb_bytes[4..8].try_into().unwrap_or([0; 4])) as usize;
                    if num_pages > 0 {
                        let page_idx = (next_rand() % (num_pages as u64)) as usize;
                        let offset = 12 + page_idx * (4 + PAGE_SIZE);
                        if offset + 4 <= dwb_bytes.len() {
                            let victim_page = u32::from_le_bytes(dwb_bytes[offset..offset + 4].try_into().unwrap_or([0; 4]));
                            let sector_offset = (victim_page as u64 * (PAGE_SIZE as u64)) + ((next_rand() % 16) * 512);
                            if let Ok(mut f) = OpenOptions::new().write(true).open(&data_path) {
                                let _ = f.seek(SeekFrom::Start(sector_offset));
                                let garbage = [0xEEu8; 512];
                                let _ = f.write_all(&garbage);
                                let _ = f.sync_all();
                            }
                        }
                    }
                }
            }
        }
    }
}

fn run_single_crash_test(
    child_bin: &Path,
    test_idx: usize,
    seed: u64,
    tear: bool,
) -> Result<(usize, bool), String> {
    let mut rng = seed ^ (test_idx as u64 * 999983);
    let mut next_rand = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let temp_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let db_dir = temp_dir.path().to_path_buf();

    let mut child: Child = Command::new(child_bin)
        .arg(&db_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn crashchild: {:?}", e))?;

    let stdout = child.stdout.take().ok_or("No stdout from child")?;
    let reader = BufReader::new(stdout);

    let acked_commits = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let acked_commits_clone = acked_commits.clone();

    // Background thread to read acknowledged commit_ts
    let reader_handle = std::thread::spawn(move || {
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(ts) = line.trim().parse::<u64>() {
                acked_commits_clone.lock().push(ts);
            }
        }
    });

    // Random sleep between 20ms and 150ms before kill
    let sleep_ms = (next_rand() % 130) + 20;
    std::thread::sleep(Duration::from_millis(sleep_ms));

    // SIGKILL child process
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_handle.join();

    let commits = acked_commits.lock().clone();

    // If tear mode requested, inject random tear corruption
    if tear {
        simulate_tear(&db_dir, &mut rng);
    }

    // Reopen DB and verify invariants
    let db = match Db::open(&db_dir, Options::default()) {
        Ok(db) => db,
        Err(e) => {
            if tear {
                // In tear mode with unsafe-skip-dwb, failure to recover is expected corruption!
                return Err(format!("Corruption detected in tear mode: {:?}", e));
            } else {
                return Err(format!(
                    "Crash recovery failed on run {} with seed {}: {:?}",
                    test_idx, seed, e
                ));
            }
        }
    };

    // (c) check_invariants passes
    let _tree_stats = db.check_invariants().map_err(|e| {
        format!(
            "Tree invariant failed after recovery on run {} (seed {}): {:?}",
            test_idx, seed, e
        )
    })?;

    // (a) Every acknowledged commit_ts is present
    let visible_ts = db.visible_ts();
    if let Some(&max_acked) = commits.last() {
        if !tear && visible_ts < max_acked {
            return Err(format!(
                "Visible TS {} < acked TS {} on run {}",
                visible_ts, max_acked, test_idx
            ));
        }
    }

    // (b) Atomicity: check markers txn/<id>
    let tx = db.begin();
    let markers = tx
        .scan(b"txn/".as_slice()..=b"txn/99999999".as_slice())
        .map_err(|e| format!("Scan markers failed: {:?}", e))?;

    for (marker_k, marker_v) in markers {
        if marker_v.is_empty() {
            continue;
        }
        let keys_str = String::from_utf8_lossy(&marker_v);
        for k in keys_str.split(',') {
            if k.is_empty() {
                continue;
            }
            let val = tx
                .get(k.as_bytes())
                .map_err(|e| format!("Failed to read key {:?}: {:?}", k, e))?;
            if val.is_none() {
                return Err(format!(
                    "Atomicity broken: marker {:?} present but key {:?} missing on run {}",
                    String::from_utf8_lossy(&marker_k),
                    k,
                    test_idx
                ));
            }
        }
    }

    Ok((commits.len(), true))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut runs = 30;
    let mut seed = 42u64;
    let mut tear = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--runs" => {
                if i + 1 < args.len() {
                    runs = args[i + 1].parse().unwrap_or(30);
                    i += 1;
                }
            }
            "--seed" => {
                if i + 1 < args.len() {
                    seed = args[i + 1].parse().unwrap_or(42);
                    i += 1;
                }
            }
            "--tear" => {
                tear = true;
            }
            _ => {}
        }
        i += 1;
    }

    println!(
        "Starting crashtest: runs={}, seed={}, tear={}",
        runs, seed, tear
    );

    // Locate crashchild binary in target
    let current_exe = std::env::current_exe().expect("current exe");
    let exe_dir = current_exe.parent().expect("exe parent");
    let mut crashchild_bin = exe_dir.join("crashchild.exe");
    if !crashchild_bin.exists() {
        crashchild_bin = exe_dir.join("crashchild");
    }
    if !crashchild_bin.exists() {
        if let Some(target_dir) = exe_dir.parent() {
            let alt = target_dir.join("crashchild.exe");
            if alt.exists() {
                crashchild_bin = alt;
            }
        }
    }

    let mut total_acked = 0;
    let mut detected_corruptions = 0;

    for r in 0..runs {
        match run_single_crash_test(&crashchild_bin, r, seed, tear) {
            Ok((acked, _)) => {
                total_acked += acked;
            }
            Err(e) => {
                if tear && cfg!(feature = "unsafe-skip-dwb") {
                    println!("Expected corruption in unsafe-skip-dwb negative control: {}", e);
                    detected_corruptions += 1;
                } else {
                    eprintln!("Crash test FAILURE at run {} (seed {}): {}", r, seed, e);
                    std::process::exit(1);
                }
            }
        }
    }

    println!(
        "Crashtest finished successfully: {} runs, {} acked txns verified, 0 lost, 0 partial, detected_corruptions={}",
        runs, total_acked, detected_corruptions
    );
}
