#!/usr/bin/env python3
"""Compare fresh-process peak RSS and whole-program p50/p95 conversion latency.

Both implementations get the same warmups, then alternate execution order for
all measured pairs. The first fresh-process pair is retained separately; file
page-cache state is uncontrolled and these are not claimed as cold-disk results.
Child wait4() RSS is bytes on macOS and KiB on Linux; Python-parent RSS excluded.

  python3 tools/benchmark.py --rust target/release/jfrconv --java-jar old.jar \
      --input recording.jfr --repeat 21 --warmup 1
  python3 tools/benchmark.py ... --repeat-inputs 8 --rust-flag=--max-rss \
      --rust-flag=64M
"""
from __future__ import annotations
import argparse
from collections import Counter
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import shutil
import subprocess
import sys
import tempfile
import time
from fixtures import generate


def file_hash(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(64 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def measure(command):
    with tempfile.TemporaryFile() as output:
        start = time.perf_counter()
        process = subprocess.Popen(list(map(str, command)), stdout=output, stderr=output)
        _, status, usage = os.wait4(process.pid, 0)
        elapsed = time.perf_counter() - start
        process.returncode = os.waitstatus_to_exitcode(status)
        if process.returncode:
            output.seek(0)
            raise RuntimeError(f'{command}\n{output.read().decode(errors="replace")}')
    rss_bytes = usage.ru_maxrss if sys.platform == 'darwin' else usage.ru_maxrss * 1024
    return {'peak_rss_bytes': rss_bytes, 'wall_seconds': elapsed,
            'user_cpu_seconds': usage.ru_utime, 'system_cpu_seconds': usage.ru_stime,
            'major_faults': usage.ru_majflt, 'minor_faults': usage.ru_minflt}


def canonical(path):
    result = Counter()
    for line in path.read_text().splitlines():
        stack, value = line.rsplit(' ', 1)
        result[stack] += int(value)
    return +result


def p95(values):
    """Nearest-rank percentile; all raw measurements are retained."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * .95) - 1)]


def summarize(runs):
    rss = [r['peak_rss_bytes'] for r in runs]
    wall = [r['wall_seconds'] for r in runs]
    return {'median_peak_rss_bytes': statistics.median(rss),
            'p95_peak_rss_bytes': p95(rss), 'max_peak_rss_bytes': max(rss),
            'median_wall_seconds': statistics.median(wall),
            'p95_wall_seconds': p95(wall), 'max_wall_seconds': max(wall)}


def verify_outputs(args, source, outputs):
    if args.format == 'collapsed':
        expected, actual = canonical(outputs['java']), canonical(outputs['rust'])
    elif args.format in ['pprof', 'pb.gz', 'otlp']:
        from validate import semantic_pprof, semantic_otlp
        parse = semantic_otlp if args.format == 'otlp' else semantic_pprof
        expected, actual = parse(outputs['java']), parse(outputs['rust'])
    elif args.format == 'heatmap':
        decoder = Path(__file__).with_name('heatmap_decode.js')
        decoded = {kind: json.loads(subprocess.check_output(['node', str(decoder), str(path)], text=True)) for kind, path in outputs.items()}
        expected, actual = decoded['java'], decoded['rust']
    elif args.format == 'html':
        converted = {}
        for kind, path in outputs.items():
            converted[kind] = args.workdir / (source.stem + '.' + kind + '.html-check.collapsed')
            subprocess.run([args.java, *args.java_option, '-jar', str(args.java_jar), '-o', 'collapsed', str(path), str(converted[kind])], capture_output=True, check=True)
        expected, actual = canonical(converted['java']), canonical(converted['rust'])
    else:
        raise ValueError('Unsupported semantic output verification format')
    if expected != actual:
        raise AssertionError(f'Output mismatch for {source}; refusing benchmark comparison')
    if isinstance(actual, Counter):
        return {'output_matches': True, 'output_total': sum(actual.values()), 'output_stacks': len(actual)}
    return {'output_matches': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rust', type=Path, required=True)
    parser.add_argument('--java-jar', type=Path, required=True)
    parser.add_argument('--java', default='java')
    parser.add_argument('--java-option', action='append', default=[], help='e.g. --java-option=-Xmx256m')
    parser.add_argument('--input', type=Path, action='append', default=[])
    parser.add_argument('--repeat', type=int, default=21)
    parser.add_argument('--warmup', type=int, default=1)
    parser.add_argument('--repeat-inputs', type=int, default=1, help='convert the same input N times in each fresh process, to check sustained memory')
    parser.add_argument('--samples', type=int, nargs='+', default=[100_000, 2_000_000])
    parser.add_argument('--chunks', type=int, default=4)
    parser.add_argument('--stacks', type=int, default=3)
    parser.add_argument('--event-mode', choices=['cpu', 'all', 'wall', 'alloc', 'lock'], default='cpu')
    parser.add_argument('--format', choices=['collapsed', 'html', 'pprof', 'pb.gz', 'otlp', 'heatmap'], default='collapsed')
    parser.add_argument('--flag', action='append', default=[], help='converter flag passed to both implementations')
    parser.add_argument('--rust-flag', action='append', default=[], help='additional Rust-only flag, e.g. --rust-flag=--max-rss')
    parser.add_argument('--workdir', type=Path, default=Path('target/benchmark'))
    args = parser.parse_args()
    if args.repeat <= 0 or args.warmup < 0 or args.repeat_inputs <= 0:
        parser.error('--repeat and --repeat-inputs must be positive; --warmup nonnegative')
    args.rust = args.rust.resolve(); args.java_jar = args.java_jar.resolve()
    args.workdir.mkdir(parents=True, exist_ok=True)
    # Rebuilds in another agent/process cannot silently mix executables within a
    # report. Measure immutable copies and record both original source paths.
    binary_source, jar_source = args.rust, args.java_jar
    measured_binary = (args.workdir / 'jfrconv-measured').resolve()
    measured_jar = (args.workdir / 'java-baseline.jar').resolve()
    if binary_source != measured_binary:
        shutil.copyfile(binary_source, measured_binary)
        measured_binary.chmod(binary_source.stat().st_mode & 0o777)
    if jar_source != measured_jar:
        shutil.copyfile(jar_source, measured_jar)
    args.rust, args.java_jar = measured_binary, measured_jar
    inputs = args.input or [generate(args.workdir / f'{n}-samples.jfr', n, args.chunks, args.stacks) for n in args.samples]
    mode_flags = [] if args.event_mode == 'all' else ['--' + args.event_mode]
    rows = []
    for source_index, source in enumerate(inputs):
        row = {'input': str(source), 'input_bytes': source.stat().st_size,
               'input_sha256': file_hash(source), 'input_count_per_process': args.repeat_inputs,
               'processed_input_bytes_per_process': source.stat().st_size * args.repeat_inputs,
               'runs': {'java': [], 'rust': []}, 'commands': {}}
        outputs = {}
        for kind in ['java', 'rust']:
            output = args.workdir / (source.stem + '.' + kind + '.' + args.format)
            target = output
            if args.repeat_inputs > 1:
                target = args.workdir / (source.stem + '.' + kind + '.outputs')
                target.mkdir(exist_ok=True)
                output = target / (source.stem + '.' + ('html' if args.format == 'heatmap' else args.format))
            command = ([args.java, *args.java_option, '-jar', str(args.java_jar)] if kind == 'java' else [str(args.rust), *args.rust_flag])
            command += [*mode_flags, *args.flag, '-o', args.format, *([str(source)] * args.repeat_inputs), str(target)]
            row['commands'][kind] = command
            outputs[kind] = output
        first_order = ['java', 'rust'] if source_index % 2 == 0 else ['rust', 'java']
        row['first_fresh_process'] = {kind: measure(row['commands'][kind]) for kind in first_order}
        row['warmup_runs'] = {'java': [], 'rust': []}
        for iteration in range(args.warmup):
            order = first_order if iteration % 2 == 0 else list(reversed(first_order))
            for kind in order: row['warmup_runs'][kind].append(measure(row['commands'][kind]))
        row['execution_order'] = []
        for iteration in range(args.repeat):
            order = first_order if iteration % 2 == 0 else list(reversed(first_order))
            row['execution_order'].append(order)
            for kind in order: row['runs'][kind].append(measure(row['commands'][kind]))
        row.update(verify_outputs(args, source, outputs))
        for kind in ['java','rust']: row[kind] = summarize(row['runs'][kind])
        java_rss, rust_rss = row['java']['median_peak_rss_bytes'], row['rust']['median_peak_rss_bytes']
        row['rss_reduction_percent'] = 100 * (1 - rust_rss / java_rss)
        row['latency_p50_speedup_ratio'] = row['java']['median_wall_seconds'] / row['rust']['median_wall_seconds']
        row['latency_p95_speedup_ratio'] = row['java']['p95_wall_seconds'] / row['rust']['p95_wall_seconds']
        rows.append(row)
        print(f'{source.name} x{args.repeat_inputs}: RSS Java {java_rss / 1048576:.2f} MiB vs Rust {rust_rss / 1048576:.2f} MiB ({row["rss_reduction_percent"]:.1f}% lower); p50 {row["java"]["median_wall_seconds"]:.4f}s vs {row["rust"]["median_wall_seconds"]:.4f}s; p95 {row["java"]["p95_wall_seconds"]:.4f}s vs {row["rust"]["p95_wall_seconds"]:.4f}s', flush=True)
    report = {'command': sys.argv, 'rust_binary_source': str(binary_source), 'java_jar_source': str(jar_source),
              'java_version': subprocess.run([args.java, '-version'], capture_output=True, text=True).stderr.strip(),
              'hardware': {'machine': platform.machine(), 'processor': platform.processor(), 'logical_cpus': os.cpu_count()},
              'rust_binary_sha256': file_hash(args.rust), 'java_jar_sha256': file_hash(args.java_jar),
              'platform': platform.platform(), 'python': platform.python_version(), 'repeat': args.repeat,
              'warmups_per_implementation': args.warmup, 'format': args.format,
              'java_options': args.java_option, 'flags': [*mode_flags, *args.flag], 'rust_flags': args.rust_flag,
              'method': 'Fresh processes measured with wait4 child ru_maxrss; equal warmups; alternating pair order; p50 median and p95 nearest-rank; first fresh pair kept separate; input page-cache state uncontrolled; macOS RSS bytes, Linux KiB normalized to bytes.',
              'recordings': rows}
    destination = args.workdir / 'benchmark.json'
    destination.write_text(json.dumps(report, indent=2) + '\n')
    print(destination)


if __name__ == '__main__':
    main()
