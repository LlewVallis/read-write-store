#![feature(bench_black_box)]

use std::time::{Duration, Instant};

use rand::Rng;
use read_write_store::{Id, RwStore};
use std::hint::black_box;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[derive(Copy, Clone)]
struct Element {
    _inner: [u64; 8],
}

const ELEMENT: Element = Element {
    _inner: [42; 8]
};

const MAX_THREAD_COUNT: usize = 8;
const TRIALS: usize = 21;
const TARGET_BATCH_DURATION: Duration = Duration::from_millis((1000 / TRIALS) as u64);
const GRAPH_WIDTH: usize = 100;

const LARGE_STORE_SIZE: usize = 1_000_000;

fn main() {
    bench_mixed_workload("read heavy", LARGE_STORE_SIZE, 98.0, 0.0, 1.0, 1.0);

    bench_mixed_workload("write heavy", LARGE_STORE_SIZE, 0.0, 98.0, 1.0, 1.0);

    bench_mixed_workload("exchange", LARGE_STORE_SIZE, 10.0, 10.0, 40.0, 40.0);

    bench_mixed_workload("rapid grow", LARGE_STORE_SIZE, 5.0, 10.0, 80.0, 5.0);

    bench_mixed_workload("one contended", 1, 90.0, 10.0, 0.0, 0.0);

    bench_mixed_workload("ten contended", 10, 90.0, 10.0, 0.0, 0.0);

    bench_workload::<_, _, _, Element>("baseline", |_store, _batch_size| {
        |_store, _operations| {
            |_store| {
                black_box(1 + black_box(1));
            }
        }
    });
}

fn bench_mixed_workload(
    name: &'static str,
    store_size: usize,
    read_weight: f64,
    write_weight: f64,
    insert_weight: f64,
    remove_weight: f64,
) {
    bench_workload(name, |store, batch_size| {
        let total_weight = read_weight + write_weight + insert_weight + remove_weight;
        let removal_ratio = remove_weight / total_weight;
        let predicted_removals = (removal_ratio * batch_size as f64) as usize;
        let adjusted_store_size = store_size + predicted_removals / 2;

        let ids: Vec<_> = (0..adjusted_store_size)
            .map(|_| store.insert(ELEMENT))
            .collect();

        move |_store, operations| {
            enum Op {
                Read(Id),
                Write(Id),
                Insert,
                Remove(Id),
            }

            let mut rng = rand::thread_rng();

            let ops: Vec<_> = (0..operations)
                .map(|_| {
                    let id = ids[rng.gen_range(0..ids.len())];

                    let op_number = rng.gen::<f64>() * total_weight;

                    if op_number < read_weight {
                        Op::Read(id)
                    } else if op_number < read_weight + write_weight {
                        Op::Write(id)
                    } else if op_number < read_weight + write_weight + insert_weight {
                        Op::Insert
                    } else {
                        Op::Remove(id)
                    }
                })
                .collect();

            let mut index = 0;

            move |store| {
                let op = unsafe { ops.get_unchecked(index) };

                match op {
                    Op::Read(id) => {
                        black_box(store.read(*id));
                    }
                    Op::Write(id) => {
                        black_box(store.write(*id));
                    }
                    Op::Insert => {
                        black_box(store.insert(ELEMENT));
                    }
                    Op::Remove(id) => {
                        black_box(store.remove(*id));
                    }
                }

                index += 1;
            }
        }
    });
}

fn bench_workload<GlobalSetup, ThreadSetup, Operate, Element>(
    name: &'static str,
    workload: GlobalSetup,
) where
    GlobalSetup: Fn(&RwStore<Element>, usize) -> ThreadSetup,
    ThreadSetup: FnMut(&RwStore<Element>, usize) -> Operate,
    Operate: FnMut(&RwStore<Element>) + Send + Sync + 'static,
    Element: Sync + Send + 'static,
{
    let start = Instant::now();
    println!("Benchmarking `{}`", name);

    let batch_size = determine_batch_size(&workload);
    println!("> Using batch size {}", batch_size);

    let results: Vec<_> = (1..=MAX_THREAD_COUNT)
        .map(|threads| {
            let mut trials: Vec<_> = (0..TRIALS)
                .map(|_| {
                    let elapsed = time_workload(threads, batch_size, &workload);
                    let throughput = batch_size as f64 / elapsed.as_secs_f64();
                    throughput
                })
                .collect();

            trials.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = trials[trials.len() / 2];

            median
        })
        .collect();

    let max = results
        .iter()
        .max_by(|a, b| a.partial_cmp(b).unwrap())
        .unwrap();

    for (i, result) in results.iter().enumerate() {
        print!("> {:>8.0} Kops, {} threads [", result / 1000f64, i + 1);

        let width = result / max * GRAPH_WIDTH as f64;

        for column in 0..GRAPH_WIDTH {
            if width >= column as f64 {
                print!("#");
            } else {
                print!(" ");
            }
        }

        println!("]");
    }

    println!("> Done in {:.0} seconds", start.elapsed().as_secs_f64())
}

fn determine_batch_size<GlobalSetup, ThreadSetup, Operate, Element>(workload: &GlobalSetup) -> usize
where
    GlobalSetup: Fn(&RwStore<Element>, usize) -> ThreadSetup,
    ThreadSetup: FnMut(&RwStore<Element>, usize) -> Operate,
    Operate: FnMut(&RwStore<Element>) + Send + Sync + 'static,
    Element: Sync + Send + 'static,
{
    let mut batch_size = 1;

    loop {
        let duration = time_workload(1, batch_size, workload);
        let duration = duration.as_secs_f64();
        let target = TARGET_BATCH_DURATION.as_secs_f64();

        if (duration - target).abs() < target / 500.0 {
            return batch_size;
        }

        let factor = (target / duration).clamp(0.25, 4.0);
        batch_size = (batch_size as f64 * factor) as usize;
    }
}

fn time_workload<GlobalSetup, ThreadSetup, Operate, Element>(
    threads: usize,
    batch_size: usize,
    workload: &GlobalSetup,
) -> Duration
where
    GlobalSetup: Fn(&RwStore<Element>, usize) -> ThreadSetup,
    ThreadSetup: FnMut(&RwStore<Element>, usize) -> Operate,
    Operate: FnMut(&RwStore<Element>) + Send + Sync + 'static,
    Element: Sync + Send + 'static,
{
    let store = Arc::new(RwStore::new());
    let start_flag = Arc::new(AtomicBool::new(false));
    let ready_count = Arc::new(AtomicUsize::new(0));
    let times = Arc::new(vec![MaybeUninit::<Instant>::uninit(); threads]);

    let mut thread_setup = workload(&store, batch_size);

    let join_handles: Vec<_> = (0..threads)
        .map(|thread_index| {
            let store = store.clone();
            let start_flag = start_flag.clone();
            let ready_count = ready_count.clone();
            let times = times.clone();

            let operations = batch_size / threads;
            let mut operation = thread_setup(&store, operations);

            thread::spawn(move || {
                ready_count.fetch_add(1, Ordering::Release);
                while !start_flag.load(Ordering::Acquire) {}

                for _ in 0..operations {
                    operation(&store);
                }

                let finish = Instant::now();

                unsafe {
                    let time = &times[thread_index] as *const _ as *mut Instant;
                    time.write(finish);
                }

                black_box(store);
            })
        })
        .collect();

    while !ready_count.load(Ordering::Acquire) < threads {}

    let start = Instant::now();
    start_flag.store(true, Ordering::Release);

    for handle in join_handles {
        handle.join().unwrap();
    }

    let end = times
        .iter()
        .map(|time| unsafe { time.assume_init_ref() })
        .max()
        .unwrap();

    *end - start
}
