use loom::model;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::Arc as LoomArc;
use loom::thread;
use loom::thread::JoinHandle;
use std::sync::Arc as StdArc;

use crate::{Id, RwStore, Timeout};

type Operation = StdArc<dyn Fn(&RwStore<u32>, Id) + Sync + Send + 'static>;

#[test]
fn debug_assertions_enabled() {
    if !cfg!(debug_assertions) {
        panic!("debug assertions must be enabled when running loom tests");
    }
}

// We iterate over a set of functions that operate on a store, running each possible combination in
// parallel in an attempt to trigger a crash
#[test]
fn concurrent_operations_dont_crash() {
    let operations = create_concurrent_operations();

    for first_operation_index in 0..operations.len() {
        for second_operation_index in first_operation_index..operations.len() {
            let first_operation: &'static Operation =
                unsafe { &*(&operations[first_operation_index] as *const _) };

            let second_operation: &'static Operation =
                unsafe { &*(&operations[second_operation_index] as *const _) };

            for use_different_elements in &[false, true] {
                let mut model = model::Builder::new();
                model.preemption_bound = Some(4);

                model.check(move || {
                    let store = LoomArc::new(RwStore::new());

                    let first_id = store.insert(42);
                    let second_id = if *use_different_elements {
                        store.insert(24)
                    } else {
                        first_id
                    };

                    let (first_store, second_store) = (store.clone(), store.clone());
                    let first_thread =
                        thread::spawn(move || first_operation(&first_store, first_id));
                    let second_thread =
                        thread::spawn(move || second_operation(&second_store, second_id));

                    first_thread.join().unwrap();
                    second_thread.join().unwrap();
                });
            }
        }
    }
}

fn create_concurrent_operations() -> Vec<Operation> {
    let mut operations = Vec::new();

    fn operation(
        operations: &mut Vec<Operation>,
        f: impl Fn(&RwStore<u32>, Id) + Sync + Send + 'static,
    ) {
        operations.push(StdArc::new(f));
    }

    operation(&mut operations, |store, id| {
        store.read(id);
    });

    operation(&mut operations, |store, id| {
        let _lock = store.read(id);
        store.read(id);
    });

    operation(&mut operations, |store, id| {
        store.write(id);
    });

    operation(&mut operations, |store, id| {
        store.remove(id);
    });

    operation(&mut operations, |store, id| {
        if let Some(lock) = store.write(id) {
            store.remove_locked(lock);
        }
    });

    operation(&mut operations, |store, id| {
        let _ = store.read_with_timeout(id, Timeout::DontBlock);
    });

    operation(&mut operations, |store, id| {
        let _ = store.write_with_timeout(id, Timeout::DontBlock);
    });

    operation(&mut operations, |store, id| {
        let _ = store.remove_with_timeout(id, Timeout::DontBlock);
    });

    stack_operations(operations)
}

fn stack_operations(operations: Vec<Operation>) -> Vec<Operation> {
    let mut new_operations = Vec::<Operation>::new();

    for first in &operations {
        for second in &operations {
            let first = first.clone();
            let second = second.clone();

            new_operations.push(StdArc::new(move |store, id| {
                first(store, id);
                second(store, id);
            }))
        }
    }

    new_operations
}

#[test]
fn write_waits_for_read() {
    model(|| {
        let store = LoomArc::new(RwStore::new());
        let id = store.insert(42);
        let _guard = store.read(id).unwrap();

        let flag = LoomArc::new(AtomicBool::new(false));

        spawn((store.clone(), flag.clone()), move |(store, flag)| {
            store.write(id).unwrap();
            flag.store(true, Ordering::SeqCst);
        });

        assert!(!flag.load(Ordering::SeqCst));
    });
}

#[test]
fn write_waits_for_write() {
    model(|| {
        let store = LoomArc::new(RwStore::new());
        let id = store.insert(42);
        let _guard = store.write(id).unwrap();

        let flag = LoomArc::new(AtomicBool::new(false));

        spawn((store.clone(), flag.clone()), move |(store, flag)| {
            store.write(id).unwrap();
            flag.store(true, Ordering::SeqCst);
        });

        assert!(!flag.load(Ordering::SeqCst));
    });
}

#[test]
fn read_waits_for_write() {
    model(|| {
        let store = LoomArc::new(RwStore::new());
        let id = store.insert(42);
        let _guard = store.write(id).unwrap();

        let flag = LoomArc::new(AtomicBool::new(false));

        spawn((store.clone(), flag.clone()), move |(store, flag)| {
            store.read(id).unwrap();
            flag.store(true, Ordering::SeqCst);
        });

        assert!(!flag.load(Ordering::SeqCst));
    });
}

fn spawn<T: 'static, U: 'static>(param: U, f: impl FnOnce(U) -> T + 'static) -> JoinHandle<T> {
    thread::spawn(move || f(param))
}
