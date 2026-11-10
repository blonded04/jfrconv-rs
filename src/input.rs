// Copyright The async-profiler authors
// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
// Collapsed/HTML decoding follows async-profiler v4.1 FlameGraph.java.
use crate::{args::Options, convert::transform_path, model::Profile, Result};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

const MAX_LINE: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 65536;
fn invalid(message: impl Into<String>) -> Box<dyn std::error::Error> {
    io::Error::new(io::ErrorKind::InvalidData, message.into()).into()
}
pub fn read(path: &Path, options: &Options, profile: &mut Profile) -> Result<()> {
    let mut reader = BufReader::with_capacity(128 * 1024, File::open(path)?);
    let extension_html = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm"));
    let prefix = reader.fill_buf()?;
    let prefix = std::str::from_utf8(&prefix[..prefix.len().min(512)])
        .unwrap_or("")
        .trim_start_matches('\u{feff}')
        .trim_start();
    if extension_html
        || prefix.starts_with("<!DOCTYPE")
        || prefix.starts_with("<!doctype")
        || prefix.starts_with("<html")
    {
        html(&mut reader, options, profile)
    } else {
        collapsed(&mut reader, options, profile)
    }
}
fn line(reader: &mut impl BufRead, text: &mut String) -> Result<bool> {
    text.clear();
    let count = Read::take(&mut *reader, MAX_LINE as u64 + 1).read_line(text)?;
    if count > MAX_LINE {
        return Err(invalid("Profile input line exceeds 16 MiB limit"));
    }
    if count == 0 {
        return Ok(false);
    }
    while text.ends_with(['\n', '\r']) {
        text.pop();
    }
    Ok(true)
}
fn thread_role(name: &str, kind: u8) -> u8 {
    if kind != 3 || !name.starts_with('[') || !name.ends_with(']') {
        return 0;
    }
    let inner = &name[1..name.len() - 1];
    let tid = inner
        .strip_prefix("tid=")
        .or_else(|| inner.rsplit_once(" tid=").map(|(_, tid)| tid));
    u8::from(tid.is_some_and(|tid| !tid.is_empty() && tid.bytes().all(|b| b.is_ascii_digit())))
}
fn type_name(name: &str) -> (&str, u8) {
    for (suffix, kind) in [
        ("_[j]", 1),
        ("_[i]", 2),
        ("_[k]", 5),
        ("_[0]", 0),
        ("_[1]", 6),
    ] {
        if let Some(name) = name.strip_suffix(suffix) {
            return (name, kind);
        }
    }
    let kind = if name.contains("::") || name.starts_with("-[") || name.starts_with("+[") {
        4
    } else if name.find('/').is_some_and(|i| i > 0) && !name.starts_with('[')
        || name.find('.').is_some_and(|i| i > 0)
            && name.chars().next().is_some_and(char::is_uppercase)
    {
        1
    } else {
        3
    };
    (name, kind)
}
fn collapsed(reader: &mut impl BufRead, options: &Options, profile: &mut Profile) -> Result<()> {
    let mut text = String::new();
    let mut number = 0;
    while line(reader, &mut text)? {
        number += 1;
        if text.trim().is_empty() {
            continue;
        }
        let (stack, value) = text
            .rsplit_once(' ')
            .ok_or_else(|| invalid(format!("Collapsed line {number} lacks a sample count")))?;
        let amount: u64 = value
            .parse()
            .map_err(|_| invalid(format!("Invalid collapsed sample count on line {number}")))?;
        let mut frames = Vec::new();
        for raw in stack.split(';') {
            if frames.len() >= MAX_DEPTH {
                return Err(invalid("Collapsed stack exceeds 65536 frames"));
            }
            let (name, kind) = type_name(raw);
            frames.push(profile.frame(name, kind, 0, thread_role(name, kind)));
        }
        if let Some(frames) = transform_path(frames, options, profile) {
            let path = profile.path(&frames);
            profile.credit(&path, amount, amount, amount, 0);
        }
    }
    Ok(())
}

struct Level {
    frame: Option<u32>,
    remaining: u64,
}
fn flush(levels: &mut Vec<Level>, options: &Options, profile: &mut Profile) -> Result<()> {
    let remaining = levels
        .last()
        .ok_or_else(|| invalid("Invalid HTML frame nesting"))?
        .remaining;
    if remaining > 0 {
        let frames = levels.iter().filter_map(|level| level.frame).collect();
        let frames = if profile.html_encounter_order {
            Some(frames)
        } else {
            transform_path(frames, options, profile)
        };
        if let Some(frames) = frames {
            let path = profile.path(&frames);
            profile.credit(&path, remaining, remaining, remaining, 0);
        }
    }
    levels.pop();
    Ok(())
}
fn html(reader: &mut impl BufRead, options: &Options, profile: &mut Profile) -> Result<()> {
    let mut text = String::new();
    let mut found_pool = false;
    while line(reader, &mut text)? {
        if text.trim_start().starts_with("const cpool") {
            found_pool = true;
            break;
        }
    }
    if !found_pool {
        return Err(invalid("HTML has no async-profiler constant pool"));
    }
    let mut pool = Vec::new();
    let mut previous = String::new();
    let mut pool_end = false;
    while line(reader, &mut text)? {
        let row = text.trim();
        if row == "];" || row == "]" {
            pool_end = true;
            break;
        }
        if row.is_empty() {
            continue;
        }
        let packed = quoted(row)?;
        if pool.is_empty() {
            pool.push(packed);
            continue;
        }
        let mut chars = packed.chars();
        let prefix = chars
            .next()
            .ok_or_else(|| invalid("Empty packed HTML constant"))? as usize;
        if !(32..=127).contains(&prefix) {
            return Err(invalid("Invalid HTML constant-pool prefix"));
        }
        let prefix = prefix - 32;
        if prefix > previous.len() || !previous.as_bytes()[..prefix].iter().all(u8::is_ascii) {
            return Err(invalid(
                "HTML constant-pool prefix exceeds prior ASCII name",
            ));
        }
        let value = format!("{}{}", &previous[..prefix], chars.as_str());
        previous.clone_from(&value);
        pool.push(value);
    }
    if !pool_end || pool.is_empty() {
        return Err(invalid("Truncated HTML constant pool"));
    }
    let preserve_mixed =
        !options.export.reverse && options.include.is_none() && options.exclude.is_none();
    profile.html_encounter_order = preserve_mixed;
    let mut levels = Vec::new();
    let mut current_level = 0usize;
    let mut total = 0u64;
    let mut saw_frames = false;
    let mut saw_root = false;
    let mut frames_end = false;
    while line(reader, &mut text)? {
        let row = text.trim();
        if row.starts_with("d=") {
            continue;
        }
        let function = row.as_bytes().first().copied().unwrap_or(0);
        let is_frame =
            matches!(function, b'f' | b'u' | b'n') && row.as_bytes().get(1) == Some(&b'(');
        if !is_frame {
            if saw_frames {
                frames_end = true;
                break;
            }
            continue;
        }
        saw_frames = true;
        let numbers = row
            .get(2..)
            .and_then(|row| row.strip_suffix(')'))
            .ok_or_else(|| invalid("Invalid HTML frame row"))?;
        let mut tokens = numbers.split(',');
        let key: u64 = tokens
            .next()
            .ok_or_else(|| invalid("HTML frame has no key"))?
            .parse()?;
        let rest: Vec<&str> = tokens.collect();
        let start = if function == b'f' {
            if rest.len() < 2 {
                return Err(invalid("HTML f row lacks level/offset"));
            }
            current_level = rest[0].parse()?;
            let _: i128 = rest[1].parse()?;
            2
        } else if function == b'u' {
            current_level = current_level
                .checked_add(1)
                .ok_or_else(|| invalid("HTML frame level overflow"))?;
            0
        } else {
            0
        };
        if current_level >= MAX_DEPTH || current_level > levels.len() {
            return Err(invalid("Invalid HTML frame depth"));
        }
        // parseHtml in v4.1 records the source level even if a later rebuild
        // filters/reverses paths. A plain --skip does not request a rebuild.
        profile.original_depth = profile.original_depth.max(current_level);
        if rest.len() > start + 4 {
            return Err(invalid("HTML frame has too many counters"));
        }
        if let Some(value) = rest.get(start) {
            total = value.parse()?;
        }
        let mut kind = (key & 7) as u8;
        if kind > 6 {
            return Err(invalid("Invalid HTML frame type"));
        }
        let extra = &rest[(start + 1).min(rest.len())..];
        if !extra.is_empty() && (kind <= 2 || kind >= 6) {
            kind = 1;
        }
        let mut counters = [0u64; 3];
        for (index, value) in extra.iter().enumerate() {
            counters[index] = value.parse()?;
        }
        if counters
            .iter()
            .try_fold(0u64, |sum, n| sum.checked_add(*n))
            .is_none_or(|sum| sum > total)
        {
            return Err(invalid("HTML Java subtype counters exceed frame total"));
        }
        if !preserve_mixed {
            // v4.1 rebuild() pushes Frame.getType(), not the normalized map
            // key. Mixed C1/interpreted counters can change this effective kind.
            kind = if counters[0] as u128 * 3 >= total as u128 {
                2
            } else if counters[1] as u128 * 2 >= total as u128 {
                6
            } else if counters[2] as u128 * 2 >= total as u128 {
                0
            } else {
                kind
            };
        }
        let name = pool
            .get(usize::try_from(key >> 3)?)
            .ok_or_else(|| invalid("HTML frame constant index out of bounds"))?;
        while levels.len() > current_level {
            flush(&mut levels, options, profile)?;
        }
        if current_level == 0 {
            if saw_root {
                return Err(invalid("HTML contains multiple root frames"));
            }
            saw_root = true;
        } else {
            let parent: &mut Level = levels
                .last_mut()
                .ok_or_else(|| invalid("HTML frame has no parent"))?;
            parent.remaining = parent
                .remaining
                .checked_sub(total)
                .ok_or_else(|| invalid("HTML child totals exceed parent total"))?;
        }
        let frame =
            (current_level != 0).then(|| profile.frame(name, kind, 0, thread_role(name, kind)));
        levels.push(Level {
            frame,
            remaining: total,
        });
        if preserve_mixed && current_level != 0 && !extra.is_empty() {
            let frames: Vec<_> = levels.iter().filter_map(|level| level.frame).collect();
            let target = profile.path(&frames).leaf as usize;
            profile.nodes[target].inlined =
                profile.nodes[target].inlined.saturating_add(counters[0]);
            profile.nodes[target].c1 = profile.nodes[target].c1.saturating_add(counters[1]);
            profile.nodes[target].interpreted = profile.nodes[target]
                .interpreted
                .saturating_add(counters[2]);
        }
    }
    if !saw_frames || !frames_end {
        return Err(invalid("Truncated HTML frame data"));
    }
    while !levels.is_empty() {
        flush(&mut levels, options, profile)?;
    }
    let mut title_ids = vec![u32::MAX; profile.strings.len()];
    title_ids[0] = 0;
    for (id, name) in pool.iter().enumerate().skip(1) {
        if let Some(name_id) = profile.name_id(name) {
            title_ids[name_id as usize] = u32::try_from(id)?;
        }
    }
    profile.source_title_ids = Some(title_ids);
    Ok(())
}

fn quoted(row: &str) -> Result<String> {
    let row = row.strip_suffix(',').unwrap_or(row);
    let raw = row
        .strip_prefix('\'')
        .and_then(|row| row.strip_suffix('\''))
        .ok_or_else(|| invalid("Invalid quoted HTML constant"))?;
    let mut result = String::new();
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }
        let esc = chars
            .next()
            .ok_or_else(|| invalid("Truncated JavaScript string escape"))?;
        match esc {
            '\\' | '\'' | '"' => result.push(esc),
            'n' => result.push('\n'),
            'r' => result.push('\r'),
            't' => result.push('\t'),
            'b' => result.push('\u{8}'),
            'f' => result.push('\u{c}'),
            'v' => result.push('\u{b}'),
            '0' => result.push('\0'),
            'x' | 'u' => {
                let n = if esc == 'x' { 2 } else { 4 };
                let mut value = 0;
                for _ in 0..n {
                    value = value * 16
                        + chars
                            .next()
                            .and_then(|c| c.to_digit(16))
                            .ok_or_else(|| invalid("Invalid JavaScript hexadecimal escape"))?;
                }
                if (0xd800..=0xdbff).contains(&value) {
                    if chars.next() != Some('\\') || chars.next() != Some('u') {
                        return Err(invalid("Unpaired JavaScript high surrogate"));
                    }
                    let mut low = 0;
                    for _ in 0..4 {
                        low = low * 16
                            + chars
                                .next()
                                .and_then(|c| c.to_digit(16))
                                .ok_or_else(|| invalid("Invalid JavaScript low surrogate"))?;
                    }
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(invalid("Invalid JavaScript surrogate pair"));
                    }
                    value = 0x10000 + ((value - 0xd800) << 10) + low - 0xdc00;
                }
                result.push(
                    char::from_u32(value)
                        .ok_or_else(|| invalid("Invalid JavaScript Unicode escape"))?,
                );
            }
            _ => return Err(invalid("Unsupported JavaScript string escape")),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Cursor;
    fn profile() -> Profile {
        Profile::new("cpu", "count", "nanoseconds")
    }
    fn collapsed_map(p: &Profile) -> BTreeMap<String, u64> {
        let mut bytes = Vec::new();
        crate::export::collapsed(p, &mut bytes, &Options::default().export).unwrap();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|row| {
                let (name, value) = row.rsplit_once(' ').unwrap();
                (name.to_owned(), value.parse().unwrap())
            })
            .collect()
    }
    #[test]
    fn imports_v41_compact_html_with_unicode_and_reused_widths() {
        // Captured from the actual v4.1 converter, not produced by this implementation.
        let source = include_str!("../tests/fixtures/wall-v41.html");
        let mut p = profile();
        html(&mut Cursor::new(source), &Options::default(), &mut p).unwrap();
        assert_eq!(p.nodes[0].total, 20);
        assert_eq!(
            collapsed_map(&p),
            BTreeMap::from([
                (
                    "[worker-α tid=1];demo/Main.run_[j];demo/Worker.work_[j]".into(),
                    7
                ),
                (
                    "[worker-α tid=1];second/Main.run_[j];second/Worker.work_[j]".into(),
                    7
                ),
                (
                    "[io-worker tid=2];demo/Main.run_[0];demo/Worker.work_[i]".into(),
                    3
                ),
                (
                    "[io-worker tid=2];second/Main.run_[0];second/Worker.work_[i]".into(),
                    3
                ),
            ])
        );
    }
    #[test]
    fn own_html_roundtrip_preserves_escaping_and_mixed_java_counters() {
        let mut p = profile();
        let java = p.frame("pkg/Frame.α'\\<\n", 1, 0, 0);
        let inline = p.frame("pkg/Frame.α'\\<\n", 2, 0, 0);
        let a = p.path(&[java]);
        let b = p.path(&[inline]);
        p.credit(&a, 5, 5, 5, 0);
        p.credit(&b, 2, 2, 2, 0);
        let mut bytes = Vec::new();
        crate::export::html(&p, &mut bytes, &Options::default().export).unwrap();
        let mut imported = profile();
        html(&mut Cursor::new(bytes), &Options::default(), &mut imported).unwrap();
        assert_eq!(imported.nodes[0].total, 7);
        let leaf = &imported.nodes[1];
        assert_eq!((leaf.total, leaf.inlined), (7, 2));
        assert_eq!(
            &*imported.strings[imported.frames[leaf.frame as usize].name as usize],
            "pkg/Frame.α'\\<\n"
        );
    }
    #[test]
    fn html_rebuild_uses_effective_c1_and_interpreted_kinds_before_reverse_skip() {
        let mut p = profile();
        let root = p.frame("Root.call", 1, 0, 0);
        for (name, kind, weight) in [
            ("C1.method", 6, 1),
            ("C1.method", 1, 1),
            ("Interp.method", 0, 3),
            ("Interp.method", 1, 1),
        ] {
            let frame = p.frame(name, kind, 0, 0);
            let path = p.path(&[root, frame]);
            p.credit(&path, weight, weight, weight, 0);
        }
        let mut bytes = Vec::new();
        crate::export::html(&p, &mut bytes, &Options::default().export).unwrap();
        let mut options = Options {
            skip: 1,
            ..Default::default()
        };
        options.export.reverse = true;
        let mut imported = profile();
        html(&mut Cursor::new(bytes), &options, &mut imported).unwrap();
        assert_eq!(
            collapsed_map(&imported),
            BTreeMap::from([("C1.method_[1]".into(), 2), ("Interp.method_[0]".into(), 4)])
        );
    }
    #[test]
    fn collapsed_types_and_reverse_preserve_thread_grouping() {
        let mut p = profile();
        let mut o = Options::default();
        o.export.reverse = true;
        collapsed(
            &mut Cursor::new("[α tid=12];pkg/Root.run_[0];C::native 4\n"),
            &o,
            &mut p,
        )
        .unwrap();
        assert_eq!(
            collapsed_map(&p),
            BTreeMap::from([("[α tid=12];C::native;pkg/Root.run_[0]".into(), 4)])
        );
        assert_eq!(type_name("java/lang/Foo.run"), ("java/lang/Foo.run", 1));
        assert_eq!(type_name("malloc"), ("malloc", 3));
    }
    #[test]
    fn malformed_html_and_collapsed_fail_without_panics() {
        for source in [
            "",
            "const cpool = [\n'all'\n",
            "const cpool = [\n'all',\n'!x'\n];\n",
            "const cpool = [\n'all'\n];\nf(3,2,0,1)\n\n",
        ] {
            assert!(html(
                &mut Cursor::new(source),
                &Options::default(),
                &mut profile()
            )
            .is_err());
        }
        for source in ["root nope\n", "root -1\n", "root\n"] {
            assert!(collapsed(
                &mut Cursor::new(source),
                &Options::default(),
                &mut profile()
            )
            .is_err());
        }
        assert!(quoted("'bad\\xXX'").is_err());
        assert_eq!(quoted("'\\ud83d\\ude00'").unwrap(), "😀");
    }
}
