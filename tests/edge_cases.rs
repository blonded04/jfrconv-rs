// SPDX-License-Identifier: Apache-2.0
// Small metadata-driven fixtures are reproducible with tools/fixtures.py.
// These regressions require neither Java nor async-profiler at test time.
use std::collections::BTreeMap;
use std::process::Command;

fn convert(input: &[u8], format: &str, flags: &[&str]) -> Vec<u8> {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("input.jfr");
    let output = scratch.path().join(format!("output.{format}"));
    std::fs::write(&source, input).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_jfrconv"))
        .args(flags)
        .args(["-o", format])
        .arg(source)
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::read(output).unwrap()
}

#[derive(Debug)]
enum Value<'a> {
    Number(u64),
    Message(&'a [u8]),
}
fn varint(data: &[u8], cursor: &mut usize) -> u64 {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*cursor).expect("truncated protobuf varint");
        *cursor += 1;
        assert!(shift != 63 || byte <= 1, "protobuf varint overflow");
        value |= ((byte & 127) as u64) << shift;
        if byte < 128 {
            return value;
        }
    }
    panic!("invalid protobuf varint")
}
fn fields(data: &[u8]) -> Vec<(u32, Value<'_>)> {
    let mut cursor = 0;
    let mut result = Vec::new();
    while cursor < data.len() {
        let key = varint(data, &mut cursor);
        assert_ne!(key >> 3, 0, "invalid protobuf field number");
        let value = match key & 7 {
            0 => Value::Number(varint(data, &mut cursor)),
            2 => {
                let length = usize::try_from(varint(data, &mut cursor)).unwrap();
                let end = cursor
                    .checked_add(length)
                    .filter(|&end| end <= data.len())
                    .expect("truncated protobuf message");
                let bytes = &data[cursor..end];
                cursor = end;
                Value::Message(bytes)
            }
            wire => panic!("unexpected protobuf wire type {wire}"),
        };
        result.push(((key >> 3) as u32, value));
    }
    result
}
fn messages<'a>(fields: &[(u32, Value<'a>)], key: u32) -> Vec<&'a [u8]> {
    fields
        .iter()
        .filter_map(|(field, value)| match value {
            Value::Message(bytes) if *field == key => Some(*bytes),
            _ => None,
        })
        .collect()
}
fn number(fields: &[(u32, Value<'_>)], key: u32) -> u64 {
    fields
        .iter()
        .find_map(|(field, value)| match value {
            Value::Number(value) if *field == key => Some(*value),
            _ => None,
        })
        .unwrap_or(0)
}
fn canonical(bytes: &[u8]) -> BTreeMap<String, u64> {
    String::from_utf8(bytes.to_owned())
        .unwrap()
        .lines()
        .map(|row| {
            let (stack, value) = row.rsplit_once(' ').unwrap();
            (stack.to_owned(), value.parse().unwrap())
        })
        .collect()
}

#[test]
fn pprof_keeps_samples_with_zero_or_missing_stack_ids() {
    let bytes = convert(
        include_bytes!("fixtures/empty-stacks.jfr"),
        "pprof",
        &["--cpu"],
    );
    let top = fields(&bytes);
    let mut total = 0;
    let mut without_stack = 0;
    for sample in messages(&top, 2) {
        let sample = fields(sample);
        let amount = number(&sample, 2);
        total += amount;
        if messages(&sample, 1)
            .iter()
            .all(|locations| locations.is_empty())
        {
            without_stack += amount;
        }
    }
    assert_eq!(total, 3, "all CPU samples must survive pprof conversion");
    assert_eq!(
        without_stack, 2,
        "zero and unresolved stack IDs have empty location lists"
    );
}

#[test]
fn otlp_keeps_empty_stack_counts_and_values() {
    let bytes = convert(
        include_bytes!("fixtures/empty-stacks.jfr"),
        "otlp",
        &["--cpu"],
    );
    let top = fields(&bytes);
    let mut count = 0;
    let mut value = 0;
    let mut without_stack = 0;
    for resource in messages(&top, 1) {
        for scope in messages(&fields(resource), 2) {
            for profile in messages(&fields(scope), 2) {
                for sample in messages(&fields(profile), 2) {
                    let sample = fields(sample);
                    let packed = messages(&sample, 3);
                    assert_eq!(packed.len(), 1);
                    let mut cursor = 0;
                    let samples = varint(packed[0], &mut cursor);
                    let total_value = varint(packed[0], &mut cursor);
                    assert_eq!(
                        cursor,
                        packed[0].len(),
                        "sample must contain count and value"
                    );
                    count += samples;
                    value += total_value;
                    if number(&sample, 2) == 0 {
                        without_stack += samples;
                    }
                }
            }
        }
    }
    assert_eq!((count, value, without_stack), (3, 3, 2));
}

#[test]
fn collapsed_output_omits_empty_stacks_like_v41() {
    let bytes = convert(
        include_bytes!("fixtures/empty-stacks.jfr"),
        "collapsed",
        &["--cpu"],
    );
    assert_eq!(
        canonical(&bytes),
        BTreeMap::from([("demo/Main.run_[j];demo/Worker.work_[j]".into(), 1)])
    );
}

#[test]
fn method_trace_appends_explicit_method_and_converts_duration_to_nanoseconds() {
    // Fixture ticks run at 2 GHz: 75M/25M ticks become 37.5M/12.5M nanoseconds.
    let bytes = convert(
        include_bytes!("fixtures/method-trace.jfr"),
        "collapsed",
        &["--trace", "--total"],
    );
    assert_eq!(
        canonical(&bytes),
        BTreeMap::from([
            (
                "demo/Main.run_[j];malloc;demo/Worker.work_[j]".into(),
                37_500_000
            ),
            ("demo/Worker.work_[j]".into(), 12_500_000),
        ])
    );
    let bytes = convert(
        include_bytes!("fixtures/method-trace.jfr"),
        "collapsed",
        &["--trace"],
    );
    assert_eq!(
        canonical(&bytes),
        BTreeMap::from([
            ("demo/Main.run_[j];malloc;demo/Worker.work_[j]".into(), 1),
            ("demo/Worker.work_[j]".into(), 1),
        ])
    );
}
