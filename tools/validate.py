#!/usr/bin/env python3
"""Differential/structural checks against async-profiler 4.1's Java converter.

Example:
  python3 tools/validate.py --rust target/release/jfrconv \
      --java-jar /path/to/v4.1/build/jar/jfr-converter.jar
Fixtures and scratch outputs are written under target/validation by default.
"""
from __future__ import annotations
import argparse
from collections import Counter
import gzip
import json
import hashlib
from pathlib import Path
import struct
import subprocess
import tempfile
import sys
from fixtures import generate, EPOCH_NS


def collapsed(path: Path):
    result = Counter()
    for line in path.read_text(encoding='utf-8').splitlines():
        stack, count = line.rsplit(' ', 1)
        result[stack] += int(count)
    return +result


def varint(data, pos):
    value = 0
    for shift in range(0, 70, 7):
        if pos >= len(data):
            raise ValueError('truncated protobuf varint')
        byte = data[pos]; pos += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, pos
    raise ValueError('protobuf varint too long')


def protobuf(data):
    pos = 0
    fields = []
    while pos < len(data):
        key, pos = varint(data, pos)
        number, wire = key >> 3, key & 7
        if not number:
            raise ValueError('zero protobuf field number')
        if wire == 0:
            value, pos = varint(data, pos)
        elif wire == 2:
            size, pos = varint(data, pos)
            value = data[pos:pos + size]
            if len(value) != size:
                raise ValueError('truncated protobuf field')
            pos += size
        elif wire in (1, 5):
            size = 8 if wire == 1 else 4
            value = data[pos:pos + size]
            if len(value) != size:
                raise ValueError('truncated fixed protobuf field')
            pos += size
        else:
            raise ValueError(f'unsupported protobuf wire type {wire}')
        fields.append((number, wire, value))
    return fields


def integers(fields, number):
    result = []
    for n, w, value in fields:
        if n != number:
            continue
        if w == 0:
            result.append(value)
        elif w == 2:
            pos = 0
            while pos < len(value):
                integer, pos = varint(value, pos)
                result.append(integer)
        else:
            raise AssertionError('expected varint or packed varints')
    return result


def pprof_totals(path):
    data = path.read_bytes()
    if data.startswith(b'\x1f\x8b'):
        data = gzip.decompress(data)
    fields = protobuf(data)
    strings = [v.decode('utf-8') for n, w, v in fields if n == 6]
    assert strings and strings[0] == '', 'pprof string table starts with empty string'
    functions = {}
    locations = {}
    for n, w, v in fields:
        if n == 5:
            f = protobuf(v)
            functions[integers(f, 1)[0]] = strings[integers(f, 2)[0]]
        elif n == 4:
            f = protobuf(v)
            lines = [protobuf(b) for k, _, b in f if k == 4]
            locations[integers(f, 1)[0]] = [integers(line, 1)[0] for line in lines]
    total = 0
    for n, w, v in fields:
        if n == 2:
            sample = protobuf(v)
            total += sum(integers(sample, 2))
            for location in integers(sample, 1):
                assert location in locations, f'unknown pprof location {location}'
                for function in locations[location]:
                    assert function in functions, f'unknown pprof function {function}'
    return total


def otlp_totals(path):
    fields = protobuf(path.read_bytes())
    profiles = []
    for n, _, resource in fields:
        if n == 1:
            for rn, _, scope in protobuf(resource):
                if rn == 2:
                    profiles += [protobuf(p) for sn, _, p in protobuf(scope) if sn == 2]
    assert profiles, 'OTLP must contain at least one profile'
    total = 0
    for profile in profiles:
        for n, _, data in profile:
            if n == 2:
                samples = integers(protobuf(data), 3)
                assert samples, 'OTLP sample has values'
                total += samples[0]
    return total


def semantic_pprof(path):
    data = path.read_bytes()
    if data.startswith(b'\x1f\x8b'):
        data = gzip.decompress(data)
    fields = protobuf(data)
    strings = [v.decode('utf-8') for n, _, v in fields if n == 6]
    functions = {}
    locations = {}
    for n, _, data in fields:
        if n == 5:
            f = protobuf(data)
            functions[integers(f, 1)[0]] = strings[(integers(f, 2) or [0])[0]]
    for n, _, data in fields:
        if n == 4:
            f = protobuf(data)
            locations[integers(f, 1)[0]] = tuple((functions[(integers(line, 1) or [0])[0]], (integers(line, 2) or [0])[0]) for k, _, b in f if k == 4 for line in [protobuf(b)])
    result = Counter()
    for n, _, data in fields:
        if n != 2:
            continue
        f = protobuf(data)
        stack = tuple(item for location in integers(f, 1) for item in locations[location])
        labels = []
        for k, _, data in f:
            if k == 3:
                label = protobuf(data)
                labels.append((strings[(integers(label, 1) or [0])[0]], strings[(integers(label, 2) or [0])[0]]))
        result[(stack, tuple(sorted(labels)))] += sum(integers(f, 2))
    return +result


def semantic_otlp(path):
    fields = protobuf(path.read_bytes())
    dictionary = protobuf(next(data for n, _, data in fields if n == 2))
    strings = [data.decode('utf-8') for n, _, data in dictionary if n == 5]
    functions = [strings[(integers(protobuf(data), 1) or [0])[0]] for n, _, data in dictionary if n == 3]
    locations = []
    for n, _, data in dictionary:
        if n == 2:
            locations.append(tuple((functions[(integers(line, 1) or [0])[0]], (integers(line, 2) or [0])[0]) for k, _, b in protobuf(data) if k == 3 for line in [protobuf(b)]))
    attributes = []
    for n, _, data in dictionary:
        if n == 6:
            f = protobuf(data)
            key = next((v.decode('utf-8') for k, _, v in f if k == 1), '')
            anyvalue = protobuf(next((v for k, _, v in f if k == 2), b''))
            value = next((v.decode('utf-8') for k, _, v in anyvalue if k == 1), '')
            attributes.append((key, value))
    profiles = []
    for n, _, resource in fields:
        if n == 1:
            for rn, _, scope in protobuf(resource):
                if rn == 2:
                    profiles += [protobuf(p) for sn, _, p in protobuf(scope) if sn == 2]
    result = []
    for profile in profiles:
        sample_types = []
        for n, _, data in profile:
            if n == 1:
                f = protobuf(data)
                sample_types.append((strings[(integers(f, 1) or [0])[0]], strings[(integers(f, 2) or [0])[0]], (integers(f, 3) or [0])[0]))
        indices = integers(profile, 3)
        samples = []
        for n, _, data in profile:
            if n == 2:
                f = protobuf(data)
                start = (integers(f, 1) or [0])[0]
                length = (integers(f, 2) or [0])[0]
                stack = tuple(item for idx in indices[start:start+length] for item in locations[idx])
                attrs = tuple(sorted(attributes[i] for i in integers(f, 4) if attributes[i] != ('','')))
                samples.append((stack, attrs, tuple(integers(f, 3)), tuple(integers(f, 6))))
        result.append(((integers(profile, 4) or [0])[0], (integers(profile, 5) or [0])[0], tuple(sample_types), tuple(sorted(samples))))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rust', type=Path, required=True)
    parser.add_argument('--java-jar', type=Path, required=True)
    parser.add_argument('--java', default='java')
    parser.add_argument('--workdir', type=Path, default=Path('target/validation'))
    parser.add_argument('--real', type=Path, action='append', default=[])
    args = parser.parse_args()
    args.rust = args.rust.resolve(); args.java_jar = args.java_jar.resolve()
    args.workdir.mkdir(parents=True, exist_ok=True)
    fixture = generate(args.workdir / 'mixed.jfr')
    passed = []
    failures = []

    def run(command, expected=0):
        result = subprocess.run(list(map(str, command)), capture_output=True, text=True, timeout=60)
        if result.returncode != expected:
            raise AssertionError(f'{command}\nexit={result.returncode}\n{result.stderr}\n{result.stdout}')
        return result

    cases = [
        ('all', []), ('cpu', ['--cpu']), ('wall', ['--wall']),
        ('threads', ['--cpu', '--threads']), ('wall-threads', ['--wall', '-t']),
        ('alloc', ['--alloc']), ('alloc-total', ['--alloc', '--total']),
        ('alloc-thread-lines', ['--alloc', '-t', '--lines']), ('live', ['--live', '--total']),
        ('lock', ['--lock']), ('lock-total', ['--lock', '--total']),
        ('malloc', ['--nativemem']), ('malloc-total', ['--nativemem', '--total']),
        ('lines', ['--lines']), ('bci', ['--bci']), ('simple', ['--simple']),
        ('dot', ['--dot']), ('norm', ['--norm']), ('classify', ['--classify']),
        ('reverse', ['--reverse']), ('reverse-thread', ['--reverse', '--threads']),
        ('skip', ['--skip', '1']), ('reverse-skip', ['--reverse', '--skip', '1']),
        ('include', ['-I', 'demo/Worker.*']), ('exclude', ['-X', '.*malloc']),
        ('include-exclude', ['-I', '.*Main.*', '-X', 'second/.*']),
        ('state', ['--state', 'sleeping']), ('states', ['-s', 'default,runnable']),
        ('relative-time', ['--from', '1000', '--to', '2000']),
        ('absolute-time', ['--from', str(EPOCH_NS // 1_000_000 + 1000)]),
        ('negative-time', ['--from', '-1000']),
        ('minwidth', ['--minwidth', '40']),
        ('grain', ['--grain', '2']), ('grain-threads', ['--grain', '2', '--threads']),
    ]

    def compare(name, flags, source=fixture):
        java_out = args.workdir / f'{name}.java.collapsed'
        rust_out = args.workdir / f'{name}.rust.collapsed'
        run([args.java, '-jar', args.java_jar, *flags, '-o', 'collapsed', source, java_out])
        run([args.rust, *flags, '-o', 'collapsed', source, rust_out])
        expected, actual = collapsed(java_out), collapsed(rust_out)
        assert actual == expected, f'{name}\nmissing: {expected - actual}\nextra: {actual - expected}'

    def check(name, fn):
        try:
            fn()
            passed.append(name)
            print('PASS', name, flush=True)
        except Exception as exc:
            failures.append({'name': name, 'error': str(exc)})
            print('FAIL', name, str(exc), flush=True)

    for name, flags in cases:
        check(name, lambda n=name, f=flags: compare(n, f))
    # Real recordings may not contain AP's synthetic STATE_DEFAULT. The baseline
    # chooses events using its own state enumeration, which the rewrite matches.
    for source in args.real:
        for name, flags in [('all', []), ('cpu', ['--cpu']), ('alloc', ['--alloc', '--total']), ('lock', ['--lock', '--total']), ('thread-lines', ['-t', '--lines'])]:
            full_name = source.stem + '-' + name
            check(full_name, lambda n=full_name, f=flags, s=source: compare(n, f, s))

    def span_check():
        output = args.workdir / 'spans.collapsed'
        run([args.rust, '--cpu', '--tag', 'request-.*', '-o', 'collapsed', fixture, output])
        actual = collapsed(output)
        assert sum(actual.values()) == 4, actual
        assert all('Worker.work' in stack for stack in actual), actual
    check('span-tag', span_check)

    def exports():
        for fmt, suffix in [('pprof', 'pprof'), ('pb.gz', 'pb.gz'), ('otlp', 'otlp')]:
            output = args.workdir / ('export.' + suffix)
            run([args.rust, '--cpu', '-o', fmt, fixture, output])
            total = otlp_totals(output) if fmt == 'otlp' else pprof_totals(output)
            assert total == 6, (fmt, total)
    check('protobuf-exports', exports)

    def protobuf_parity(fmt, flags, source, name):
        rust_out = args.workdir / (name + '.rust.' + fmt)
        java_out = args.workdir / (name + '.java.' + fmt)
        run([args.rust, *flags, '-o', fmt, source, rust_out])
        run([args.java, '-jar', args.java_jar, *flags, '-o', fmt, source, java_out])
        canonical = semantic_otlp if fmt == 'otlp' else semantic_pprof
        expected, actual = canonical(java_out), canonical(rust_out)
        assert actual == expected, f'{fmt} semantic mismatch for {name}\nexpected={expected}\nactual={actual}'
    for fmt in ['pprof', 'otlp']:
        for name, flags in [('cpu', ['--cpu']), ('cpu-threads', ['--cpu','--threads']), ('alloc', ['--alloc','--total']), ('locks', ['--lock','--total'])]:
            check(fmt + '-' + name, lambda f=fmt, fl=flags, n=name: protobuf_parity(f, fl, fixture, n))
        for source in args.real:
            check(source.stem + '-' + fmt, lambda f=fmt, s=source: protobuf_parity(f, [], s, s.stem + '-all'))

    def html_roundtrip():
        output = args.workdir / 'roundtrip.html'
        back = args.workdir / 'roundtrip.collapsed'
        run([args.rust, '--cpu', '-o', 'html', fixture, output])
        run([args.java, '-jar', args.java_jar, '-o', 'collapsed', output, back])
        assert collapsed(back) == collapsed(args.workdir / 'cpu.java.collapsed')
    check('html-java-roundtrip', html_roundtrip)

    def constructor_roundtrip():
        source = args.workdir / 'constructors.collapsed'
        source.write_text('pkg/Foo.<init>_[j];pkg/Foo.<clinit>_[j] 2\npkg/Foo.<init>_[j];pkg/Bar.<init>_[0] 1\n')
        html = args.workdir / 'constructors.html'
        back = args.workdir / 'constructors.roundtrip.collapsed'
        run([args.rust, '-o', 'html', source, html])
        run([args.java, '-jar', args.java_jar, '-o', 'collapsed', html, back])
        assert collapsed(back) == collapsed(source)
    check('html-constructors-java-roundtrip', constructor_roundtrip)

    def heatmap_parity(source, flags, name):
        decoded = {}
        for kind in ['java','rust']:
            output = args.workdir / (name + '.' + kind + '.heatmap.html')
            command = [args.java, '-jar', args.java_jar] if kind == 'java' else [args.rust]
            run([*command, *flags, '-o', 'heatmap', source, output])
            decoder = Path(__file__).with_name('heatmap_decode.js')
            decoded[kind] = json.loads(subprocess.check_output(['node', str(decoder), str(output)], text=True))
        assert decoded['rust'] == decoded['java'], decoded
    for mode in ['cpu','wall']:
        check('heatmap-' + mode, lambda m=mode: heatmap_parity(fixture, ['--'+m], 'heatmap-' + m))
    for source in args.real:
        for mode in ['wall','alloc','lock']:
            check(source.stem + '-heatmap-' + mode, lambda s=source, m=mode: heatmap_parity(s, ['--'+m], s.stem + '-heatmap-' + m))

    def malformed():
        raw = fixture.read_bytes()
        variants = [('empty', b''), ('header', raw[:32]), ('truncated', raw[:-8]), ('signature', b'NOPE' + raw[4:]), ('zero-size', raw[:68] + b'\0' + raw[69:])]
        for name, data in variants:
            source = args.workdir / f'malformed-{name}.jfr'
            source.write_bytes(data)
            result = subprocess.run([str(args.rust), '-o', 'collapsed', str(source), str(args.workdir / 'bad.collapsed')], capture_output=True, text=True, timeout=5)
            assert result.returncode != 0, f'{name} input was accepted'
            assert 'panicked' not in result.stderr, result.stderr
    check('malformed-inputs', malformed)

    def multi_file_names(limited):
        source_dir = args.workdir / 'dotted-inputs'
        source_dir.mkdir(exist_ok=True)
        inputs = [source_dir / 'profile.cpu.jfr', source_dir / 'profile.wall.jfr']
        for source in inputs:
            source.write_bytes(fixture.read_bytes())
        outputs = {}
        for kind in ['java','rust']:
            with tempfile.TemporaryDirectory(prefix='dotted-' + kind + '-', dir=args.workdir) as directory:
                target = Path(directory)
                command = [args.java, '-jar', args.java_jar] if kind == 'java' else [args.rust]
                flags = ['--memory-limit','16M'] if limited and kind == 'rust' else []
                run([*command, *flags, '--cpu', '-o', 'collapsed', *inputs, target])
                outputs[kind] = {p.name: collapsed(p) for p in target.glob('*.collapsed')}
        assert outputs['java'] == outputs['rust'], outputs
        assert set(outputs['rust']) == {'profile.cpu.collapsed','profile.wall.collapsed'}
    check('multi-file-dotted-names', lambda: multi_file_names(False))
    check('multi-file-dotted-names-limited', lambda: multi_file_names(True))
    report = {'passed': passed, 'failures': failures, 'count': len(passed), 'java_baseline': str(args.java_jar), 'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(), 'java_jar_sha256': hashlib.sha256(args.java_jar.read_bytes()).hexdigest()}
    (args.workdir / 'validation.json').write_text(json.dumps(report, indent=2) + '\n')
    print(f'{len(passed)} checks passed; {len(failures)} failed')
    return bool(failures)


if __name__ == '__main__':
    sys.exit(main())
