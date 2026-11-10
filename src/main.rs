// SPDX-License-Identifier: Apache-2.0
mod args;
mod convert;
mod export;
mod heatmap;
mod input;
mod jfr;
mod leak_sort;
mod memory;
mod model;
mod proto;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

use args::Options;
use flate2::write::GzEncoder;
use flate2::Compression;
use model::Profile;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

fn is_jfr(path: &Path) -> Result<bool> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    let count = f.read(&mut magic)?;
    if path.extension().is_some_and(|s| s == "jfr") {
        return Ok(true);
    }
    Ok(count == 4 && magic == *b"FLR\0")
}

fn run() -> Result<()> {
    let mut options = Options::parse(std::env::args().skip(1))?;
    if options.version {
        println!(
            "jfrconv-rs {} (async-profiler 4.1 compatibility target; span extensions)",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    if options.help || options.files.is_empty() {
        print!("{}", args::HELP);
        return Ok(());
    }
    if options.files.len() == if options.diff { 2 } else { 1 } {
        options.files.push(".".into());
    }
    let target = PathBuf::from(options.files.pop().unwrap());
    let directory = target.is_dir();
    if options.files.len() > 1 && !directory && !options.diff {
        return Err("For multiple inputs, output must be an existing directory (each input is converted separately)".into());
    }
    if options.output.is_none() {
        options.output = Some(if directory {
            "html".into()
        } else {
            target
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("html")
                .to_owned()
        });
    }
    let format = options.output.as_deref().unwrap();
    if ![
        "html",
        "collapsed",
        "pprof",
        "pb",
        "pb.gz",
        "gz",
        "otlp",
        "heatmap",
    ]
    .contains(&format)
    {
        return Err(format!("Unknown output format {format}").into());
    }
    let mut files = options.files.clone();
    let mut base = None;
    if options.diff {
        if files.len() != 2 || !matches!(format, "html" | "collapsed") {
            return Err("--diff requires two inputs and html or collapsed output".into());
        }
        options.norm = true;
        let input = files.remove(0);
        let path = Path::new(&input);
        if std::fs::canonicalize(path).ok() == std::fs::canonicalize(&target).ok()
            && target.exists()
            && !directory
        {
            return Err("Input and output must be different files".into());
        }
        let mut profile = Profile::new(
            options.value_type(),
            if options.total {
                options.total_unit()
            } else {
                "count"
            },
            options.total_unit(),
        );
        if is_jfr(path)? {
            convert::jfr(path, &options, &mut profile, None)?;
        } else {
            input::read(path, &options, &mut profile)?;
        }
        base = Some(profile);
    }
    for file in files {
        let path = Path::new(&file);
        let extension = if options.diff {
            format!("diff.{format}")
        } else if format == "heatmap" {
            "html".to_owned()
        } else if format == "gz" {
            "pb.gz".to_owned()
        } else {
            format.to_owned()
        };
        let output = if directory {
            target
                .join(path.file_name().ok_or("Invalid input filename")?)
                .with_extension(extension)
        } else {
            target.clone()
        };
        if std::fs::canonicalize(path).ok() == std::fs::canonicalize(&output).ok()
            && output.exists()
        {
            return Err("Input and output must be different files".into());
        }
        let mut profile = Profile::new(
            options.value_type(),
            if options.total {
                options.total_unit()
            } else {
                "count"
            },
            options.total_unit(),
        );
        let mut spool = if matches!(format, "heatmap" | "otlp") {
            Some(tempfile::tempfile()?)
        } else {
            None
        };
        if is_jfr(path)? {
            convert::jfr(path, &options, &mut profile, spool.as_mut())?;
        } else {
            if !matches!(format, "html" | "collapsed") {
                return Err("HTML/collapsed inputs support HTML or collapsed output".into());
            }
            input::read(path, &options, &mut profile)?;
        }
        if let Some(original) = base.as_ref() {
            profile.diff(original);
        }
        drop(base.take());
        profile.seal();
        // Atomic replacement protects an existing output on parse or write errors.
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(memory::output_temp_dir(parent, &output))?;
        {
            let mut out = BufWriter::with_capacity(128 * 1024, temp.as_file_mut());
            match format {
                "html" => export::html(&profile, &mut out, &options.export)?,
                "collapsed" => export::collapsed(&profile, &mut out, &options.export)?,
                "pprof" | "pb" => export::pprof(&profile, &mut out, &options.export)?,
                "pb.gz" | "gz" => {
                    // Buffer protobuf's small fields before compression too;
                    // invoking a compressor for every varint is expensive.
                    let gz = GzEncoder::new(&mut out, Compression::default());
                    let mut buffered = BufWriter::with_capacity(128 * 1024, gz);
                    export::pprof(&profile, &mut buffered, &options.export)?;
                    buffered.flush()?;
                    let gz = buffered.into_inner().map_err(|e| e.into_error())?;
                    gz.finish()?;
                }
                "otlp" => export::otlp_spooled(
                    &profile,
                    spool.as_mut().unwrap(),
                    &mut out,
                    &options.export,
                )?,
                "heatmap" => {
                    heatmap::heatmap(&profile, spool.as_mut().unwrap(), &mut out, &options.export)?
                }
                _ => unreachable!(),
            }
            out.flush()?;
        }
        memory::publish_output(temp, &output)?;
        eprintln!(
            "Converted {} -> {} ({} samples/units)",
            path.display(),
            output.display(),
            profile.nodes[0].total
        );
        drop(profile);
        drop(spool);
        if options.memory_stats {
            let (live, peak) = memory::allocation_bytes();
            eprintln!(
                "memory: live={live} peak_reserved={peak} rss={}",
                memory::resident_bytes(std::process::id()).unwrap_or(0)
            );
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = memory::configure_worker() {
        eprintln!("jfrconv: {error}");
        std::process::exit(1);
    }
    match memory::supervise() {
        Ok(Some(code)) => std::process::exit(code),
        Ok(None) => {}
        Err(error) => {
            eprintln!("jfrconv: {error}");
            std::process::exit(1);
        }
    }
    if let Err(error) = run() {
        eprintln!("jfrconv: {error}");
        std::process::exit(1);
    }
}
