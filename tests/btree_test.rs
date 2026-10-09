use std::collections::BTreeMap;
use std::sync::Arc;
use tempfile::tempdir;

use pagedb::btree::BTree;
use pagedb::bufpool::BufPool;
use pagedb::page::{MetaPage, PageType, META_PAGE_ID};
use pagedb::pager::Pager;

#[test]
fn test_btree_basic_insert_get_delete_scan() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    let pager = Pager::open(&db_path).unwrap();
    let bufpool = Arc::new(BufPool::new(pager, 100));

    // Allocate meta page at 0, root leaf at 1
    let mut meta_guard = bufpool.fetch_page_mut(META_PAGE_ID).unwrap();
    meta_guard.with_page_mut(|p| {
        MetaPage::init_new(p, 1, 2);
    });
    drop(meta_guard);

    let mut root_guard = bufpool.fetch_page_mut(1).unwrap();
    root_guard.with_page_mut(|p| {
        pagedb::page::SlottedNode::new(p, 1, PageType::Leaf);
    });
    drop(root_guard);

    let btree = BTree::new(bufpool.clone(), 1, 2);

    // Insert 500 keys
    for i in 0..500u32 {
        let k = format!("key_{:05}", i).into_bytes();
        let v = format!("val_{:05}", i).into_bytes();
        btree.insert(&k, &v).unwrap();
    }

    // Verify all keys present
    for i in 0..500u32 {
        let k = format!("key_{:05}", i).into_bytes();
        let expected = format!("val_{:05}", i).into_bytes();
        let val = btree.get(&k).unwrap();
        assert_eq!(val, Some(expected));
    }

    // Verify non-existent key
    assert_eq!(btree.get(b"non_existent").unwrap(), None);

    // Scan
    let scan_all = btree.scan(..).unwrap();
    assert_eq!(scan_all.len(), 500);

    // Check invariants
    let stats = btree.check_invariants().unwrap();
    assert!(stats.height >= 2);
    assert_eq!(stats.total_keys, 500);

    // Delete odd keys
    for i in (1..500u32).step_by(2) {
        let k = format!("key_{:05}", i).into_bytes();
        let deleted = btree.delete(&k).unwrap();
        assert!(deleted);
    }

    let scan_remaining = btree.scan(..).unwrap();
    assert_eq!(scan_remaining.len(), 250);

    let stats_after = btree.check_invariants().unwrap();
    assert_eq!(stats_after.total_keys, 250);
}

#[test]
fn test_btree_model_against_std_btreemap() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("model.db");

    let pager = Pager::open(&db_path).unwrap();
    let bufpool = Arc::new(BufPool::new(pager, 500));

    let mut meta_guard = bufpool.fetch_page_mut(META_PAGE_ID).unwrap();
    meta_guard.with_page_mut(|p| {
        MetaPage::init_new(p, 1, 2);
    });
    drop(meta_guard);

    let mut root_guard = bufpool.fetch_page_mut(1).unwrap();
    root_guard.with_page_mut(|p| {
        pagedb::page::SlottedNode::new(p, 1, PageType::Leaf);
    });
    drop(root_guard);

    let btree = BTree::new(bufpool.clone(), 1, 2);
    let mut model = BTreeMap::new();

    let mut rng_state = 123456789u64;
    let mut next_rand = || {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        rng_state
    };

    for step in 0..1000 {
        let op = next_rand() % 3;
        let k_num = (next_rand() % 300) as u32;
        let key = format!("k_{:04}", k_num).into_bytes();

        match op {
            0 => {
                // Insert
                let val = format!("val_{}_{}", k_num, step).into_bytes();
                btree.insert(&key, &val).unwrap();
                model.insert(key, val);
            }
            1 => {
                // Delete
                let btree_res = btree.delete(&key).unwrap();
                let model_res = model.remove(&key).is_some();
                assert_eq!(btree_res, model_res);
            }
            _ => {
                // Get
                let btree_res = btree.get(&key).unwrap();
                let model_res = model.get(&key).cloned();
                assert_eq!(btree_res, model_res);
            }
        }

        if step % 200 == 0 {
            let stats = btree.check_invariants().unwrap();
            assert_eq!(stats.total_keys, model.len());

            let scanned = btree.scan(..).unwrap();
            let model_entries: Vec<(Vec<u8>, Vec<u8>)> =
                model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(scanned, model_entries);
        }
    }

    let final_stats = btree.check_invariants().unwrap();
    assert_eq!(final_stats.total_keys, model.len());
}
