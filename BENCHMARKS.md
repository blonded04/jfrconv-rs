# jfrconv-rs performance and validation evidence

## v0.2.1 targeted followup

Current macOS arm64 executable: SHA-256 `654cce62946df6a1bfc25d8e8a7334c32506f37381fa64e8d936a9fe86f55497`. These **five cases were declared before measurement**, with **21 measured pairs each** (105 total), equal one-process warmups, alternating order and separately retained first pairs. All outputs match Java 4.1. All five rows improve median RSS, p50 and nearest-rank p95. Every raw trial is retained; no outliers were removed. File page-cache state is uncontrolled.

| Workload | Pairs | RSS MiB, Java / Rust | p50 ms, Java / Rust | p95 ms, Java / Rust |
|---|---:|---:|---:|---:|
| 2M collapsed | 21 | 93.70 / 3.41 | 109.0 / 29.8 | 117.2 / 33.5 |
| 10M collapsed | 21 | 197.20 / 3.58 | 234.0 / 139.5 | 273.2 / 158.7 |
| 2M html | 21 | 94.58 / 3.42 | 113.5 / 29.6 | 127.1 / 32.4 |
| 2M diverse HTML --lines | 21 | 198.05 / 57.14 | 553.4 / 310.5 | 593.1 / 345.8 |
| 2M heatmap | 21 | 249.12 / 3.77 | 275.5 / 128.7 | 300.1 / 138.4 |

Raw results: [collapsed](benchmarks/v0.2.1-collapsed.json), [HTML](benchmarks/v0.2.1-html.json), [diverse HTML](benchmarks/v0.2.1-diverse-html.json), [heatmap](benchmarks/v0.2.1-heatmap.json), [predeclared plan](benchmarks/v0.2.1-plan.json), [compact summary](benchmarks/v0.2.1-summary.json).

Fresh checks passed: **72 macOS and 73 Linux Cargo tests**, format and strict Clippy, **71 semantic checks**, and **all 15 scoped published converter cases** (14 macOS + 1 Linux-only, unchanged assertions). The current historical suite passed on its first attempt. All 18 UI pairs pass static/data comparisons; the actual original flamegraph JavaScript has identical draw/event results at DPR 1 and 2. See [UI compatibility](UI-PARITY.md), [verification index](benchmarks/verification.json) and [Linux source manifest](reports/linux/summary.json). The 5,000-mutation smoke test below remains labeled v0.2.0; the parser source is unchanged.

## v0.2.0 previous full matrix

The 277 measured pairs and 21 rows below describe the **v0.2.0** macOS arm64 executable, SHA-256 `d55d0e0b66f4b9f577947ce30cd3a9789e0efd9a30c5d729906ade9c16a7f1ff`. Every row in the v0.2.0 tables was measured against that executable, with output equivalence checked before accepting the comparison. Each row had lower median high-water RSS and lower p50 and p95 latency than its Java reference on this host. These are measured results for the listed recordings, not a guarantee for every recording or host.

### Environment and method

- Host: macOS 15.8.2, arm64, 8 logical CPUs. Rust 1.91.1 release build with thin LTO; Java reference runtime OpenJDK 25.0.1 Homebrew.
- Main Java reference: async-profiler **v4.1**, source commit `5930966a92860f6e5d2d89ab6faab5815720bad9`, rebuilt from clean public source. Each JSON records the exact Java JAR and native binary hashes, input hash and size, flags, command, all raw trials and execution order.
- Each timed conversion runs in a **fresh process**, including startup and output writing. Both implementations receive one equal untimed warmup. Measured pairs alternate execution order. p50 is the median; p95 is the nearest-rank percentile. With 5 or 11 pairs, p95 is the largest observed sample; the key baseline cases use 21 pairs.
- A separate first fresh-process pair is retained in each JSON and excluded from percentiles. File page-cache state is uncontrolled; these are not controlled cold-disk measurements. Each Java process starts its own JVM and JIT.
- RSS is the median of each launched command's `wait4` high-water RSS, excluding the Python harness. It is **not a simultaneous sum of supervisor and worker RSS** in the limited run. Java/Rust comparisons are paired within each row; absolute times vary across batches and host load.
- The synthetic event-count fixtures have 4 chunks and 3 stack IDs per chunk. The diverse fixture has 4 chunks and 50,000 constant-pool stack IDs per chunk; line numbers exercise many distinct output prefixes. Real recordings come from the included Java workload, recorded with JDK 25 JFR and native async-profiler 4.1. Real JDK rows use all execution states, since v4.1's AP-specific `--cpu` default-state filter yields an empty JDK result.

### Primary collapsed-stack comparisons

| Workload / mode | Pairs | RSS MiB, Java / Rust | p50 ms, Java / Rust | p95 ms, Java / Rust |
|---|---:|---:|---:|---:|
| 100k CPU events | 21 | 47.17 / 3.41 | 56.4 / 5.7 | 69.2 / 9.6 |
| 2M CPU events | 21 | 94.05 / 3.41 | 111.4 / 29.2 | 114.9 / 31.2 |
| 10M CPU events | 21 | 196.34 / 3.48 | 209.3 / 128.3 | 223.4 / 132.7 |
| 2M events / 50k CP stacks per chunk (`--lines`) | 21 | 205.58 / 56.48 | 492.9 / 265.1 | 578.4 / 291.5 |
| Real JDK recording (all states) | 11 | 43.72 / 3.78 | 50.6 / 7.0 | 53.3 / 11.9 |
| Real async-profiler 4.1 recording (all states) | 11 | 44.45 / 4.09 | 54.9 / 6.0 | 62.0 / 6.5 |

Raw data: [defaults](benchmarks/defaults-collapsed.json), [diverse stacks](benchmarks/high-cardinality-collapsed.json), [real recordings](benchmarks/real-recordings-collapsed.json).

### All supported output formats

These rows use the 2M-event recordings. HTML, pprof, gzip and OTLP rows pass `--lines`; heatmaps preserve their native method/line/BCI data. The 5-pair diverse heatmap row is smaller because the Java reference is expensive; its raw first pair, warmups and all 5 measured pairs remain available.

| Workload / mode | Pairs | RSS MiB, Java / Rust | p50 ms, Java / Rust | p95 ms, Java / Rust |
|---|---:|---:|---:|---:|
| html — 2M CPU events | 11 | 94.62 / 3.55 | 113.1 / 29.0 | 117.4 / 42.6 |
| html — 2M events / 50k CP stacks per chunk | 11 | 197.84 / 55.19 | 511.3 / 292.4 | 560.6 / 325.8 |
| pprof — 2M CPU events | 11 | 94.36 / 3.52 | 113.5 / 30.9 | 127.7 / 31.7 |
| pprof — 2M events / 50k CP stacks per chunk | 11 | 166.58 / 49.11 | 216.8 / 173.8 | 266.0 / 202.3 |
| pb.gz — 2M CPU events | 11 | 93.72 / 3.77 | 106.4 / 29.2 | 107.9 / 30.9 |
| pb.gz — 2M events / 50k CP stacks per chunk | 11 | 164.14 / 48.69 | 293.5 / 193.9 | 328.0 / 215.3 |
| otlp — 2M CPU events | 21 | 98.38 / 3.50 | 111.2 / 29.8 | 122.1 / 33.8 |
| otlp — 2M events / 50k CP stacks per chunk | 21 | 209.56 / 48.00 | 307.5 / 255.6 | 346.0 / 319.8 |
| heatmap — 2M CPU events | 11 | 249.20 / 3.80 | 277.8 / 126.9 | 419.8 / 144.7 |
| heatmap — 2M events / 50k CP stacks per chunk | 5 | 297.69 / 72.23 | 13135.3 / 802.8 | 16446.5 / 1695.5 |

Raw data: [HTML](benchmarks/format-html.json), [pprof](benchmarks/format-pprof.json), [pb.gz](benchmarks/format-pb.gz.json), [OTLP](benchmarks/format-otlp.json), [heatmap low cardinality](benchmarks/heatmap-low-cardinality.json), [heatmap diverse stacks](benchmarks/heatmap-high-cardinality.json).

Collapsed outputs are compared by full stack/count maps. HTML is imported through the original Java converter and compared as collapsed stacks. pprof and OTLP comparisons resolve dictionaries and check sample values, names, locations, labels/attributes and timing/chunk structure. Heatmap comparison decodes the native 4.1 payload and hashes normalized 20 ms block/stack/count content, including line/BCI/frame kinds, independent of IDs and compression order. Verification runs outside the timed conversion.

### Additional references and sustained conversion

| Workload / mode | Pairs | RSS MiB, Java / Rust | p50 ms, Java / Rust | p95 ms, Java / Rust |
|---|---:|---:|---:|---:|
| 2M CPU events (SerialGC, 64 MiB heap) | 11 | 49.25 / 3.56 | 294.2 / 82.3 | 324.7 / 92.5 |
| 10M CPU events (SerialGC, 64 MiB heap) | 11 | 49.39 / 3.58 | 460.6 / 291.4 | 646.1 / 404.8 |
| 8×2M events in one invocation; Rust 16 MiB allocation / 32 MiB worker RSS guard | 11 | 208.22 / 3.75 | 587.8 / 415.5 | 907.9 / 650.1 |
| 10M events; current public Java e98d8c2 | 7 | 196.91 / 3.41 | 419.5 / 248.6 | 600.7 / 332.0 |
| 10M events; existing user Java JAR | 7 | 198.00 / 3.55 | 714.7 / 381.4 | 781.5 / 404.1 |

The tuned reference uses `-Xms16m -Xmx64m -XX:+UseSerialGC`. The sustained row converts the same input 8 times within each fresh process; the final output is compared with Java and raw per-process measurements are retained. Its RSS is the command's reported high-water value, not aggregate simultaneous supervisor-plus-worker memory. The current-public reference is rebuilt from clean commit `e98d8c2b2d6e954ed1b39685fed1b25dc08db603`. The existing user JAR was read without rebuilding or modifying its repository; its hash is `f89a41fd8a5d107a6b5278ad0d6796b17bd94c275700789222b7d75f89402119`, and it is not asserted to exactly match uncommitted Java source changes.

Raw data: [tuned Java](benchmarks/tuned-java-collapsed.json), [sustained limited run](benchmarks/sustained-limited.json), [current public Java](benchmarks/current-public-java.json), [existing user JAR](benchmarks/existing-user-jar.json).

### v0.2.0 correctness and public-test scope

- **68 macOS Cargo tests passed:** 48 unit, 1 multi-file path, 3 stream-output, 6 compatibility, 4 edge and 6 memory-limit tests. Format check and strict Clippy passed. Device/descriptor tests check actual `/dev/null`, regular-file-backed stdout offsets, descriptor alias identity and task-owned FIFO identity in ordinary and limited modes; system-device probes are deliberately skipped for root test runners.
- **71 differential/structural checks passed** against Java 4.1, including real JDK/AP recordings, constructors in Java-importable HTML, every output codec, span tags and malformed-input/output-preservation cases. [Report](benchmarks/validation.json).
- **5,000 deterministic malformed-JFR mutations passed** without panic, signal crash or timeout. This is a smoke fuzz test, not a security audit. [Report](benchmarks/parser-mutations.json).
- **15 published converter cases passed within the stated scope:** all 9 converter-consuming declarations in v4.1, plus all 6 tests in the current public `JfrconverterTests` class. 14 run on macOS; the historical Linux-only `NativememTests.ldpreload` runs on Linux. Published assertion files are byte-for-byte unchanged; transport adapters invoke the Rust CLI with the actual output path, including real `/dev/null`. This does not claim execution of the entire current async-profiler profiler test tree or post 4.1 features such as native-lock profiling.
- Historical `regularPeak` is sensitive to native profiler attach/start timing. One v0.2.0 attempt violated the fixture's first 2.5 s no-peak assumption. Java and Rust produced identical 112 samples, including 20 early `calculateTop` samples, from that failed recording. The full unchanged historical suite passed on the retained retry. [Failure log](reports/timing-sensitive-fixture/failed-attempt.log), [reference replay](reports/timing-sensitive-fixture/reference-replay.json), [final historical report](reports/releases/v0.2.0/public-macos/v41-rust.json), [current dedicated report](reports/releases/v0.2.0/public-macos/current-rust.json).
- **69 Linux release Cargo tests passed**, including 7 memory-limit tests, plus the Linux-only published case passed for both Java and Rust. All 29 source/assets/Cargo/test hashes match the v0.2.0 macOS source before and after the Linux build and tests. The two system-device/descriptor probe branches return early under the root container user; the task-owned FIFO and Node decoder tests ran, and the system probes passed separately on non-root macOS. This is an Ubuntu 26.04/glibc 2.43 functional test build, rather than a portable Linux release artifact. [Linux summary and source manifest](reports/linux/trial-b80e9b7c6311-v0.2.0/summary.json), [full Cargo log](reports/linux/trial-b80e9b7c6311-v0.2.0/cargo-tests-final.log), [final macOS replay of Linux recording](reports/linux/final-macos-replay.json).

The UI embeds the original 4.1 flamegraph and heatmap resources. Span/trace filters and differential graphs are extensions beyond 4.1. Known 4.1 native-leak/heatmap raw-ID reuse bugs are fixed: earlier chunks keep their own methods, and TLAB/outside-TLAB allocation frames remain distinct. These intentional corrections can differ from the buggy Java 4.1 output for constructed aliasing cases; ordinary real-profile heatmap digests and all benchmark outputs match the reference.

### Memory-limit scope

`--memory-limit` bounds tracked live Rust allocations. `--max-rss` supervises the **worker** with a scheduled polling interval of about 2 ms, so macOS can briefly overshoot between observations. The derived allocation budget is `max(RSS limit − 16 MiB, 1 MiB)`, limited further by an explicit allocation budget. That 16 MiB is accounting headroom, not preallocated resident memory; the supervisor is outside the worker budget. Linux additionally checks startup virtual size and sets a conservative `RLIMIT_AS`, which may reject a run that would fit the requested resident size. A limit failure preserves ordinary existing outputs and cleans supervisor-owned temporary files. Streams are copied to existing endpoints after successful conversion and preserve device/FIFO/descriptor identity.

A separate earlier diagnostic repeated one input 64 times in one process: tracked live allocations stayed between 10,173 and 14,772 bytes, peak reserved allocations between 163,036 and 163,083 bytes, and reported RSS between 3.27 and 3.77 MiB. This is prior-candidate evidence (`86c038…`), not a measurement of the v0.2.0 executable. [Retained memory-plateau diagnostic](reports/prior-candidates/memory-plateau-86c038.json).

### Reproduction and retained trials

Build with `cargo build --release --locked`, generate fixtures with `python3 tools/fixtures.py`, reproduce real JFRs with `tools/real_fixtures.py`, run the 71 checks with `tools/validate.py`, and use `tools/benchmark.py --help` for the exact pairing procedure. Each raw JSON's command line is the complete invocation used for that row. `tools/upstream_tests.py` stages clean public Git revisions and records assertion-source hashes before bridging transport.

[v0.2.0 benchmark summary](benchmarks/summary.json), [v0.2.0 verification index](reports/releases/v0.2.0/verification.json), [v0.2.0 release/source manifest](reports/releases/v0.2.0.json). Earlier a2ed10 candidate trials remain under [prior-candidate reports](reports/prior-candidates/a2ed10/); they retain their actual hashes and are excluded from these final tables. The earlier OTLP tail regression and the subsequent byte-identical optimization are retained in [old/new optimization trials](reports/otlp-optimization/old-new-pairs.json), [byte-identity checks](reports/otlp-optimization/byte-identity.json), and [633c9e OTLP trial](reports/prior-candidates/633c9e/otlp-21-pairs.json).
