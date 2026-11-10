# jfrconv-rs

Native Rust converter targeting async-profiler **4.1**, with the original HTML
interfaces, span filters, and differential flame graphs. No JVM is required.

## Build

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

Rust 1.85+ is required by locked dependencies. Supported operating systems are
Linux and macOS. The supplied macOS binary is arm64.

## Convert

```sh
./target/release/jfrconv --cpu recording.jfr profile.html
./target/release/jfrconv --alloc --total recording.jfr allocations.collapsed
./target/release/jfrconv --cpu -t recording.jfr profile.pb.gz
./target/release/jfrconv -o heatmap recording.jfr heatmap.html
./target/release/jfrconv --latency 100 --tag 'request-.*' recording.jfr requests.html
./target/release/jfrconv --trace --total recording.jfr method-time.collapsed
./target/release/jfrconv --diff base.collapsed current.collapsed difference.html
```

JFR inputs support HTML, collapsed, pprof, pb.gz, OTLP, and heatmap. Collapsed
stacks and async-profiler flame-graph HTML support HTML/collapsed conversions.
Multiple independent inputs take an existing output directory. With a single
input and no output path, the converter writes to the working directory.

`--cpu` and `--wall` retain 4.1's `STATE_DEFAULT` thread-state convention. For a
JDK recording containing RUNNABLE execution samples, omit these switches to
include those samples. See `--help` for all filters and formatting options.

## Original interfaces

* `assets/flame.html` is the unchanged async-profiler 4.1 flame-graph template.
* `assets/heatmap.html` is the unchanged async-profiler 4.1 heatmap template,
  including its JavaScript decoder. Rust emits its native binary payload.
* Differential flame graphs use the unchanged official upstream diff template
  from public commit `e98d8c2b2d6e954ed1b39685fed1b25dc08db603` (2026-11-09).

There is no separate replacement UI. Data encoding may differ while remaining
compatible with the official decoders. Heatmap samples are sorted externally;
branch segments share stack prefixes and keep browser stack storage linear in
trie size. Original line numbers, bytecode indexes, frame kinds, time blocks,
and per-record counts are preserved.

See [UI-PARITY.md](UI-PARITY.md) for same-input examples, browser screenshots,
renderer/event comparisons, and the limits of pixel equality.

## Memory limits

```sh
# A hard allocation budget and an operating-system RSS guard
./target/release/jfrconv --memory-limit 64M --max-rss 128M --cpu input.jfr out.html

# Observe memory after each converted file in a long batch
./target/release/jfrconv --memory-stats a.jfr b.jfr c.jfr output-directory/
```

`K`, `M`, `G`, `KiB`, `MiB`, and `GiB` use binary units. `--memory-limit` bounds
all live Rust allocations, including transient capacity during reallocations.
It is intentionally conservative: old and new allocations are both reserved
before reallocating. This is a limit on allocated bytes, not a claim that the
allocator's reserved pages or shared libraries have the same size.

Limited conversions run in an isolated worker. A supervisor owns its temporary
files. Exceeding an allocation/RSS limit returns **exit code 75**, reports a
specific error, and removes spill/output scratch files. A limited worker stages
outputs; the supervisor publishes them only after the worker exits successfully.
An ordinary conversion replaces each output atomically after it is complete.
Character devices, FIFOs, and descriptor paths such as `/dev/null` and
`/dev/stdout` receive a copy of the completed output through an existing handle;
they are never renamed or truncated. Descriptor aliases retain their current
offset. Streams cannot provide regular-file atomic replacement; limited workers
still finish successfully before the supervisor starts publishing to them.

`--max-rss` checks actual worker resident memory every 2 ms. On **Linux**, it
also sets `RLIMIT_AS`, conservatively bounding mapped address space; this may
reject a workload even when its RSS would fit. On **macOS**, the RSS check is a
sampled guard and can overshoot briefly between samples. For a kernel limit on
an entire process tree, use Linux cgroup v2 `memory.max` (or a container memory
limit). The small supervisor is separate from the worker's RSS budget.
An RSS limit also supplies a conservative allocation budget with 16 MiB reserved
for runtime/allocator overhead (at least 1 MiB of allocation space); an explicit
`--memory-limit` can reduce this further.
[Linux resource limits](https://man7.org/linux/man-pages/man2/getrlimit.2.html),
[cgroup memory controller](https://docs.kernel.org/admin-guide/cgroup-v2.html).

Memory usage is proportional to the largest chunk's dictionaries, distinct
output stack prefixes, outstanding native allocations, and selected span
intervals (merged before the filtering pass). The default has no artificial memory ceiling; use the explicit limits
for production constraints. Repeated input files release their dictionaries,
aggregation tables, and profiles. `--memory-stats` reports live allocations,
peak reserved bytes, and current RSS after each file; allocator page retention
may keep RSS above the live heap size without a growing live allocation set.

The parser uses a 128 KiB buffer, bounded varint lookahead, and a per-chunk event
encoding cache. It neither loads nor maps the entire recording. There is no
per-CPU-sample allocation. Aggregation uses integer table slots and compact
representative records. Protobuf output streams without a full serialized
profile buffer. Native leak analysis uses stable external sorting with bounded
runs; outstanding addresses share resolved paths. Heatmap/OTLP use disk spools.
Span filters perform a second pass and retain merged per-thread intervals.

## Compatibility

The core reference is tag `v4.1`, commit
`5930966a92860f6e5d2d89ab6faab5815720bad9`. Span/MethodTrace and `--diff` are
extensions. `--diff` writes base/current counts in collapsed output; its HTML
retains upstream differential markers and can reconstruct the current profile.

Whole-frame filters support Java's `\Q…\E` quoting. The Rust regex engine
explicitly rejects unsupported lookaround and backreferences. OTLP uses the
Profiles protobuf schema bundled with 4.1. Negative time offsets preserve 4.1's
progressively observed recording-end behavior; local calendar fractions are
literal milliseconds (`.5` means 5 ms). Native leak stack identities remain
correct across chunks with reused IDs. Leak grain rounding can differ from the
original raw-ID grouping. Heatmap also fixes 4.1's extra-stack cache aliasing:
reused chunk-local IDs and TLAB/outside-TLAB allocations retain their own method
identities and frame kinds. Multi-chunk allocation/lock heatmaps therefore differ
from the original in those buggy cases. Multiple ordinary inputs require a directory to avoid
repeatedly overwriting one output path.

## Verification and benchmarks

See [BENCHMARKS.md](BENCHMARKS.md) for measured RSS/latency results, test scope,
limitations, and links to raw evidence. Public-test adapters in
`tools/upstream_tests.py` run unchanged upstream assertion sources, replacing only
Java-to-converter transport entry points with calls to the Rust executable.
They record the Git revision and verify test-source SHA256 hashes before running.
The historical 4.1 suite and the current public converter suite are reported
separately; platform prerequisites and any skips are recorded explicitly.

```sh
# Stage a pristine upstream revision and route converter calls through Rust
python3 tools/upstream_tests.py stage --source /path/to/async-profiler \
  --ref v4.1 --dest work/public-v41 --suite v4.1 --bridge
JFRCONV_BINARY="$PWD/target/release/jfrconv" \
  python3 tools/upstream_tests.py run --dest work/public-v41 --suite v4.1

# Semantic comparisons against the original Java converter
python3 tools/validate.py --rust target/release/jfrconv \
  --java-jar /path/to/v4.1/build/jar/jfr-converter.jar

# Reproduce deterministic recordings and process-level measurements
python3 tools/fixtures.py work/large.jfr --samples 10000000 --chunks 4
python3 tools/benchmark.py --rust target/release/jfrconv \
  --java-jar /path/to/v4.1/build/jar/jfr-converter.jar --input work/large.jfr
```

Unit/integration coverage includes recorded upstream golden outputs, clocks,
weighted samples, changed chunk IDs, native leak ordering, cross-chunk Span API
tags, MethodTrace leaves, empty protobuf stack locations, official HTML decoders,
diff reconstruction, deep stacks, malformed input, external sort runs, memory
budget failure cleanup, and repeated-input memory plateaus. Optional Node.js
checks execute the original JavaScript decoders. Mutation and throughput tests
have reproducible fixtures and separate reports.

## License

Apache-2.0; upstream attribution is preserved in `NOTICE`, source files, and
unchanged HTML assets. See `LICENSE`.
