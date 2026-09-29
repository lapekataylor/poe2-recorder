// SPDX-License-Identifier: GPL-3.0-or-later

//! Follow a `Client.txt` and print what the recorder would do, without
//! recording anything. A development tool for checking detection against
//! the simulator or the real game.
//!
//! ```text
//! cargo run --manifest-path native/poe2-log/Cargo.toml --example watch -- <Client.txt> [options]
//!
//!   --from-start       read the existing lines too (default: only new ones)
//!   --grace <seconds>  grace period after leaving a map (default 60)
//!   --verbose          also print every line that did not parse
//! ```

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use poe2_log::tracker::{DEFAULT_GRACE_MS, MapAction, MapTracker, map_display_name};
use poe2_log::{LogEvent, parse_line};

const POLL_INTERVAL: Duration = Duration::from_millis(250);

struct Options {
    path: PathBuf,
    from_start: bool,
    grace_ms: i64,
    verbose: bool,
}

fn main() -> ExitCode {
    let options = match parse_arguments() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("usage: watch <Client.txt> [--from-start] [--grace <seconds>] [--verbose]");
            return ExitCode::FAILURE;
        }
    };
    let utc_offset_minutes = local_utc_offset_minutes();
    let mut tracker = MapTracker::new(options.grace_ms);
    let mut offset = if options.from_start {
        0
    } else {
        std::fs::metadata(&options.path).map_or(0, |metadata| metadata.len())
    };
    let mut pending = Vec::new();
    eprintln!(
        "Watching {} (grace {}s, UTC{:+}h). Ctrl+C to stop.",
        options.path.display(),
        options.grace_ms / 1_000,
        f64::from(utc_offset_minutes) / 60.0
    );

    loop {
        let length = std::fs::metadata(&options.path).map_or(0, |metadata| metadata.len());
        if length < offset {
            eprintln!("-- file got shorter; reading it again from the start");
            offset = 0;
            pending.clear();
        }
        if length > offset {
            match read_from(&options.path, offset) {
                Ok(bytes) => {
                    offset += bytes.len() as u64;
                    pending.extend_from_slice(&bytes);
                }
                Err(error) => eprintln!("-- read failed: {error}"),
            }
        }
        // Handle each complete line; keep a partly written one for later.
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line = String::from_utf8_lossy(&pending[..end]).into_owned();
            pending.drain(..=end);
            match parse_line(&line, utc_offset_minutes) {
                Some(parsed) => {
                    print_event(&parsed.event);
                    for action in tracker.handle(&parsed) {
                        print_action(&action);
                    }
                }
                None if options.verbose && !line.trim().is_empty() => {
                    println!("   ignored  {}", line.trim_end());
                }
                None => {}
            }
        }
        for action in tracker.tick(now_ms()) {
            print_action(&action);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn parse_arguments() -> Result<Options, String> {
    let mut arguments = std::env::args().skip(1);
    let mut path = None;
    let mut options = Options {
        path: PathBuf::new(),
        from_start: false,
        grace_ms: DEFAULT_GRACE_MS,
        verbose: false,
    };
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--from-start" => options.from_start = true,
            "--verbose" => options.verbose = true,
            "--grace" => {
                let seconds: i64 = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .filter(|seconds| *seconds >= 0)
                    .ok_or("--grace needs a number of seconds")?;
                options.grace_ms = seconds * 1_000;
            }
            _ if argument.starts_with("--") => return Err(format!("unknown option {argument}")),
            _ => path = Some(PathBuf::from(argument)),
        }
    }
    options.path = path.ok_or("which Client.txt should I watch?")?;
    if !options.path.is_file() {
        return Err(format!("{} is not a file", options.path.display()));
    }
    Ok(options)
}

fn read_from(path: &PathBuf, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn print_event(event: &LogEvent) {
    match event {
        LogEvent::AreaEntered {
            area_id,
            area_level,
            seed,
            kind,
        } => println!("   area     {area_id} (level {area_level}, seed {seed}, {kind:?})"),
        LogEvent::Slain { name } => println!("   death    {name}"),
    }
}

fn print_action(action: &MapAction) {
    match action {
        MapAction::Begin(start) => println!(
            ">> START recording: {} (level {})",
            map_display_name(&start.area_id),
            start.area_level
        ),
        MapAction::Complete(run) => {
            let seconds = run.duration_ms() / 1_000;
            let away: i64 = run
                .away
                .iter()
                .map(|away| away.returned_at_ms - away.left_at_ms)
                .sum::<i64>()
                / 1_000;
            println!(
                "<< STOP recording: {} took {}m{:02}s, {} death(s), {} portal trip(s) ({}s away)",
                map_display_name(&run.start.area_id),
                seconds / 60,
                seconds % 60,
                run.deaths.len(),
                run.away.len(),
                away
            );
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// The game writes local time. `date +%z` prints e.g. `+0200` on both Linux
/// and macOS, which avoids a time-zone dependency in this tool.
fn local_utc_offset_minutes() -> i32 {
    let output = Command::new("date").arg("+%z").output();
    let text = output
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_default();
    let text = text.trim();
    let (sign, digits) = match text.split_at_checked(1) {
        Some(("+", digits)) => (1, digits),
        Some(("-", digits)) => (-1, digits),
        _ => return 0,
    };
    let hours: i32 = digits
        .get(..2)
        .and_then(|hours| hours.parse().ok())
        .unwrap_or(0);
    let minutes: i32 = digits
        .get(2..4)
        .and_then(|minutes| minutes.parse().ok())
        .unwrap_or(0);
    sign * (hours * 60 + minutes)
}
