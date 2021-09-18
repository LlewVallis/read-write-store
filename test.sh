#!/bin/sh

RUST_BACKTRACE=1 MIRIFLAGS="-Zmiri-ignore-leaks -Zmiri-disable-isolation" cargo miri test || exit 1
RUST_BACKTRACE=1 MIRIFLAGS="-Zmiri-ignore-leaks -Zmiri-disable-isolation" cargo miri test --release || exit 1
RUST_BACKTRACE=1 RUSTFLAGS="--cfg loom --cfg debug_assertions" cargo test --release --lib loom || exit 1
