use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

use pagedb::db::{Db, Options};
use pagedb::page::Error;

#[test]
fn test_read_your_writes_and_abort() {
    let dir = tempdir().unwrap();
    let db = Db::open(dir.path(), Options::default()).unwrap();

    let mut tx1 = db.begin();
    tx1.put(b"foo", b"bar").unwrap();
    assert_eq!(tx1.get(b"foo").unwrap(), Some(b"bar".to_vec()));
    tx1.commit().unwrap();

    let mut tx2 = db.begin();
    tx2.put(b"foo", b"baz").unwrap();
    assert_eq!(tx2.get(b"foo").unwrap(), Some(b"baz".to_vec()));
    tx2.abort();

    let tx3 = db.begin();
    assert_eq!(tx3.get(b"foo").unwrap(), Some(b"bar".to_vec()));
}

#[test]
fn test_snapshot_stability() {
    let dir = tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path(), Options::default()).unwrap());

    // Insert initial data
    {
        let mut tx = db.begin();
        tx.put(b"k1", b"v1").unwrap();
        tx.put(b"k2", b"v2").unwrap();
        tx.commit().unwrap();
    }

    let db_reader = db.clone();
    let reader_handle = thread::spawn(move || {
        let tx = db_reader.begin();
        for _ in 0..100 {
            let scan = tx.scan(..).unwrap();
            assert_eq!(scan.len(), 2);
            assert_eq!(scan[0], (b"k1".to_vec(), b"v1".to_vec()));
            assert_eq!(scan[1], (b"k2".to_vec(), b"v2".to_vec()));
            thread::sleep(Duration::from_millis(2));
        }
    });

    let db_writer = db.clone();
    let writer_handle = thread::spawn(move || {
        for i in 0..50 {
            let mut tx = db_writer.begin();
            let k = format!("writer_key_{}", i).into_bytes();
            tx.put(&k, b"val").unwrap();
            tx.commit().unwrap();
            thread::sleep(Duration::from_millis(3));
        }
    });

    reader_handle.join().unwrap();
    writer_handle.join().unwrap();
}

#[test]
fn test_bank_transfers_invariant() {
    let dir = tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path(), Options::default()).unwrap());

    let num_accounts = 16;
    let initial_balance = 1000u64;
    let total_expected = (num_accounts as u64) * initial_balance;

    // Initialize accounts
    {
        let mut tx = db.begin();
        for i in 0..num_accounts {
            let k = format!("acct_{:02}", i).into_bytes();
            tx.put(&k, &initial_balance.to_le_bytes()).unwrap();
        }
        tx.commit().unwrap();
    }

    let running = Arc::new(AtomicBool::new(true));
    let scans_checked = Arc::new(AtomicU64::new(0));

    // Spawn 4 read-only scan threads checking invariant
    let mut reader_handles = Vec::new();
    for _ in 0..4 {
        let db_cl = db.clone();
        let running_cl = running.clone();
        let scans_cl = scans_checked.clone();
        reader_handles.push(thread::spawn(move || {
            while running_cl.load(Ordering::Relaxed) {
                let tx = db_cl.begin();
                let entries = tx
                    .scan(b"acct_".as_slice()..=b"acct_99".as_slice())
                    .unwrap();
                let mut sum = 0u64;
                for (_, v) in entries {
                    let bal = u64::from_le_bytes(v.try_into().unwrap());
                    sum += bal;
                }
                assert_eq!(sum, total_expected, "Bank sum invariant violated!");
                scans_cl.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Spawn 8 transfer threads with Conflict retry
    let mut transfer_handles = Vec::new();
    for thread_id in 0..8 {
        let db_cl = db.clone();
        let running_cl = running.clone();
        transfer_handles.push(thread::spawn(move || {
            let mut rng = 123456789u64 + thread_id * 1000;
            while running_cl.load(Ordering::Relaxed) {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let from_idx = (rng % (num_accounts as u64)) as usize;
                let to_idx = ((rng >> 8) % (num_accounts as u64)) as usize;
                if from_idx == to_idx {
                    continue;
                }

                let amount = (rng % 50) + 1;
                let from_key = format!("acct_{:02}", from_idx).into_bytes();
                let to_key = format!("acct_{:02}", to_idx).into_bytes();

                // Loop retry on Conflict
                loop {
                    let mut tx = db_cl.begin();
                    let from_bal = u64::from_le_bytes(
                        tx.get(&from_key).unwrap().unwrap().try_into().unwrap(),
                    );
                    let to_bal =
                        u64::from_le_bytes(tx.get(&to_key).unwrap().unwrap().try_into().unwrap());

                    if from_bal < amount {
                        break;
                    }

                    tx.put(&from_key, &(from_bal - amount).to_le_bytes())
                        .unwrap();
                    tx.put(&to_key, &(to_bal + amount).to_le_bytes()).unwrap();

                    match tx.commit() {
                        Ok(_) => break,
                        Err(Error::Conflict { .. }) => continue,
                        Err(e) => panic!("Unexpected error: {:?}", e),
                    }
                }
            }
        }));
    }

    thread::sleep(Duration::from_millis(1500));
    running.store(false, Ordering::Relaxed);

    for h in transfer_handles {
        h.join().unwrap();
    }
    for h in reader_handles {
        h.join().unwrap();
    }

    assert!(scans_checked.load(Ordering::Relaxed) > 50);
}

#[test]
fn test_lost_update_8k() {
    let dir = tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path(), Options::default()).unwrap());

    // Counter initialized to 0
    {
        let mut tx = db.begin();
        tx.put(b"counter", &0u64.to_le_bytes()).unwrap();
        tx.commit().unwrap();
    }

    let mut handles = Vec::new();
    // 8 threads x 1,000 increments
    for _ in 0..8 {
        let db_cl = db.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..1000 {
                loop {
                    let mut tx = db_cl.begin();
                    let curr = u64::from_le_bytes(
                        tx.get(b"counter").unwrap().unwrap().try_into().unwrap(),
                    );
                    tx.put(b"counter", &(curr + 1).to_le_bytes()).unwrap();
                    match tx.commit() {
                        Ok(_) => break,
                        Err(Error::Conflict { .. }) => continue,
                        Err(e) => panic!("Unexpected error: {:?}", e),
                    }
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let tx = db.begin();
    let final_val =
        u64::from_le_bytes(tx.get(b"counter").unwrap().unwrap().try_into().unwrap());

    #[cfg(not(feature = "unsafe-no-conflict-check"))]
    assert_eq!(final_val, 8000);

    #[cfg(feature = "unsafe-no-conflict-check")]
    assert!(
        final_val < 8000,
        "Negative control: without conflict check, lost updates must occur! Got {}",
        final_val
    );
}

#[test]
fn test_write_skew_allowed_under_si() {
    // Classic two doctors on call invariant: at least one doctor must be on call.
    // Both doctors check: is the other doctor on call? If yes, doctor goes off call.
    let dir = tempdir().unwrap();
    let db = Db::open(dir.path(), Options::default()).unwrap();

    // Dr. Alice (doc_1) and Dr. Bob (doc_2) are both on call (true = 1)
    {
        let mut tx = db.begin();
        tx.put(b"doc_1", &[1u8]).unwrap();
        tx.put(b"doc_2", &[1u8]).unwrap();
        tx.commit().unwrap();
    }

    // Txn A (Dr. Alice wants to leave)
    let mut tx_a = db.begin();
    let bob_on_call = tx_a.get(b"doc_2").unwrap().unwrap()[0] == 1;
    if bob_on_call {
        tx_a.put(b"doc_1", &[0u8]).unwrap();
    }

    // Txn B (Dr. Bob concurrently wants to leave)
    let mut tx_b = db.begin();
    let alice_on_call = tx_b.get(b"doc_1").unwrap().unwrap()[0] == 1;
    if alice_on_call {
        tx_b.put(b"doc_2", &[0u8]).unwrap();
    }

    // Under Snapshot Isolation, write sets do not overlap (tx_a writes doc_1, tx_b writes doc_2)
    // Both commits succeed!
    let res_a = tx_a.commit();
    let res_b = tx_b.commit();

    assert!(res_a.is_ok());
    assert!(res_b.is_ok());

    // Both doctors are now off call -> write skew has occurred, as expected under SI!
    let tx_check = db.begin();
    let alice = tx_check.get(b"doc_1").unwrap().unwrap()[0];
    let bob = tx_check.get(b"doc_2").unwrap().unwrap()[0];
    assert_eq!(alice, 0);
    assert_eq!(bob, 0);
}

#[test]
fn lost_update_detected() {
    // This is the negative control test entry point for CI
    test_lost_update_8k();
}
