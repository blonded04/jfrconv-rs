// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
#![cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::process::{Command, Stdio};

fn counter(text: &str) -> u64 {
    text.lines()
        .map(|line| line.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap())
        .sum()
}
fn command(input: &std::path::Path, output: &str, limited: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jfrconv"));
    if limited {
        command.args(["--memory-limit", "16M"]);
    }
    command
        .args(["--cpu", "-o", "collapsed"])
        .arg(input)
        .arg(output);
    command
}
fn fixture(directory: &std::path::Path) -> std::path::PathBuf {
    let input = directory.join("input.jfr");
    std::fs::write(&input, include_bytes!("fixtures/mixed.jfr")).unwrap();
    input
}

#[test]
fn dev_null_succeeds_without_replacing_the_character_device() {
    // Never run a regression probe against a system device with root rights.
    // The task-owned FIFO test below covers stream publication for root CI.
    // SAFETY: geteuid has no pointer arguments and only observes this process.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIP direct /dev/null identity probe as root; task-owned FIFO is tested");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let input = fixture(scratch.path());
    let before = std::fs::metadata("/dev/null").unwrap();
    assert!(before.file_type().is_char_device());
    for limited in [false, true] {
        let result = command(&input, "/dev/null", limited).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let after = std::fs::metadata("/dev/null").unwrap();
        assert!(after.file_type().is_char_device());
        assert_eq!(
            (after.dev(), after.ino(), after.rdev()),
            (before.dev(), before.ino(), before.rdev())
        );
    }
}

#[test]
fn stdout_descriptor_aliases_preserve_identity_and_existing_offset() {
    // A regression must not be able to replace /dev/stdout as root.
    // SAFETY: geteuid only observes this process and accepts no pointers.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIP system descriptor aliases as root; task-owned FIFO is tested");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let input = fixture(scratch.path());
    let mut aliases = vec!["/dev/stdout", "/dev/fd/1"];
    if cfg!(target_os = "linux") {
        aliases.push("/proc/self/fd/1");
    }
    for alias in aliases {
        let identity = std::fs::symlink_metadata(alias).unwrap();
        for limited in [false, true] {
            let path =
                scratch
                    .path()
                    .join(format!("capture-{}-{}", alias.replace('/', "_"), limited));
            let mut file = std::fs::File::create(&path).unwrap();
            use std::io::Write;
            file.write_all(b"PREFIX\n").unwrap();
            let result = command(&input, alias, limited)
                .stdout(Stdio::from(file))
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{alias}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                text.starts_with("PREFIX\n"),
                "descriptor offset reset: {alias}: {text}"
            );
            assert_eq!(counter(text.strip_prefix("PREFIX\n").unwrap()), 6);
            let after = std::fs::symlink_metadata(alias).unwrap();
            assert_eq!(
                (after.dev(), after.ino(), after.file_type()),
                (identity.dev(), identity.ino(), identity.file_type())
            );
        }
    }
}

#[test]
fn task_owned_fifo_is_streamed_without_replacing_its_inode() {
    let scratch = tempfile::tempdir().unwrap();
    let input = fixture(scratch.path());
    for limited in [false, true] {
        let path = scratch.path().join(format!("output-{limited}.fifo"));
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated task-owned path and mode is valid.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let before = std::fs::metadata(&path).unwrap();
        assert!(before.file_type().is_fifo());
        let mut reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        // The fixture is below PIPE_BUF, so a complete writer can finish before
        // the test reads. Nonblocking reader avoids hanging on a failed writer.
        let result = command(&input, path.to_str().unwrap(), limited)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let after = std::fs::metadata(&path).unwrap();
        assert!(after.file_type().is_fifo());
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        let mut text = String::new();
        use std::io::Read;
        reader.read_to_string(&mut text).unwrap();
        assert_eq!(counter(&text), 6);
    }
}
