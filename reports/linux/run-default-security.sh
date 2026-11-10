#!/usr/bin/sh
set -eu
export PATH=/usr/bin:/usr/sbin:/bin:/sbin:/opt/java/openjdk/bin
export JAVA_HOME=/opt/java/openjdk
export DEBIAN_FRONTEND=noninteractive
mkdir -p /work/linux-results
printf 'Installing task-local compiler in ephemeral tmpfs\n'
dpkg --configure -a || true
apt-get update
apt-get install -f -y --no-install-recommends
apt-get install -y --no-install-recommends build-essential python3 rustc cargo nodejs
rustc --version
cargo --version
node --version
java -version
export CARGO_HOME=/work/cargo
export CARGO_TARGET_DIR=/work/linux-target
cd /work/source-snapshot
cargo build --release --locked --offline
sha256sum /work/linux-target/release/jfrconv > /work/linux-results/rust-binary-final.sha256
set +e
cargo test --release --locked --offline -- --nocapture --test-threads=1 > /work/linux-results/cargo-tests-final.log 2>&1
cargo_status=$?
cat /work/linux-results/cargo-tests-final.log
python3 /project/tools/upstream_tests.py run --dest /work/v41-java --suite v4.1 --test NativememTests.ldpreload --no-build
java_status=$?
JFRCONV_BINARY=/work/linux-target/release/jfrconv python3 /project/tools/upstream_tests.py run --dest /work/v41-rust --suite v4.1 --test NativememTests.ldpreload --no-build
rust_status=$?
printf '{"cargo":%s,"java":%s,"rust":%s}\n' "$cargo_status" "$java_status" "$rust_status" > /work/linux-results/final-exit-status.json
if test "$cargo_status" -ne 0 || test "$java_status" -ne 0 || test "$rust_status" -ne 0; then exit 1; fi
printf 'Final frozen Linux default-security checks completed\n'
