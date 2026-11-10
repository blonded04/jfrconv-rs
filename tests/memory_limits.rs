// SPDX-License-Identifier: Apache-2.0
// Exercise the supervisor and allocator in isolated child processes. A failed
// worker must never terminate the test process or replace a prior output.
use std::path::Path;
use std::process::{Command, Output};

const MIXED: &[u8] = include_bytes!("fixtures/mixed.jfr");
fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jfrconv"));
    for key in [
        "_JFRCONV_LIMITED_WORKER",
        "_JFRCONV_ALLOCATION_BUDGET",
        "_JFRCONV_RSS_LIMIT",
        "_JFRCONV_OUTPUT_SCRATCH",
    ] {
        command.env_remove(key);
    }
    command
}
fn assert_success(result: &Output) {
    assert!(
        result.status.success(),
        "exit {:?}: {}",
        result.status.code(),
        String::from_utf8_lossy(&result.stderr)
    );
}
fn assert_no_scratch(path: &Path) {
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        assert!(
            !name.starts_with(".jfrconv-worker-") && !name.starts_with("jfrconv-spill-"),
            "supervisor left scratch directory {}",
            entry.path().display()
        );
        if entry.file_type().unwrap().is_dir() {
            assert_no_scratch(&entry.path());
        }
    }
}
#[derive(Debug)]
struct Stats {
    live: u64,
    peak: u64,
    rss: u64,
}
fn stats(result: &Output) -> Vec<Stats> {
    String::from_utf8_lossy(&result.stderr)
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("memory: ")?;
            let value = |key| {
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix(key))
                    .unwrap()
                    .parse()
                    .unwrap()
            };
            Some(Stats {
                live: value("live="),
                peak: value("peak_reserved="),
                rss: value("rss="),
            })
        })
        .collect()
}
fn total(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|row| row.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap())
        .sum()
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}
fn jfr_varint(bytes: &[u8], cursor: &mut usize) -> u64 {
    let mut value = 0;
    for shift in (0..56).step_by(7) {
        let byte = bytes[*cursor];
        *cursor += 1;
        value |= ((byte & 127) as u64) << shift;
        if byte < 128 {
            return value;
        }
    }
    let byte = bytes[*cursor];
    *cursor += 1;
    value | ((byte as u64) << 56)
}
fn repeated_cpu_file(path: &Path, repetitions: usize) {
    // Reuse the first fixture event and its real metadata/pool. The recording
    // grows by event count while its dictionary and output cardinality stay fixed.
    let mut cursor = 68;
    let event_length = jfr_varint(MIXED, &mut cursor) as usize;
    let event = &MIXED[68..68 + event_length];
    let old_size = u64_at(MIXED, 8) as usize;
    let old_cp = u64_at(MIXED, 16) as usize;
    let old_meta = u64_at(MIXED, 24) as usize;
    let new_meta = 68 + event_length * repetitions;
    let new_cp = new_meta + old_cp - old_meta;
    let new_size = new_cp + old_size - old_cp;
    let mut bytes = Vec::with_capacity(new_size);
    bytes.extend_from_slice(&MIXED[..68]);
    put_u64(&mut bytes, 8, new_size as u64);
    put_u64(&mut bytes, 16, new_cp as u64);
    put_u64(&mut bytes, 24, new_meta as u64);
    for _ in 0..repetitions {
        bytes.extend_from_slice(event);
    }
    bytes.extend_from_slice(&MIXED[old_meta..old_size]);
    assert_eq!(bytes.len(), new_size);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn tiny_allocation_budget_preserves_output_and_cleans_both_scratch_directories() {
    let scratch = tempfile::tempdir().unwrap();
    let input = scratch.path().join("input.jfr");
    let output = scratch.path().join("existing.collapsed");
    let temp = scratch.path().join("system-temp");
    std::fs::create_dir(&temp).unwrap();
    std::fs::write(&input, MIXED).unwrap();
    std::fs::write(&output, b"existing result\n").unwrap();
    let result = command()
        .args(["--memory-limit", "1K", "--cpu", "-o", "collapsed"])
        .arg(&input)
        .arg(&output)
        .env("TMPDIR", &temp)
        .env("TMP", &temp)
        .env("TEMP", &temp)
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(75),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("allocation memory limit exceeded"));
    assert_eq!(std::fs::read(&output).unwrap(), b"existing result\n");
    assert_no_scratch(scratch.path());
    assert_eq!(std::fs::read_dir(temp).unwrap().count(), 0);
}

#[test]
fn sixteen_megabyte_budget_handles_a_million_samples_with_fixed_cardinality() {
    let scratch = tempfile::tempdir().unwrap();
    let input = scratch.path().join("small.jfr");
    let output = scratch.path().join("small.collapsed");
    std::fs::write(&input, MIXED).unwrap();
    let small = command()
        .args([
            "--memory-limit",
            "16M",
            "--memory-stats",
            "--cpu",
            "-o",
            "collapsed",
        ])
        .arg(&input)
        .arg(&output)
        .output()
        .unwrap();
    assert_success(&small);
    assert_eq!(total(&output), 6);
    let large_input = scratch.path().join("million.jfr");
    let large_output = scratch.path().join("million.collapsed");
    repeated_cpu_file(&large_input, 1_000_000);
    let large = command()
        .args([
            "--memory-limit",
            "16M",
            "--memory-stats",
            "--cpu",
            "-o",
            "collapsed",
        ])
        .arg(&large_input)
        .arg(&large_output)
        .output()
        .unwrap();
    assert_success(&large);
    assert_eq!(total(&large_output), 1_000_000);
    let small = stats(&small);
    let large = stats(&large);
    assert_eq!(small.len(), 1);
    assert_eq!(large.len(), 1);
    assert!(large[0].peak < 16 * 1024 * 1024);
    assert!(
        large[0].peak <= small[0].peak + 512 * 1024,
        "sample count increased reserved heap: small={:?}, large={:?}",
        small[0],
        large[0]
    );
    assert_no_scratch(scratch.path());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn thirty_two_megabyte_rss_guard_succeeds_and_observes_worker_resident_memory() {
    let scratch = tempfile::tempdir().unwrap();
    let input = scratch.path().join("input.jfr");
    let output = scratch.path().join("output.collapsed");
    std::fs::write(&input, MIXED).unwrap();
    let result = command()
        .args([
            "--max-rss",
            "32M",
            "--memory-stats",
            "--cpu",
            "-o",
            "collapsed",
        ])
        .arg(input)
        .arg(&output)
        .output()
        .unwrap();
    assert_success(&result);
    assert_eq!(total(&output), 6);
    let observed = stats(&result);
    assert_eq!(observed.len(), 1);
    assert!(
        observed[0].rss > 0,
        "worker RSS must come from the operating system"
    );
    assert!(
        observed[0].rss <= 32 * 1024 * 1024,
        "worker RSS exceeded guard: {:?}",
        observed[0]
    );
    assert_no_scratch(scratch.path());
}

#[test]
fn repeated_inputs_release_live_heap_and_resident_memory_plateaus() {
    let scratch = tempfile::tempdir().unwrap();
    let input = scratch.path().join("input.jfr");
    let output = scratch.path().join("outputs");
    std::fs::write(&input, MIXED).unwrap();
    std::fs::create_dir(&output).unwrap();
    let mut cmd = command();
    cmd.args(["--memory-stats", "--cpu", "-o", "collapsed"]);
    for _ in 0..64 {
        cmd.arg(&input);
    }
    let result = cmd.arg(&output).output().unwrap();
    assert_success(&result);
    let observed = stats(&result);
    assert_eq!(observed.len(), 64);
    assert!(observed.iter().all(|s| s.peak >= s.live));
    let min_live = observed.iter().map(|s| s.live).min().unwrap();
    let max_live = observed.iter().map(|s| s.live).max().unwrap();
    assert!(
        max_live - min_live < 128 * 1024,
        "live heap accumulated across files: {observed:?}"
    );
    assert!(
        observed.last().unwrap().live <= observed[0].live + 16 * 1024,
        "live heap was not released after each file: {observed:?}"
    );
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let min = observed.iter().map(|s| s.rss).min().unwrap();
        let max = observed.iter().map(|s| s.rss).max().unwrap();
        assert!(min > 0);
        assert!(
            max - min < 4 * 1024 * 1024,
            "RSS grows with repeated files: {observed:?}"
        );
    }
    assert_eq!(total(&output.join("input.collapsed")), 6);
    assert_no_scratch(scratch.path());
}

#[cfg(unix)]
#[test]
fn limited_diff_with_implicit_output_uses_working_directory_for_scratch() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();
    let inputs = scratch.path().join("read-only-inputs");
    std::fs::create_dir(&inputs).unwrap();
    let before = inputs.join("before.jfr");
    let after = inputs.join("after.jfr");
    std::fs::write(&before, MIXED).unwrap();
    std::fs::write(&after, MIXED).unwrap();
    std::fs::set_permissions(&inputs, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = command()
        .args(["--memory-limit", "16M", "--diff", "-o", "html"])
        .arg(&before)
        .arg(&after)
        .current_dir(scratch.path())
        .output();
    std::fs::set_permissions(&inputs, std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = result.unwrap();
    assert_success(&result);
    assert!(scratch.path().join("after.diff.html").is_file());
    assert_eq!(std::fs::read(before).unwrap(), MIXED);
    assert_eq!(std::fs::read(after).unwrap(), MIXED);
    assert_no_scratch(scratch.path());
}

#[cfg(target_os = "linux")]
#[test]
fn restrictive_address_space_limit_exits_cleanly_instead_of_aborting() {
    let scratch = tempfile::tempdir().unwrap();
    let input = scratch.path().join("input.jfr");
    let output = scratch.path().join("output.collapsed");
    std::fs::write(&input, MIXED).unwrap();
    std::fs::write(&output, b"prior result\n").unwrap();
    let result = command()
        .args(["--max-rss", "4M", "--cpu", "-o", "collapsed"])
        .arg(input)
        .arg(&output)
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(75),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(output).unwrap(), b"prior result\n");
    assert_no_scratch(scratch.path());
}

#[test]
fn later_budget_failure_discards_earlier_staged_results_for_the_whole_worker() {
    let scratch = tempfile::tempdir().unwrap();
    let first = scratch.path().join("small.jfr");
    let second = scratch.path().join("large.collapsed");
    let output = scratch.path().join("outputs");
    std::fs::create_dir(&output).unwrap();
    std::fs::write(&first, MIXED).unwrap();
    let mut large = vec![b'x'; 1_000_000];
    large.extend_from_slice(b" 1\n");
    std::fs::write(&second, large).unwrap();
    let first_output = output.join("small.collapsed");
    let second_output = output.join("large.collapsed");
    std::fs::write(&first_output, b"prior first\n").unwrap();
    std::fs::write(&second_output, b"prior second\n").unwrap();
    let result = command()
        .args(["--memory-limit", "256K", "--cpu", "-o", "collapsed"])
        .arg(first)
        .arg(second)
        .arg(&output)
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(75),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("small.jfr ->"),
        "first file must have completed staging before the second fails: {stderr}"
    );
    assert_eq!(std::fs::read(first_output).unwrap(), b"prior first\n");
    assert_eq!(std::fs::read(second_output).unwrap(), b"prior second\n");
    assert_no_scratch(scratch.path());
}
