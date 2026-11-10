// SPDX-License-Identifier: Apache-2.0
// Bounded-memory, stable timestamp sorting for native allocation/free events.
use crate::Result;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use tempfile::TempPath;

const RUN_ITEMS: usize = 32768;
// One output plus 63 inputs keeps an individual merge within 64 descriptors.
const FAN_IN: usize = 63;
const RECORD_BYTES: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub time: u64,
    pub address: u64,
    /// Zero represents a free event, as in async-profiler v4.1.
    pub size: u64,
    pub tid: u64,
    pub stack_id: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    record: Record,
    sequence: u64,
}
struct Run {
    path: TempPath,
}
pub struct Sorter {
    buffer: Vec<Entry>,
    runs: Vec<Run>,
    next_sequence: u64,
}
impl Default for Sorter {
    fn default() -> Self {
        Self::new()
    }
}
impl Sorter {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            runs: Vec::new(),
            next_sequence: 0,
        }
    }
    pub fn push(&mut self, record: Record) -> Result<()> {
        let sequence = self.next_sequence;
        self.next_sequence = sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Too many native allocation events",
            )
        })?;
        self.buffer.push(Entry { record, sequence });
        if self.buffer.len() >= RUN_ITEMS {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer
            .sort_unstable_by_key(|entry| (entry.record.time, entry.sequence));
        let mut file = tempfile::NamedTempFile::new()?;
        {
            let mut output = BufWriter::with_capacity(128 * 1024, file.as_file_mut());
            for entry in &self.buffer {
                write_entry(&mut output, *entry)?;
            }
            output.flush()?;
        }
        self.runs.push(Run {
            path: file.into_temp_path(),
        });
        self.buffer.clear();
        Ok(())
    }
    pub fn for_each(mut self, mut visit: impl FnMut(Record) -> Result<()>) -> Result<()> {
        if self.runs.is_empty() {
            self.buffer
                .sort_unstable_by_key(|entry| (entry.record.time, entry.sequence));
            for entry in self.buffer {
                visit(entry.record)?;
            }
            return Ok(());
        }
        self.flush()?;
        // Release the run creation buffer before allocating merge input buffers.
        self.buffer = Vec::new();
        while self.runs.len() > FAN_IN {
            let old = std::mem::take(&mut self.runs);
            let mut input = old.into_iter();
            loop {
                let group: Vec<Run> = input.by_ref().take(FAN_IN).collect();
                if group.is_empty() {
                    break;
                }
                if group.len() == 1 {
                    self.runs.extend(group);
                    continue;
                }
                let mut file = tempfile::NamedTempFile::new()?;
                {
                    let mut output = BufWriter::with_capacity(128 * 1024, file.as_file_mut());
                    merge(&group, |entry| write_entry(&mut output, entry))?;
                    output.flush()?;
                }
                self.runs.push(Run {
                    path: file.into_temp_path(),
                });
                // Dropping group unlinks its input runs after the merge closes them.
            }
        }
        merge(&self.runs, |entry| visit(entry.record))
    }
}
fn write_entry(output: &mut impl Write, entry: Entry) -> Result<()> {
    let r = entry.record;
    let mut bytes = [0; RECORD_BYTES];
    for (i, value) in [r.time, r.address, r.size, r.tid, r.stack_id, entry.sequence]
        .into_iter()
        .enumerate()
    {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    output.write_all(&bytes)?;
    Ok(())
}
fn read_entry(input: &mut impl Read) -> Result<Option<Entry>> {
    let mut bytes = [0; RECORD_BYTES];
    if input.read(&mut bytes[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut bytes[1..])?;
    let get = |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
    Ok(Some(Entry {
        record: Record {
            time: get(0),
            address: get(1),
            size: get(2),
            tid: get(3),
            stack_id: get(4),
        },
        sequence: get(5),
    }))
}
#[derive(PartialEq, Eq)]
struct Head {
    entry: Entry,
    run: usize,
}
impl Ord for Head {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.entry.record.time, self.entry.sequence, self.run).cmp(&(
            other.entry.record.time,
            other.entry.sequence,
            other.run,
        ))
    }
}
impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
fn merge(runs: &[Run], mut visit: impl FnMut(Entry) -> Result<()>) -> Result<()> {
    let mut inputs: Vec<BufReader<File>> = Vec::with_capacity(runs.len());
    let mut heap = BinaryHeap::with_capacity(runs.len());
    for (run, file) in runs.iter().enumerate() {
        let mut reader = BufReader::with_capacity(16 * 1024, File::open(&file.path)?);
        if let Some(entry) = read_entry(&mut reader)? {
            heap.push(Reverse(Head { entry, run }));
        }
        inputs.push(reader);
    }
    while let Some(Reverse(head)) = heap.pop() {
        visit(head.entry)?;
        if let Some(entry) = read_entry(&mut inputs[head.run])? {
            heap.push(Reverse(Head {
                entry,
                run: head.run,
            }));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(time: u64, index: u64) -> Record {
        Record {
            time,
            address: index,
            size: index * 2,
            tid: 3,
            stack_id: 4,
        }
    }
    #[test]
    fn memory_path_orders_timestamps_stably() {
        let mut sorter = Sorter::new();
        for (i, time) in [3, 1, 2, 1, 3].into_iter().enumerate() {
            sorter.push(record(time, i as u64)).unwrap();
        }
        let mut order = Vec::new();
        sorter
            .for_each(|r| {
                order.push(r.address);
                Ok(())
            })
            .unwrap();
        assert_eq!(order, [1, 3, 2, 0, 4]);
        Sorter::new()
            .for_each(|_| panic!("empty sorter yielded a record"))
            .unwrap();
    }
    #[test]
    fn disk_runs_merge_and_preserve_equal_timestamp_order() {
        let count = RUN_ITEMS * 2 + 137;
        let mut sorter = Sorter::new();
        for index in 0..count {
            sorter
                .push(record((index % 97) as u64, index as u64))
                .unwrap();
        }
        assert_eq!(sorter.runs.len(), 2);
        assert_eq!(sorter.buffer.len(), 137);
        let mut prev = None;
        let mut seen = 0;
        sorter
            .for_each(|r| {
                let key = (r.time, r.address);
                if let Some(prev) = prev {
                    assert!(prev < key);
                }
                assert_eq!(r.size, r.address * 2);
                prev = Some(key);
                seen += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, count);
    }
    #[test]
    fn compaction_merges_more_than_descriptor_budget() {
        // Small manually flushed runs exercise multi-pass compaction cheaply.
        let mut sorter = Sorter::new();
        for index in 0..FAN_IN + 9 {
            sorter
                .push(record((index % 7) as u64, index as u64))
                .unwrap();
            sorter.flush().unwrap();
        }
        let mut result = Vec::new();
        sorter
            .for_each(|r| {
                result.push((r.time, r.address));
                Ok(())
            })
            .unwrap();
        let mut expected = result.clone();
        expected.sort_unstable();
        assert_eq!(result, expected);
        assert_eq!(result.len(), FAN_IN + 9);
    }
    #[test]
    fn truncated_spill_record_returns_an_error() {
        assert!(read_entry(&mut &b"bad"[..]).is_err());
    }
}
