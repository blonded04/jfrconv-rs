// Portions derived from async-profiler 4.1. Copyright The async-profiler authors.
// SPDX-License-Identifier: Apache-2.0
use crate::args::{Mode, Options};
use crate::jfr::{Chunk, Event, EventKind, EventMask, JfrReader};
use crate::model::{CachedPath, Profile};
use crate::{leak_sort, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

#[derive(Default)]
struct SpanFilter {
    intervals: HashMap<u64, Vec<(u64, u64)>>,
}
impl SpanFilter {
    fn read(path: &Path, o: &Options) -> Result<Self> {
        let mut result = Self::default();
        let mut reader = JfrReader::open(path)?;
        let mut found = false;
        while let Some(chunk) = reader.next_chunk()? {
            reader.events_selected(&chunk, EventMask::SPAN, |e| {
                if e.kind != EventKind::Span {
                    return Ok(());
                }
                found = true;
                let duration_ns =
                    (e.duration as u128 * 1_000_000_000 / chunk.ticks_per_sec as u128) as u64;
                if o.latency
                    .is_some_and(|min| (duration_ns as f64) < min * 1_000_000.)
                {
                    return Ok(());
                }
                if o.tag
                    .as_ref()
                    .is_some_and(|re| !e.tag.as_ref().is_some_and(|tag| re.is_match(tag)))
                {
                    return Ok(());
                }
                let start = event_ns(&chunk, e.time);
                result
                    .intervals
                    .entry(e.tid)
                    .or_default()
                    .push((start, start.saturating_add(duration_ns)));
                Ok(())
            })?;
        }
        if !found {
            return Err(
                "No profiler.Span / profiler.MethodTrace events found for span filtering".into(),
            );
        }
        for intervals in result.intervals.values_mut() {
            intervals.sort_unstable();
            let mut n = 0;
            for i in 0..intervals.len() {
                let pair = intervals[i];
                if n > 0 && pair.0 <= intervals[n - 1].1 {
                    intervals[n - 1].1 = intervals[n - 1].1.max(pair.1);
                } else {
                    intervals[n] = pair;
                    n += 1;
                }
            }
            intervals.truncate(n);
        }
        Ok(result)
    }
    fn contains(&self, tid: u64, time: u64) -> bool {
        self.intervals.get(&tid).is_some_and(|pairs| {
            let at = pairs.partition_point(|p| p.0 <= time);
            at > 0 && time <= pairs[at - 1].1
        })
    }
}

pub fn event_ns(chunk: &Chunk, ticks: u64) -> u64 {
    let delta = ticks as i128 - chunk.start_ticks as i128;
    (chunk.start_ns as i128 + delta * 1_000_000_000 / chunk.ticks_per_sec as i128)
        .clamp(0, u64::MAX as i128) as u64
}

#[derive(Clone, Copy)]
struct Representative {
    time: u64,
    tid: u64,
    stack_id: u64,
    class_id: u64,
    method_id: u64,
    kind: EventKind,
    tlab: bool,
}
impl Representative {
    fn expand(self) -> Event {
        Event {
            kind: self.kind,
            time: self.time,
            tid: self.tid,
            stack_id: self.stack_id,
            class_id: self.class_id,
            method_id: self.method_id,
            state: 0,
            samples: 1,
            value: 0,
            address: 0,
            duration: 0,
            tlab: self.tlab,
            tag: None,
            wall: false,
        }
    }
}
#[derive(Clone, Copy)]
struct Aggregate {
    event: Representative,
    samples: u64,
    value: u64,
}
/// Reproduce 4.1's deterministic coarsening order without allocating per event.
/// Slots contain only indexes. Large Event structs must not occupy empty slots.
struct Aggregator {
    slots: Vec<u32>,
    items: Vec<Aggregate>,
    threads: bool,
}
const EMPTY: u32 = u32::MAX;
impl Aggregator {
    fn new(threads: bool) -> Self {
        Self {
            slots: vec![EMPTY; 1024],
            items: Vec::new(),
            threads,
        }
    }
    fn hash(&self, e: &Representative) -> usize {
        let mut h = e.stack_id as u32;
        if matches!(
            e.kind,
            EventKind::Allocation | EventKind::Live | EventKind::Lock
        ) {
            h = h.wrapping_add((e.class_id as u32).wrapping_mul(127));
            if e.kind == EventKind::Allocation && !e.tlab {
                h = h.wrapping_add(17);
            }
        }
        if self.threads {
            h = h.wrapping_add((e.tid as u32).wrapping_mul(31));
        }
        h as usize
    }
    fn same(&self, a: &Representative, b: &Representative) -> bool {
        a.stack_id == b.stack_id
            && (!self.threads || a.tid == b.tid)
            && a.method_id == b.method_id
            && ((matches!(a.kind, EventKind::Malloc | EventKind::Free)
                && matches!(b.kind, EventKind::Malloc | EventKind::Free))
                || a.kind == b.kind
                    && (!matches!(
                        a.kind,
                        EventKind::Allocation | EventKind::Live | EventKind::Lock
                    ) || a.class_id == b.class_id)
                    && (a.kind != EventKind::Allocation || a.tlab == b.tlab))
    }
    fn insert(&mut self, item: Aggregate) {
        let mask = self.slots.len() - 1;
        let mut at = self.hash(&item.event) & mask;
        loop {
            let id = self.slots[at];
            if id == EMPTY {
                let id = u32::try_from(self.items.len()).expect("too many aggregate groups");
                self.items.push(item);
                self.slots[at] = id;
                break;
            } else if self.same(&self.items[id as usize].event, &item.event) {
                let old = &mut self.items[id as usize];
                old.samples = old.samples.saturating_add(item.samples);
                old.value = old.value.saturating_add(item.value);
                return;
            } else {
                at = (at + 1) & mask;
            }
        }
        if self.items.len() * 2 > self.slots.len() {
            let capacity = self.slots.len() * 2;
            let old = std::mem::replace(&mut self.slots, vec![EMPTY; capacity]);
            for id in old.into_iter().filter(|&id| id != EMPTY) {
                let mut at = self.hash(&self.items[id as usize].event) & (capacity - 1);
                while self.slots[at] != EMPTY {
                    at = (at + 1) & (capacity - 1);
                }
                self.slots[at] = id;
            }
        }
    }
    fn push(&mut self, e: Event) {
        self.insert(Aggregate {
            event: Representative {
                time: e.time,
                tid: e.tid,
                stack_id: e.stack_id,
                class_id: e.class_id,
                method_id: e.method_id,
                kind: e.kind,
                tlab: e.tlab,
            },
            samples: e.samples,
            value: e.value,
        });
    }
    fn finish(self, grain: f64) -> impl Iterator<Item = Aggregate> {
        let mut fraction = 0.;
        self.slots
            .into_iter()
            .filter(|&id| id != EMPTY)
            .filter_map(move |id| {
                let mut a = self.items[id as usize];
                if grain > 0. {
                    let count = a.samples as f64 / grain;
                    let mut rounded = count as u64;
                    fraction += count - rounded as f64;
                    if fraction >= 1. {
                        fraction -= 1.;
                        rounded += 1;
                    }
                    a.value = if a.samples == 0 {
                        0
                    } else {
                        (a.value as f64 * rounded as f64 / a.samples as f64) as u64
                    };
                    a.samples = rounded;
                    if rounded == 0 {
                        return None;
                    }
                }
                Some(a)
            })
    }
}

fn states(chunk: &Chunk, o: &Options) -> Result<Option<Vec<u64>>> {
    if o.mode != Mode::Execution {
        return Ok(None);
    }
    let map = chunk.enums.get("jdk.types.ThreadState");
    if let Some(names) = &o.state {
        let mut ids = Vec::new();
        for name in names {
            let id = map
                .and_then(|m| {
                    m.iter().find(|(_, value)| {
                        value
                            .strip_prefix("STATE_")
                            .is_some_and(|s| s.starts_with(name))
                    })
                })
                .map(|(id, _)| *id)
                .ok_or_else(|| format!("Unknown thread state {name}"))?;
            ids.push(id);
        }
        Ok(Some(ids))
    } else if o.cpu || o.wall {
        Ok(Some(
            map.into_iter()
                .flat_map(|m| m.iter())
                .filter(|(_, name)| (name.as_str() == "STATE_DEFAULT") == o.cpu)
                .map(|(id, _)| *id)
                .collect(),
        ))
    } else {
        Ok(None)
    }
}

fn selected(e: &Event, o: &Options) -> bool {
    match o.mode {
        Mode::Execution => e.kind == EventKind::Execution,
        Mode::Allocation => e.kind == EventKind::Allocation,
        Mode::Live => e.kind == EventKind::Live,
        Mode::Lock => e.kind == EventKind::Lock,
        Mode::Native => matches!(e.kind, EventKind::Malloc | EventKind::Free),
        Mode::Trace => e.kind == EventKind::Span,
    }
}

fn bound(ms: i64, start: u64, end: u64) -> i128 {
    ms as i128 * 1_000_000
        + if ms < 0 {
            end as i128
        } else if ms < 1_500_000_000_000 {
            start as i128
        } else {
            0
        }
}

#[derive(Clone)]
struct Outstanding {
    path: Arc<CachedPath>,
    time: u64,
    value: u64,
    thread: u32,
}

type HeatGroups = HashMap<(u64, u32), (u64, u64)>;
const HEAT_GROUPS: usize = 8192;
fn heat_record(out: &mut impl Write, time: u64, leaf: u32, count: u64, value: u64) -> Result<()> {
    let mut record = [0u8; 32];
    for (index, n) in [time, leaf as u64, count, value].into_iter().enumerate() {
        record[index * 8..index * 8 + 8].copy_from_slice(&n.to_le_bytes());
    }
    out.write_all(&record)?;
    Ok(())
}
fn flush_heat(groups: &mut HeatGroups, out: &mut impl Write, profile: &mut Profile) -> Result<()> {
    for ((time, leaf), (count, value)) in groups.drain() {
        heat_record(out, time, leaf, count, value)?;
        profile.credit_leaf(leaf, count, count, value, time);
    }
    Ok(())
}

pub fn jfr(
    path: &Path,
    o: &Options,
    profile: &mut Profile,
    spool: Option<&mut File>,
) -> Result<()> {
    let mut spool = spool.map(|file| BufWriter::with_capacity(128 * 1024, file));
    let filter = if o.span_filter() {
        Some(SpanFilter::read(path, o)?)
    } else {
        None
    };
    let mut reader = JfrReader::open(path)?;
    let start = reader.start_ns;
    let end = reader.end_ns;
    profile.start_ns = if profile.start_ns == 0 {
        start
    } else {
        profile.start_ns.min(start)
    };
    profile.end_ns = profile.end_ns.max(end);
    let mut outstanding: HashMap<u64, Outstanding> = HashMap::new();
    let mut min_time = u64::MAX;
    let mut max_time = 0;
    let otlp = o.output.as_deref() == Some("otlp");
    let mut heat_groups = HeatGroups::new();
    let mut heat_origin = None;
    let needs_event_time = o.from != 0
        || o.to != 0
        || filter.is_some()
        || o.leak
        || o.output.as_deref() == Some("heatmap");
    let mut observed_end = 0;
    while let Some(chunk) = reader.next_chunk()? {
        // Version 4.1 interprets negative offsets using the bounds observed so
        // far. Preserve that CLI behavior even though final timing is known.
        observed_end = observed_end.max(chunk.end_ns);
        if otlp && !o.leak {
            if let Some(spool) = spool.as_mut() {
                for n in [chunk.start_ns, u64::MAX, chunk.end_ns, 0, 0] {
                    spool.write_all(&n.to_le_bytes())?;
                }
            }
        }
        let from = if o.from == 0 {
            i128::MIN
        } else {
            bound(o.from, start, observed_end)
        };
        let to = if o.to == 0 {
            i128::MAX
        } else {
            bound(o.to, start, observed_end)
        };
        let thread_states = states(&chunk, o)?;
        let mut aggregator = Aggregator::new(o.threads);
        let mut sorter = leak_sort::Sorter::new();
        let mut cache = HashMap::new();
        let mut method_names = MethodNames::new();
        let event_mask = match o.mode {
            Mode::Execution => EventMask::EXECUTION,
            Mode::Allocation => EventMask::ALLOCATION,
            Mode::Live => EventMask::LIVE,
            Mode::Lock => EventMask::LOCK,
            Mode::Native => EventMask::NATIVE,
            Mode::Trace => EventMask::SPAN,
        };
        reader.events_selected(&chunk, event_mask, |e| {
            if !selected(&e, o) {
                return Ok(());
            }
            let time = if needs_event_time {
                event_ns(&chunk, e.time)
            } else {
                0
            };
            if (time as i128) < from || (time as i128) > to {
                return Ok(());
            }
            if thread_states
                .as_ref()
                .is_some_and(|s| !s.contains(&e.state))
            {
                return Ok(());
            }
            if filter.as_ref().is_some_and(|f| !f.contains(e.tid, time)) {
                return Ok(());
            }
            if o.leak {
                min_time = min_time.min(time);
                max_time = max_time.max(time);
                sorter.push(leak_sort::Record {
                    time: e.time,
                    address: e.address,
                    size: if e.kind == EventKind::Free {
                        0
                    } else {
                        e.value
                    },
                    tid: e.tid,
                    stack_id: e.stack_id,
                })?;
            } else if !otlp && spool.is_some() {
                let spool = spool.as_mut().unwrap();
                let key = (
                    e.stack_id,
                    e.class_id,
                    if o.threads { e.tid } else { 0 },
                    e.tlab,
                );
                let path = if let Some(found) = cache.get(&key) {
                    found
                } else {
                    let resolved = resolve_path(&chunk, &e, o, profile, &mut method_names);
                    cache.insert(key, resolved);
                    cache.get(&key).unwrap()
                };
                if let Some(path) = path {
                    let value = normalize_value(e.value, &chunk, o);
                    let heat_ms = chunk.start_ns as i128 / 1_000_000
                        + (e.time as i128 - chunk.start_ticks as i128) * 1_000
                            / chunk.ticks_per_sec as i128;
                    let heat_time = (heat_ms.max(0) as u64).saturating_mul(1_000_000);
                    // The original heatmap counts physical records, including
                    // one record for a coalesced wall-clock event. Preserve its
                    // initial timestamp before writing unordered partial groups.
                    if let Some(origin) = heat_origin {
                        let block = (heat_time as i128 - origin as i128) / 20_000_000;
                        let time =
                            (origin as i128 + block * 20_000_000).clamp(0, u64::MAX as i128) as u64;
                        let entry = heat_groups.entry((time, path.leaf)).or_default();
                        entry.0 = entry
                            .0
                            .checked_add(1)
                            .ok_or("Heatmap sample count overflow")?;
                        entry.1 = entry.1.saturating_add(value);
                        if heat_groups.len() >= HEAT_GROUPS {
                            flush_heat(&mut heat_groups, spool, profile)?;
                        }
                    } else {
                        heat_origin = Some(heat_time);
                        heat_record(spool, heat_time, path.leaf, 1, value)?;
                        profile.credit_leaf(path.leaf, 1, 1, value, heat_time);
                    }
                }
            } else {
                aggregator.push(e);
            }
            Ok(())
        })?;
        if !otlp {
            if let Some(out) = spool.as_mut() {
                flush_heat(&mut heat_groups, out, profile)?;
            }
        }
        if o.leak {
            let mut paths: HashMap<(u64, u64), Option<Arc<CachedPath>>> = HashMap::new();
            sorter.for_each(|r| {
                if r.size == 0 {
                    outstanding.remove(&r.address);
                } else {
                    let e = Event {
                        kind: EventKind::Malloc,
                        time: r.time,
                        tid: r.tid,
                        stack_id: r.stack_id,
                        class_id: 0,
                        method_id: 0,
                        state: 0,
                        samples: 1,
                        value: r.size,
                        address: r.address,
                        duration: 0,
                        tlab: false,
                        tag: None,
                        wall: false,
                    };
                    let key = (r.stack_id, if o.threads { r.tid } else { 0 });
                    let path = paths.entry(key).or_insert_with(|| {
                        resolve_path(&chunk, &e, o, profile, &mut method_names).map(Arc::new)
                    });
                    if let Some(path) = path {
                        let thread = if otlp {
                            profile.intern(&thread_name(&chunk, r.tid))
                        } else {
                            0
                        };
                        outstanding.insert(
                            r.address,
                            Outstanding {
                                path: Arc::clone(path),
                                time: event_ns(&chunk, r.time),
                                value: r.size,
                                thread,
                            },
                        );
                    } else {
                        outstanding.remove(&r.address);
                    }
                }
                Ok(())
            })?;
        } else if spool.is_none() || otlp {
            for a in aggregator.finish(o.grain) {
                if let Some(path) =
                    resolve_path(&chunk, &a.event.expand(), o, profile, &mut method_names)
                {
                    let value = normalize_value(a.value, &chunk, o);
                    profile.credit(
                        &path,
                        if o.total { value } else { a.samples },
                        a.samples,
                        value,
                        event_ns(&chunk, a.event.time),
                    );
                    if otlp {
                        if let Some(spool) = spool.as_mut() {
                            let thread = profile.intern(&thread_name(&chunk, a.event.tid));
                            for n in [
                                event_ns(&chunk, a.event.time),
                                path.leaf as u64,
                                a.samples,
                                value,
                                thread as u64,
                            ] {
                                spool.write_all(&n.to_le_bytes())?;
                            }
                        }
                    }
                }
            }
        }
        // All chunk-specific dictionaries, caches and aggregators drop here.
    }
    if o.leak && min_time != u64::MAX {
        if otlp {
            if let Some(spool) = spool.as_mut() {
                for n in [start, u64::MAX, end, 0, 0] {
                    spool.write_all(&n.to_le_bytes())?;
                }
            }
        }
        let cutoff = min_time.saturating_add(((max_time - min_time) as f64 * (1. - o.tail)) as u64);
        let mut grouped: HashMap<u32, (Arc<CachedPath>, u64, u64, u64, u32)> = HashMap::new();
        for item in outstanding.into_values().filter(|v| v.time <= cutoff) {
            let entry =
                grouped
                    .entry(item.path.leaf)
                    .or_insert((item.path, 0, 0, item.time, item.thread));
            entry.1 += 1;
            entry.2 = entry.2.saturating_add(item.value);
            entry.3 = entry.3.min(item.time);
        }
        let mut groups: Vec<_> = grouped.into_values().collect();
        groups.sort_unstable_by_key(|v| v.0.leaf);
        let mut fraction = 0.;
        for (path, mut count, mut value, time, thread) in groups {
            if o.grain > 0. {
                let scaled = count as f64 / o.grain;
                let mut new_count = scaled as u64;
                fraction += scaled - new_count as f64;
                if fraction >= 1. {
                    fraction -= 1.;
                    new_count += 1;
                }
                value = (value as f64 * new_count as f64 / count as f64) as u64;
                count = new_count;
            }
            if count == 0 {
                continue;
            }
            profile.credit(
                &path,
                if o.total { value } else { count },
                count,
                value,
                time,
            );
            if let Some(spool) = spool.as_mut() {
                for n in [time, path.leaf as u64, count, value] {
                    spool.write_all(&n.to_le_bytes())?;
                }
                if otlp {
                    spool.write_all(&(thread as u64).to_le_bytes())?;
                }
            }
        }
    }
    if let Some(spool) = spool.as_mut() {
        spool.flush()?;
    }
    Ok(())
}

fn normalize_value(value: u64, chunk: &Chunk, o: &Options) -> u64 {
    if matches!(o.mode, Mode::Lock | Mode::Trace) {
        (value as f64 * (1e9 / chunk.ticks_per_sec as f64)) as u64
    } else {
        value
    }
}

fn java_name(symbol: &str, array: bool, dotted: bool, o: &Options) -> String {
    let depth = if array {
        symbol.bytes().take_while(|b| *b == b'[').count()
    } else {
        0
    };
    let mut s = &symbol[depth..];
    if depth > 0 {
        s = match s {
            "B" => "byte",
            "C" => "char",
            "S" => "short",
            "I" => "int",
            "J" => "long",
            "Z" => "boolean",
            "F" => "float",
            "D" => "double",
            _ => s
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(s),
        };
    }
    if o.norm {
        if let Some(i) = s.rfind(['/', '.']) {
            if s.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit) {
                let mut end = i;
                if i > 19 && s.as_bytes()[i - 19] == b'+' && s.as_bytes()[i - 18] == b'0' {
                    end = i - 19;
                }
                s = &s[..end];
            }
        }
    }
    if o.simple {
        if let Some(i) = s.rfind('/') {
            if !s.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit) {
                s = &s[i + 1..];
            }
        }
    }
    let mut result = if dotted {
        s.replace('/', ".")
    } else {
        s.to_owned()
    };
    for _ in 0..depth {
        result.push_str("[]");
    }
    result
}

fn method_name(chunk: &Chunk, id: u64, kind: u8, o: &Options) -> String {
    let Some(method) = chunk.methods.get(&id) else {
        return "unknown".into();
    };
    let class = chunk
        .classes
        .get(&method.class)
        .and_then(|c| chunk.symbols.get(&c.name))
        .map(String::as_str)
        .unwrap_or("");
    let method = chunk
        .symbols
        .get(&method.name)
        .map(String::as_str)
        .unwrap_or("");
    let native = kind == 4
        || kind == 5
        || kind == 3
            && chunk
                .enums
                .get("jdk.types.FrameType")
                .is_some_and(|m| m.contains_key(&5));
    if class.is_empty() || native {
        method.into()
    } else {
        let class = java_name(class, false, o.dot, o);
        if method.is_empty() {
            class
        } else {
            format!("{class}.{method}")
        }
    }
}

fn class_name(chunk: &Chunk, id: u64, o: &Options) -> String {
    chunk
        .classes
        .get(&id)
        .and_then(|c| chunk.symbols.get(&c.name))
        .map(|s| java_name(s, true, true, o))
        .unwrap_or_else(|| "null".into())
}

fn thread_name(chunk: &Chunk, id: u64) -> String {
    match chunk.threads.get(&id) {
        Some(s) if s.starts_with("[tid=") => s.clone(),
        Some(s) => format!("[{s} tid={id}]"),
        None => format!("[tid={id}]"),
    }
}

fn category(frames: &[(Arc<str>, u8)]) -> (&'static str, u8) {
    let mut vm = false;
    for (name, kind) in frames.iter().rev() {
        if *kind == 4 {
            match name.as_ref() {
                "CompileBroker::compiler_thread_loop" => return ("[jit]", 4),
                "GCTaskThread::run" | "WorkerThread::run" => return ("[gc]", 4),
                "java_start" | "thread_native_entry" => vm = true,
                _ => {}
            }
        } else if *kind != 3 {
            break;
        }
    }
    if vm {
        return ("[vm]", 4);
    }
    for (i, (name, _)) in frames.iter().enumerate() {
        let c = if name.as_ref() == "Verifier::verify" {
            Some(("[class_verify]", 4))
        } else if name.starts_with("InstanceKlass::initialize") {
            Some(("[class_init]", 4))
        } else if name.starts_with("LinkResolver::")
            || name.starts_with("InterpreterRuntime::resolve")
            || name.starts_with("SystemDictionary::resolve")
        {
            Some(("[class_resolve]", 4))
        } else if name.ends_with("ClassLoader.loadClass") {
            Some(("[class_load]", 4))
        } else if name.ends_with("LambdaMetafactory.metafactory")
            || name.ends_with("LambdaMetafactory.altMetafactory")
        {
            Some(("[lambda_init]", 4))
        } else if name.ends_with("table stub") {
            Some(("[vtable_stubs]", 3))
        } else if name.as_ref() == "Interpreter" {
            Some(("[Interpreter]", 3))
        } else if name.starts_with("I2C/C2I") {
            Some(if frames.get(i + 1).is_some_and(|f| f.1 == 0) {
                ("[Interpreter]", 3)
            } else {
                ("[c2i_adapter]", 2)
            })
        } else {
            None
        };
        if let Some(c) = c {
            return c;
        }
    }
    let mut java = true;
    for (name, kind) in frames {
        match kind {
            0 => {
                return if java {
                    ("[Interpreter]", 3)
                } else {
                    ("[native]", 3)
                }
            }
            1 => {
                return if java {
                    ("[c2_comp]", 2)
                } else {
                    ("[native]", 3)
                }
            }
            2 => java = true,
            3 => {
                if ["JVM_", "Unsafe_", "MHN_", "jni_"]
                    .iter()
                    .any(|p| name.starts_with(p))
                    || [
                        "call_stub",
                        "deoptimization",
                        "unknown_Java",
                        "not_walkable_Java",
                        "InlineCacheBuffer",
                    ]
                    .contains(&name.as_ref())
                {
                    return ("[vm]", 4);
                }
                if !name.ends_with("_arraycopy") && !name.contains("pthread_cond") {
                    java = false;
                }
            }
            4 if name.starts_with("Runtime1::") => return ("[c1_comp]", 6),
            6 => {
                return if java {
                    ("[c1_comp]", 6)
                } else {
                    ("[native]", 3)
                }
            }
            _ => {}
        }
    }
    ("[native]", 3)
}

type MethodNames = HashMap<(u64, u8), Arc<str>>;
fn cached_method_name(
    chunk: &Chunk,
    id: u64,
    kind: u8,
    o: &Options,
    cache: &mut MethodNames,
) -> Arc<str> {
    Arc::clone(cache.entry((id, kind)).or_insert_with(|| {
        if o.output.as_deref() == Some("heatmap") && matches!(kind, 3..=5) {
            Arc::from(
                chunk
                    .methods
                    .get(&id)
                    .and_then(|m| chunk.symbols.get(&m.name))
                    .map(String::as_str)
                    .unwrap_or("unknown"),
            )
        } else {
            Arc::from(method_name(chunk, id, kind, o))
        }
    }))
}

fn resolve_path(
    chunk: &Chunk,
    e: &Event,
    o: &Options,
    p: &mut Profile,
    method_names: &mut MethodNames,
) -> Option<CachedPath> {
    if o.output.as_deref() == Some("heatmap") {
        let stack = chunk.stacks.get(&e.stack_id)?;
        let mut frames = Vec::with_capacity(stack.len() + 1);
        for f in stack.iter().rev() {
            let name = cached_method_name(chunk, f.method, f.kind, o, method_names);
            frames.push(p.frame_location(
                &name,
                f.kind,
                f.line as u32 as u16 as i32,
                f.bci as u32 as u16 as i32,
                0,
            ));
        }
        if e.class_id != 0 && matches!(e.kind, EventKind::Allocation | EventKind::Lock) {
            frames.push(p.frame_location(
                &class_name(chunk, e.class_id, o),
                if e.kind == EventKind::Allocation && e.tlab {
                    2
                } else {
                    5
                },
                -1,
                -1,
                0,
            ));
        }
        return Some(p.path_exact(&frames));
    }
    let flame = !matches!(
        o.output.as_deref(),
        Some("pprof" | "pb" | "pb.gz" | "gz" | "otlp")
    );
    let stack = match chunk.stacks.get(&e.stack_id) {
        Some(stack) => stack.as_slice(),
        None if !flame || e.method_id != 0 => &[],
        None => return None,
    };
    let names: Vec<_> = stack
        .iter()
        .map(|f| {
            (
                cached_method_name(chunk, f.method, f.kind, o, method_names),
                f.kind,
            )
        })
        .collect();
    let mut descriptions = Vec::with_capacity(stack.len() + 3);
    if o.threads {
        descriptions.push((Arc::from(thread_name(chunk, e.tid)), 3, 0, 1));
    }
    if o.classify {
        let (name, kind) = category(&names);
        descriptions.push((name.into(), kind, 0, 2));
    }
    for (f, (name, _)) in stack.iter().zip(&names).rev() {
        let mut name = Arc::clone(name);
        let line = (f.line as u32 & 0xffff) as i32;
        if flame {
            if o.lines && line != 0 {
                name = Arc::from(format!("{name}:{line}"));
            } else if o.bci && f.bci != 0 {
                name = Arc::from(format!("{name}@{}", f.bci as u32 & 0xffff));
            }
        }
        descriptions.push((name, f.kind, if flame { 0 } else { line }, 0));
    }
    if e.class_id != 0 && o.output.as_deref() != Some("otlp") {
        descriptions.push((
            Arc::from(class_name(chunk, e.class_id, o)),
            if e.kind == EventKind::Allocation && !e.tlab {
                5
            } else {
                2
            },
            0,
            0,
        ));
    }
    if e.method_id != 0 {
        descriptions.push((
            cached_method_name(chunk, e.method_id, 1, o, method_names),
            1,
            0,
            0,
        ));
    }
    // Reject filtered stacks before interning. Otherwise an include filter could
    // retain names for every discarded stack for the lifetime of the process.
    if flame
        && (o
            .exclude
            .as_ref()
            .is_some_and(|re| descriptions.iter().any(|d| re.is_match(&d.0)))
            || o.include
                .as_ref()
                .is_some_and(|re| !descriptions.iter().any(|d| re.is_match(&d.0))))
    {
        return None;
    }
    let frames = descriptions
        .into_iter()
        .map(|(name, kind, line, role)| p.frame(&name, kind, line, role))
        .collect();
    if flame {
        transform_path(frames, o, p).map(|frames| p.path(&frames))
    } else {
        Some(p.path(&frames))
    }
}

pub fn transform_path(mut frames: Vec<u32>, o: &Options, p: &mut Profile) -> Option<Vec<u32>> {
    if o.exclude.as_ref().is_some_and(|re| {
        frames
            .iter()
            .any(|&id| re.is_match(&p.strings[p.frames[id as usize].name as usize]))
    }) {
        return None;
    }
    if o.include.as_ref().is_some_and(|re| {
        !frames
            .iter()
            .any(|&id| re.is_match(&p.strings[p.frames[id as usize].name as usize]))
    }) {
        return None;
    }
    p.original_depth = p.original_depth.max(frames.len());
    if o.export.reverse {
        let skip = if o.skip == 0
            && frames
                .first()
                .is_some_and(|&id| p.frames[id as usize].role == 1)
        {
            1
        } else {
            o.skip
        };
        if skip >= frames.len() {
            frames.truncate(usize::from(skip == 1 && o.skip == 0));
        } else {
            if o.skip > 0 {
                frames.drain(..skip);
            }
            frames[if o.skip == 0 { skip } else { 0 }..].reverse();
        }
    } else {
        frames.drain(..o.skip.min(frames.len()));
    }
    Some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlapping_span_intervals_are_inclusive() {
        let f = SpanFilter {
            intervals: HashMap::from([(1, vec![(10, 20), (30, 50)])]),
        };
        assert!(f.contains(1, 20));
        assert!(!f.contains(1, 21));
        assert!(!f.contains(2, 15));
    }
    #[test]
    fn normalizes_lambda_arrays_and_simple_names() {
        let mut o = Options {
            norm: true,
            ..Options::default()
        };
        assert_eq!(
            java_name(
                "pkg/Foo$$Lambda+0x00007f8177090218/543846639",
                false,
                false,
                &o
            ),
            "pkg/Foo$$Lambda"
        );
        assert_eq!(java_name("[[I", true, true, &o), "int[][]");
        o.simple = true;
        assert_eq!(java_name("[Ljava/lang/String;", true, true, &o), "String[]");
    }
}
