# Read Write Store

A concurrent, unordered collection for Rust, where each element has an internally generated ID and a read-write lock.

A store has O(1) time complexity for insertion, removal and lookups, although memory allocations triggered by insertion may cause a performance spike.

## Example

```rust
// Note that we only need an immutable reference to the store
let store = RwStore::new();

// Inserting an element yields an ID
let id = store.insert(42);

{
    // You can read the element's value using that ID
    let read_lock = store.read(id).unwrap();
    assert_eq!(*read_lock, 42);

    // Concurrent reads are possible
    assert!(store.read_with_timeout(id, DontBlock).is_ok());
    // But reading and writing at the same time won't work
    assert!(store.write_with_timeout(id, DontBlock).is_err());
}

{
    // You can also acquire a write lock using an ID
    let mut write_lock = store.write(id).unwrap();
    *write_lock = 24;
    assert_eq!(*write_lock, 24);

    // Predictably, you cannot have multiple writers
    assert!(store.write_with_timeout(id, DontBlock).is_err());
}

// Elements can of course be removed using their ID
assert_eq!(store.remove(id), Some(24));

// Now if we try to read using the ID, it will fail gracefully
assert!(store.read(id).is_none());
```
