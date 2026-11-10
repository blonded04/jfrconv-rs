// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
// Small streaming protobuf encoder. Nested messages are measured before writing;
// the serialized profile is never retained in memory.

use std::io::{self, Write};

pub fn varint(out: &mut dyn Write, mut value: u64) -> io::Result<()> {
    let mut bytes = [0u8; 10];
    let mut length = 0;
    while value >= 128 {
        bytes[length] = value as u8 | 128;
        value >>= 7;
        length += 1;
    }
    bytes[length] = value as u8;
    out.write_all(&bytes[..=length])
}

pub fn uint(out: &mut dyn Write, field: u32, value: u64) -> io::Result<()> {
    varint(out, (field as u64) << 3)?;
    varint(out, value)
}

pub fn bytes(out: &mut dyn Write, field: u32, value: &[u8]) -> io::Result<()> {
    varint(out, ((field as u64) << 3) | 2)?;
    varint(out, value.len() as u64)?;
    out.write_all(value)
}

pub fn header(out: &mut dyn Write, field: u32, length: u64) -> io::Result<()> {
    varint(out, ((field as u64) << 3) | 2)?;
    varint(out, length)
}

struct Count(u64);

impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("protobuf message is too large"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn message(
    out: &mut dyn Write,
    field: u32,
    body: impl Fn(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let mut count = Count(0);
    body(&mut count)?;
    varint(out, ((field as u64) << 3) | 2)?;
    varint(out, count.0)?;
    body(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_message_has_exact_lengths() {
        let mut out = Vec::new();
        message(&mut out, 1, |out| {
            uint(out, 1, 150)?;
            bytes(out, 2, b"abc")
        })
        .unwrap();
        assert_eq!(out, [10, 8, 8, 150, 1, 18, 3, b'a', b'b', b'c']);
    }

    #[test]
    fn largest_varint_uses_ten_bytes() {
        let mut out = Vec::new();
        varint(&mut out, u64::MAX).unwrap();
        assert_eq!(out.len(), 10);
        assert_eq!(out[9], 1);
    }
}
