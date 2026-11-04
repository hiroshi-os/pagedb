use pagedb::page::*;
use proptest::prelude::*;

#[test]
fn test_page_header_crc_roundtrip() {
    let mut page = Page::new();
    page.set_page_id(42);
    page.set_page_type(PageType::Leaf);
    page.set_lsn(1001);
    page.update_crc();

    assert_eq!(page.page_id(), 42);
    assert_eq!(page.page_type().unwrap(), PageType::Leaf);
    assert_eq!(page.lsn(), 1001);
    assert!(page.verify_crc().is_ok());

    // Corrupt one byte
    page.data[100] ^= 0xFF;
    assert!(page.verify_crc().is_err());
}

#[test]
fn test_meta_page_roundtrip() {
    let mut page = Page::new();
    MetaPage::init_new(&mut page, 1, 2);

    let meta = MetaPage::read(&page).unwrap();
    assert_eq!(meta.root_page_id, 1);
    assert_eq!(meta.next_page_id, 2);
    assert_eq!(meta.last_checkpoint_lsn, 0);
    assert_eq!(meta.last_checkpoint_commit_ts, 0);

    let updated = MetaPage {
        root_page_id: 5,
        next_page_id: 10,
        last_checkpoint_lsn: 500,
        last_checkpoint_commit_ts: 42,
    };
    updated.write(&mut page, 500);

    let read_back = MetaPage::read(&page).unwrap();
    assert_eq!(read_back, updated);
}

#[test]
fn test_slotted_leaf_operations() {
    let mut page = Page::new();
    let mut node = SlottedNode::new(&mut page, 1, PageType::Leaf);

    assert_eq!(node.num_slots(), 0);
    node.insert_leaf_cell(0, b"apple", b"red").unwrap();
    node.insert_leaf_cell(1, b"banana", b"yellow").unwrap();
    node.insert_leaf_cell(1, b"apricot", b"orange").unwrap();

    assert_eq!(node.num_slots(), 3);
    assert_eq!(node.get_leaf_cell(0).unwrap(), (&b"apple"[..], &b"red"[..]));
    assert_eq!(
        node.get_leaf_cell(1).unwrap(),
        (&b"apricot"[..], &b"orange"[..])
    );
    assert_eq!(
        node.get_leaf_cell(2).unwrap(),
        (&b"banana"[..], &b"yellow"[..])
    );

    node.delete_slot(1).unwrap();
    assert_eq!(node.num_slots(), 2);
    assert_eq!(node.get_leaf_cell(0).unwrap(), (&b"apple"[..], &b"red"[..]));
    assert_eq!(
        node.get_leaf_cell(1).unwrap(),
        (&b"banana"[..], &b"yellow"[..])
    );
}

#[test]
fn test_slotted_internal_operations() {
    let mut page = Page::new();
    let mut node = SlottedNode::new(&mut page, 2, PageType::Internal);
    node.set_rightmost_child(99);

    node.insert_internal_cell(0, 10, b"key1").unwrap();
    node.insert_internal_cell(1, 20, b"key2").unwrap();

    assert_eq!(node.num_slots(), 2);
    assert_eq!(node.rightmost_child(), 99);
    assert_eq!(node.get_internal_cell(0).unwrap(), (10, &b"key1"[..]));
    assert_eq!(node.get_internal_cell(1).unwrap(), (20, &b"key2"[..]));
}

proptest! {
    #[test]
    fn proptest_arbitrary_page_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), PAGE_SIZE)) {
        let mut page_data = [0u8; PAGE_SIZE];
        page_data.copy_from_slice(&bytes);
        let page = Page::from_bytes(page_data);

        // verify_crc returns Err on mismatch or Ok if crc happens to match
        let _ = page.verify_crc();
        let _ = page.page_type();
        let _ = MetaPage::read(&page);
    }
}
