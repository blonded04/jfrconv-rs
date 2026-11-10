# UI compatibility with async-profiler 4.1

Version 0.2.1 embeds the original 4.1 flamegraph and heatmap HTML, CSS and JavaScript byte-for-byte. The generated pages retain the original controls, labels, fonts, palettes, mouse actions and keyboard handlers. The Rust converter writes the original profile-data formats consumed by those pages. Differential graphs remain an extension using the newer public diff template.

Open the same-input examples: [Java 4.1 flamegraph](reports/ui-parity/example/java.html), [Rust flamegraph](reports/ui-parity/example/rust.html). These are ordinary generated HTML files. [Test-only seeded Java](reports/ui-parity/example/java.seeded.html) and [seeded Rust](reports/ui-parity/example/rust.seeded.html) use the same random sequence for visual comparison.

An actual browser comparison of the same allocation recording and fixed random seed is retained in this [Java 4.1 / Rust screenshot](reports/ui-parity/comparison.jpg).

## What was compared

The bounded corpus covers a two-chunk synthetic recording, a real async-profiler 4.1 recording and collapsed/HTML input. It exercises CPU and allocation profiles, reverse and inverted layouts, custom title markup, highlight, include/exclude filters and `--skip`, including Java's direct-HTML skip behavior.

The frozen macOS executable has SHA-256 `654cce62946df6a1bfc25d8e8a7334c32506f37381fa64e8d936a9fe86f55497`. All 36 pages in 18 pairs match the original static template. All 15 flamegraph pairs match full profiles, geometry, frame kinds and compilation counters; all 3 heatmap pairs match decoded data. At each of DPR 1 and DPR 2, 510 page-behavior assertions pass and all 195 interaction-phase comparisons have identical draw calls and state. [Compact summary](reports/ui-parity/summary.json), [static/data results](reports/ui-parity/static-data-report.json), [DPR 1 renderer results](reports/ui-parity/renderer-dpr1.json), [DPR 2 renderer results](reports/ui-parity/renderer-dpr2.json).

The renderer test runs the complete generated flamegraph application with a deterministic random source and a small DOM/canvas host. It compares frame geometry, every canvas draw call, DOM updates, tooltip text and the original event handlers. Each page checks initial highlight, hover, text selection, zoom, invert, search, next/previous match navigation, Escape, Ctrl-F, reset, cancel and mouseout. Search uses a stub response to the original `prompt()` call; the in-app browser blocks that dialog for both Java and Rust.

The [real-browser heatmap comparison](reports/ui-parity/browser-runtime-observation.json) also read the application's actual runtime state through test-only DOM instrumentation. Java and Rust had identical start time, all 101 block counts, zoom, scale, start block, min/max values and canvas coordinates, with identical recorded timeline draw calls. Separately, the [native decoder and timeline renderer checks](reports/ui-parity/heatmap-timeline.json) preserve leading/trailing empty blocks and verify normalized stack/count data, frame kinds, lines and BCIs. An apparent moving band in the in-app browser's screenshots was not reproduced in runtime data or draw calls, so no production codec change was made for that observation.

## Compatibility details and limits

Version 0.2.1 matches Java's accepted-stack height before `--skip` when its cutoff is at most one, preserves raw custom title HTML, and follows Java's integer-hash bucket ordering for same-named frame siblings. Java's rare treeified `HashMap` bucket reordering and a resize caused by an existing-key compute hit exactly at the capacity threshold are not emulated. The converter avoids retaining that full operation history to keep memory usage low. Such cases can still place equal-name siblings differently.

Flamegraph shades use the original `Math.random()` implementation, so separate production page loads naturally vary slightly. The test-only seeded copies use the same random sequence for comparison. Native heatmap frame shades hash method IDs; Rust's compression dictionaries can assign different IDs while preserving decoded methods, counts, time blocks and the unchanged UI. Those shades can therefore differ. Pixel equality across every recording and browser is not asserted.

The JavaScript test uses DOM/canvas mocks rather than a browser layout engine. Browser pixels, native dialogs and font rendering need real-browser checks. Only controlled, generated comparison pages should be passed to the test harness.

## Reproduce a renderer comparison

Generate both files from the same input and options, then run:

```sh
node tools/ui_renderer_parity.js --java java.html --rust rust.html \
  --output target/ui-parity --strict
```

The output contains a compact JSON report and seeded HTML copies; input files remain unchanged. `--dpr 2` exercises Retina-scale canvas calls. `--trace` additionally writes full frame/draw arrays. The manifest mode accepts multiple pairs with paths relative to the manifest. Heatmap entries receive seeded copies; their full interaction checks remain browser work.

The historical 277-pair performance matrix refers to the v0.2.0 `d55d0e0b…` executable. Version 0.2.1 has separately labeled targeted measurements in [BENCHMARKS.md](BENCHMARKS.md).
