#!/usr/bin/env python3
"""Run actual published async-profiler converter assertions against a native CLI.

Stage a pristine Git revision, patch transport entry points only, then build/run
its unchanged Java tests. Requires a JDK plus native C/C++ build tools. Linux is
needed for the entire historical v4.1 set (one LD_PRELOAD test).

  python3 tools/upstream_tests.py stage --source /path/to/async-profiler \
      --dest target/public-v41 --suite v4.1 --bridge
  JFRCONV_BINARY=/absolute/path/to/jfrconv python3 tools/upstream_tests.py run \
      --dest target/public-v41 --suite v4.1

For the Java baseline omit --bridge at stage. Current public converter tests
include --diff, Span tags and trace latency, beyond v4.1's original interface.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import time

HISTORICAL = ['JfrTests.regularPeak', 'JfrTests.ttsp', 'StackwalkerTests', 'NativememTests.ldpreload']
CURRENT = ['JfrconverterTests']


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def body_replace(source, signature, body):
    start = source.index(signature)
    opening = source.index('{', start)
    depth = 1
    i = opening + 1
    quote = None
    comment = None
    while depth:
        ch = source[i]
        nxt = source[i:i+2]
        if comment == '//':
            if ch == '\n': comment = None
        elif comment == '/*':
            if nxt == '*/': comment = None; i += 1
        elif quote:
            if ch == '\\': i += 1
            elif ch == quote: quote = None
        elif nxt in ['//', '/*']:
            comment = nxt; i += 1
        elif ch in ['"', "'"]:
            quote = ch
        elif ch == '{': depth += 1
        elif ch == '}': depth -= 1
        i += 1
    return source[:opening + 1] + '\n' + body + '\n    ' + source[i-1:]


def stage(args):
    source, destination = args.source.resolve(), args.dest.resolve()
    if destination.exists():
        raise SystemExit(f'Refusing to replace existing staging directory: {destination}')
    destination.mkdir(parents=True)
    commit = subprocess.check_output(['git', '-C', str(source), 'rev-parse', args.ref], text=True).strip()
    with tempfile.TemporaryFile() as archive:
        subprocess.run(['git', '-C', str(source), 'archive', args.ref], stdout=archive, check=True)
        archive.seek(0)
        with tarfile.open(fileobj=archive) as contents:
            contents.extractall(destination, filter='data')
    assertion_hashes = {str(p.relative_to(destination)): digest(p) for p in sorted((destination / 'test/test').rglob('*.java'))}
    if args.bridge:
        bridge = Path(__file__).with_name('upstream') / 'RustBridge.java'
        shutil.copyfile(bridge, destination / 'src/converter/one/convert/RustBridge.java')
        path = destination / 'test/one/profiler/test/Output.java'
        text = path.read_text()
        text = body_replace(text, 'public static Output convertJfrToCollapsed(', '        return new Output(new String(one.convert.RustBridge.collapsed(input, args), "UTF-8").split(System.lineSeparator()));')
        path.write_text(text)
        for name, fmt in [('JfrToFlame', 'html'), ('JfrToHeatmap', 'heatmap')]:
            path = destination / f'src/converter/one/convert/{name}.java'
            text = path.read_text()
            format_expr = '"collapsed".equals(args.output) ? "collapsed" : "html"' if fmt == 'html' else '"heatmap"'
            text = body_replace(text, 'public static void convert(String input, String output, Arguments args)', f'        RustBridge.convert(input, output, args, {format_expr});')
            path.write_text(text)
        path = destination / 'src/converter/one/convert/Main.java'
        if not path.exists(): path = destination / 'src/converter/Main.java'
        text = path.read_text()
        text = body_replace(text, 'public static void main(String[] argv)', '        one.convert.RustBridge.main(argv);')
        path.write_text(text)
    after = {str(p.relative_to(destination)): digest(p) for p in sorted((destination / 'test/test').rglob('*.java'))}
    assert after == assertion_hashes, 'A published assertion source changed!'
    manifest = {'source': str(source), 'revision': commit, 'suite': args.suite, 'transport_bridge': args.bridge, 'assertion_sources': assertion_hashes, 'adapter': ('Only Output.convertJfrToCollapsed, static JfrToFlame/JfrToHeatmap.convert and Main.main transport entry points are redirected. Published test assertions are byte-for-byte unchanged. /dev/null and all actual requested output paths are passed directly to the native CLI.' if args.bridge else 'Unmodified upstream Java converter and native profiler; no Rust transport bridge.')}
    (destination / 'public-test-manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print(destination)


def run(args):
    destination = args.dest.resolve()
    manifest_path = destination / 'public-test-manifest.json'
    manifest = json.loads(manifest_path.read_text())
    for name, expected in manifest['assertion_sources'].items():
        assert digest(destination / name) == expected, f'Assertion changed: {name}'
    logs = destination / 'public-test-results'
    logs.mkdir(exist_ok=True)
    if not args.no_build:
        with (logs / 'build.log').open('w') as output:
            result = subprocess.run(['make', '-j2', 'build-test-java'], cwd=destination, stdout=output, stderr=subprocess.STDOUT)
        if result.returncode:
            print((logs / 'build.log').read_text()[-12000:])
            raise SystemExit('Public test build failed')
    suite = args.test or (HISTORICAL if args.suite == 'v4.1' else CURRENT)
    command = [args.java, '-ea', '-Djava.library.path=build/test/lib', '-DthreadCount=1', '-DretryCount=0', '-DlogDir=' + str(logs / 'profiles'), '-cp', 'build/test.jar:build/jar/*:build/lib/*:test/deps/*:test/gen/*', 'one.profiler.test.Runner', *suite]
    start = time.perf_counter()
    with (logs / 'tests.log').open('w') as output:
        result = subprocess.run(command, cwd=destination, stdout=output, stderr=subprocess.STDOUT)
    elapsed = time.perf_counter() - start
    text = (logs / 'tests.log').read_text(errors='replace')
    print(text)
    parsed = []
    for line in text.splitlines():
        match = re.match(r'^(PASS|FAIL|SKIP_[A-Z_]+) \[(\d+)/(\d+)\] (.*) took ([0-9.,]+) s$', line)
        if match:
            parsed.append({'status': match[1], 'test': match[4], 'seconds': float(match[5].replace(',', '.'))})
    counts = {}
    for row in parsed: counts[row['status']] = counts.get(row['status'], 0) + 1
    report = {'revision': manifest['revision'], 'suite': args.suite, 'transport_bridge': manifest['transport_bridge'], 'assertion_files_unchanged': True, 'command': command, 'returncode': result.returncode, 'elapsed_seconds': elapsed, 'counts': counts, 'tests': parsed, 'platform': os.uname().sysname + ' ' + os.uname().machine, 'java_version': subprocess.run([args.java, '-version'], capture_output=True, text=True).stderr.strip(), 'rust_binary': os.environ.get('JFRCONV_BINARY'), 'adapter': manifest['adapter']}
    binary = os.environ.get('JFRCONV_BINARY')
    if binary and Path(binary).exists(): report['rust_binary_sha256'] = digest(Path(binary))
    (logs / 'public-tests.json').write_text(json.dumps(report, indent=2) + '\n')
    print(logs / 'public-tests.json')
    raise SystemExit(result.returncode)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action', required=True)
    prepare = sub.add_parser('stage')
    prepare.add_argument('--source', type=Path, required=True)
    prepare.add_argument('--ref', default='HEAD')
    prepare.add_argument('--dest', type=Path, required=True)
    prepare.add_argument('--suite', choices=['v4.1','current'], required=True)
    prepare.add_argument('--bridge', action='store_true')
    execute = sub.add_parser('run')
    execute.add_argument('--dest', type=Path, required=True)
    execute.add_argument('--suite', choices=['v4.1','current'], required=True)
    execute.add_argument('--java', default='java')
    execute.add_argument('--no-build', action='store_true')
    execute.add_argument('--test', action='append', default=[], help='explicit published method/class filter; default full converter suite')
    args = parser.parse_args()
    stage(args) if args.action == 'stage' else run(args)


if __name__ == '__main__':
    main()
