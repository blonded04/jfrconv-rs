// Copyright The async-profiler authors
// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
// The original 4.1 UI is unchanged. Its decoder accepts the profile prefix trie
// as an LZ dictionary, empty synonym tables, and uniform canonical Huffman codes.
use crate::model::{ExportOptions, Profile};
use crate::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
#[path = "heatmap_sort.rs"]
mod sort;
const TEMPLATE: &str = include_str!("../assets/heatmap.html");
const LIMIT: u64 = (1 << 30) - 1;
struct HtmlOut<'a> {
    out: &'a mut dyn Write,
}
impl HtmlOut<'_> {
    fn byte(&mut self, value: u8) -> io::Result<()> {
        self.out.write_all(&[match value {
            0 => 127,
            13 => 126,
            38 => 125,
            60 => 124,
            62 => 123,
            n => n,
        }])
    }
    fn var(&mut self, mut value: u64) -> io::Result<()> {
        while value >= 61 {
            self.byte(61 + (value % 61) as u8)?;
            value /= 61;
        }
        self.byte(value as u8)
    }
    fn int18(&mut self, mut value: u32) -> io::Result<()> {
        for _ in 0..3 {
            self.byte((value & 63) as u8)?;
            value >>= 6;
        }
        Ok(())
    }
    fn int30(&mut self, mut value: u64) -> io::Result<()> {
        if value > LIMIT {
            return Err(io::Error::other(
                "heatmap value exceeds the 4.1 30-bit format limit",
            ));
        }
        for _ in 0..5 {
            self.byte((value & 63) as u8)?;
            value >>= 6;
        }
        Ok(())
    }
    fn bits27(&mut self, value: u32) -> io::Result<()> {
        self.byte((value / 1_860_867) as u8)?;
        self.byte((value / 15_129 % 123) as u8)?;
        self.byte((value / 123 % 123) as u8)?;
        self.byte((value % 123) as u8)
    }
}
fn until(out: &mut dyn Write, tail: &mut &str, marker: &str) -> io::Result<()> {
    let at = tail
        .find(marker)
        .ok_or_else(|| io::Error::other("invalid upstream heatmap template"))?;
    out.write_all(&tail.as_bytes()[..at])?;
    *tail = &tail[at + marker.len()..];
    Ok(())
}
fn json(out: &mut dyn Write, text: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    for c in text.chars() {
        match c {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            '<' => out.write_all(b"\\u003c")?,
            '\n' => out.write_all(b"\\n")?,
            '\r' => out.write_all(b"\\r")?,
            '\t' => out.write_all(b"\\t")?,
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                write!(out, "\\u{:04x}", c as u32)?
            }
            c => {
                let mut bytes = [0; 4];
                out.write_all(c.encode_utf8(&mut bytes).as_bytes())?;
            }
        }
    }
    out.write_all(b"\"")
}
struct Prepared {
    keys: File,
    counts: File,
    block_values: Vec<u32>,
    blocks: u64,
    samples: u64,
    chunks: u64,
}
struct Tree {
    children: Vec<u8>,
    parts: Vec<u32>,
    starts: BTreeMap<u32, u32>,
}
impl Tree {
    fn new(profile: &Profile) -> Result<Self> {
        let mut children = vec![0u8; profile.nodes.len()];
        for (id, node) in profile.nodes.iter().enumerate().skip(1) {
            if node.parent as usize >= id {
                return Err("invalid heatmap trie parent".into());
            }
            let count = &mut children[node.parent as usize];
            *count = (*count + 1).min(2);
        }
        let mut parts = vec![0u32; profile.nodes.len()];
        let mut starts = BTreeMap::new();
        for (id, node) in profile.nodes.iter().enumerate().skip(1) {
            let parent = node.parent as usize;
            parts[id] = parts[parent] + u32::from(parent == 0 || children[parent] > 1);
            if parent == 0 {
                starts.insert(node.frame, 0);
            }
        }
        for (i, method) in starts.values_mut().enumerate() {
            *method = profile.frames.len() as u32 + i as u32;
        }
        Ok(Self {
            children,
            parts,
            starts,
        })
    }
    fn dictionary_parent(&self, profile: &Profile, id: usize) -> u32 {
        let parent = profile.nodes[id].parent;
        if self.children[parent as usize] > 1 {
            0
        } else {
            parent
        }
    }
    fn sample_parts(&self, profile: &Profile, mut leaf: u32, buffer: &mut Vec<u32>) {
        buffer.clear();
        buffer.push(leaf);
        loop {
            let parent = profile.nodes[leaf as usize].parent;
            if parent == 0 {
                break;
            }
            if self.children[parent as usize] > 1 {
                buffer.push(parent);
            }
            leaf = parent;
        }
        buffer.reverse();
    }
}
fn prepare(profile: &Profile, tree: &Tree, spool: &mut File) -> Result<Prepared> {
    let bytes = spool.metadata()?.len();
    if bytes % 32 != 0 {
        return Err("truncated heatmap sample spool".into());
    }
    let records = bytes / 32;
    if records > LIMIT {
        return Err("too many samples for the original heatmap format".into());
    }
    spool.seek(SeekFrom::Start(0))?;
    let mut input = BufReader::with_capacity(64 * 1024, spool);
    let mut sorter = sort::Sorter::new();
    let mut first_ms = None;
    let mut record = [0; 32];
    let mut chunks = 0u64;
    let mut samples = 0u64;
    for _ in 0..records {
        input.read_exact(&mut record)?;
        let ms = u64::from_le_bytes(record[..8].try_into().unwrap()) / 1_000_000;
        let node = u64::from_le_bytes(record[8..16].try_into().unwrap());
        if node == 0 || node >= profile.nodes.len() as u64 {
            return Err("invalid heatmap stack reference".into());
        }
        let count = u64::from_le_bytes(record[16..24].try_into().unwrap());
        if count == 0 {
            return Err("heatmap sample multiplicity must be positive".into());
        }
        samples = samples
            .checked_add(count)
            .filter(|total| *total <= LIMIT)
            .ok_or("too many samples for the original heatmap format")?;
        let references = (tree.parts[node as usize] as u64)
            .checked_mul(count)
            .ok_or("heatmap dictionary reference count overflow")?;
        chunks = chunks
            .checked_add(references)
            .filter(|total| *total <= LIMIT)
            .ok_or("too many dictionary references for the original heatmap format")?;
        let first = *first_ms.get_or_insert(ms);
        let block = i32::try_from((ms as i128 - first as i128) / 20)
            .map_err(|_| "heatmap time range exceeds the 4.1 format limit")?;
        sorter.push_counted(((block as u32 ^ 0x8000_0000) as u64) << 32 | node, count)?;
    }
    let mut keys = sorter.finish()?;
    let mut counts = tempfile::tempfile()?;
    let mut distinct = BTreeSet::new();
    let mut blocks = 0;
    {
        let mut input = BufReader::with_capacity(64 * 1024, &mut keys);
        let mut output = BufWriter::with_capacity(64 * 1024, &mut counts);
        let mut previous = None;
        let mut count = 0u32;
        while let Some((key, multiplicity)) = sort::read_key_count(&mut input)? {
            let block = key >> 32;
            if let Some(prev) = previous {
                if block != prev {
                    output.write_all(&count.to_le_bytes())?;
                    distinct.insert(count);
                    blocks += 1;
                    let gap = block - prev - 1;
                    if blocks + gap > LIMIT {
                        return Err("too many time blocks for the original heatmap format".into());
                    }
                    if gap != 0 {
                        distinct.insert(0);
                    }
                    for _ in 0..gap {
                        output.write_all(&0u32.to_le_bytes())?;
                    }
                    blocks += gap;
                    count = 0;
                }
            }
            previous = Some(block);
            count = count
                .checked_add(u32::try_from(multiplicity)?)
                .filter(|total| *total as u64 <= LIMIT)
                .ok_or("heatmap block sample count exceeds 30-bit format limit")?;
        }
        if previous.is_some() {
            output.write_all(&count.to_le_bytes())?;
            distinct.insert(count);
            blocks += 1;
        }
        output.flush()?;
    }
    keys.seek(SeekFrom::Start(0))?;
    counts.seek(SeekFrom::Start(0))?;
    Ok(Prepared {
        keys,
        counts,
        block_values: distinct.into_iter().collect(),
        blocks,
        samples,
        chunks,
    })
}
fn write_blocks(out: &mut HtmlOut<'_>, data: &mut Prepared) -> Result<()> {
    let size = data.block_values.len();
    let bits = if size <= 1 {
        0
    } else {
        usize::BITS - (size - 1).leading_zeros()
    };
    out.var(size as u64)?;
    out.var(bits as u64)?;
    for &value in &data.block_values {
        out.var(value as u64)?;
        out.var(bits as u64)?;
    }
    if bits == 0 {
        return Ok(());
    }
    data.counts.seek(SeekFrom::Start(0))?;
    let mut input = BufReader::with_capacity(64 * 1024, &mut data.counts);
    let (mut packed, mut used) = (0u32, 0);
    for _ in 0..data.blocks {
        let mut bytes = [0; 4];
        input.read_exact(&mut bytes)?;
        let value = u32::from_le_bytes(bytes);
        let code = data.block_values.binary_search(&value).unwrap() as u32;
        for shift in (0..bits).rev() {
            packed |= ((code >> shift) & 1) << used;
            used += 1;
            if used == 27 {
                out.bits27(packed)?;
                packed = 0;
                used = 0;
            }
        }
    }
    if used != 0 {
        out.bits27(packed)?;
    }
    Ok(())
}
pub fn heatmap(
    profile: &Profile,
    spool: &mut File,
    out: &mut dyn Write,
    opts: &ExportOptions,
) -> Result<()> {
    let tree = Tree::new(profile)?;
    let mut data = prepare(profile, &tree, spool)?;
    if data.samples == 0 {
        out.write_all(b"No samples found\n")?;
        return Ok(());
    }
    if profile.nodes.len() as u64 > LIMIT {
        return Err("too many stack nodes for the original heatmap format".into());
    }
    let storage = profile.nodes.len().saturating_sub(1) as u64;
    let mut tail = TEMPLATE;
    until(out, &mut tail, "/*executionsHeatmap:*/")?;
    out.write_all(b"S")?;
    {
        let mut encoded = HtmlOut { out };
        encoded.var(tree.starts.len() as u64)?;
        for &method in tree.starts.values() {
            encoded.var(method as u64)?;
        }
        write_blocks(&mut encoded, &mut data)?;
        encoded.var(0)?;
        for (id, node) in profile.nodes.iter().enumerate().skip(1) {
            encoded.var(tree.dictionary_parent(profile, id) as u64)?;
            let method = if node.parent == 0 {
                tree.starts[&node.frame]
            } else {
                node.frame
            };
            encoded.var(method as u64)?;
        }
        encoded.var(0)?;
        let mut input = BufReader::with_capacity(64 * 1024, &mut data.keys);
        let mut previous = 0;
        let mut parts = Vec::new();
        while let Some((key, multiplicity)) = sort::read_key_count(&mut input)? {
            let leaf = key as u32;
            if previous != leaf {
                tree.sample_parts(profile, leaf, &mut parts);
                previous = leaf;
            }
            for _ in 0..multiplicity {
                for &part in &parts {
                    encoded.var(part as u64)?;
                }
            }
        }
        encoded.int30(profile.nodes.len() as u64)?;
        encoded.int30(data.blocks)?;
        encoded.int30(storage)?;
        encoded.int30(data.chunks)?;
        encoded.int30(data.samples)?;
    }
    out.write_all(b"E")?;
    until(out, &mut tail, "/*methods:*/")?;
    out.write_all(b"SA")?;
    {
        let mut encoded = HtmlOut { out };
        encoded.var((profile.frames.len().saturating_sub(1) + tree.starts.len()) as u64)?;
        let normal = profile.frames.iter().skip(1);
        let starts = tree.starts.keys().map(|&id| &profile.frames[id as usize]);
        for frame in normal.chain(starts) {
            if frame.name == 0 {
                encoded.var(profile.strings.len() as u64)?;
                encoded.var(0)?;
            } else {
                encoded.var(0)?;
                encoded.var(frame.name as u64)?;
            }
            encoded.int18(frame.bci as u32 & 0xffff)?;
            encoded.int18(frame.line as u32 & 0xffff)?;
            encoded.byte(frame.kind)?;
        }
    }
    out.write_all(b"AE")?;
    until(out, &mut tail, "/*title:*/")?;
    out.write_all(opts.title.as_bytes())?;
    until(out, &mut tail, "/*startMs:*/0")?;
    write!(out, "{}", profile.start_ns / 1_000_000)?;
    until(out, &mut tail, "/*cpool:*/")?;
    for text in &profile.strings {
        json(out, text)?;
        out.write_all(b",")?;
    }
    json(out, "all")?;
    out.write_all(b",")?;
    out.write_all(tail.as_bytes())?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upstream_decoder_reads_sorted_blocks_and_shared_prefixes() {
        use std::process::{Command, Stdio};
        let mut p = Profile::new("cpu", "count", "nanoseconds");
        p.start_ns = 1_000_000_000;
        p.end_ns = 2_000_000_000;
        let a = p.frame_location("Main.run", 1, 10, 1, 0);
        let b = p.frame_location("native", 3, 0, 0, 0);
        let c = p.frame_location("Child.work", 2, 20, 2, 0);
        let ab = p.path_exact(&[a, b]);
        let ac = p.path_exact(&[a, c]);
        let aba = p.path_exact(&[a, b, a]);
        p.path_exact(&[a, b, c]); // branch forces A to begin a continuation piece
        let only_a = p.path_exact(&[a]);
        let mut spool = tempfile::tempfile().unwrap();
        for (time, leaf) in [
            (1_250_000_000, ab.leaf),
            (1_290_000_000, ac.leaf),
            (1_300_000_000, aba.leaf),
            (1_230_000_000, only_a.leaf),
        ] {
            for value in [time, leaf as u64, 1, 1] {
                spool.write_all(&value.to_le_bytes()).unwrap();
            }
        }
        let opts = ExportOptions {
            title: "Original UI".into(),
            minwidth: 0.,
            reverse: false,
            inverted: false,
            highlight: None,
        };
        let mut html = Vec::new();
        heatmap(&p, &mut spool, &mut html, &opts).unwrap();
        let decoder = &TEMPLATE[TEMPLATE.find("class DataBuffer {").unwrap()
            ..TEMPLATE.find("class HeatmapCollection extends").unwrap()];
        let minmax = &TEMPLATE[TEMPLATE.find("function calculateMinMax(").unwrap()
            ..TEMPLATE.find("function calculateInitialZoom(").unwrap()];
        let colors = &TEMPLATE[TEMPLATE.find("const palette =").unwrap()
            ..TEMPLATE.find("function samples(").unwrap()];
        let methods = &TEMPLATE
            [TEMPLATE.find("function methodName(").unwrap()..TEMPLATE.find("let sq = 5;").unwrap()];
        let mut script=String::from("const assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm');const zoomToGroupSize=[1,50,250];\n");
        script.push_str(decoder);
        script.push_str(minmax);
        script.push_str(colors);
        script.push_str(methods);
        script.push_str(r#"
const html=fs.readFileSync(0,'utf8');
function data(id){const at=html.indexOf('<pre id="'+id+'">');assert(at>=0);const from=html.indexOf('>',at)+1;return html.slice(from,html.indexOf('</pre>',from))}
const decoded=decodeExecutions(new DataBuffer(data('executionsHeatmap')));
assert.deepEqual(Array.from(decoded.counts),[1,2,2,4]);
assert.equal(decoded.sampleToChunkEnds.length,4);
assert.deepEqual(Array.from(decoded.sampleToChunkEnds),[1,3,5,8]);
assert.equal(decoded.storage.length,5);
const pool=html.match(/let cpool = \[([\s\S]*?)\];/);assert(pool);const cpool=vm.runInNewContext('['+pool[1]+']');
const names=readMethods(new DataBuffer(data('methods'))).n;
function stack(sample){const result=[];const from=sample?decoded.sampleToChunkEnds[sample-1]:0;for(let chunk=from;chunk<decoded.sampleToChunkEnds[sample];chunk++){const pos=decoded.chunksToStorage[chunk];const size=decoded.storageSizes[pos];result.push(...Array.from(decoded.storage.slice(pos,pos+size)).reverse().map(id=>names[id]))}return result;}
assert.deepEqual(stack(0),['Main.run:10']);
assert.deepEqual(stack(1),['Main.run:10','native']);
assert.deepEqual(stack(2),['Main.run:10','Child.work:20']);
assert.deepEqual(stack(3),['Main.run:10','native','Main.run:10']);
"#);
        let mut node = match Command::new("node")
            .args(["-e", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => panic!("Cannot run upstream decoder: {e}"),
        };
        node.stdin.take().unwrap().write_all(&html).unwrap();
        let result = node.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        spool.set_len(31).unwrap();
        assert!(heatmap(&p, &mut spool, &mut io::sink(), &opts).is_err());
    }
    #[test]
    fn unchanged_upstream_template_is_used() {
        assert!(TEMPLATE.contains("function decodeExecutions(data)"));
        assert!(TEMPLATE.contains("prepareFramesDiff"));
        assert!(TEMPLATE.contains("class HeatmapCollection extends DataBuffer"));
        assert!(!TEMPLATE.contains("jfrconv-rs"));
    }
    #[test]
    fn weighted_millions_prepare_compactly_using_physical_counts() {
        let mut p = Profile::new("cpu", "count", "nanoseconds");
        let frame = p.frame("Main.run", 1, 0, 0);
        let leaf = p.path_exact(&[frame]).leaf;
        let tree = Tree::new(&p).unwrap();
        let mut spool = tempfile::tempfile().unwrap();
        // Three compact groups represent four million physical records. The final field is
        // sample weight/value, which must not change the native heatmap count.
        for record in [
            [1_000_000_000, leaf as u64, 1, 7],
            [1_000_000_000, leaf as u64, 1_999_999, 13_999_993],
            [1_040_000_000, leaf as u64, 2_000_000, 14_000_000],
        ] {
            for value in record {
                spool.write_all(&value.to_le_bytes()).unwrap();
            }
        }
        assert_eq!(spool.metadata().unwrap().len(), 96);
        let mut data = prepare(&p, &tree, &mut spool).unwrap();
        assert_eq!(
            (data.samples, data.chunks, data.blocks),
            (4_000_000, 4_000_000, 3)
        );
        assert_eq!(data.keys.metadata().unwrap().len(), 32);
        assert_eq!(data.block_values, [0, 2_000_000]);
        let mut bytes = [0; 12];
        data.counts.read_exact(&mut bytes).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            2_000_000
        );
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(bytes[8..].try_into().unwrap()),
            2_000_000
        );
    }
    #[test]
    fn multiplicities_reject_zero_overflow_and_30_bit_excess() {
        let mut p = Profile::new("cpu", "count", "nanoseconds");
        let frame = p.frame("Main.run", 1, 0, 0);
        let leaf = p.path_exact(&[frame]).leaf;
        let tree = Tree::new(&p).unwrap();
        for counts in [vec![0], vec![u64::MAX], vec![LIMIT + 1], vec![LIMIT, 1]] {
            let mut spool = tempfile::tempfile().unwrap();
            for count in counts {
                for value in [1_000_000_000, leaf as u64, count, 1] {
                    spool.write_all(&value.to_le_bytes()).unwrap();
                }
            }
            assert!(prepare(&p, &tree, &mut spool).is_err());
        }
    }
}
