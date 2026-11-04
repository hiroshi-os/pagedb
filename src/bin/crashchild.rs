use pagedb::db::{Db, Options};
use std::io::Write;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: crashchild <db_dir>");
        std::process::exit(1);
    }
    let db_dir = &args[1];

    let db = match Db::open(db_dir, Options::default()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("crashchild failed to open DB: {:?}", e);
            std::process::exit(1);
        }
    };

    let mut rng = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let mut next_rand = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let mut txn_counter = 0u64;

    loop {
        txn_counter += 1;
        let mut tx = db.begin();
        let num_keys = (next_rand() % 8) + 1; // 1 to 8 keys
        let mut keys_written = Vec::new();

        for i in 0..num_keys {
            let k = format!("user_key_{:04}", (next_rand() % 200)).into_bytes();
            let v = format!("val_{}_{}_{}", txn_counter, i, next_rand()).into_bytes();
            if let Err(e) = tx.put(&k, &v) {
                eprintln!("put failed: {:?}", e);
                break;
            }
            keys_written.push(k);
        }

        // Marker key txn/<id> listing all keys written in this txn
        let marker_key = format!("txn/{:08}", txn_counter).into_bytes();
        let marker_val = keys_written.join(&b","[..]);
        let _ = tx.put(&marker_key, &marker_val);

        match tx.commit() {
            Ok(commit_ts) => {
                // Acknowledge commit to parent through stdout pipe
                println!("{}", commit_ts);
                let _ = std::io::stdout().flush();
            }
            Err(pagedb::page::Error::Conflict { .. }) => {
                // write-write conflict: retry loop
                continue;
            }
            Err(e) => {
                eprintln!("commit failed: {:?}", e);
                break;
            }
        }

        // Random checkpoint trigger
        if next_rand() % 5 == 0 {
            let _ = db.checkpoint();
        }

        std::thread::sleep(Duration::from_millis(5));
    }
}
