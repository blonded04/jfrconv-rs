#!/usr/bin/env python3
"""Generate small, deterministic JFR 2.0 fixtures, including async-profiler events.

The byte layout follows async-profiler v4.1's JfrReader/JFR writer. This is a
fixture writer, not a general-purpose JFR implementation. No external packages.
"""
from __future__ import annotations
import argparse
from pathlib import Path
import struct

EPOCH_NS = 1_700_000_000_000_000_000
TICKS = 1_000_000_000


def varint(value: int) -> bytes:
    value &= (1 << 64) - 1
    result = bytearray()
    for _ in range(8):
        if value < 128:
            result.append(value)
            return bytes(result)
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def string(value: str | None) -> bytes:
    if value is None:
        return b'\0'
    if not value:
        return b'\1'
    data = value.encode('utf-8')
    return b'\3' + varint(len(data)) + data


def values(*items: int) -> bytes:
    return b''.join(map(varint, items))


def record(type_id: int, payload: bytes) -> bytes:
    body = varint(type_id) + payload
    size = len(body) + 1
    while True:
        prefix = varint(size)
        actual = len(prefix) + len(body)
        if actual == size:
            return prefix + body
        size = actual


# JFR class IDs; these intentionally differ from typical HotSpot/AP IDs.
LONG, INT, BYTE, BOOL, STRING, SYMBOL, THREAD, CLASS, METHOD, STACK, FRAME, FTYPE, STATE = range(1, 14)
EXEC, WALL, TLAB, OUTSIDE, ALLOC, LIVE, LOCK, PARK, MALLOC, FREE, SPAN = range(100, 111)


def field(name: str, class_id: int, cp: bool = False, array: bool = False):
    attributes = {'name': name, 'class': str(class_id)}
    if cp:
        attributes['constantPool'] = 'true'
    if array:
        attributes['dimension'] = '1'
    return ('field', attributes, [])


def klass(type_id: int, name: str, fields=(), event=False, simple=False):
    attributes = {'id': str(type_id), 'name': name}
    if event:
        attributes['superType'] = 'jdk.jfr.Event'
    if simple:
        attributes['simpleType'] = 'true'
    return ('class', attributes, list(fields))


def metadata(span_current: bool = False, method_trace: bool = False) -> bytes:
    sample = [field('startTime', LONG), field('sampledThread', THREAD, True), field('stackTrace', STACK, True), field('state', STATE, True)]
    allocation = [field('startTime', LONG), field('eventThread', THREAD, True), field('stackTrace', STACK, True), field('objectClass', CLASS, True), field('allocationSize', LONG)]
    lock = [field('startTime', LONG), field('duration', LONG), field('eventThread', THREAD, True), field('stackTrace', STACK, True), field('monitorClass', CLASS, True)]
    malloc = [field('startTime', LONG), field('eventThread', THREAD, True), field('stackTrace', STACK, True), field('address', LONG)]
    classes = [
        klass(LONG, 'long'), klass(INT, 'int'), klass(BYTE, 'byte'), klass(BOOL, 'boolean'), klass(STRING, 'java.lang.String'),
        klass(SYMBOL, 'jdk.types.Symbol', [field('string', STRING)], simple=True),
        klass(THREAD, 'java.lang.Thread', [field('osName', STRING), field('osThreadId', LONG), field('javaName', STRING), field('javaThreadId', LONG)]),
        klass(CLASS, 'java.lang.Class', [field('classLoader', LONG), field('name', SYMBOL, True), field('package', LONG), field('modifiers', INT)]),
        klass(METHOD, 'jdk.types.Method', [field('type', CLASS, True), field('name', SYMBOL, True), field('descriptor', SYMBOL, True), field('modifiers', INT), field('hidden', BOOL)]),
        klass(STACK, 'jdk.types.StackTrace', [field('truncated', BOOL), field('frames', FRAME, array=True)]),
        klass(FRAME, 'jdk.types.StackFrame', [field('method', METHOD, True), field('lineNumber', INT), field('bytecodeIndex', INT), field('type', FTYPE, True)]),
        klass(FTYPE, 'jdk.types.FrameType', [field('description', STRING)], simple=True),
        klass(STATE, 'jdk.types.ThreadState', [field('name', STRING)], simple=True),
        klass(EXEC, 'jdk.ExecutionSample', sample, event=True),
        klass(WALL, 'profiler.WallClockSample', sample + [field('samples', INT)], event=True),
        klass(TLAB, 'jdk.ObjectAllocationInNewTLAB', allocation + [field('tlabSize', LONG)], event=True),
        klass(OUTSIDE, 'jdk.ObjectAllocationOutsideTLAB', allocation, event=True),
        klass(ALLOC, 'jdk.ObjectAllocationSample', allocation, event=True),
        klass(LIVE, 'profiler.LiveObject', allocation + [field('allocationTime', LONG)], event=True),
        klass(LOCK, 'jdk.JavaMonitorEnter', lock + [field('previousOwner', LONG), field('address', LONG)], event=True),
        klass(PARK, 'jdk.ThreadPark', lock + [field('timeout', LONG), field('until', LONG), field('address', LONG)], event=True),
        klass(MALLOC, 'profiler.Malloc', malloc + [field('size', LONG)], event=True),
        klass(FREE, 'profiler.Free', malloc, event=True),
        klass(SPAN, 'profiler.Span', [field('startTime', LONG), field('duration', LONG), field('eventThread', THREAD, True)] + ([field('tag', STRING, True)] if span_current else [field('stackTrace', STACK, True), field('tag', STRING)]), event=True),
    ]
    if method_trace:
        classes.append(klass(111, 'jdk.MethodTrace', [field('startTime', LONG), field('duration', LONG), field('eventThread', THREAD, True), field('stackTrace', STACK, True), field('method', METHOD, True)], event=True))
    root = ('root', {}, [('metadata', {}, classes), ('region', {'locale': 'en_US', 'gmtOffset': '0'}, [])])
    strings = []
    indices = {}
    def intern(s):
        if s not in indices:
            indices[s] = len(strings)
            strings.append(s)
        return indices[s]
    def collect(element):
        name, attrs, children = element
        intern(name)
        for k, v in attrs.items():
            intern(k); intern(v)
        for child in children:
            collect(child)
    collect(root)
    def encode(element):
        name, attrs, children = element
        result = values(indices[name], len(attrs))
        for k, v in attrs.items():
            result += values(indices[k], indices[v])
        result += varint(len(children))
        return result + b''.join(encode(c) for c in children)
    return record(0, values(0, 0, 1, len(strings)) + b''.join(map(string, strings)) + encode(root))


def constant_pool(label: str = 'demo', stacks: int = 3, tag_strings=None) -> bytes:
    symbols = ['', f'{label}/Main', 'run', '()V', f'{label}/Worker', 'work', '[B', 'malloc', f'{label}/Lock']
    pools = []
    pools.append(values(SYMBOL, len(symbols)) + b''.join(varint(i) + b'\3' + varint(len(s.encode('utf-8'))) + s.encode('utf-8') for i, s in enumerate(symbols, 1)))
    pools.append(values(THREAD, 2) + values(1) + string('worker-α') + values(101) + string('worker-α') + values(1) + values(2) + string('io-worker') + values(102) + string('io-worker') + values(2))
    pools.append(values(CLASS, 5) + b''.join(values(i, 0, name, 0, 1) for i, name in [(1, 2), (2, 5), (3, 7), (4, 1), (5, 9)]))
    pools.append(values(METHOD, 3) + b''.join(values(*row) for row in [(1, 1, 3, 4, 1, 0), (2, 2, 6, 4, 1, 0), (3, 4, 8, 4, 1, 0)]))
    stack_entries = []
    for i in range(1, stacks + 1):
        # Distinct line numbers exercise metadata size and output tree growth.
        if i % 3 == 1:
            frames = [(2, 20 + (i // 3), 3, 1), (1, 10, 0, 1)]
        elif i % 3 == 2:
            frames = [(3, 0, 0, 3), (1, 10, 0, 1)]
        else:
            frames = [(2, 21 + (i // 3), 4, 2), (1, 10, 0, 0)]
        stack_entries.append(values(i, 0, len(frames)) + b''.join(values(method, line, bci) + bytes([kind]) for method, line, bci, kind in frames))
    pools.append(values(STACK, stacks) + b''.join(stack_entries))
    if tag_strings:
        pools.append(values(STRING, len(tag_strings)) + b''.join(varint(i) + string(s) for i, s in enumerate(tag_strings, 1)))
    pools.append(values(FTYPE, 7) + b''.join(varint(i) + string(s) for i, s in enumerate(['Interpreted', 'JIT compiled', 'Inlined', 'Native', 'C++', 'Kernel', 'C1 compiled'])))
    pools.append(values(STATE, 3) + b''.join(varint(i) + string(s) for i, s in enumerate(['STATE_DEFAULT', 'STATE_RUNNABLE', 'STATE_SLEEPING'])))
    return record(1, values(0, 0, 0, 1, len(pools)) + b''.join(pools))


def mixed_events(tick: int):
    return [
        record(EXEC, values(tick + 100, 1, 1, 0)),
        record(EXEC, values(tick + 200, 1, 1, 0)),
        record(EXEC, values(tick + 300, 2, 2, 0)),
        record(WALL, values(tick + 400, 1, 1, 1, 7)),
        record(WALL, values(tick + 500, 2, 3, 2, 3)),
        record(TLAB, values(tick + 1000, 1, 1, 3, 64, 1024)),
        record(OUTSIDE, values(tick + 1100, 1, 1, 3, 128)),
        record(ALLOC, values(tick + 1200, 2, 3, 3, 256)),
        record(LIVE, values(tick + 1300, 2, 3, 3, 512, tick + 700)),
        record(LOCK, values(tick + 1400, 20000, 1, 1, 5, 0, 0x42)),
        record(PARK, values(tick + 1500, 30000, 2, 3, 5, 0, 0, 0x43)),
        record(MALLOC, values(tick + 1600, 1, 2, 0x100, 2048)),
        record(MALLOC, values(tick + 1700, 2, 1, 0x200, 4096)),
        record(FREE, values(tick + 1900, 1, 2, 0x100)),
        record(SPAN, values(tick + 50, 400, 1, 1) + string('request-α')),
        record(SPAN, values(tick + 450, 100, 2, 3) + string('background')),
    ]


def write_chunk(out, events: list[bytes], repetitions: int = 1, label='demo', chunk_index=0, stacks=3, span_current=False, tag_strings=None, method_trace=False, ticks_per_second=TICKS):
    meta = metadata(span_current, method_trace)
    cp = constant_pool(label, stacks, tag_strings)
    event_bytes = sum(map(len, events)) * repetitions
    meta_offset = 68 + event_bytes
    cp_offset = meta_offset + len(meta)
    chunk_size = cp_offset + len(cp)
    start_ticks = TICKS + chunk_index * TICKS
    header = struct.pack('>4sHHQQQQQQQI', b'FLR\0', 2, 0, chunk_size, cp_offset, meta_offset, EPOCH_NS + chunk_index * TICKS, TICKS, start_ticks, ticks_per_second, 1)
    assert len(header) == 68
    out.write(header)
    batch = b''.join(events)
    for _ in range(repetitions):
        out.write(batch)
    out.write(meta)
    out.write(cp)


def generate(destination: Path, samples=0, chunks=2, stacks=3):
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open('wb') as out:
        for i in range(chunks):
            if samples:
                batch_size = max(1000, stacks)
                events = [record(EXEC, values(TICKS + i * TICKS + j * 1000, 1 + j % 2, 1 + j % stacks, 0)) for j in range(batch_size)]
                write_chunk(out, events, repetitions=max(1, samples // chunks // batch_size), label=f'chunk{i}', chunk_index=i, stacks=stacks)
            else:
                write_chunk(out, mixed_events(TICKS + i * TICKS), label='demo' if i == 0 else 'second', chunk_index=i, stacks=stacks)
    return destination


def generate_empty_stacks(destination: Path):
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open('wb') as out:
        events = [
            record(EXEC, values(TICKS + 100, 1, 0, 0)),
            record(EXEC, values(TICKS + 200, 2, 999, 0)),
            record(EXEC, values(TICKS + 300, 1, 1, 0)),
        ]
        write_chunk(out, events, label='demo', chunk_index=0)
    return destination


def generate_method_trace(destination: Path):
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open('wb') as out:
        events = [
            record(111, values(TICKS + 100, 75_000_000, 1, 2, 2)),
            record(111, values(TICKS + 200, 25_000_000, 2, 0, 2)),
        ]
        write_chunk(out, events, label='demo', chunk_index=0, method_trace=True, ticks_per_second=2_000_000_000)
    return destination


def generate_cross_span(destination: Path):
    """A current Span API tag from the string pool crossing a chunk boundary."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open('wb') as out:
        first = [
            record(EXEC, values(TICKS + 800_000_000, 1, 1, 0)),
            record(EXEC, values(TICKS + 950_000_000, 1, 1, 0)),
            record(EXEC, values(TICKS + 950_000_000, 2, 1, 0)),
            record(SPAN, values(TICKS + 900_000_000, 200_000_000, 1, 1)),
        ]
        write_chunk(out, first, label='before', chunk_index=0, span_current=True, tag_strings=['cross-request'])
        second = [
            record(EXEC, values(TICKS * 2 + 50_000_000, 1, 1, 0)),
            record(EXEC, values(TICKS * 2 + 150_000_000, 1, 1, 0)),
        ]
        write_chunk(out, second, label='after', chunk_index=1, span_current=True, tag_strings=['cross-request'])
    return destination


def generate_leaks(destination: Path):
    """Out-of-order frees and reused chunk IDs for native leak regression tests."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open('wb') as out:
        first = [
            record(MALLOC, values(TICKS + 100, 1, 1, 0x100, 1024)),
            record(FREE, values(TICKS + 300, 1, 1, 0x100)),
            record(MALLOC, values(TICKS + 200, 1, 2, 0x100, 2048)),
            record(MALLOC, values(TICKS + 400, 1, 1, 0x200, 4096)),
        ]
        write_chunk(out, first, label='original', chunk_index=0)
        second = [
            record(MALLOC, values(TICKS * 2 + 100, 1, 2, 0x300, 8192)),
            record(FREE, values(TICKS * 2 + 200, 1, 2, 0x300)),
        ]
        write_chunk(out, second, label='reused', chunk_index=1)
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--empty-stacks', action='store_true', help='write missing/zero stack regression fixture')
    parser.add_argument('--method-trace', action='store_true', help='write current MethodTrace API regression fixture')
    parser.add_argument('--cross-span', action='store_true', help='write a current Span API event crossing chunks')
    parser.add_argument('--leak-order', action='store_true', help='write out-of-order native allocation regression fixture')
    parser.add_argument('--samples', type=int, default=0, help='large CPU-only fixture; rounded to max(1000,stacks) samples/chunk')
    parser.add_argument('--chunks', type=int, default=2)
    parser.add_argument('--stacks', type=int, default=3)
    args = parser.parse_args()
    if args.samples < 0 or args.chunks <= 0 or args.stacks <= 0:
        parser.error('samples must be nonnegative; chunks and stacks must be positive')
    print(generate_empty_stacks(args.output) if args.empty_stacks else generate_method_trace(args.output) if args.method_trace else generate_cross_span(args.output) if args.cross_span else generate_leaks(args.output) if args.leak_order else generate(args.output, args.samples, args.chunks, args.stacks))


if __name__ == '__main__':
    main()
