#!/usr/bin/env python3
"""Record the included Java workload using JDK JFR and optionally async-profiler.

Requires a JDK. For the AP recording pass --async-profiler-lib with the path to
libasyncProfiler.so (Linux) or libasyncProfiler.dylib (macOS) from release 4.1.
"""
import argparse
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output-dir', type=Path, default=Path('target/real-fixtures'))
    parser.add_argument('--async-profiler-lib', type=Path)
    parser.add_argument('--java', default='java')
    parser.add_argument('--javac', default='javac')
    args = parser.parse_args()
    destination = args.output_dir.resolve()
    destination.mkdir(parents=True, exist_ok=True)
    source = Path(__file__).resolve().parent.parent / 'tests' / 'RecordingWorkload.java'
    subprocess.run([args.javac, '-d', str(destination), str(source)], check=True)
    jdk = destination / 'jdk.jfr'
    subprocess.run([args.java, '-Xmx128m', '-cp', str(destination), 'RecordingWorkload', str(jdk), 'jdk'], check=True)
    print(jdk)
    if args.async_profiler_lib:
        ap = destination / 'ap41.jfr'
        option = '-agentpath:' + str(args.async_profiler_lib.resolve()) + '=start,event=cpu,interval=1ms,alloc=64k,lock=1us,file=' + str(ap)
        subprocess.run([args.java, '-Xmx128m', option, '-cp', str(destination), 'RecordingWorkload', str(ap), 'agent'], check=True)
        print(ap)


if __name__ == '__main__':
    main()
