
use std::convert::TryInto;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let db: sled::Db = sled::open("my_db").unwrap();

    // insert and get
    for i in 0..1000 {
        let key = (i as u64).to_be_bytes();
        let val = (i as u64).to_be_bytes();
        db.insert(&key, &val)?;
    }

    let k1: u64 = 30;
    assert_eq!(db.get(k1.to_be_bytes()).unwrap().unwrap(), k1.to_be_bytes());

    let k2: u64 = 998;

    for kv in db.range(k2.to_be_bytes()..) {
        println!("entry {:?}", kv);
        println!("key {:?} value {:?}", 
        u64::from_be_bytes(kv.clone().unwrap().0[0..8].try_into().unwrap()),
        u64::from_be_bytes(kv.unwrap().1[0..8].try_into().unwrap())
    );

    }

/*
    // Atomic compare-and-swap.
    db.compare_and_swap(
        b"yo!",      // key
        Some(b"v1"), // old value, None for not present
        Some(b"v2"), // new value, None for delete
    )
    .unwrap();

    // Iterates over key-value pairs, starting at the given key.
    let scan_key: &[u8] = b"a non-present key before yo!";
    let mut iter = db.range(scan_key..);
    assert_eq!(&iter.next().unwrap().unwrap().0, b"yo!");
    assert_eq!(iter.next(), None);

    db.remove(b"yo!");
    assert_eq!(db.get(b"yo!"), Ok(None));

    let other_tree: sled::Tree = db.open_tree(b"cool db facts").unwrap();
    other_tree
        .insert(
            b"k1",
            &b"a Db acts like a Tree due to implementing Deref<Target = Tree>"[..],
        )
        .unwrap();
        */
    Ok(())
}
