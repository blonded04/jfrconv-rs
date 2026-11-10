// Copyright The async-profiler authors
// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
// Flame graph layout and protobuf schema follow async-profiler v4.1.

use crate::model::{ExportOptions, Node, Profile};
use crate::proto as pb;
use std::collections::HashMap;
use std::io::{self, Write};

fn node_kind(profile: &Profile, node: &Node) -> u8 {
    // Frame.getType() tests inlined * 3 >= total first, including total == 0.
    if (node.inlined as u128) * 3 >= node.total as u128 {
        return 2;
    }
    if (node.c1 as u128) * 2 >= node.total as u128 {
        return 6;
    }
    if (node.interpreted as u128) * 2 >= node.total as u128 {
        return 0;
    }
    profile.frames[node.frame as usize].kind
}

fn suffix(kind: u8) -> &'static str {
    match kind {
        0 => "_[0]",
        1 => "_[j]",
        2 => "_[i]",
        5 => "_[k]",
        6 => "_[1]",
        _ => "",
    }
}

/// Compact adjacency index, built only for output. Sorting is in-place and the
/// index uses eight bytes per trie node rather than a HashMap per call frame.
struct Children {
    starts: Vec<u32>,
    ids: Vec<u32>,
}

impl Children {
    fn new(profile: &Profile) -> Self {
        let mut starts = vec![0u32; profile.nodes.len() + 1];
        let mut ids: Vec<u32> = (1..profile.nodes.len() as u32).collect();
        for node in profile.nodes.iter().skip(1) {
            starts[node.parent as usize + 1] += 1;
        }
        for i in 1..starts.len() {
            starts[i] += starts[i - 1];
        }
        let encountered_titles = if profile.source_title_ids.is_none() {
            let mut titles = vec![u32::MAX; profile.strings.len()];
            titles[0] = 0;
            let mut next = 1;
            for node in profile.nodes.iter().skip(1) {
                let name = profile.frames[node.frame as usize].name as usize;
                if titles[name] == u32::MAX {
                    titles[name] = next;
                    next += 1;
                }
            }
            Some(titles)
        } else {
            None
        };
        let titles = profile
            .source_title_ids
            .as_ref()
            .or(encountered_titles.as_ref())
            .unwrap();
        let bucket = |node: &Node| {
            let parent = node.parent as usize;
            let size = (starts[parent + 1] - starts[parent]) as u64;
            // computeIfAbsent checks the old size before insertion; put checks
            // the new size afterwards (JDK25 verified at 13/14 and3073/3074).
            let size = if profile.html_encounter_order {
                size
            } else {
                size.saturating_sub(1)
            };
            let capacity = (size * 4)
                .div_ceil(3)
                .max(16)
                .next_power_of_two()
                .min(1 << 30) as u32;
            let frame = profile.frames[node.frame as usize];
            let title = titles[frame.name as usize];
            let title = if title == u32::MAX { frame.name } else { title };
            let key = title | ((frame.kind as u32) << 28);
            (key ^ (key >> 16)) & (capacity - 1)
        };
        ids.sort_unstable_by(|&a, &b| {
            let na = &profile.nodes[a as usize];
            let nb = &profile.nodes[b as usize];
            na.parent.cmp(&nb.parent).then_with(|| {
                let fa = &profile.frames[na.frame as usize];
                let fb = &profile.frames[nb.frame as usize];
                profile.strings[fa.name as usize]
                    .encode_utf16()
                    .cmp(profile.strings[fb.name as usize].encode_utf16())
                    // Java sorts by title only, retaining HashMap encounter
                    // order for ties. Ordinary bins use computeIfAbsent head
                    // insertion; parseHtml uses put tail insertion. Treeified
                    // collision-bin reordering is deliberately not emulated.
                    .then_with(|| bucket(na).cmp(&bucket(nb)))
                    .then_with(|| {
                        if profile.html_encounter_order {
                            a.cmp(&b)
                        } else {
                            b.cmp(&a)
                        }
                    })
            })
        });
        Self { starts, ids }
    }

    fn children(&self, id: usize) -> &[u32] {
        &self.ids[self.starts[id] as usize..self.starts[id + 1] as usize]
    }
}

struct Visit {
    id: u32,
    next: usize,
    next_x: u64,
}

fn walk(
    profile: &Profile,
    children: &Children,
    cutoff: u64,
    mut visit: impl FnMut(u32, usize, u64) -> io::Result<()>,
) -> io::Result<()> {
    if profile.nodes.is_empty() {
        return Ok(());
    }
    visit(0, 0, 0)?;
    let mut stack = vec![Visit {
        id: 0,
        next: 0,
        next_x: profile.nodes[0].self_value,
    }];
    while let Some(top) = stack.last_mut() {
        let siblings = children.children(top.id as usize);
        if top.next == siblings.len() {
            stack.pop();
            continue;
        }
        let id = siblings[top.next];
        top.next += 1;
        let x = top.next_x;
        let node = &profile.nodes[id as usize];
        top.next_x = top.next_x.saturating_add(node.total);
        if node.total < cutoff {
            continue;
        }
        let level = stack.len();
        visit(id, level, x)?;
        stack.push(Visit {
            id,
            next: 0,
            next_x: x.saturating_add(node.self_value),
        });
    }
    Ok(())
}

pub fn collapsed(
    profile: &Profile,
    out: &mut dyn Write,
    opts: &ExportOptions,
) -> crate::Result<()> {
    let children = Children::new(profile);
    let cutoff = (profile.nodes[0].total as f64 * opts.minwidth / 100.0) as u64;
    let mut path: Vec<u32> = Vec::new();
    walk(profile, &children, cutoff, |id, level, _| {
        if level == 0 {
            return Ok(());
        }
        path.truncate(level - 1);
        path.push(id);
        let node = &profile.nodes[id as usize];
        if node.self_value != 0 {
            for (i, &part) in path.iter().enumerate() {
                if i > 0 {
                    out.write_all(b";")?;
                }
                let part = &profile.nodes[part as usize];
                let frame = &profile.frames[part.frame as usize];
                out.write_all(profile.strings[frame.name as usize].as_bytes())?;
                out.write_all(suffix(node_kind(profile, part)).as_bytes())?;
            }
            if profile.is_diff {
                let delta = profile.diffs.as_ref().and_then(|ds| ds[id as usize]);
                let base = delta.map_or(0, |d| node.self_value as i128 - d as i128);
                write!(out, " {base}")?;
            }
            writeln!(out, " {}", node.self_value)?;
        }
        Ok(())
    })?;
    Ok(())
}

fn js_string(out: &mut dyn Write, value: &str) -> io::Result<()> {
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.write_all(b"\\\\")?,
            '\'' => out.write_all(b"\\'")?,
            '\n' => out.write_all(b"\\n")?,
            '\r' => out.write_all(b"\\r")?,
            '\t' => out.write_all(b"\\t")?,
            '<' if chars.peek() == Some(&'/') => out.write_all(b"\\x3c")?,
            '\u{2028}' => out.write_all(b"\\u2028")?,
            '\u{2029}' => out.write_all(b"\\u2029")?,
            c if c.is_control() => write!(out, "\\u{:04x}", c as u32)?,
            c => {
                let mut bytes = [0; 4];
                out.write_all(c.encode_utf8(&mut bytes).as_bytes())?;
            }
        }
    }
    Ok(())
}

fn template_until(out: &mut dyn Write, tail: &mut &str, marker: &str) -> io::Result<()> {
    let pos = tail
        .find(marker)
        .ok_or_else(|| io::Error::other("invalid flame graph template"))?;
    out.write_all(&tail.as_bytes()[..pos])?;
    *tail = &tail[pos + marker.len()..];
    Ok(())
}

pub fn html(profile: &Profile, out: &mut dyn Write, opts: &ExportOptions) -> crate::Result<()> {
    let children = Children::new(profile);
    let cutoff = (profile.nodes[0].total as f64 * opts.minwidth / 100.0) as u64;
    let mut depth = 1;
    walk(profile, &children, cutoff, |_, level, _| {
        depth = depth.max(level + 1);
        Ok(())
    })?;
    if cutoff <= 1 {
        depth = profile.original_depth + 1;
    }
    let mut tail = if profile.is_diff {
        include_str!("../assets/flame-diff.html")
    } else {
        include_str!("../assets/flame.html")
    };
    template_until(out, &mut tail, "/*height:*/300")?;
    write!(out, "{}", (depth * 16).min(32767))?;
    template_until(out, &mut tail, "/*title:*/")?;
    // --title is trusted CLI markup, matching async-profiler 4.1 exactly.
    out.write_all(opts.title.as_bytes())?;
    template_until(out, &mut tail, "/*inverted:*/false")?;
    write!(out, "{}", opts.reverse ^ opts.inverted)?;
    if profile.is_diff {
        template_until(out, &mut tail, "/*treeview:*/false")?;
        out.write_all(b"false")?;
        template_until(out, &mut tail, "/*units:*/")?;
        js_string(
            out,
            if profile.unit == "count" {
                "samples"
            } else {
                &profile.unit
            },
        )?;
        template_until(out, &mut tail, "/*maxdiff:*/-1")?;
        let max = profile
            .diffs
            .as_ref()
            .into_iter()
            .flatten()
            .flatten()
            .map(|d| d.unsigned_abs())
            .max()
            .unwrap_or(0);
        write!(out, "{max}")?;
    }
    template_until(out, &mut tail, "/*depth:*/0")?;
    write!(out, "{depth}")?;
    template_until(out, &mut tail, "/*cpool:*/")?;

    let mut strings: Vec<u32> = (1..profile.strings.len() as u32).collect();
    strings.sort_unstable_by(|&a, &b| {
        profile.strings[a as usize]
            .encode_utf16()
            .cmp(profile.strings[b as usize].encode_utf16())
    });
    let mut order = vec![0u32; profile.strings.len()];
    out.write_all(b"'all'")?;
    let mut previous = "";
    for (i, &id) in strings.iter().enumerate() {
        let text = &*profile.strings[id as usize];
        let common = previous
            .bytes()
            .zip(text.bytes())
            .take(95)
            .take_while(|(a, b)| a == b && *a < 128)
            .count();
        write!(out, ",\n'")?;
        if common as u8 + b' ' == b'<' && text[common..].starts_with('/') {
            out.write_all(b"\\x3c")?;
        } else {
            js_string(out, &((common as u8 + b' ') as char).to_string())?;
        }
        js_string(out, &text[common..])?;
        out.write_all(b"'")?;
        order[id as usize] = i as u32 + 1;
        previous = text;
    }
    drop(strings);
    template_until(out, &mut tail, "/*frames:*/")?;
    let mut last_x = 0;
    walk(profile, &children, cutoff, |id, level, x| {
        let node = &profile.nodes[id as usize];
        let frame = &profile.frames[node.frame as usize];
        let key = (order[frame.name as usize] as u64) << 3 | node_kind(profile, node) as u64;
        if profile.is_diff {
            if let Some(delta) = profile.diffs.as_ref().and_then(|ds| ds[id as usize]) {
                writeln!(out, "d={delta}")?;
            } else {
                out.write_all(b"d=U\n")?;
            }
        }
        write!(
            out,
            "f({key},{level},{},{}",
            x as i128 - last_x as i128,
            node.total
        )?;
        let mixed = node.inlined | node.c1 | node.interpreted != 0
            && node.inlined < node.total
            && node.interpreted < node.total;
        if mixed {
            write!(out, ",{},{},{}", node.inlined, node.c1, node.interpreted)?;
        }
        out.write_all(b")\n")?;
        last_x = x;
        Ok(())
    })?;
    template_until(out, &mut tail, "/*highlight:*/")?;
    if let Some(highlight) = &opts.highlight {
        out.write_all(b"'")?;
        js_string(out, highlight)?;
        out.write_all(b"'")?;
    }
    out.write_all(tail.as_bytes())?;
    Ok(())
}

struct Path<'a> {
    profile: &'a Profile,
    next: u32,
}

impl Iterator for Path<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.next == 0 {
            return None;
        }
        let node = &self.profile.nodes[self.next as usize];
        self.next = node.parent;
        Some(node.frame)
    }
}

fn path(profile: &Profile, node: u32) -> Path<'_> {
    Path {
        profile,
        next: node,
    }
}

/// pprof's string, function, and location tables directly reference the existing
/// interned frame table. Samples and all protobuf fields stream to the writer.
pub fn pprof(profile: &Profile, out: &mut dyn Write, _opts: &ExportOptions) -> crate::Result<()> {
    let mut used_frames = vec![false; profile.frames.len()];
    let mut used_names = vec![false; profile.strings.len()];
    for node in profile.nodes.iter().skip(1) {
        let frame = &profile.frames[node.frame as usize];
        if frame.role == 0 {
            used_frames[node.frame as usize] = true;
            used_names[frame.name as usize] = true;
        }
    }
    let base = profile.strings.len() as u64 + 1;
    pb::message(out, 1, |out| {
        pb::uint(out, 1, base)?;
        pb::uint(out, 2, base + 1)
    })?;
    pb::uint(out, 13, base + 3)?;
    for (id, node) in profile.nodes.iter().enumerate() {
        if node.self_value == 0 && node.samples == 0 && node.value == 0 {
            continue;
        }
        pb::message(out, 2, |out| {
            pb::message(out, 1, |out| {
                for frame in path(profile, id as u32) {
                    if profile.frames[frame as usize].role == 0 {
                        pb::varint(out, frame as u64 + 1)?;
                    }
                }
                Ok(())
            })?;
            pb::uint(out, 2, node.self_value)?;
            for frame in path(profile, id as u32) {
                let frame = &profile.frames[frame as usize];
                let key = match frame.role {
                    1 => base + 5,
                    2 => base + 6,
                    _ => continue,
                };
                pb::message(out, 3, |out| {
                    pb::uint(out, 1, key)?;
                    pb::uint(out, 2, frame.name as u64 + 1)
                })?;
            }
            Ok(())
        })?;
    }
    pb::message(out, 3, |out| {
        pb::uint(out, 1, 1)?;
        pb::uint(out, 2, 0)?;
        pb::uint(out, 3, i64::MAX as u64)?;
        pb::uint(out, 5, base + 4)
    })?;
    for (id, frame) in profile.frames.iter().enumerate().skip(1) {
        if !used_frames[id] {
            continue;
        }
        let location = id as u64 + 1;
        let function = frame.name as u64 + 1;
        let line = frame.line.max(0) as u64;
        let line_size = 2 + varint_size(function) + varint_size(line);
        pb::header(
            out,
            4,
            1 + varint_size(location) + message_size(4, line_size),
        )?;
        pb::uint(out, 1, location)?;
        pb::header(out, 4, line_size)?;
        pb::uint(out, 1, function)?;
        pb::uint(out, 2, line)?;
    }
    for (id, &used) in used_names.iter().enumerate() {
        if used {
            let function = id as u64 + 1;
            pb::header(out, 5, 2 + 2 * varint_size(function))?;
            pb::uint(out, 1, function)?;
            pb::uint(out, 2, function)?;
        }
    }
    pb::bytes(out, 6, b"")?;
    for text in &profile.strings {
        pb::bytes(out, 6, text.as_bytes())?;
    }
    for text in [
        profile.value_type.as_str(),
        profile.unit.as_str(),
        profile.total_unit.as_str(),
        "Produced by jfrconv-rs (async-profiler 4.1 compatible)",
        "jfrconv-rs",
        "thread",
        "category",
    ] {
        pb::bytes(out, 6, text.as_bytes())?;
    }
    pb::uint(out, 9, profile.start_ns)?;
    pb::uint(out, 10, profile.end_ns.saturating_sub(profile.start_ns))?;
    Ok(())
}

/// Emits the OTLP Profiles schema shipped by async-profiler 4.1. The trie holds
/// aggregate samples: each has the earliest timestamp observed for that path.
#[cfg(test)]
pub fn otlp(profile: &Profile, out: &mut dyn Write, _opts: &ExportOptions) -> crate::Result<()> {
    let locations = otlp_locations(profile);
    let mut attributes = vec![(0u8, 0u32)];
    let mut attribute_ids = HashMap::new();
    for frame in &profile.frames {
        if frame.role != 0 {
            let key = (frame.role, frame.name);
            if let std::collections::hash_map::Entry::Vacant(entry) = attribute_ids.entry(key) {
                let id = attributes.len() as u64;
                attributes.push(key);
                entry.insert(id);
            }
        }
    }
    let base = profile.strings.len() as u64;
    pb::message(out, 1, |out| {
        pb::message(out, 2, |out| {
            pb::message(out, 2, |out| {
                for unit in [base + 1, base + 2] {
                    pb::message(out, 1, |out| {
                        pb::uint(out, 1, base)?;
                        pb::uint(out, 2, unit)?;
                        pb::uint(out, 3, 2)
                    })?;
                }
                pb::uint(out, 4, profile.start_ns)?;
                pb::uint(out, 5, profile.end_ns.saturating_sub(profile.start_ns))?;
                let mut next_location = 0;
                for (id, node) in profile.nodes.iter().enumerate() {
                    if node.self_value == 0 && node.samples == 0 && node.value == 0 {
                        continue;
                    }
                    let length = path(profile, id as u32)
                        .filter(|&f| profile.frames[f as usize].role == 0)
                        .count() as u64;
                    pb::message(out, 2, |out| {
                        pb::uint(out, 1, next_location)?;
                        pb::uint(out, 2, length)?;
                        pb::uint(out, 6, node.time_ns)?;
                        pb::message(out, 3, |out| {
                            pb::varint(out, node.samples)?;
                            pb::varint(out, node.value)
                        })?;
                        for f in path(profile, id as u32) {
                            let frame = &profile.frames[f as usize];
                            if frame.role != 0 {
                                pb::uint(out, 4, attribute_ids[&(frame.role, frame.name)])?;
                            }
                        }
                        Ok(())
                    })?;
                    next_location += length;
                }
                pb::message(out, 3, |out| {
                    for (id, node) in profile.nodes.iter().enumerate() {
                        if node.self_value == 0 && node.samples == 0 && node.value == 0 {
                            continue;
                        }
                        for f in path(profile, id as u32) {
                            if profile.frames[f as usize].role == 0 {
                                pb::varint(out, locations[f as usize] as u64)?;
                            }
                        }
                    }
                    Ok(())
                })
            })
        })
    })?;

    write_otlp_dictionary(profile, out, &attributes, &locations)?;
    Ok(())
}

/// Compact location IDs include only frames referenced by the resolved trie.
/// Original Java execution variants can remain in Profile.frames after color
/// normalization, but need not inflate protobuf dictionaries.
fn otlp_locations(profile: &Profile) -> Vec<u32> {
    let mut locations = vec![u32::MAX; profile.frames.len()];
    locations[0] = 0;
    for node in profile.nodes.iter().skip(1) {
        if profile.frames[node.frame as usize].role == 0 {
            locations[node.frame as usize] = 0;
        }
    }
    let mut next = 1;
    for location in locations.iter_mut().skip(1) {
        if *location != u32::MAX {
            *location = next;
            next += 1;
        }
    }
    locations
}

fn write_otlp_dictionary(
    profile: &Profile,
    out: &mut dyn Write,
    attributes: &[(u8, u32)],
    locations: &[u32],
) -> io::Result<()> {
    // Dictionary field sizes are known from integers/string lengths. Computing
    // these directly avoids serializing large tables repeatedly into Count.
    let mut size = message_size(1, 0);
    for name in 0..profile.strings.len() {
        size += message_size(3, 1 + varint_size(name as u64));
    }
    for (id, frame) in profile.frames.iter().enumerate() {
        if locations[id] != u32::MAX {
            let line = 2 + varint_size(frame.name as u64) + varint_size(frame.line.max(0) as u64);
            size += message_size(2, 2 + message_size(3, line));
        }
    }
    for text in &profile.strings {
        size += message_size(5, text.len() as u64);
    }
    for text in [
        profile.value_type.as_str(),
        "count",
        profile.total_unit.as_str(),
    ] {
        size += message_size(5, text.len() as u64);
    }
    for &(role, name) in attributes {
        let key = match role {
            1 => "thread.name",
            2 => "category",
            _ => "",
        };
        let value = profile.strings[name as usize].len() as u64;
        size += message_size(
            6,
            message_size(1, key.len() as u64) + message_size(2, message_size(1, value)),
        );
    }
    pb::header(out, 2, size)?;
    pb::message(out, 1, |_| Ok(()))?; // required default mapping
    for name in 0..profile.strings.len() {
        pb::header(out, 3, 1 + varint_size(name as u64))?;
        pb::uint(out, 1, name as u64)?;
    }
    for (id, frame) in profile.frames.iter().enumerate() {
        if locations[id] == u32::MAX {
            continue;
        }
        let line = frame.line.max(0) as u64;
        let line_size = 2 + varint_size(frame.name as u64) + varint_size(line);
        pb::header(out, 2, 2 + message_size(3, line_size))?;
        pb::uint(out, 1, 0)?;
        pb::header(out, 3, line_size)?;
        pb::uint(out, 1, frame.name as u64)?;
        pb::uint(out, 2, line)?;
    }
    for text in &profile.strings {
        pb::bytes(out, 5, text.as_bytes())?;
    }
    for text in [
        profile.value_type.as_str(),
        "count",
        profile.total_unit.as_str(),
    ] {
        pb::bytes(out, 5, text.as_bytes())?;
    }
    for &(role, name) in attributes {
        pb::message(out, 6, |out| {
            let key = match role {
                1 => "thread.name",
                2 => "category",
                _ => "",
            };
            pb::bytes(out, 1, key.as_bytes())?;
            pb::message(out, 2, |out| {
                pb::bytes(out, 1, profile.strings[name as usize].as_bytes())
            })
        })?;
    }
    Ok(())
}

struct OtlpChunk {
    start_ns: u64,
    end_ns: u64,
    data_start: u64,
    data_end: u64,
    encoded_size: u64,
}

fn spool_record(reader: &mut impl std::io::Read) -> io::Result<[u64; 5]> {
    let mut bytes = [0u8; 40];
    reader.read_exact(&mut bytes)?;
    let mut record = [0; 5];
    for (i, value) in record.iter_mut().enumerate() {
        *value = u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
    }
    Ok(record)
}

fn spool_records(
    spool: &mut std::fs::File,
    chunk: &OtlpChunk,
    mut visit: impl FnMut([u64; 5]) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::{BufReader, Seek, SeekFrom};
    spool.seek(SeekFrom::Start(chunk.data_start))?;
    let mut reader = BufReader::with_capacity(64 * 1024, spool);
    for _ in 0..(chunk.data_end - chunk.data_start) / 40 {
        visit(spool_record(&mut reader)?)?;
    }
    Ok(())
}

fn varint_size(mut value: u64) -> u64 {
    let mut bytes = 1;
    while value >= 128 {
        value >>= 7;
        bytes += 1;
    }
    bytes
}

fn message_size(field: u32, length: u64) -> u64 {
    varint_size(((field as u64) << 3) | 2) + varint_size(length) + length
}

fn write_otlp_chunk(
    profile: &Profile,
    spool: &mut std::fs::File,
    chunk: &OtlpChunk,
    attributes: &HashMap<(u8, u32), u64>,
    locations: &[u32],
    out: &mut dyn Write,
) -> io::Result<()> {
    let base = profile.strings.len() as u64;
    for unit in [base + 1, base + 2] {
        pb::message(out, 1, |out| {
            pb::uint(out, 1, base)?;
            pb::uint(out, 2, unit)?;
            pb::uint(out, 3, 2)
        })?;
    }
    pb::uint(out, 4, chunk.start_ns)?;
    pb::uint(out, 5, chunk.end_ns.saturating_sub(chunk.start_ns))?;
    let mut next_location = 0;
    let mut packed_size = 0;
    spool_records(spool, chunk, |record| {
        let [time_ns, leaf, samples, value, thread_name] = record;
        let mut length = 0;
        let mut category_size = 0;
        for id in path(profile, leaf as u32) {
            let frame = &profile.frames[id as usize];
            if frame.role == 0 {
                length += 1;
                packed_size += varint_size(locations[id as usize] as u64);
            } else if frame.role == 2 {
                category_size += 1 + varint_size(attributes[&(2, frame.name)]);
            }
        }
        let thread = attributes[&(1, thread_name as u32)];
        let values_size = varint_size(samples) + varint_size(value);
        let sample_size = 4
            + varint_size(next_location)
            + varint_size(length)
            + varint_size(time_ns)
            + varint_size(thread)
            + category_size
            + message_size(3, values_size);
        pb::header(out, 2, sample_size)?;
        pb::uint(out, 1, next_location)?;
        pb::uint(out, 2, length)?;
        pb::uint(out, 6, time_ns)?;
        pb::uint(out, 4, thread)?;
        for id in path(profile, leaf as u32) {
            let frame = &profile.frames[id as usize];
            if frame.role == 2 {
                pb::uint(out, 4, attributes[&(2, frame.name)])?;
            }
        }
        pb::header(out, 3, values_size)?;
        pb::varint(out, samples)?;
        pb::varint(out, value)?;
        next_location += length;
        Ok(())
    })?;
    pb::header(out, 3, packed_size)?;
    spool_records(spool, chunk, |record| {
        for id in path(profile, record[1] as u32) {
            if profile.frames[id as usize].role == 0 {
                pb::varint(out, locations[id as usize] as u64)?;
            }
        }
        Ok(())
    })
}

/// OTLP conversion retains chunk boundaries, representative event timestamps and
/// default thread attributes using a small temporary spool, not an events Vec.
pub fn otlp_spooled(
    profile: &Profile,
    spool: &mut std::fs::File,
    out: &mut dyn Write,
    _opts: &ExportOptions,
) -> crate::Result<()> {
    let locations = otlp_locations(profile);
    use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom};
    let bytes = spool.metadata()?.len();
    if bytes % 40 != 0 {
        return Err("truncated OTLP sample spool".into());
    }
    spool.seek(SeekFrom::Start(0))?;
    let mut attributes = vec![(0u8, 0u32)];
    let mut attribute_ids = HashMap::new();
    for frame in &profile.frames {
        if frame.role == 2 {
            let key = (2, frame.name);
            if let std::collections::hash_map::Entry::Vacant(entry) = attribute_ids.entry(key) {
                entry.insert(attributes.len() as u64);
                attributes.push(key);
            }
        }
    }
    let mut chunks: Vec<OtlpChunk> = Vec::new();
    {
        let mut reader = BufReader::with_capacity(64 * 1024, &mut *spool);
        for offset in (0..bytes).step_by(40) {
            let record = spool_record(&mut reader)?;
            if record[1] == u64::MAX {
                if let Some(previous) = chunks.last_mut() {
                    previous.data_end = offset;
                }
                if record[2] < record[0] {
                    return Err("invalid OTLP chunk time range".into());
                }
                chunks.push(OtlpChunk {
                    start_ns: record[0],
                    end_ns: record[2],
                    data_start: offset + 40,
                    data_end: bytes,
                    encoded_size: 0,
                });
            } else {
                if chunks.is_empty() {
                    return Err("OTLP sample precedes its chunk header".into());
                }
                if record[1] >= profile.nodes.len() as u64
                    || record[4] >= profile.strings.len() as u64
                {
                    return Err("invalid OTLP sample spool reference".into());
                }
                let key = (1, record[4] as u32);
                if let std::collections::hash_map::Entry::Vacant(entry) = attribute_ids.entry(key) {
                    entry.insert(attributes.len() as u64);
                    attributes.push(key);
                }
            }
        }
    }
    let mut scope_size: u64 = 0;
    // Encode each chunk once. A temporary file supplies lengths required by
    // enclosing protobuf messages without serializing every sample into Count
    // and repeating all raw-spool scans. The file is unlinked automatically.
    let mut encoded = BufWriter::with_capacity(128 * 1024, tempfile::tempfile()?);
    let mut previous_end = 0;
    for chunk in &mut chunks {
        write_otlp_chunk(
            profile,
            spool,
            chunk,
            &attribute_ids,
            &locations,
            &mut encoded,
        )?;
        let end = encoded.get_mut().stream_position()? + encoded.buffer().len() as u64;
        chunk.encoded_size = end - previous_end;
        previous_end = end;
        scope_size = scope_size
            .checked_add(message_size(2, chunk.encoded_size))
            .ok_or("OTLP profile size overflow")?;
    }
    let resource_size = message_size(2, scope_size);
    let mut encoded = encoded.into_inner().map_err(|e| e.into_error())?;
    encoded.seek(SeekFrom::Start(0))?;
    let mut encoded = BufReader::with_capacity(128 * 1024, encoded);
    pb::header(out, 1, resource_size)?;
    pb::header(out, 2, scope_size)?;
    for chunk in &chunks {
        pb::header(out, 2, chunk.encoded_size)?;
        let mut remaining = chunk.encoded_size;
        while remaining != 0 {
            let bytes = encoded.fill_buf()?;
            if bytes.is_empty() {
                return Err("truncated encoded OTLP chunk spool".into());
            }
            let n = bytes.len().min(remaining as usize);
            out.write_all(&bytes[..n])?;
            encoded.consume(n);
            remaining -= n as u64;
        }
    }
    write_otlp_dictionary(profile, out, &attributes, &locations)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ExportOptions {
        ExportOptions {
            title: "Test <profile>".into(),
            minwidth: 0.0,
            reverse: false,
            inverted: false,
            highlight: None,
        }
    }

    fn fixture() -> Profile {
        let mut p = Profile::new("allocations", "bytes", "bytes");
        p.start_ns = 1_000;
        p.end_ns = 2_000;
        let t = p.frame("[worker tid=9]", 3, 0, 1);
        let c = p.frame("[c2_comp]", 2, 0, 2);
        let a = p.frame("pkg/Main.run", 1, 17, 0);
        let b = p.frame("leaf", 3, 0, 0);
        let path = p.path(&[t, c, a, b]);
        p.credit(&path, 600, 3, 600, 1_100);
        p
    }

    #[test]
    fn collapsed_retains_prefixes_and_execution_suffixes() {
        let p = fixture();
        let mut bytes = Vec::new();
        collapsed(&p, &mut bytes, &options()).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "[worker tid=9];[c2_comp]_[i];pkg/Main.run_[j];leaf 600\n"
        );
    }

    #[test]
    fn differential_exports_use_upstream_ui_and_reconstruct_current_graph() {
        let base = fixture();
        let mut current = fixture();
        let leaf = current
            .nodes
            .iter()
            .rposition(|n| n.self_value != 0)
            .unwrap();
        current.nodes[leaf].self_value = 597;
        for node in &mut current.nodes {
            node.total -= 3;
        }
        let fresh = current.frame("New.run", 1, 0, 0);
        let path = current.path(&[fresh]);
        current.credit(&path, 1, 1, 1, 1_200);
        let mut original = Vec::new();
        html(&current, &mut original, &options()).unwrap();
        current.diff(&base);
        let mut collapsed_bytes = Vec::new();
        collapsed(&current, &mut collapsed_bytes, &options()).unwrap();
        let collapsed_text = String::from_utf8(collapsed_bytes).unwrap();
        assert!(collapsed_text.contains("leaf 600 597\n"));
        assert!(collapsed_text.contains("New.run_[j] 0 1\n"));
        let mut bytes = Vec::new();
        html(&current, &mut bytes, &options()).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains("d=-3\n"));
        assert!(text.contains("d=0\n"));
        assert!(text.contains("d=U\n"));
        assert!(text.contains("const maxdiff = 3;"));
        assert!(text.contains("function getDiffColor(diff, tree)"));
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();
        let mut reconstructed = Profile::new("cpu", "count", "nanoseconds");
        crate::input::read(
            file.path(),
            &crate::args::Options::default(),
            &mut reconstructed,
        )
        .unwrap();
        let mut reconstructed_html = Vec::new();
        html(&reconstructed, &mut reconstructed_html, &options()).unwrap();
        assert_eq!(original, reconstructed_html);
    }

    #[test]
    fn html_preserves_trusted_title_and_escapes_script_data() {
        let mut p = fixture();
        let f = p.frame("evil</script>\n'\\name", 3, 0, 0);
        let path = p.path(&[f]);
        p.credit(&path, 1, 1, 1, 1_200);
        let mut bytes = Vec::new();
        html(&p, &mut bytes, &options()).unwrap();
        let html = String::from_utf8(bytes).unwrap();
        assert!(html.contains("<h1>Test <profile></h1>"));
        assert!(html.contains("evil\\x3c/script>\\n\\'\\\\name"));
        assert!(!html.contains("evil</script>"));
        assert!(html.contains("f(3,0,0,601)"));
    }

    fn collapsed_profile(text: &str, options: &crate::args::Options) -> Profile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        let mut p = Profile::new("cpu", "count", "nanoseconds");
        crate::input::read(file.path(), options, &mut p).unwrap();
        p
    }

    fn page(profile: &Profile, options: &ExportOptions) -> String {
        let mut bytes = Vec::new();
        html(profile, &mut bytes, options).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn flame_height_keeps_pre_skip_depth_until_mintotal_exceeds_one() {
        let mut o = crate::args::Options {
            skip: 2,
            ..Default::default()
        };
        let p = collapsed_profile("a;b;c 1\n", &o);
        assert_eq!(p.original_depth, 3);
        let text = page(&p, &o.export);
        assert!(text.contains("height: 64px"));
        assert!(text.contains("const levels = Array(4)"));
        let p = collapsed_profile("a;b;c 1\nx;y;z;deep 0\n", &o);
        assert!(page(&p, &o.export).contains("height: 80px"));
        o.export.minwidth = 100.;
        assert!(page(&p, &o.export).contains("height: 80px"));
        o.export.minwidth = 200.;
        assert!(page(&p, &o.export).contains("height: 16px"));
        let o = crate::args::Options {
            skip: 2,
            include: Some(crate::args::pattern("a").unwrap()),
            ..Default::default()
        };
        let p = collapsed_profile("a;b;c 1\nx;y;z;deep 0\n", &o);
        assert_eq!(p.original_depth, 3);
    }

    #[test]
    fn same_name_types_follow_compute_head_and_html_put_tail_order() {
        for (input, kinds) in [
            ("a;byte[]_[i] 1\na;byte[]_[k] 2\n", [5, 2]),
            ("a;byte[]_[k] 2\na;byte[]_[i] 1\n", [2, 5]),
        ] {
            let p = collapsed_profile(input, &crate::args::Options::default());
            let index = Children::new(&p);
            let parent = index.children(0)[0] as usize;
            let actual: Vec<_> = index
                .children(parent)
                .iter()
                .map(|&id| node_kind(&p, &p.nodes[id as usize]))
                .collect();
            assert_eq!(actual, kinds);
            let original = page(&p, &options());
            let mut file = tempfile::NamedTempFile::new().unwrap();
            file.write_all(original.as_bytes()).unwrap();
            let mut imported = Profile::new("cpu", "count", "nanoseconds");
            let import_options = crate::args::Options {
                skip: 10,
                ..Default::default()
            };
            crate::input::read(file.path(), &import_options, &mut imported).unwrap();
            assert!(imported.html_encounter_order);
            assert!(imported.source_title_ids.is_some());
            assert_eq!(page(&imported, &options()), original);
        }
    }

    #[test]
    fn wide_same_name_types_follow_java_integer_hash_buckets() {
        let mut p = Profile::new("cpu", "count", "nanoseconds");
        let root = p.frame("root", 3, 0, 0);
        let cpp = p.frame("Same::frame", 4, 0, 0);
        let kernel = p.frame("Same::frame", 5, 0, 0);
        for frame in [cpp, kernel] {
            let path = p.path(&[root, frame]);
            p.credit(&path, 1, 1, 1, 0);
        }
        let kinds = |p: &Profile| {
            let children = Children::new(p);
            let parent = children.children(0)[0] as usize;
            children
                .children(parent)
                .iter()
                .filter_map(|&id| {
                    let node = &p.nodes[id as usize];
                    (p.strings[p.frames[node.frame as usize].name as usize].as_ref()
                        == "Same::frame")
                        .then(|| node_kind(p, node))
                })
                .collect::<Vec<_>>()
        };
        for i in 0..3998 {
            let frame = p.frame(&format!("f{i}"), 3, 0, 0);
            let path = p.path(&[root, frame]);
            p.credit(&path, 1, 1, 1, 0);
            if i == 3070 {
                // 3073 children: computeIfAbsent still has4096 buckets
                assert_eq!(kinds(&p), [5, 4]);
                p.html_encounter_order = true;
                assert_eq!(kinds(&p), [4, 5]);
                p.html_encounter_order = false;
            }
            if i == 3071 {
                assert_eq!(kinds(&p), [4, 5]);
            } // 3074 =>8192 buckets
        }
        assert_eq!(kinds(&p), [4, 5]);
        let mut titles: Vec<_> = (0..p.strings.len() as u32).collect();
        titles[p.frames[cpp as usize].name as usize] = 4098;
        p.source_title_ids = Some(titles);
        p.html_encounter_order = true;
        assert_eq!(kinds(&p), [5, 4]);
    }

    #[test]
    fn html_preserves_java_constructor_names_and_protects_packed_prefixes() {
        let mut p = Profile::new("cpu", "count", "nanoseconds");
        let prefix = "a".repeat(28);
        for name in [
            "Foo.<init>".to_owned(),
            "Foo.<clinit>".to_owned(),
            format!("{prefix}+"),
            format!("{prefix}/script>"),
        ] {
            let frame = p.frame(&name, 1, 0, 0);
            let path = p.path(&[frame]);
            p.credit(&path, 1, 1, 1, 1);
        }
        let mut out = Vec::new();
        html(&p, &mut out, &options()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Foo.<clinit>"));
        assert!(text.contains("init>"));
        assert!(!text.contains("\\x3cinit>"));
        assert!(text.contains("\\x3c/script>"));
        assert_eq!(text.matches("</script>").count(), 1);
    }

    #[test]
    fn protobuf_dictionaries_deduplicate_names_and_skip_unused_frame_variants() {
        let mut p = Profile::new("cpu", "count", "nanoseconds");
        let a = p.frame("Repeated.method", 0, 10, 0);
        let b = p.frame("Repeated.method", 2, 20, 0);
        for frame in [a, b] {
            let path = p.path(&[frame]);
            p.credit(&path, 1, 1, 1, 1);
        }
        p.frame("unused", 1, 999, 0);
        let mut bytes = Vec::new();
        pprof(&p, &mut bytes, &options()).unwrap();
        let profile = fields(&bytes);
        assert_eq!(profile.iter().filter(|(key, _)| *key == 5).count(), 1);
        assert_eq!(profile.iter().filter(|(key, _)| *key == 4).count(), 2);
        let function = fields(message(&profile, 5));
        let function_id = number(&function, 1);
        for (key, value) in &profile {
            if *key == 4 {
                if let Value::Bytes(bytes) = value {
                    let location = fields(bytes);
                    let line = fields(message(&location, 4));
                    assert_eq!(number(&line, 1), function_id);
                }
            }
        }
        let mut bytes = Vec::new();
        otlp(&p, &mut bytes, &options()).unwrap();
        let top = fields(&bytes);
        let dictionary = fields(message(&top, 2));
        assert_eq!(
            dictionary.iter().filter(|(key, _)| *key == 3).count(),
            p.strings.len()
        );
        assert_eq!(dictionary.iter().filter(|(key, _)| *key == 2).count(), 3);
    }

    // Minimal independent wire decoder to verify table references and values.
    enum Value<'a> {
        Int(u64),
        Bytes(&'a [u8]),
    }
    fn varint(data: &[u8], pos: &mut usize) -> u64 {
        let mut value = 0;
        for shift in (0..70).step_by(7) {
            let b = data[*pos];
            *pos += 1;
            value |= ((b & 127) as u64) << shift;
            if b < 128 {
                return value;
            }
        }
        panic!("invalid varint");
    }
    fn fields(data: &[u8]) -> Vec<(u32, Value<'_>)> {
        let mut pos = 0;
        let mut values = Vec::new();
        while pos < data.len() {
            let key = varint(data, &mut pos);
            let value = match key & 7 {
                0 => Value::Int(varint(data, &mut pos)),
                2 => {
                    let n = varint(data, &mut pos) as usize;
                    let b = &data[pos..pos + n];
                    pos += n;
                    Value::Bytes(b)
                }
                w => panic!("unexpected wire type {w}"),
            };
            values.push(((key >> 3) as u32, value));
        }
        values
    }
    fn message<'a>(fields: &'a [(u32, Value<'a>)], key: u32) -> &'a [u8] {
        fields
            .iter()
            .find_map(|(k, value)| {
                if *k == key {
                    if let Value::Bytes(bytes) = value {
                        Some(*bytes)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .unwrap()
    }
    fn number(fields: &[(u32, Value<'_>)], key: u32) -> u64 {
        fields
            .iter()
            .find_map(|(k, value)| {
                if *k == key {
                    if let Value::Int(n) = value {
                        Some(*n)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .unwrap()
    }

    #[test]
    fn pprof_has_leaf_first_locations_and_thread_label() {
        let p = fixture();
        let mut bytes = Vec::new();
        pprof(&p, &mut bytes, &options()).unwrap();
        let top = fields(&bytes);
        let samples: Vec<_> = top
            .iter()
            .filter_map(|(k, v)| {
                if *k == 2 {
                    if let Value::Bytes(b) = v {
                        Some(fields(b))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(samples.len(), 1);
        let sample = &samples[0];
        assert_eq!(number(sample, 2), 600);
        let locations = message(sample, 1);
        let mut pos = 0;
        let first = varint(locations, &mut pos);
        let second = varint(locations, &mut pos);
        assert_eq!(pos, locations.len());
        assert_eq!(
            &*p.strings[p.frames[first as usize - 1].name as usize],
            "leaf"
        );
        assert_eq!(
            &*p.strings[p.frames[second as usize - 1].name as usize],
            "pkg/Main.run"
        );
        let strings: Vec<_> = top
            .iter()
            .filter_map(|(k, v)| {
                if *k == 6 {
                    if let Value::Bytes(b) = v {
                        Some(std::str::from_utf8(b).unwrap())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        let labels: Vec<_> = sample
            .iter()
            .filter_map(|(k, v)| {
                if *k == 3 {
                    if let Value::Bytes(b) = v {
                        Some(fields(b))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        assert!(labels
            .iter()
            .any(|l| strings[number(l, 1) as usize] == "thread"
                && strings[number(l, 2) as usize] == "[worker tid=9]"));
    }

    #[test]
    fn otlp_contains_dual_values_and_original_first_timestamp() {
        let p = fixture();
        let mut bytes = Vec::new();
        otlp(&p, &mut bytes, &options()).unwrap();
        let top = fields(&bytes);
        let rp = fields(message(&top, 1));
        let sp = fields(message(&rp, 2));
        let profile = fields(message(&sp, 2));
        let sample = fields(message(&profile, 2));
        assert_eq!(number(&sample, 6), 1_100);
        assert_eq!(number(&sample, 2), 2);
        let values = message(&sample, 3);
        let mut pos = 0;
        assert_eq!(varint(values, &mut pos), 3);
        assert_eq!(varint(values, &mut pos), 600);
        assert_eq!(pos, values.len());
        let dictionary = fields(message(&top, 2));
        assert!(dictionary.iter().any(|(key, _)| *key == 1));
    }

    #[test]
    fn spooled_otlp_retains_chunk_groups_and_default_thread_attributes() {
        let mut p = Profile::new("allocations", "count", "bytes");
        p.start_ns = 1_000;
        p.end_ns = 2_000;
        let f = p.frame("Main.run", 1, 7, 0);
        let path = p.path(&[f]);
        p.credit(&path, 7, 7, 1_400, 1_100);
        let thread = p.intern("[worker tid=9]");
        let mut spool = tempfile::tempfile().unwrap();
        for record in [
            [1_000, u64::MAX, 1_500, 0, 0],
            [1_100, path.leaf as u64, 3, 600, thread as u64],
            [1_500, u64::MAX, 2_000, 0, 0],
            [1_600, path.leaf as u64, 4, 800, thread as u64],
        ] {
            for value in record {
                spool.write_all(&value.to_le_bytes()).unwrap();
            }
        }
        let mut bytes = Vec::new();
        otlp_spooled(&p, &mut spool, &mut bytes, &options()).unwrap();
        let top = fields(&bytes);
        let rp = fields(message(&top, 1));
        let scope = fields(message(&rp, 2));
        let profiles: Vec<_> = scope
            .iter()
            .filter_map(|(k, v)| {
                if *k == 2 {
                    if let Value::Bytes(b) = v {
                        Some(fields(b))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(profiles.len(), 2);
        for (i, profile) in profiles.iter().enumerate() {
            assert_eq!(number(profile, 4), 1_000 + i as u64 * 500);
            assert_eq!(number(profile, 5), 500);
            let sample = fields(message(profile, 2));
            assert_eq!(number(&sample, 6), 1_100 + i as u64 * 500);
            assert_eq!(number(&sample, 2), 1);
            assert_eq!(number(&sample, 4), 1);
            let values = message(&sample, 3);
            let mut pos = 0;
            assert_eq!(varint(values, &mut pos), 3 + i as u64);
            assert_eq!(varint(values, &mut pos), 600 + i as u64 * 200);
            assert_eq!(pos, values.len());
        }
        let dictionary = fields(message(&top, 2));
        let attrs: Vec<_> = dictionary
            .iter()
            .filter_map(|(k, v)| {
                if *k == 6 {
                    if let Value::Bytes(b) = v {
                        Some(fields(b))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(message(&attrs[1], 1), b"thread.name");
        assert_eq!(
            message(&fields(message(&attrs[1], 2)), 1),
            b"[worker tid=9]"
        );
        spool.set_len(39).unwrap();
        assert!(otlp_spooled(&p, &mut spool, &mut io::sink(), &options()).is_err());
    }

    #[test]
    fn encoded_otlp_spool_crosses_buffers_with_exact_chunk_lengths_and_large_values() {
        let mut p = fixture();
        let leaf = p.nodes.iter().rposition(|n| n.self_value != 0).unwrap() as u64;
        let thread = p.intern("[worker tid=9]") as u64;
        let mut spool = tempfile::tempfile().unwrap();
        let epoch = 1_700_000_000_000_000_000u64;
        let mut first = 0;
        for (chunk, count) in [9_000, 6_000].into_iter().enumerate() {
            for n in [
                epoch + chunk as u64 * 1_000_000,
                u64::MAX,
                epoch + (chunk as u64 + 1) * 1_000_000,
                0,
                0,
            ] {
                spool.write_all(&n.to_le_bytes()).unwrap();
            }
            for i in first..first + count {
                let value = if i % 3 == 0 { u64::MAX } else { 16_384 };
                for n in [epoch + i, leaf, 128, value, thread] {
                    spool.write_all(&n.to_le_bytes()).unwrap();
                }
            }
            first += count;
        }
        let mut bytes = Vec::new();
        otlp_spooled(&p, &mut spool, &mut bytes, &options()).unwrap();
        assert!(bytes.len() > 256 * 1024);
        let top = fields(&bytes);
        let resource = fields(message(&top, 1));
        let scope = fields(message(&resource, 2));
        let profiles: Vec<_> = scope
            .iter()
            .filter_map(|(key, value)| {
                if *key == 2 {
                    if let Value::Bytes(b) = value {
                        Some(fields(b))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(profiles.len(), 2);
        for (index, expected) in [9_000, 6_000].into_iter().enumerate() {
            let samples: Vec<_> = profiles[index]
                .iter()
                .filter_map(|(key, value)| {
                    if *key == 2 {
                        if let Value::Bytes(b) = value {
                            Some(fields(b))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(samples.len(), expected);
            let first = if index == 0 { 0 } else { 9_000 };
            assert_eq!(number(&samples[0], 6), epoch + first);
            assert_eq!(
                number(samples.last().unwrap(), 6),
                epoch + first + expected as u64 - 1
            );
            let values = message(&samples[0], 3);
            let mut pos = 0;
            assert_eq!(varint(values, &mut pos), 128);
            assert_eq!(varint(values, &mut pos), u64::MAX);
            assert_eq!(pos, values.len());
            assert_eq!(samples[0].iter().filter(|(key, _)| *key == 4).count(), 2);
        }
    }

    #[test]
    fn deep_stacks_export_without_recursion() {
        let mut p = Profile::new("cpu", "count", "nanoseconds");
        let f = p.frame("recursive", 1, 0, 0);
        let frames = vec![f; 25_000];
        let path = p.path(&frames);
        p.credit(&path, 1, 1, 1, 1);
        let mut out = io::sink();
        collapsed(&p, &mut out, &options()).unwrap();
        html(&p, &mut out, &options()).unwrap();
    }
}
