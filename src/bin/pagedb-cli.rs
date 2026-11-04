use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use pagedb::db::{Db, Options, Txn};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: pagedb-cli <path>");
        std::process::exit(1);
    }
    let db_path = PathBuf::from(&args[1]);

    let db = match Db::open(&db_path, Options::default()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error opening database at {:?}: {:?}", db_path, e);
            std::process::exit(1);
        }
    };

    println!("pagedb interactive CLI. Type HELP for commands, EXIT to quit.");

    let stdin = io::stdin();
    let mut current_tx: Option<Txn> = None;

    print!("pagedb> ");
    let _ = io::stdout().flush();

    for line in stdin.lock().lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            print!("pagedb> ");
            let _ = io::stdout().flush();
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd = parts[0].to_uppercase();

        match cmd.as_str() {
            "BEGIN" => {
                if current_tx.is_some() {
                    println!("Error: transaction already active");
                } else {
                    current_tx = Some(db.begin());
                    println!("OK: transaction started at snapshot");
                }
            }
            "GET" => {
                if parts.len() < 2 {
                    println!("Usage: GET <key>");
                } else {
                    let key = parts[1].as_bytes();
                    if let Some(tx) = &current_tx {
                        match tx.get(key) {
                            Ok(Some(val)) => {
                                println!("VALUE: {}", String::from_utf8_lossy(&val))
                            }
                            Ok(None) => println!("NOT FOUND"),
                            Err(e) => println!("Error: {:?}", e),
                        }
                    } else {
                        let tx = db.begin();
                        match tx.get(key) {
                            Ok(Some(val)) => {
                                println!("VALUE: {}", String::from_utf8_lossy(&val))
                            }
                            Ok(None) => println!("NOT FOUND"),
                            Err(e) => println!("Error: {:?}", e),
                        }
                    }
                }
            }
            "PUT" => {
                if parts.len() < 3 {
                    println!("Usage: PUT <key> <val>");
                } else if let Some(tx) = &mut current_tx {
                    let key = parts[1].as_bytes();
                    let val = parts[2].as_bytes();
                    match tx.put(key, val) {
                        Ok(()) => println!("OK"),
                        Err(e) => println!("Error: {:?}", e),
                    }
                } else {
                    let mut tx = db.begin();
                    let key = parts[1].as_bytes();
                    let val = parts[2].as_bytes();
                    if let Err(e) = tx.put(key, val) {
                        println!("Error: {:?}", e);
                    } else {
                        match tx.commit() {
                            Ok(ts) => println!("OK (committed at ts {})", ts),
                            Err(e) => println!("Error: {:?}", e),
                        }
                    }
                }
            }
            "DEL" => {
                if parts.len() < 2 {
                    println!("Usage: DEL <key>");
                } else if let Some(tx) = &mut current_tx {
                    let key = parts[1].as_bytes();
                    match tx.delete(key) {
                        Ok(()) => println!("OK"),
                        Err(e) => println!("Error: {:?}", e),
                    }
                } else {
                    let mut tx = db.begin();
                    let key = parts[1].as_bytes();
                    if let Err(e) = tx.delete(key) {
                        println!("Error: {:?}", e);
                    } else {
                        match tx.commit() {
                            Ok(ts) => println!("OK (committed at ts {})", ts),
                            Err(e) => println!("Error: {:?}", e),
                        }
                    }
                }
            }
            "SCAN" => {
                let start = if parts.len() > 1 {
                    Some(parts[1].as_bytes())
                } else {
                    None
                };
                let end = if parts.len() > 2 {
                    Some(parts[2].as_bytes())
                } else {
                    None
                };

                let scan_res = if let Some(tx) = &current_tx {
                    match (start, end) {
                        (Some(s), Some(e)) => tx.scan(s..e),
                        (Some(s), None) => tx.scan(s..),
                        (None, Some(e)) => tx.scan(..e),
                        (None, None) => tx.scan(..),
                    }
                } else {
                    let tx = db.begin();
                    match (start, end) {
                        (Some(s), Some(e)) => tx.scan(s..e),
                        (Some(s), None) => tx.scan(s..),
                        (None, Some(e)) => tx.scan(..e),
                        (None, None) => tx.scan(..),
                    }
                };

                match scan_res {
                    Ok(entries) => {
                        println!("Total: {} entries", entries.len());
                        for (k, v) in entries {
                            println!(
                                "  {} => {}",
                                String::from_utf8_lossy(&k),
                                String::from_utf8_lossy(&v)
                            );
                        }
                    }
                    Err(e) => println!("Error: {:?}", e),
                }
            }
            "COMMIT" => {
                if let Some(tx) = current_tx.take() {
                    match tx.commit() {
                        Ok(ts) => println!("OK: committed at ts {}", ts),
                        Err(e) => println!("Commit failed: {:?}", e),
                    }
                } else {
                    println!("Error: no active transaction to commit");
                }
            }
            "ABORT" => {
                if let Some(tx) = current_tx.take() {
                    tx.abort();
                    println!("OK: transaction aborted");
                } else {
                    println!("Error: no active transaction to abort");
                }
            }
            "CHECK" => match db.check_invariants() {
                Ok(stats) => {
                    println!("Tree Invariants Passed: {:?}", stats);
                }
                Err(e) => println!("Tree invariant failed: {:?}", e),
            },
            "STATS" => match db.check_invariants() {
                Ok(stats) => {
                    println!("Height: {}", stats.height);
                    println!("Leaf pages: {}", stats.leaf_pages);
                    println!("Internal pages: {}", stats.internal_pages);
                    println!("Total keys: {}", stats.total_keys);
                    println!("Fill factor: {:.2}%", stats.fill_factor * 100.0);
                }
                Err(e) => println!("Error obtaining stats: {:?}", e),
            },
            "VACUUM" => match db.vacuum() {
                Ok(removed) => println!("OK: vacuum removed {} obsolete versions", removed),
                Err(e) => println!("Vacuum failed: {:?}", e),
            },
            "HELP" => {
                println!("Commands: BEGIN, GET <k>, PUT <k> <v>, DEL <k>, SCAN [start] [end], COMMIT, ABORT, CHECK, STATS, VACUUM, EXIT");
            }
            "EXIT" | "QUIT" => {
                break;
            }
            other => {
                println!("Unknown command: {}. Type HELP for options.", other);
            }
        }

        print!("pagedb> ");
        let _ = io::stdout().flush();
    }
}
