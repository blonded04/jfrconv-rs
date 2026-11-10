// Copyright The async-profiler authors
// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
// JFR format handling derived from async-profiler v4.1 one.jfr.JfrReader.

use crate::Result;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const HEADER: u64 = 68;
const MAX_STRING: usize = 16 * 1024 * 1024;
const MAX_ITEMS: usize = 16 * 1024 * 1024;
fn invalid(message: impl Into<String>) -> Box<dyn std::error::Error> {
    io::Error::new(io::ErrorKind::InvalidData, message.into()).into()
}

#[derive(Clone, Debug)]
pub struct ClassRef {
    pub name: u64,
}
#[derive(Clone, Debug)]
pub struct MethodRef {
    pub class: u64,
    pub name: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct StackFrame {
    pub method: u64,
    pub kind: u8,
    pub line: i32,
    pub bci: i32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Execution,
    Allocation,
    Live,
    Lock,
    Malloc,
    Free,
    Span,
}
#[derive(Clone, Debug)]
pub struct Event {
    pub kind: EventKind,
    pub time: u64,
    pub tid: u64,
    pub stack_id: u64,
    pub method_id: u64,
    pub class_id: u64,
    pub state: u64,
    pub samples: u64,
    pub value: u64,
    pub address: u64,
    pub duration: u64,
    pub tlab: bool,
    pub tag: Option<String>,
    /// Explicit source identity, independent of the recording's thread-state enum.
    pub wall: bool,
}
impl Event {
    fn new(kind: EventKind) -> Self {
        Self {
            kind,
            time: 0,
            tid: 0,
            stack_id: 0,
            method_id: 0,
            class_id: 0,
            state: 0,
            samples: 1,
            value: 1,
            address: 0,
            duration: 0,
            tlab: false,
            tag: None,
            wall: false,
        }
    }
}
/// Restrict decoding to relevant event families. Skipped events still have their
/// record boundaries validated; their payloads are never materialized.
#[derive(Clone, Copy, Debug)]
pub struct EventMask(u8);
impl EventMask {
    #[allow(dead_code)]
    pub const ALL: Self = Self(127);
    pub const EXECUTION: Self = Self(1);
    pub const ALLOCATION: Self = Self(2);
    pub const LIVE: Self = Self(4);
    pub const LOCK: Self = Self(8);
    pub const NATIVE: Self = Self(16 | 32);
    pub const SPAN: Self = Self(64);
    #[inline(always)]
    fn includes(self, kind: EventKind) -> bool {
        self.0 & (1 << kind as u8) != 0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Unknown,
    Execution,
    Wall,
    Allocation,
    Tlab,
    Live,
    Lock,
    Park,
    Malloc,
    Free,
    Span,
}
impl Encoding {
    fn from_name(name: &str) -> Self {
        match name {
            "jdk.ExecutionSample" | "jdk.NativeMethodSample" => Self::Execution,
            "profiler.WallClockSample" => Self::Wall,
            "jdk.ObjectAllocationInNewTLAB" => Self::Tlab,
            "jdk.ObjectAllocationOutsideTLAB" | "jdk.ObjectAllocationSample" => Self::Allocation,
            "profiler.LiveObject" => Self::Live,
            "jdk.JavaMonitorEnter" => Self::Lock,
            "jdk.ThreadPark" => Self::Park,
            "profiler.Malloc" => Self::Malloc,
            "profiler.Free" => Self::Free,
            "profiler.Span" | "profiler.MethodTrace" | "jdk.MethodTrace" => Self::Span,
            _ => Self::Unknown,
        }
    }
    #[inline(always)]
    fn kind(self) -> Option<EventKind> {
        match self {
            Self::Execution | Self::Wall => Some(EventKind::Execution),
            Self::Allocation | Self::Tlab => Some(EventKind::Allocation),
            Self::Live => Some(EventKind::Live),
            Self::Lock | Self::Park => Some(EventKind::Lock),
            Self::Malloc => Some(EventKind::Malloc),
            Self::Free => Some(EventKind::Free),
            Self::Span => Some(EventKind::Span),
            Self::Unknown => None,
        }
    }
}
#[derive(Clone, Debug)]
struct Field {
    name: String,
    type_id: u64,
    constant_pool: bool,
    dimension: usize,
}
#[derive(Clone, Debug)]
struct Type {
    name: String,
    simple: bool,
    fields: Vec<Field>,
}
#[derive(Debug)]
pub struct Chunk {
    pub start_ns: u64,
    pub end_ns: u64,
    pub start_ticks: u64,
    pub ticks_per_sec: u64,
    pub threads: HashMap<u64, String>,
    pub classes: HashMap<u64, ClassRef>,
    pub symbols: HashMap<u64, String>,
    pub methods: HashMap<u64, MethodRef>,
    pub stacks: HashMap<u64, Vec<StackFrame>>,
    pub enums: HashMap<String, HashMap<u64, String>>,
    types: HashMap<u64, Type>,
    event_encodings: Vec<Encoding>,
    strings: HashMap<u64, String>,
    offset: u64,
    end: u64,
}

const BUFFER_SIZE: usize = 128 * 1024;
struct Input {
    file: File,
    buffer: Box<[u8]>,
    base: u64,
    cursor: usize,
    filled: usize,
    limit: u64,
}
impl Input {
    fn new(file: File, limit: u64) -> Self {
        Self {
            file,
            buffer: vec![0; BUFFER_SIZE].into_boxed_slice(),
            base: 0,
            cursor: 0,
            filled: 0,
            limit,
        }
    }
    #[inline(always)]
    fn position(&self) -> u64 {
        self.base + self.cursor as u64
    }
    fn seek(&mut self, pos: u64) -> Result<()> {
        if pos > self.limit {
            return Err(invalid("JFR seek exceeds record boundary"));
        }
        if pos >= self.base && pos <= self.base + self.filled as u64 {
            self.cursor = (pos - self.base) as usize;
        } else {
            self.file.seek(SeekFrom::Start(pos))?;
            self.base = pos;
            self.cursor = 0;
            self.filled = 0;
        }
        Ok(())
    }
    #[cold]
    #[inline(never)]
    fn refill(&mut self) -> Result<()> {
        let remaining = self.filled - self.cursor;
        if remaining > 0 {
            self.buffer.copy_within(self.cursor..self.filled, 0);
        }
        self.base += self.cursor as u64;
        self.cursor = 0;
        self.filled = remaining;
        loop {
            match self.file.read(&mut self.buffer[remaining..]) {
                Ok(0) => return Err(invalid("Truncated JFR input")),
                Ok(n) => {
                    self.filled += n;
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    fn take(&mut self, bytes: &mut [u8]) -> Result<()> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.position()) {
            return Err(invalid(format!(
                "Truncated JFR record at byte {}",
                self.position()
            )));
        }
        let mut written = 0;
        while written < bytes.len() {
            if self.cursor == self.filled {
                self.refill()?;
            }
            let count = (self.filled - self.cursor).min(bytes.len() - written);
            bytes[written..written + count]
                .copy_from_slice(&self.buffer[self.cursor..self.cursor + count]);
            self.cursor += count;
            written += count;
        }
        Ok(())
    }
    #[inline(always)]
    fn byte(&mut self) -> Result<u8> {
        if self.position() >= self.limit {
            return Err(invalid("Truncated JFR record"));
        }
        if self.cursor == self.filled {
            self.refill()?;
        }
        let value = self.buffer[self.cursor];
        self.cursor += 1;
        Ok(value)
    }
    // Decode from a fixed nine-byte lookahead. No unchecked indexing or pointers.
    #[inline(always)]
    fn var9(bytes: &[u8; 9]) -> (u64, usize) {
        let b = bytes[0];
        if b < 128 {
            return (b as u64, 1);
        }
        let mut n = (b & 127) as u64;
        let b = bytes[1];
        n |= ((b & 127) as u64) << 7;
        if b < 128 {
            return (n, 2);
        }
        let b = bytes[2];
        n |= ((b & 127) as u64) << 14;
        if b < 128 {
            return (n, 3);
        }
        let b = bytes[3];
        n |= ((b & 127) as u64) << 21;
        if b < 128 {
            return (n, 4);
        }
        let b = bytes[4];
        n |= ((b & 127) as u64) << 28;
        if b < 128 {
            return (n, 5);
        }
        let b = bytes[5];
        n |= ((b & 127) as u64) << 35;
        if b < 128 {
            return (n, 6);
        }
        let b = bytes[6];
        n |= ((b & 127) as u64) << 42;
        if b < 128 {
            return (n, 7);
        }
        let b = bytes[7];
        n |= ((b & 127) as u64) << 49;
        if b < 128 {
            return (n, 8);
        }
        (n | ((bytes[8] as u64) << 56), 9)
    }
    #[inline(always)]
    fn buffered_var(bytes: &[u8], cursor: &mut usize) -> u64 {
        let first = bytes[*cursor];
        if first < 128 {
            *cursor += 1;
            return first as u64;
        }
        let (value, length) = Self::var9(bytes[*cursor..*cursor + 9].try_into().unwrap());
        *cursor += length;
        value
    }
    #[inline(always)]
    fn var(&mut self) -> Result<u64> {
        if self.cursor < self.filled && self.position() < self.limit {
            let first = self.buffer[self.cursor];
            if first < 128 {
                self.cursor += 1;
                return Ok(first as u64);
            }
            if self.filled - self.cursor >= 9 {
                let (value, length) = Self::var9(
                    self.buffer[self.cursor..self.cursor + 9]
                        .try_into()
                        .unwrap(),
                );
                if length as u64 > self.limit - self.position() {
                    return Err(invalid("Truncated JFR varint"));
                }
                self.cursor += length;
                return Ok(value);
            }
        }
        self.var_slow()
    }
    #[cold]
    #[inline(never)]
    fn var_slow(&mut self) -> Result<u64> {
        let mut value = 0;
        for shift in (0..56).step_by(7) {
            let b = self.byte()?;
            value |= ((b & 127) as u64) << shift;
            if b < 128 {
                return Ok(value);
            }
        }
        Ok(value | ((self.byte()? as u64) << 56))
    }
    fn count(&mut self) -> Result<usize> {
        let n = self.var()?;
        if n > MAX_ITEMS as u64 || n > self.limit.saturating_sub(self.position()) {
            return Err(invalid("Invalid or excessive JFR element count"));
        }
        Ok(n as usize)
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let len = self.count()?;
        if len > MAX_STRING {
            return Err(invalid("JFR string exceeds 16 MiB limit"));
        }
        let mut bytes = vec![0; len];
        self.take(&mut bytes)?;
        Ok(bytes)
    }
    fn string(&mut self, strings: &HashMap<u64, String>) -> Result<Option<String>> {
        Ok(match self.byte()? {
            0 => None,
            1 => Some(String::new()),
            2 => {
                let id = self.var()?;
                if id == 0 {
                    None
                } else {
                    Some(
                        strings
                            .get(&id)
                            .ok_or_else(|| invalid(format!("Missing JFR string constant {id}")))?
                            .clone(),
                    )
                }
            }
            3 => Some(String::from_utf8_lossy(&self.bytes()?).into_owned()),
            4 => {
                let len = self.count()?;
                if len > MAX_STRING / 2 {
                    return Err(invalid("JFR UTF-16 string too long"));
                }
                let mut chars = Vec::with_capacity(len);
                for _ in 0..len {
                    let c = self.var()?;
                    if c > 0xffff {
                        return Err(invalid("Invalid UTF-16 code unit"));
                    }
                    chars.push(c as u16);
                }
                Some(String::from_utf16_lossy(&chars))
            }
            5 => Some(self.bytes()?.into_iter().map(char::from).collect()),
            n => return Err(invalid(format!("Invalid JFR string encoding {n}"))),
        })
    }
    #[inline(always)]
    fn record(&mut self, offset: u64, chunk_end: u64) -> Result<(u64, u64)> {
        self.limit = chunk_end;
        if self.position() != offset {
            self.seek(offset)?;
        }
        let size = self.var()?;
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= chunk_end)
            .ok_or_else(|| invalid("JFR event extends past chunk"))?;
        if size < 2 || end <= self.position() {
            return Err(invalid("Invalid JFR event size"));
        }
        self.limit = end;
        let type_id = self.var()?;
        Ok((type_id, end))
    }
}

pub struct JfrReader {
    input: Input,
    file_size: u64,
    next_offset: u64,
    pub start_ns: u64,
    pub end_ns: u64,
}
struct Header {
    size: u64,
    cp: u64,
    metadata: u64,
    start: u64,
    end: u64,
    ticks: u64,
    frequency: u64,
}
impl JfrReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let file_size = file.metadata()?.len();
        let mut reader = Self {
            input: Input::new(file, file_size),
            file_size,
            next_offset: 0,
            start_ns: u64::MAX,
            end_ns: 0,
        };
        if file_size < HEADER {
            return Err(invalid("Incomplete JFR file"));
        }
        // A header-only pass determines absolute recording bounds without retaining chunks.
        let mut offset = 0;
        while offset < file_size {
            let h = reader.header(offset)?;
            reader.start_ns = reader.start_ns.min(h.start);
            reader.end_ns = reader.end_ns.max(h.end);
            offset += h.size;
        }
        Ok(reader)
    }
    fn header(&mut self, offset: u64) -> Result<Header> {
        self.input.limit = self.file_size;
        self.input.seek(offset)?;
        let mut bytes = [0; HEADER as usize];
        self.input.take(&mut bytes)?;
        if &bytes[..4] != b"FLR\0" {
            return Err(invalid(format!(
                "Invalid JFR chunk signature at byte {offset}"
            )));
        }
        let version = u32::from_be_bytes(bytes[4..8].try_into()?);
        if !(0x20000..=0x2ffff).contains(&version) {
            return Err(invalid(format!(
                "Unsupported JFR version {}.{}",
                version >> 16,
                version & 0xffff
            )));
        }
        let get = |i| u64::from_be_bytes(bytes[i..i + 8].try_into().unwrap());
        let size = get(8);
        if size < HEADER
            || offset
                .checked_add(size)
                .filter(|end| *end <= self.file_size)
                .is_none()
        {
            return Err(invalid("Incomplete or invalid JFR chunk"));
        }
        let cp = get(16);
        let metadata = get(24);
        if cp < HEADER || cp >= size || metadata < HEADER || metadata >= size {
            return Err(invalid(
                "Incomplete JFR chunk: invalid checkpoint/metadata offset",
            ));
        }
        let start = get(32);
        let end = start
            .checked_add(get(40))
            .ok_or_else(|| invalid("JFR timestamp overflow"))?;
        let frequency = get(56);
        if frequency == 0 {
            return Err(invalid("JFR tick frequency is zero"));
        }
        Ok(Header {
            size,
            cp,
            metadata,
            start,
            end,
            ticks: get(48),
            frequency,
        })
    }
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        if self.next_offset == self.file_size {
            return Ok(None);
        }
        let offset = self.next_offset;
        let h = self.header(offset)?;
        let mut chunk = Chunk {
            start_ns: h.start,
            end_ns: h.end,
            start_ticks: h.ticks,
            ticks_per_sec: h.frequency,
            threads: HashMap::new(),
            classes: HashMap::new(),
            symbols: HashMap::new(),
            methods: HashMap::new(),
            stacks: HashMap::new(),
            enums: HashMap::new(),
            types: HashMap::new(),
            event_encodings: Vec::new(),
            strings: HashMap::new(),
            offset,
            end: offset + h.size,
        };
        self.metadata(&mut chunk, offset + h.metadata)?;
        let max_id = chunk.types.keys().copied().max().unwrap_or(0).min(65535) as usize;
        chunk.event_encodings.resize(max_id + 1, Encoding::Unknown);
        for (&id, ty) in &chunk.types {
            if id < chunk.event_encodings.len() as u64 {
                chunk.event_encodings[id as usize] = Encoding::from_name(&ty.name);
            }
        }
        self.constants(&mut chunk, offset + h.cp)?;
        self.next_offset = chunk.end;
        Ok(Some(chunk))
    }
    fn metadata(&mut self, chunk: &mut Chunk, offset: u64) -> Result<()> {
        let (kind, _) = self.input.record(offset, chunk.end)?;
        if kind != 0 {
            return Err(invalid("Invalid JFR metadata event type"));
        }
        for _ in 0..3 {
            self.input.var()?;
        }
        let count = self.input.count()?;
        if count > 262144 {
            return Err(invalid("JFR metadata string table exceeds 262144 entries"));
        }
        let empty = HashMap::new();
        let mut strings = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            strings.push(self.input.string(&empty)?.unwrap_or_default());
        }
        self.element(chunk, &strings, 0)?;
        Ok(())
    }
    fn element(
        &mut self,
        chunk: &mut Chunk,
        strings: &[String],
        depth: usize,
    ) -> Result<Option<Field>> {
        if depth >= 128 {
            return Err(invalid("JFR metadata nesting too deep"));
        }
        fn text(strings: &[String], index: u64) -> Result<String> {
            strings
                .get(usize::try_from(index)?)
                .cloned()
                .ok_or_else(|| invalid("JFR metadata string index out of range"))
        }
        let name = text(strings, self.input.var()?)?;
        let count = self.input.count()?;
        let mut attrs = HashMap::new();
        for _ in 0..count {
            let key = text(strings, self.input.var()?)?;
            let val = text(strings, self.input.var()?)?;
            attrs.insert(key, val);
        }
        let count = self.input.count()?;
        let mut fields = Vec::new();
        for _ in 0..count {
            if let Some(field) = self.element(chunk, strings, depth + 1)? {
                fields.push(field);
            }
        }
        let attr = |key: &str| {
            attrs
                .get(key)
                .ok_or_else(|| invalid(format!("JFR {name} missing {key}")))
        };
        if name == "class" {
            let id = attr("id")?.parse()?;
            chunk.types.insert(
                id,
                Type {
                    name: attr("name")?.clone(),
                    simple: attrs.get("simpleType").is_some_and(|v| v == "true"),
                    fields,
                },
            );
        } else if name == "field" {
            return Ok(Some(Field {
                name: attr("name")?.clone(),
                type_id: attr("class")?.parse()?,
                constant_pool: attrs.get("constantPool").is_some_and(|v| v == "true"),
                dimension: attrs
                    .get("dimension")
                    .map(|v| v.parse())
                    .transpose()?
                    .unwrap_or(0),
            }));
        }
        Ok(None)
    }
    fn constants(&mut self, chunk: &mut Chunk, mut offset: u64) -> Result<()> {
        let mut visited = HashSet::new();
        loop {
            if offset < chunk.offset + HEADER || !visited.insert(offset) {
                return Err(invalid("Invalid/cyclic JFR checkpoint chain"));
            }
            let (kind, _) = self.input.record(offset, chunk.end)?;
            if kind != 1 {
                return Err(invalid("Invalid JFR checkpoint event type"));
            }
            self.input.var()?;
            self.input.var()?;
            let delta = self.input.var()? as i64;
            self.input.byte()?; // flush flag
            let pools = self.input.count()?;
            for _ in 0..pools {
                let id = self.input.var()?;
                let ty = chunk
                    .types
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| invalid(format!("Unknown JFR constant-pool type {id}")))?;
                self.pool(chunk, &ty)?;
            }
            if delta == 0 {
                break;
            }
            offset = u64::try_from(offset as i128 + delta as i128)
                .map_err(|_| invalid("JFR checkpoint offset overflow"))?;
        }
        Ok(())
    }
    fn pool(&mut self, chunk: &mut Chunk, ty: &Type) -> Result<()> {
        // ChunkHeader is an opaque fixed-size checkpoint payload used by JDK.
        if ty.name == "jdk.types.ChunkHeader" {
            let mut bytes = [0; HEADER as usize + 3];
            self.input.take(&mut bytes)?;
            return Ok(());
        }
        let count = self.input.count()?;
        for _ in 0..count {
            let id = self.input.var()?;
            match ty.name.as_str() {
                "java.lang.Thread" => {
                    let os = self.input.string(&chunk.strings)?;
                    self.input.var()?;
                    let java = self.input.string(&chunk.strings)?;
                    self.input.var()?;
                    self.extra_fields(chunk, ty, 4)?;
                    if let Some(name) = java.or(os) {
                        chunk.threads.entry(id).or_insert(name);
                    }
                }
                "java.lang.Class" => {
                    self.input.var()?;
                    let name = self.input.var()?;
                    self.input.var()?;
                    self.input.var()?;
                    self.extra_fields(chunk, ty, 4)?;
                    chunk.classes.entry(id).or_insert(ClassRef { name });
                }
                "java.lang.String" => {
                    let s = self.input.string(&chunk.strings)?;
                    if let Some(s) = s {
                        chunk.strings.entry(id).or_insert(s);
                    }
                }
                "jdk.types.Symbol" => {
                    let s = self.input.string(&chunk.strings)?.unwrap_or_default();
                    chunk.symbols.entry(id).or_insert(s);
                }
                "jdk.types.Method" => {
                    let class = self.input.var()?;
                    let name = self.input.var()?;
                    self.input.var()?;
                    self.input.var()?;
                    self.input.var()?;
                    self.extra_fields(chunk, ty, 5)?;
                    chunk.methods.entry(id).or_insert(MethodRef { class, name });
                }
                "jdk.types.StackTrace" => {
                    self.input.byte()?;
                    let depth = self.input.count()?;
                    if depth > 65536 {
                        return Err(invalid("JFR stack depth exceeds 65536"));
                    }
                    let mut stack = Vec::with_capacity(depth);
                    for _ in 0..depth {
                        let method = self.input.var()?;
                        let line = self.input.var()? as i32;
                        let bci = self.input.var()? as i32;
                        let kind = self.input.byte()?;
                        stack.push(StackFrame {
                            method,
                            line,
                            bci,
                            kind,
                        });
                    }
                    chunk.stacks.entry(id).or_insert(stack);
                }
                _ if ty.simple && ty.fields.len() == 1 => {
                    let name = self.input.string(&chunk.strings)?.unwrap_or_default();
                    chunk
                        .enums
                        .entry(ty.name.clone())
                        .or_default()
                        .entry(id)
                        .or_insert(name);
                }
                _ => {
                    for field in &ty.fields {
                        self.skip_field(chunk, field, 0)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn extra_fields(&mut self, chunk: &Chunk, ty: &Type, start: usize) -> Result<()> {
        for field in ty.fields.iter().skip(start) {
            self.skip_field(chunk, field, 0)?;
        }
        Ok(())
    }
    fn skip_field(&mut self, chunk: &Chunk, field: &Field, depth: usize) -> Result<()> {
        if depth >= 128 {
            return Err(invalid("JFR field nesting too deep"));
        }
        if field.dimension > 0 {
            let count = self.input.count()?;
            let mut element = field.clone();
            element.dimension -= 1;
            for _ in 0..count {
                self.skip_field(chunk, &element, depth + 1)?;
            }
            return Ok(());
        }
        if field.constant_pool {
            self.input.var()?;
            return Ok(());
        }
        let ty = chunk
            .types
            .get(&field.type_id)
            .ok_or_else(|| invalid("Unknown JFR field type"))?;
        match ty.name.as_str() {
            "java.lang.String" => {
                self.input.string(&chunk.strings)?;
            }
            "float" => {
                let mut b = [0; 4];
                self.input.take(&mut b)?;
            }
            "double" => {
                let mut b = [0; 8];
                self.input.take(&mut b)?;
            }
            "boolean" | "byte" => {
                self.input.byte()?;
            }
            "char" | "short" | "int" | "long" => {
                self.input.var()?;
            }
            _ if ty.fields.is_empty() => {
                self.input.var()?;
            }
            _ => {
                for f in &ty.fields {
                    self.skip_field(chunk, f, depth + 1)?;
                }
            }
        }
        Ok(())
    }
    /// Stream recognized profiling events. No event list or recording buffer is retained.
    #[allow(dead_code)]
    pub fn events(&mut self, chunk: &Chunk, visit: impl FnMut(Event) -> Result<()>) -> Result<()> {
        self.events_selected(chunk, EventMask::ALL, visit)
    }
    /// Stream only selected families, avoiding payload decoding and allocations
    /// for unrelated events (including allocation of span tags).
    #[inline]
    pub fn events_selected(
        &mut self,
        chunk: &Chunk,
        mask: EventMask,
        mut visit: impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        let mut offset = chunk.offset + HEADER;
        while offset < chunk.end {
            let (id, end) = self.input.record(offset, chunk.end)?;
            let encoding = if id < chunk.event_encodings.len() as u64 {
                chunk.event_encodings[id as usize]
            } else {
                chunk
                    .types
                    .get(&id)
                    .map_or(Encoding::Unknown, |ty| Encoding::from_name(&ty.name))
            };
            if let Some(kind) = encoding.kind() {
                if mask.includes(kind) {
                    if kind == EventKind::Execution {
                        visit(self.execution(encoding == Encoding::Wall, end)?)?;
                    } else if let Some(ty) = chunk.types.get(&id) {
                        if let Some(event) = self.event_encoded(chunk, ty, encoding)? {
                            visit(event)?;
                        }
                    }
                }
            }
            offset = end;
        }
        Ok(())
    }
    #[inline(always)]
    fn execution(&mut self, wall: bool, end: u64) -> Result<Event> {
        // Five JFR values need at most45 bytes of lookahead. Reads may inspect
        // buffered bytes after the record, but validation precedes the callback.
        // This removes per-byte I/O calls while retaining record-boundary checks.
        if self.input.filled - self.input.cursor >= 45 {
            let mut cursor = self.input.cursor;
            let bytes = &self.input.buffer;
            let time = Input::buffered_var(bytes, &mut cursor);
            let tid = Input::buffered_var(bytes, &mut cursor);
            let stack_id = Input::buffered_var(bytes, &mut cursor);
            let state = Input::buffered_var(bytes, &mut cursor);
            let samples = if wall {
                Input::buffered_var(bytes, &mut cursor)
            } else {
                1
            };
            if self.input.base + cursor as u64 > end {
                return Err(invalid("Truncated JFR execution sample"));
            }
            self.input.cursor = cursor;
            let mut e = Event::new(EventKind::Execution);
            e.time = time;
            e.tid = tid;
            e.stack_id = stack_id;
            e.state = state;
            e.samples = samples;
            e.value = samples;
            e.wall = wall;
            Ok(e)
        } else {
            let mut e = Event::new(EventKind::Execution);
            e.time = self.input.var()?;
            e.tid = self.input.var()?;
            e.stack_id = self.input.var()?;
            e.state = self.input.var()?;
            e.samples = if wall { self.input.var()? } else { 1 };
            e.value = e.samples;
            e.wall = wall;
            Ok(e)
        }
    }
    #[cfg(test)]
    fn event(&mut self, chunk: &Chunk, ty: &Type) -> Result<Option<Event>> {
        self.event_encoded(chunk, ty, Encoding::from_name(&ty.name))
    }
    fn event_encoded(
        &mut self,
        chunk: &Chunk,
        ty: &Type,
        encoding: Encoding,
    ) -> Result<Option<Event>> {
        let Some(kind) = encoding.kind() else {
            return Ok(None);
        };
        let mut e = Event::new(kind);
        if kind == EventKind::Span {
            // Extensions are decoded by their own metadata; v4.1 has no built-in span event.
            for field in &ty.fields {
                if field.name == "tag"
                    && chunk
                        .types
                        .get(&field.type_id)
                        .is_some_and(|ty| ty.name == "java.lang.String")
                {
                    e.tag = if field.constant_pool {
                        let id = self.input.var()?;
                        if id == 0 {
                            None
                        } else {
                            Some(
                                chunk
                                    .strings
                                    .get(&id)
                                    .ok_or_else(|| invalid("Missing span tag string constant"))?
                                    .clone(),
                            )
                        }
                    } else {
                        self.input.string(&chunk.strings)?
                    };
                } else if matches!(
                    field.name.as_str(),
                    "startTime"
                        | "time"
                        | "duration"
                        | "eventThread"
                        | "sampledThread"
                        | "thread"
                        | "stackTrace"
                        | "method"
                ) {
                    let value = self.input.var()?;
                    match field.name.as_str() {
                        "startTime" | "time" => e.time = value,
                        "duration" => {
                            e.duration = value;
                            e.value = value;
                        }
                        "stackTrace" => e.stack_id = value,
                        "method" => e.method_id = value,
                        _ => e.tid = value,
                    }
                } else {
                    self.skip_field(chunk, field, 0)?;
                }
            }
            return Ok(Some(e));
        }
        e.time = self.input.var()?;
        if kind == EventKind::Lock {
            e.duration = self.input.var()?;
            e.value = e.duration;
        }
        e.tid = self.input.var()?;
        e.stack_id = self.input.var()?;
        match kind {
            EventKind::Execution => {
                e.state = self.input.var()?;
                e.wall = encoding == Encoding::Wall;
                if e.wall {
                    e.samples = self.input.var()?;
                }
                e.value = e.samples;
            }
            EventKind::Allocation => {
                e.class_id = self.input.var()?;
                e.value = self.input.var()?;
                if encoding == Encoding::Tlab {
                    let tlab_size = self.input.var()?;
                    if tlab_size != 0 {
                        e.value = tlab_size;
                        e.tlab = true;
                    }
                }
            }
            EventKind::Live => {
                e.class_id = self.input.var()?;
                e.value = self.input.var()?;
                self.input.var()?;
            }
            EventKind::Lock => {
                e.class_id = self.input.var()?;
                if encoding == Encoding::Park {
                    self.input.var()?;
                }
                self.input.var()?;
                e.address = self.input.var()?;
            }
            EventKind::Malloc | EventKind::Free => {
                e.address = self.input.var()?;
                e.value = if kind == EventKind::Malloc {
                    self.input.var()?
                } else {
                    0
                };
            }
            EventKind::Span => unreachable!(),
        }
        Ok(Some(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn input(bytes: &[u8]) -> Input {
        let path = std::env::temp_dir().join(format!(
            "jfrconv-parser-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).unwrap();
        let file = File::open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        Input::new(file, bytes.len() as u64)
    }
    fn encoded(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..8 {
            if value < 128 {
                out.push(value as u8);
                return out;
            }
            out.push((value as u8 & 127) | 128);
            value >>= 7;
        }
        out.push(value as u8);
        out
    }
    #[test]
    fn jfr_varlong_roundtrips_full_width_and_signed_delta_bits() {
        for n in [
            0,
            1,
            127,
            128,
            16383,
            16384,
            1 << 55,
            1 << 56,
            u64::MAX - 1024,
            u64::MAX,
        ] {
            let bytes = encoded(n);
            assert!(bytes.len() <= 9);
            assert_eq!(input(&bytes).var().unwrap(), n);
        }
        assert_eq!(input(&[255; 9]).var().unwrap(), u64::MAX);
    }
    #[test]
    fn truncated_varlong_and_cross_record_reads_fail() {
        assert!(input(&[128]).var().is_err());
        let mut inp = input(&[2, 42, 99, 99]);
        assert_eq!(inp.record(0, 4).unwrap(), (42, 2));
        assert!(inp.byte().is_err());
        assert!(input(&[5, 42, 0]).record(0, 3).is_err());
        assert!(input(&[0, 0]).record(0, 2).is_err());
    }
    #[test]
    fn strings_handle_encodings_and_reject_invalid_counts() {
        let empty = HashMap::new();
        assert_eq!(input(&[0]).string(&empty).unwrap(), None);
        assert_eq!(input(&[1]).string(&empty).unwrap(), Some(String::new()));
        assert_eq!(
            input(&[3, 2, 0xce, 0xb1])
                .string(&empty)
                .unwrap()
                .as_deref(),
            Some("α")
        );
        assert_eq!(
            input(&[5, 1, 0xe9]).string(&empty).unwrap().as_deref(),
            Some("é")
        );
        let mut chars = vec![4, 2];
        chars.extend(encoded(0xd83d));
        chars.extend(encoded(0xde00));
        assert_eq!(input(&chars).string(&empty).unwrap().as_deref(), Some("😀"));
        assert!(input(&[3, 127]).string(&empty).is_err());
        assert!(input(&[255]).string(&empty).is_err());
        assert!(input(&[2, 12]).string(&empty).is_err());
    }
    #[test]
    fn extension_span_uses_string_constant_pool_without_stack_and_method_trace_has_method() {
        let mut chunk = Chunk {
            start_ns: 0,
            end_ns: 1000,
            start_ticks: 0,
            ticks_per_sec: 1_000_000_000,
            threads: HashMap::new(),
            classes: HashMap::new(),
            symbols: HashMap::new(),
            methods: HashMap::new(),
            stacks: HashMap::new(),
            enums: HashMap::new(),
            types: HashMap::new(),
            event_encodings: Vec::new(),
            strings: HashMap::from([(42, "request-α".to_owned())]),
            offset: 0,
            end: 1000,
        };
        chunk.types.insert(
            1,
            Type {
                name: "long".into(),
                simple: false,
                fields: Vec::new(),
            },
        );
        chunk.types.insert(
            2,
            Type {
                name: "java.lang.String".into(),
                simple: false,
                fields: Vec::new(),
            },
        );
        let field = |name: &str, type_id, constant_pool| Field {
            name: name.into(),
            type_id,
            constant_pool,
            dimension: 0,
        };
        let span = Type {
            name: "profiler.Span".into(),
            simple: false,
            fields: vec![
                field("startTime", 1, false),
                field("duration", 1, false),
                field("eventThread", 1, true),
                field("tag", 2, true),
            ],
        };
        let mut bytes = Vec::new();
        for value in [100, 500, 7, 42] {
            bytes.extend(encoded(value));
        }
        let mut reader = JfrReader {
            input: input(&bytes),
            file_size: bytes.len() as u64,
            next_offset: 0,
            start_ns: 0,
            end_ns: 1000,
        };
        let event = reader.event(&chunk, &span).unwrap().unwrap();
        assert_eq!(
            (event.time, event.duration, event.tid, event.stack_id),
            (100, 500, 7, 0)
        );
        assert_eq!(event.tag.as_deref(), Some("request-α"));
        assert_eq!(reader.input.position(), bytes.len() as u64);
        let mut trace = Type {
            name: "profiler.MethodTrace".into(),
            simple: false,
            fields: vec![
                field("startTime", 1, false),
                field("duration", 1, false),
                field("eventThread", 1, true),
                field("stackTrace", 1, true),
                field("method", 1, true),
            ],
        };
        let mut bytes = Vec::new();
        for value in [200, 800, 7, 23, 99] {
            bytes.extend(encoded(value));
        }
        reader.input = input(&bytes);
        let event = reader.event(&chunk, &trace).unwrap().unwrap();
        assert_eq!(
            (
                event.time,
                event.duration,
                event.tid,
                event.stack_id,
                event.method_id
            ),
            (200, 800, 7, 23, 99)
        );
        assert_eq!(event.tag, None);
        assert_eq!(reader.input.position(), bytes.len() as u64);
        // Current async-profiler records this extension under the jdk namespace.
        trace.name = "jdk.MethodTrace".into();
        reader.input = input(&bytes);
        let event = reader.event(&chunk, &trace).unwrap().unwrap();
        assert_eq!(
            (
                event.time,
                event.duration,
                event.tid,
                event.stack_id,
                event.method_id
            ),
            (200, 800, 7, 23, 99)
        );
    }
    #[test]
    fn varints_cross_buffer_boundaries_and_seeks_keep_positions() {
        let mut bytes = vec![0; BUFFER_SIZE - 4];
        bytes.extend(encoded(u64::MAX));
        bytes.push(66);
        let mut inp = input(&bytes);
        let mut prefix = vec![0; BUFFER_SIZE - 4];
        inp.take(&mut prefix).unwrap();
        assert_eq!(inp.var().unwrap(), u64::MAX);
        assert_eq!(inp.byte().unwrap(), 66);
        assert_eq!(inp.position(), bytes.len() as u64);
        inp.seek((BUFFER_SIZE - 4) as u64).unwrap();
        assert_eq!(inp.var().unwrap(), u64::MAX);
        inp.seek(0).unwrap();
        assert_eq!(inp.byte().unwrap(), 0);
    }
    #[test]
    fn fast_lookahead_never_accepts_values_past_record_boundary() {
        let mut bytes = vec![0; 100];
        bytes[..4].copy_from_slice(&[1, 2, 3, 4]);
        let mut reader = JfrReader {
            input: input(&bytes),
            file_size: bytes.len() as u64,
            next_offset: 0,
            start_ns: 0,
            end_ns: 1000,
        };
        reader.input.refill().unwrap();
        assert!(reader.execution(false, 3).is_err());
        assert_eq!(reader.input.position(), 0);
        let mut bytes = vec![0; 100];
        bytes[0] = 128;
        let mut inp = input(&bytes);
        inp.refill().unwrap();
        inp.limit = 1;
        assert!(inp.var().is_err());
        assert_eq!(inp.position(), 0);
    }
    #[test]
    fn selected_event_stream_preserves_weights_and_validates_before_callback() {
        let bytes = include_bytes!("../tests/fixtures/mixed.jfr");
        let reader = || JfrReader {
            input: input(bytes),
            file_size: bytes.len() as u64,
            next_offset: 0,
            start_ns: 0,
            end_ns: 0,
        };
        let mut r = reader();
        let mut events = 0;
        let mut samples = 0;
        while let Some(chunk) = r.next_chunk().unwrap() {
            r.events_selected(&chunk, EventMask::EXECUTION, |e| {
                assert_eq!(e.kind, EventKind::Execution);
                events += 1;
                samples += e.samples;
                Ok(())
            })
            .unwrap();
        }
        assert_eq!((events, samples), (10, 26));
        let mut r = reader();
        let mut events = 0;
        while let Some(chunk) = r.next_chunk().unwrap() {
            r.events_selected(&chunk, EventMask::SPAN, |e| {
                assert_eq!(e.kind, EventKind::Span);
                events += 1;
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(events, 4);
        let mut corrupt = bytes.to_vec();
        corrupt[HEADER as usize] = 2;
        let mut r = JfrReader {
            input: input(&corrupt),
            file_size: corrupt.len() as u64,
            next_offset: 0,
            start_ns: 0,
            end_ns: 0,
        };
        let chunk = r.next_chunk().unwrap().unwrap();
        let mut callbacks = 0;
        assert!(r
            .events_selected(&chunk, EventMask::EXECUTION, |_| {
                callbacks += 1;
                Ok(())
            })
            .is_err());
        assert_eq!(callbacks, 0);
    }
}
