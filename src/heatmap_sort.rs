// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
// Bounded-memory sorting and coalescing of (20ms block, stack, count) records.
use crate::Result;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use tempfile::TempPath;

// Weighted records are 16 bytes; this retains the original 1 MiB run buffer bound.
const RUN_KEYS: usize = 65_536;
const FAN_IN: usize = 32;
const LIMIT: u64 = (1 << 30) - 1;
pub struct Sorter {
    keys: Vec<(u64, u64)>,
    runs: Vec<TempPath>,
    total: u64,
}
impl Sorter {
    pub fn new() -> Self {
        Self {
            keys: Vec::new(),
            runs: Vec::new(),
            total: 0,
        }
    }
    #[cfg(test)]
    pub fn push(&mut self, key: u64) -> Result<()> {
        self.push_counted(key, 1)
    }
    pub fn push_counted(&mut self, key: u64, count: u64) -> Result<()> {
        if count == 0 {
            return Err("heatmap sample multiplicity must be positive".into());
        }
        self.total = self
            .total
            .checked_add(count)
            .filter(|total| *total <= LIMIT)
            .ok_or("too many samples for the original heatmap format")?;
        self.keys.push((key, count));
        if self.keys.len() == RUN_KEYS {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if self.keys.is_empty() {
            return Ok(());
        }
        let mut file = tempfile::NamedTempFile::new()?;
        {
            let mut out = BufWriter::with_capacity(64 * 1024, file.as_file_mut());
            write_coalesced(&mut self.keys, &mut out)?;
            out.flush()?;
        }
        self.keys.clear();
        self.runs.push(file.into_temp_path());
        Ok(())
    }
    pub fn finish(mut self) -> Result<File> {
        let mut file = tempfile::tempfile()?;
        if self.runs.is_empty() {
            // Small weighted inputs need neither a named run nor a merge pass.
            let mut out = BufWriter::with_capacity(64 * 1024, &mut file);
            write_coalesced(&mut self.keys, &mut out)?;
            out.flush()?;
        } else {
            self.flush()?;
            self.keys = Vec::new();
            while self.runs.len() > FAN_IN {
                let mut previous = std::mem::take(&mut self.runs).into_iter();
                loop {
                    let group: Vec<_> = previous.by_ref().take(FAN_IN).collect();
                    if group.is_empty() {
                        break;
                    }
                    if group.len() == 1 {
                        self.runs.extend(group);
                        continue;
                    }
                    let mut run = tempfile::NamedTempFile::new()?;
                    {
                        let mut out = BufWriter::with_capacity(64 * 1024, run.as_file_mut());
                        merge(&group, &mut out)?;
                        out.flush()?;
                    }
                    self.runs.push(run.into_temp_path());
                }
            }
            let mut out = BufWriter::with_capacity(64 * 1024, &mut file);
            merge(&self.runs, &mut out)?;
            out.flush()?;
        }
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }
}
fn add_count(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .filter(|sum| *sum <= LIMIT)
        .ok_or_else(|| "heatmap sample count exceeds 30-bit format limit".into())
}
fn write_key_count(out: &mut impl Write, key: u64, count: u64) -> Result<()> {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&key.to_le_bytes());
    bytes[8..].copy_from_slice(&count.to_le_bytes());
    out.write_all(&bytes)?;
    Ok(())
}
fn write_coalesced(keys: &mut [(u64, u64)], out: &mut impl Write) -> Result<()> {
    keys.sort_unstable_by_key(|entry| entry.0);
    let mut previous = None;
    let mut total = 0;
    for &(key, count) in keys.iter() {
        if previous == Some(key) {
            total = add_count(total, count)?;
        } else {
            if let Some(key) = previous {
                write_key_count(out, key, total)?;
            }
            previous = Some(key);
            total = count;
        }
    }
    if let Some(key) = previous {
        write_key_count(out, key, total)?;
    }
    Ok(())
}
pub fn read_key_count(input: &mut impl Read) -> Result<Option<(u64, u64)>> {
    let mut bytes = [0; 16];
    if input.read(&mut bytes[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut bytes[1..])?;
    let key = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let count = u64::from_le_bytes(bytes[8..].try_into().unwrap());
    if count == 0 || count > LIMIT {
        return Err("invalid heatmap sample multiplicity".into());
    }
    Ok(Some((key, count)))
}
fn merge(runs: &[TempPath], out: &mut impl Write) -> Result<()> {
    let mut inputs = Vec::with_capacity(runs.len());
    let mut heap = BinaryHeap::new();
    for (id, path) in runs.iter().enumerate() {
        let mut input = BufReader::with_capacity(16 * 1024, File::open(path)?);
        if let Some((key, count)) = read_key_count(&mut input)? {
            heap.push(Reverse((key, id, count)));
        }
        inputs.push(input);
    }
    let mut previous = None;
    let mut total = 0;
    while let Some(Reverse((key, id, count))) = heap.pop() {
        if previous == Some(key) {
            total = add_count(total, count)?;
        } else {
            if let Some(key) = previous {
                write_key_count(out, key, total)?;
            }
            previous = Some(key);
            total = count;
        }
        if let Some((next, count)) = read_key_count(&mut inputs[id])? {
            heap.push(Reverse((next, id, count)));
        }
    }
    if let Some(key) = previous {
        write_key_count(out, key, total)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn external_runs_sort_and_coalesce_all_keys() {
        let mut sorter = Sorter::new();
        let mut expected = BTreeMap::new();
        for i in (0..RUN_KEYS * 2 + 137).rev() {
            let key = ((i as u64 % 79) << 32) | (i as u64 % 113);
            sorter.push(key).unwrap();
            *expected.entry(key).or_insert(0u64) += 1;
        }
        let mut input = BufReader::new(sorter.finish().unwrap());
        let mut actual = BTreeMap::new();
        while let Some((key, count)) = read_key_count(&mut input).unwrap() {
            assert!(actual.insert(key, count).is_none());
        }
        assert_eq!(actual, expected);
    }
    #[test]
    fn weighted_millions_stay_compact_and_reject_invalid_counts() {
        let mut sorter = Sorter::new();
        sorter.push_counted(12, 1_500_000).unwrap();
        sorter.push_counted(3, 500_000).unwrap();
        sorter.push_counted(12, 50).unwrap();
        let mut file = sorter.finish().unwrap();
        assert_eq!(file.metadata().unwrap().len(), 32);
        assert_eq!(read_key_count(&mut file).unwrap(), Some((3, 500_000)));
        assert_eq!(read_key_count(&mut file).unwrap(), Some((12, 1_500_050)));
        assert_eq!(read_key_count(&mut file).unwrap(), None);
        assert!(Sorter::new().push_counted(1, 0).is_err());
        let mut sorter = Sorter::new();
        sorter.push_counted(1, LIMIT).unwrap();
        assert!(sorter.push_counted(2, 1).is_err());
        assert!(Sorter::new().push_counted(1, u64::MAX).is_err());
        assert!(read_key_count(&mut &[0u8; 16][..]).is_err());
        assert!(read_key_count(&mut &[1u8; 15][..]).is_err());
    }
    #[test]
    fn compaction_coalesces_equal_keys_across_more_than_fan_in_runs() {
        let mut sorter = Sorter::new();
        for i in 0..FAN_IN + 5 {
            sorter.push_counted(7, i as u64 + 1).unwrap();
            sorter.flush().unwrap();
        }
        let mut file = sorter.finish().unwrap();
        assert_eq!(file.metadata().unwrap().len(), 16);
        assert_eq!(
            read_key_count(&mut file).unwrap(),
            Some((7, ((FAN_IN + 5) * (FAN_IN + 6) / 2) as u64))
        );
    }
}
