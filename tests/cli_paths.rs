// Copyright 2026 jfrconv-rs contributors
// SPDX-License-Identifier: Apache-2.0
use std::process::Command;

#[test]
fn preserves_dotted_input_names_in_multi_file_output_directories() {
    let scratch = tempfile::tempdir().unwrap();
    let first = scratch.path().join("profile.cpu.jfr");
    let second = scratch.path().join("profile.wall.jfr");
    let raw = include_bytes!("fixtures/mixed.jfr");
    std::fs::write(&first, raw).unwrap();
    std::fs::write(&second, raw).unwrap();
    for limited in [false, true] {
        let directory = scratch
            .path()
            .join(if limited { "limited" } else { "ordinary" });
        std::fs::create_dir(&directory).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_jfrconv"));
        if limited {
            command.args(["--memory-limit", "16M"]);
        }
        let result = command
            .args(["--cpu", "-o", "collapsed"])
            .arg(&first)
            .arg(&second)
            .arg(&directory)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut names: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["profile.cpu.collapsed", "profile.wall.collapsed"]);
        for name in names {
            let text = std::fs::read_to_string(directory.join(name)).unwrap();
            let count: u64 = text
                .lines()
                .map(|line| line.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap())
                .sum();
            assert_eq!(count, 6);
        }
    }
}
