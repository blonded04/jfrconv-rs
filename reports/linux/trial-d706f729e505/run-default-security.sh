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
apt-get install -y --no-install-recommends build-essential python3 rustc cargo
rustc --version
cargo --version
java -version
mkdir -p /work/source-snapshot
for file in Cargo.toml Cargo.lock; do cp /project/$file /work/source-snapshot/$file; done
for dir in src assets tests tools; do cp -R /project/$dir /work/source-snapshot/; done
chmod -R u+rwX /work/source-snapshot
export CARGO_HOME=/work/cargo
export CARGO_TARGET_DIR=/work/linux-target
cd /work/source-snapshot
cargo build --release --locked --offline
sha256sum /work/linux-target/release/jfrconv > /work/linux-results/rust-binary.sha256
cargo test --release --locked --offline --test memory_limits -- --nocapture --test-threads=1 > /work/linux-results/memory-tests.log 2>&1
cat /work/linux-results/memory-tests.log
python3 /project/tools/upstream_tests.py run --dest /work/v41-java --suite v4.1 --test NativememTests.ldpreload
JFRCONV_BINARY=/work/linux-target/release/jfrconv python3 /project/tools/upstream_tests.py run --dest /work/v41-rust --suite v4.1 --test NativememTests.ldpreload
printf 'Linux default-security checks completed\n'
