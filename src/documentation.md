A concurrent, unordered collection where each element has an internally generated ID and a
read-write lock.

A store has O(1) time complexity for insertion, removal and lookups, although memory
allocations triggered by insertion may cause a performance spike.

# Example

```
# use read_write_store::RwStore;
# use read_write_store::timeout::Timeout::DontBlock;
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

# Allocation behavior

* An empty store does not require any allocation.
* Once an element is inserted, a fixed size block of element slots will be allocated.
* As more space is needed, more blocks will be allocated which tend to increase in size. All
  allocated blocks are used alongside all previously allocated blocks; elements are never moved.
* All removed elements whose memory is ready for reuse are tracked, so removing an element may
  allocate.

# Caveats

* When a block of elements is allocated internally, it won't be deallocated until the store is
  dropped. This allows the element ID's to contain a pointer to the element that is valid for
  the lifetime of the store.
* Every element has a 8 bytes of overhead.
* The store itself has a non-trivial memory overhead.
* No facilities are currently provided for parallel iteration. Although this could be done, it
  would mean an element whose ID is not shared between threads would no longer be guaranteed to
  be accessible only from its inserting thread.
* For performance reasons, no facilities are provided for querying the number of elements in the
  store.
* A maximum of 2<sup>32</sup> - 1 elements can ever be inserted into any given store, including
  elements that have been removed. When debug assertions are enabled, breaking this invariant
  will panic, but when debug assertions are not enabled (i.e. in release mode) inserting more
  than 2<sup>32</sup> - 1 elements *may cause undefined behavior*.

# Safety

Each element inserted into a store is given an *internal* unique ID. This begs the question,
what happens when an ID from one store is used with another? When debug assertions are enabled,
a best effort attempt is made to panic when using an ID from a different store, but when they
are disabled (i.e. in release mode), or if this best effort fails, this
*may cause undefined behavior*.

Similarly, inserting more than 2<sup>32</sup> - 1 elements, including removed elements, into a
store will panic when debug assertions are enabled but *may cause undefined behavior* when they
are not.
