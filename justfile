default: run

# fmt + clippy --fix + fmt (nightly fmt for import grouping)
fix:
    cargo +nightly fmt
    cargo clippy --fix --allow-dirty --allow-staged --all-features --all-targets
    cargo +nightly fmt
    cargo fmt

fixn:
    cargo +nightly fmt
    cargo +nightly clippy --fix --allow-dirty --allow-staged --all-features --all-targets
    cargo +nightly fmt
    cargo fmt

run *ARGS:
    RUST_LOG="${RUST_LOG:-info,mykrut_app=debug,mykrut_core=debug}" cargo run --bin mykrut -- {{ARGS}}

runr *ARGS:
    RUST_LOG="${RUST_LOG:-info,mykrut_app=debug,mykrut_core=debug}" cargo run --release --bin mykrut -- {{ARGS}}

run-trace *ARGS:
    RUST_LOG="trace,wgpu_core=info,wgpu_hal=info,naga=info,winit=info" cargo run --bin mykrut -- {{ARGS}}

build:
    cargo build

buildr:
    cargo build --release

check:
    cargo check --workspace --all-targets

test:
    cargo test --workspace -- --nocapture

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

clippy-pedantic:
    cargo clippy --workspace --all-targets -- -W clippy::pedantic -A clippy::module_name_repetitions -A clippy::missing_errors_doc

ci: fmt-check clippy test

clean:
    cargo clean

tree:
    @cargo tree -e normal | grep -oE '[a-z0-9_-]+-sys v[0-9.]+' | sort -u > /tmp/mykrut-sys-list.txt; \
    ALLOWED='libc libdbus-sys inotify-sys dirs-sys wayland-sys yeslogic-fontconfig-sys linux-raw-sys errno-sys mio-sys'; \
    UNEXPECTED=$(grep -vE "^($(echo $ALLOWED | tr ' ' '|'))" /tmp/mykrut-sys-list.txt | grep -v '^libc') ; \
    if [ -z "$UNEXPECTED" ]; then echo "OK: only kernel/Slint-stack *-sys present"; cat /tmp/mykrut-sys-list.txt; \
    else echo "UNEXPECTED *-sys (review!):"; echo "$UNEXPECTED"; exit 1; fi

deps:
    cargo tree --workspace

bloat:
    cargo bloat --release --crates -n 30

watch:
    cargo watch -x 'check --workspace' -x 'test --workspace'

## release binaries

# Linux portable binary (old glibc compat via zigbuild).
# Requires: cargo install cargo-zigbuild && rustup target add x86_64-unknown-linux-gnu
binaries:
    rm -rf binaries || true
    mkdir binaries
    cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28
    cp target/x86_64-unknown-linux-gnu/release/mykrut binaries/linux_mykrut

## dependencies / install

upgrade:
    cargo +nightly -Z unstable-options update --breaking
    cargo update

install *ARGS:
    cargo install --path app {{ARGS}}

## setup
# init = one-time bootstrap of a fresh checkout; sync = re-sync after pulling.

init:
    # Rust toolchain is pinned via rust-toolchain.toml (rustup auto-installs it).
    cargo fetch

sync:
    cargo fetch

setup_sanitizer:
    rustup install nightly
    rustup component add rust-src --toolchain nightly-x86_64-unknown-linux-gnu
    rustup component add llvm-tools-preview --toolchain nightly-x86_64-unknown-linux-gnu

## debugging / sanitizers
# Valgrind (requires valgrind installed)
runv *ARGS:
    cargo build --bin mykrut --profile rdebug
    valgrind --leak-check=full --show-leak-kinds=definite --track-origins=yes target/rdebug/mykrut {{ARGS}}

# AddressSanitizer (requires nightly + rust-src + llvm-tools-preview; see setup_sanitizer)
runs *ARGS:
    ASAN_OPTIONS="symbolize=1:detect_leaks=0" RUST_BACKTRACE=1 ASAN_SYMBOLIZER_PATH=$(which llvm-symbolizer) RUSTFLAGS="-Zsanitizer=address" cargo +nightly run --target x86_64-unknown-linux-gnu --bin mykrut {{ARGS}}

## profiling
# perf permissions (one-time):
#   echo '-1' | sudo tee /proc/sys/kernel/perf_event_paranoid

# just samply
samply *ARGS:
    cargo build --bin mykrut --profile rdebug
    samply record target/rdebug/mykrut {{ARGS}}

# just heaptrack; heaptrack_gui "$(ls -t heaptrack.* 2>/dev/null | head -n1)"
heaptrack *ARGS:
    cargo build --bin mykrut --profile rdebug
    heaptrack target/rdebug/mykrut {{ARGS}}

# just hotspot; hotspot perf.data
hotspot *ARGS:
    cargo build --bin mykrut --profile rdebug
    perf record -o perf.data --call-graph dwarf,8192 --aio -z --sample-cpu target/rdebug/mykrut {{ARGS}}
    hotspot
