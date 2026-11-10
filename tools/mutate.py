#!/usr/bin/env python3
"""Deterministic malformed-JFR smoke fuzzing (not a security audit)."""
import argparse
import hashlib
import json
from pathlib import Path
import random
import subprocess
import tempfile
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, required=True)
    p.add_argument('--fixture', type=Path, default=Path(__file__).parent.parent / 'tests/fixtures/mixed.jfr')
    p.add_argument('--reader', action='store_true', help='binary is a standalone reader taking just an input path')
    p.add_argument('--cases', type=int, default=1000)
    p.add_argument('--seed', type=int, default=4101)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    binary = args.binary.resolve()
    data = args.fixture.read_bytes()
    rng = random.Random(args.seed)
    accepted = rejected = 0
    failures = []
    start = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix='jfrconv-mutations-') as scratch:
        source = Path(scratch) / 'input.jfr'
        output = Path(scratch) / 'result.collapsed'
        for index in range(args.cases):
            changed = bytearray(data)
            if index % 10 == 0:
                changed = changed[:rng.randrange(len(changed))]
            else:
                for _ in range(1 + rng.randrange(5)):
                    changed[rng.randrange(len(changed))] ^= 1 << rng.randrange(8)
            source.write_bytes(changed)
            command = [str(binary), str(source)] if args.reader else [str(binary), '-o', 'collapsed', str(source), str(output)]
            try:
                result = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=1)
                if result.returncode < 0 or b'panicked' in result.stderr:
                    failures.append({'case':index, 'status':result.returncode, 'stderr':result.stderr.decode(errors='replace')})
                elif result.returncode == 0:
                    accepted += 1
                else:
                    rejected += 1
            except subprocess.TimeoutExpired:
                failures.append({'case':index, 'timeout':True})
    report = {'cases':args.cases, 'seed':args.seed, 'accepted':accepted, 'rejected':rejected, 'failures':failures, 'seconds':time.perf_counter()-start, 'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(), 'fixture_sha256':hashlib.sha256(data).hexdigest(), 'reader_only':args.reader}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(report))
    raise SystemExit(bool(failures))


if __name__ == '__main__':
    main()
