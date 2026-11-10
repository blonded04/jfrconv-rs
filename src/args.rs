// SPDX-License-Identifier: Apache-2.0
use crate::{model::ExportOptions, Result};
use chrono::{Duration, Local, NaiveDate, TimeZone};
use regex::Regex;

pub const HELP: &str = "jfrconv-rs — native converter targeting async-profiler 4.1
Usage: jfrconv [options] <input> [<input>...] [<output>]

  -o, --output FORMAT   html (default), collapsed, pprof, pb.gz, otlp, heatmap
      --cpu / --wall    Select CPU / wall-clock samples
      --alloc / --live  Select allocation / live object events
      --lock           Select monitor-enter and thread-park events
      --nativemem      Select native malloc/free events
      --leak           Retain outstanding native allocations only
      --tail RATIO     Ignore last fraction of leak allocations (default 0.1)
  -t, --threads        Group stacks by thread
  -s, --state LIST     Filter thread states (comma-separated prefixes)
      --total          Sum bytes/durations instead of sample counts
      --from TIME      Start: milliseconds relative to start/end or epoch
      --to TIME        End: milliseconds or local HH:mm:ss / ISO date-time
      --lines / --bci  Include source line / bytecode index in flame graphs
      --simple         Short class names
      --norm           Normalize hidden class and lambda names
      --dot            Dotted class names
      --classify       Group stacks into JVM activity categories
  -I, --include REGEX   Keep stacks containing a matching frame (full match)
  -X, --exclude REGEX   Remove stacks containing a matching frame (full match)
      --title TEXT     Flame graph title
      --minwidth N     Minimum frame width as percent of total
      --grain N        Coarsen samples by this factor
      --skip N         Skip N root frames
  -r, --reverse        Reverse stack direction, retaining thread grouping
  -i, --inverted       Toggle flame graph / icicle layout
      --highlight RE   Initial HTML search expression
      --diff           Compare two profiles (base and current)
      --memory-stats   Print live heap, reserved peak, and RSS after each file
      --memory-limit N Hard live allocation budget, e.g. 64M (worker is isolated)
      --max-rss N      Resident memory guard; see README for OS enforcement

Span extensions (in addition to the 4.1 feature set):
      --latency MS     Keep samples inside same-thread spans at least MS long
      --tag REGEX      Keep samples inside spans whose tag fully matches
      --trace          Convert profiler.Span / profiler.MethodTrace durations

      --version        Print version and compatibility target
  -h, --help           Show this help
";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Mode {
    Execution,
    Allocation,
    Live,
    Lock,
    Native,
    Trace,
}

pub struct Options {
    pub export: ExportOptions,
    pub output: Option<String>,
    pub files: Vec<String>,
    pub mode: Mode,
    pub cpu: bool,
    pub wall: bool,
    pub state: Option<Vec<String>>,
    pub threads: bool,
    pub classify: bool,
    pub total: bool,
    pub lines: bool,
    pub bci: bool,
    pub simple: bool,
    pub norm: bool,
    pub dot: bool,
    pub from: i64,
    pub to: i64,
    pub include: Option<Regex>,
    pub exclude: Option<Regex>,
    pub grain: f64,
    pub skip: usize,
    pub leak: bool,
    pub tail: f64,
    pub latency: Option<f64>,
    pub tag: Option<Regex>,
    pub help: bool,
    pub version: bool,
    pub memory_limit: Option<u64>,
    pub max_rss: Option<u64>,
    pub memory_stats: bool,
    pub diff: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            export: ExportOptions {
                title: "Flame Graph".into(),
                minwidth: 0.,
                reverse: false,
                inverted: false,
                highlight: None,
            },
            output: None,
            files: Vec::new(),
            mode: Mode::Execution,
            cpu: false,
            wall: false,
            state: None,
            threads: false,
            classify: false,
            total: false,
            lines: false,
            bci: false,
            simple: false,
            norm: false,
            dot: false,
            from: 0,
            to: 0,
            include: None,
            exclude: None,
            grain: 0.,
            skip: 0,
            leak: false,
            tail: 0.1,
            latency: None,
            tag: None,
            help: false,
            version: false,
            memory_limit: None,
            max_rss: None,
            memory_stats: false,
            diff: false,
        }
    }
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut o = Self::default();
        let mut args = args.into_iter();
        let mut literal = false;
        let mut alloc = false;
        let mut live = false;
        let mut lock = false;
        let mut native = false;
        let mut trace = false;
        while let Some(arg) = args.next() {
            if literal || !arg.starts_with('-') || arg == "-" {
                o.files.push(arg);
                continue;
            }
            if arg == "--" {
                literal = true;
                continue;
            }
            let mut value = || -> Result<String> {
                args.next()
                    .ok_or_else(|| format!("{arg} requires a value").into())
            };
            match arg.as_str() {
                "--help" | "-h" => o.help = true,
                "--version" => o.version = true,
                "--memory-limit" => o.memory_limit = Some(crate::memory::parse_size(&value()?)?),
                "--max-rss" => o.max_rss = Some(crate::memory::parse_size(&value()?)?),
                "--memory-stats" => o.memory_stats = true,
                "--diff" => o.diff = true,
                "--output" | "-o" => o.output = Some(value()?),
                "--cpu" => o.cpu = true,
                "--wall" => o.wall = true,
                "--alloc" => alloc = true,
                "--live" => live = true,
                "--lock" => lock = true,
                "--nativemem" => native = true,
                "--trace" => trace = true,
                "--threads" | "-t" => o.threads = true,
                "--state" | "-s" => {
                    o.state = Some(value()?.split(',').map(str::to_uppercase).collect())
                }
                "--total" => o.total = true,
                "--lines" => o.lines = true,
                "--bci" => o.bci = true,
                "--simple" => o.simple = true,
                "--norm" => o.norm = true,
                "--dot" => o.dot = true,
                "--classify" => o.classify = true,
                "--from" => o.from = timestamp(&value()?)?,
                "--to" => o.to = timestamp(&value()?)?,
                "--include" | "-I" => o.include = Some(pattern(&value()?)?),
                "--exclude" | "-X" => o.exclude = Some(pattern(&value()?)?),
                "--title" => o.export.title = value()?,
                "--highlight" => o.export.highlight = Some(value()?),
                "--minwidth" => o.export.minwidth = nonnegative(&value()?)?,
                "--grain" => o.grain = nonnegative(&value()?)?,
                "--skip" => o.skip = value()?.parse()?,
                "--reverse" | "-r" => o.export.reverse = true,
                "--inverted" | "-i" => o.export.inverted = true,
                "--leak" => o.leak = true,
                "--tail" => o.tail = nonnegative(&value()?)?,
                "--latency" => o.latency = Some(nonnegative(&value()?)?),
                "--tag" => o.tag = Some(pattern(&value()?)?),
                _ => return Err(format!("Unknown option {arg}; use --help").into()),
            }
        }
        o.mode = if native {
            Mode::Native
        } else if live {
            Mode::Live
        } else if alloc {
            Mode::Allocation
        } else if lock {
            Mode::Lock
        } else if trace {
            Mode::Trace
        } else {
            Mode::Execution
        };
        if o.tail > 1. {
            return Err("--tail must be between 0 and 1".into());
        }
        if o.leak && o.mode != Mode::Native {
            return Err("--leak requires --nativemem".into());
        }
        if o.mode != Mode::Execution && (o.cpu || o.wall || o.state.is_some()) {
            return Err("CPU/wall/thread-state filters require execution samples".into());
        }
        Ok(o)
    }
    pub fn span_filter(&self) -> bool {
        self.latency.is_some() || self.tag.is_some()
    }
    pub fn value_type(&self) -> &'static str {
        match self.mode {
            Mode::Native => "malloc",
            Mode::Allocation | Mode::Live => "allocations",
            Mode::Lock => "locks",
            Mode::Trace => "wall",
            _ => "cpu",
        }
    }
    pub fn total_unit(&self) -> &'static str {
        match self.mode {
            Mode::Native | Mode::Allocation | Mode::Live => "bytes",
            _ => "nanoseconds",
        }
    }
}

fn nonnegative(s: &str) -> Result<f64> {
    let n: f64 = if let Some(s) = s.strip_suffix('%') {
        s.parse::<f64>()? / 100.
    } else {
        s.parse()?
    };
    if !n.is_finite() || n < 0. {
        return Err("Expected a finite, nonnegative number".into());
    }
    Ok(n)
}

/// Java's CLI uses Matcher.matches(), not substring matching. Support Java's
/// common literal quoting but reject unsupported regex constructs explicitly.
pub fn pattern(s: &str) -> Result<Regex> {
    let mut rest = s;
    let mut expanded = String::new();
    while let Some(at) = rest.find("\\Q") {
        expanded.push_str(&rest[..at]);
        rest = &rest[at + 2..];
        if let Some(end) = rest.find("\\E") {
            expanded.push_str(&regex::escape(&rest[..end]));
            rest = &rest[end + 2..];
        } else {
            expanded.push_str(&regex::escape(rest));
            rest = "";
            break;
        }
    }
    expanded.push_str(rest);
    Regex::new(&format!("\\A(?:{expanded})\\z"))
        .map_err(|e| format!("Unsupported or invalid filter regex: {e}").into())
}

fn timestamp(s: &str) -> Result<i64> {
    if !s.contains(':') {
        return Ok(s.parse()?);
    }
    let (date, time) = if let Some((date, time)) = s.split_once('T') {
        (NaiveDate::parse_from_str(date, "%Y-%m-%d")?, time)
    } else {
        (Local::now().date_naive(), s)
    };
    let mut parts = time.split(':');
    let hour = parts.next().ok_or("Missing hour")?.parse()?;
    let minute = parts.next().unwrap_or("0").parse()?;
    let seconds = parts.next().unwrap_or("0");
    if parts.next().is_some() {
        return Err("Invalid timestamp".into());
    }
    let (second, millis) = seconds.split_once('.').unwrap_or((seconds, "0"));
    // GregorianCalendar in 4.1 treats .5 as five milliseconds, not 0.5s.
    let dt = date
        .and_hms_opt(hour, minute, second.parse()?)
        .ok_or("Invalid local time")?
        .checked_add_signed(Duration::milliseconds(millis.parse()?))
        .ok_or("Timestamp overflow")?;
    Local
        .from_local_datetime(&dt)
        .single()
        .map(|t| t.timestamp_millis())
        .ok_or_else(|| "Ambiguous or nonexistent local date-time; use epoch milliseconds".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_entire_frame_and_java_quotes() {
        let re = pattern(r"\Qdemo/Main.run\E").unwrap();
        assert!(re.is_match("demo/Main.run"));
        assert!(!re.is_match("Xdemo/Main.run"));
    }
    #[test]
    fn rejects_invalid_numbers_and_missing_values() {
        for args in [
            vec!["--grain", "NaN"],
            vec!["--tail", "2"],
            vec!["--title"],
            vec!["--leak"],
        ] {
            assert!(Options::parse(args.into_iter().map(String::from)).is_err());
        }
    }
    #[test]
    fn calendar_fraction_is_literal_milliseconds_like_v41() {
        let base = timestamp("2023-11-14T22:13:20").unwrap();
        assert_eq!(timestamp("2023-11-14T22:13:20.5").unwrap() - base, 5);
        assert_eq!(timestamp("2023-11-14T22:13:20.500").unwrap() - base, 500);
    }
}
