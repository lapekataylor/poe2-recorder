// SPDX-License-Identifier: GPL-3.0-or-later

//! Path of Exile map runs as recording drafts.
//!
//! `Poe2Source` follows one game's `<logs>/Client.txt` from its end, feeds each line to
//! the `poe2_log` map tracker, and turns the tracker's begin/complete
//! decisions into `RecordingDraft`s for the coordinator to record, overrun
//! and finalize. Path of Exile 1 and 2 write the same log lines, so each
//! enabled game gets its own source, tagged with its `GameFlavor`.
//!
//! Notes:
//! - The game only appends to `Client.txt`; a shorter file or a different
//!   inode means it was cleared or replaced, and reading starts again from
//!   the top.
//! - A completed run ends when the player left the map, so the capture keeps
//!   up to one grace period of hideout footage after it.
//! - Opening mid-map picks the run up: the end of the log is replayed, and a
//!   player still inside a map begins recording at once. One already back in
//!   the hideout is not recorded until they return to the map.

use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use poe2_log::parse_line;
use poe2_log::tracker::{MapAction, MapRun, MapStart, MapTracker, map_display_name};

use crate::domain::{
    ActivityDetails, Category, GameFlavor, Outcome, PlayerSummary, RecordingDraft, RecordingId,
    TimelineItem, TimelineKind,
};

pub const CLIENT_LOG: &str = "Client.txt";
/// Bounds one poll's read; the rest waits for the next poll.
const READ_CHUNK_BYTES: u64 = 1024 * 1024;
/// A line longer than this is not one the tracker cares about.
const MAX_LINE_BYTES: usize = 64 * 1024;
/// How far back from the end of the log opening looks for a run in progress.
const RESUME_TAIL_BYTES: u64 = 2 * 1024 * 1024;
/// Path of Exile 2 writes nothing for up to ~10 minutes of normal play in a
/// map. A log quiet for longer means the game is closed or has crashed.
pub const QUIET_LOG_MS: i64 = 20 * 60_000;

#[derive(Debug)]
pub enum Poe2Action {
    Begin(Box<RecordingDraft>),
    /// The finished draft for the run that began with the same id.
    Complete(Box<RecordingDraft>),
}

#[derive(Debug)]
pub struct Poe2Source {
    flavor: GameFlavor,
    path: PathBuf,
    offset: u64,
    /// Identifies the file `offset` belongs to.
    inode: Option<u64>,
    pending: Vec<u8>,
    utc_offset_minutes: i32,
    tracker: MapTracker,
    run_id: Option<RecordingId>,
    /// Wall-clock time new bytes last arrived.
    last_read_ms: Option<i64>,
    /// A run found in progress on opening, handed out by the first poll.
    resumed: Option<Box<RecordingDraft>>,
}

impl Poe2Source {
    /// Follow `Client.txt` from its end, picking up a map the player is in
    /// right now. The file may not exist yet on a fresh install.
    pub fn open(
        flavor: GameFlavor,
        log_dir: &Path,
        utc_offset_minutes: i32,
        grace_ms: i64,
        now_ms: i64,
    ) -> io::Result<Self> {
        if !std::fs::metadata(log_dir)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a folder", log_dir.display()),
            ));
        }
        let path = log_dir.join(CLIENT_LOG);
        let metadata = std::fs::metadata(&path).ok();
        let mut source = Self {
            flavor,
            path,
            offset: metadata.as_ref().map_or(0, std::fs::Metadata::len),
            inode: metadata.as_ref().map(MetadataExt::ino),
            pending: Vec::new(),
            utc_offset_minutes,
            tracker: MapTracker::new(grace_ms),
            run_id: None,
            last_read_ms: None,
            resumed: None,
        };
        if let Some(metadata) = &metadata {
            source.resume(metadata, now_ms);
        }
        Ok(source)
    }

    /// Replay the end of a recently written log to find a run in progress.
    fn resume(&mut self, metadata: &Metadata, now_ms: i64) {
        let Some(modified_ms) = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_millis() as i64)
        else {
            return;
        };
        if now_ms - modified_ms > QUIET_LOG_MS {
            return;
        }
        let Ok(tail) = read_tail(&self.path, metadata.len()) else {
            return;
        };
        for raw in tail.split(|byte| *byte == b'\n') {
            if let Some(parsed) = parse_line(&String::from_utf8_lossy(raw), self.utc_offset_minutes)
            {
                // Runs that began and ended before now are history.
                let _ = self.tracker.handle(&parsed);
            }
        }
        let _ = self.tracker.tick(now_ms);
        match self.tracker.current() {
            Some((start, true)) => {
                let id = RecordingId::new();
                self.run_id = Some(id.clone());
                self.resumed = Some(Box::new(begin_draft(id, &self.flavor, start)));
                self.last_read_ms = Some(modified_ms);
            }
            // Recording the hideout now would show nothing of the map; the
            // run begins again when the player goes back in.
            Some((_, false)) => {
                let _ = self.tracker.force_end(now_ms);
            }
            None => {}
        }
    }

    pub fn flavor(&self) -> &GameFlavor {
        &self.flavor
    }

    /// When the game last wrote to `Client.txt` while this source watched it.
    pub fn last_read_ms(&self) -> Option<i64> {
        self.last_read_ms
    }

    pub fn is_running(&self) -> bool {
        self.run_id.is_some()
    }

    /// Read what the game appended, then let an expired grace period end the
    /// run at `now_ms`.
    pub fn poll(&mut self, now_ms: i64) -> io::Result<Vec<Poe2Action>> {
        let mut actions: Vec<Poe2Action> = self
            .resumed
            .take()
            .map(Poe2Action::Begin)
            .into_iter()
            .collect();
        let read = self.read_new(now_ms);
        let complete = self
            .pending
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |end| end + 1);
        let lines: Vec<u8> = self.pending.drain(..complete).collect();
        for raw in lines.split(|byte| *byte == b'\n') {
            if raw.len() > MAX_LINE_BYTES {
                continue;
            }
            let Some(parsed) = parse_line(&String::from_utf8_lossy(raw), self.utc_offset_minutes)
            else {
                continue;
            };
            for action in self.tracker.handle(&parsed) {
                actions.push(self.convert(action));
            }
        }
        if self.pending.len() > MAX_LINE_BYTES {
            self.pending.clear();
        }
        for action in self.tracker.tick(now_ms) {
            actions.push(self.convert(action));
        }
        read.map(|()| actions)
    }

    /// End any run now: the user stopped it, or the app is quitting.
    pub fn force_end(&mut self, now_ms: i64) -> Vec<Poe2Action> {
        self.tracker
            .force_end(now_ms)
            .into_iter()
            .map(|action| self.convert(action))
            .collect()
    }

    /// Forget the current run without producing anything, e.g. when its
    /// capture could not start.
    pub fn drop_run(&mut self, now_ms: i64) {
        let _ = self.tracker.force_end(now_ms);
        self.run_id = None;
    }

    fn read_new(&mut self, now_ms: i64) -> io::Result<()> {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            // Not created yet: nothing to read.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let length = metadata.len();
        if length < self.offset || self.inode.is_some_and(|inode| inode != metadata.ino()) {
            self.offset = 0;
            self.pending.clear();
        }
        self.inode = Some(metadata.ino());
        if length == self.offset {
            return Ok(());
        }
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let read = file.take(READ_CHUNK_BYTES).read_to_end(&mut self.pending)?;
        self.offset += read as u64;
        if read > 0 {
            self.last_read_ms = Some(now_ms);
        }
        Ok(())
    }

    fn convert(&mut self, action: MapAction) -> Poe2Action {
        match action {
            MapAction::Begin(start) => {
                let id = RecordingId::new();
                self.run_id = Some(id.clone());
                Poe2Action::Begin(Box::new(begin_draft(id, &self.flavor, &start)))
            }
            MapAction::Complete(run) => {
                let id = self.run_id.take().unwrap_or_default();
                Poe2Action::Complete(Box::new(finished_draft(id, &self.flavor, &run)))
            }
        }
    }
}

/// The whole lines in the last `RESUME_TAIL_BYTES` of a `length`-byte file.
fn read_tail(path: &Path, length: u64) -> io::Result<Vec<u8>> {
    let start = length.saturating_sub(RESUME_TAIL_BYTES);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    file.take(length - start).read_to_end(&mut tail)?;
    if start > 0 {
        // The first line was cut in half.
        let first = tail
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(tail.len(), |end| end + 1);
        tail.drain(..first);
    }
    Ok(tail)
}

fn details(start: &MapStart, run: Option<&MapRun>) -> ActivityDetails {
    ActivityDetails::MapRun {
        area_id: start.area_id.clone(),
        map_name: map_display_name(&start.area_id),
        area_level: start.area_level,
        seed: start.seed,
        deaths: run.map_or(0, |run| run.deaths.len() as u32),
        portal_trips: run.map_or(0, |run| run.away.len() as u32),
        away_ms: run.map_or(0, |run| {
            run.away
                .iter()
                .map(|away| (away.returned_at_ms - away.left_at_ms).max(0) as u64)
                .sum()
        }),
    }
}

fn begin_draft(id: RecordingId, flavor: &GameFlavor, start: &MapStart) -> RecordingDraft {
    RecordingDraft {
        id,
        category: Category::MapRuns,
        flavor: flavor.clone(),
        started_at_ms: start.started_at_ms,
        overrun_ms: 0,
        details: details(start, None),
        player: None,
        timeline: Vec::new(),
        outcome: None,
        ended_at_ms: None,
        duration_ms: None,
        title: Some(map_display_name(&start.area_id)),
    }
}

fn finished_draft(id: RecordingId, flavor: &GameFlavor, run: &MapRun) -> RecordingDraft {
    let start_ms = run.start.started_at_ms;
    let offset = |at_ms: i64| (at_ms - start_ms).max(0) as u64;
    let mut timeline: Vec<TimelineItem> = run
        .deaths
        .iter()
        .map(|death| {
            TimelineItem::point(
                TimelineKind::Death,
                offset(death.at_ms),
                Some(death.name.clone()),
                None,
                None,
            )
        })
        .collect();
    timeline.extend(run.away.iter().filter_map(|away| {
        TimelineItem::span(
            TimelineKind::Activity,
            offset(away.left_at_ms),
            offset(away.returned_at_ms),
            Some("Out of the map".to_owned()),
            None,
            None,
        )
        .ok()
    }));
    timeline.sort_by_key(TimelineItem::start_ms);

    RecordingDraft {
        details: details(&run.start, Some(run)),
        player: most_deaths(run).map(|name| PlayerSummary { name }),
        timeline,
        outcome: Some(Outcome::Complete),
        ended_at_ms: Some(run.ended_at_ms),
        duration_ms: Some(run.duration_ms().max(0) as u64),
        ..begin_draft(id, flavor, &run.start)
    }
}

/// The character that died most in the run: the log names no player
/// otherwise, and "own deaths" markers need one. First to die wins a tie.
fn most_deaths(run: &MapRun) -> Option<String> {
    let mut best: Option<(&str, usize)> = None;
    for death in &run.deaths {
        let count = run
            .deaths
            .iter()
            .filter(|other| other.name == death.name)
            .count();
        if best.is_none_or(|(_, most)| count > most) {
            best = Some((&death.name, count));
        }
    }
    best.map(|(name, _)| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const GRACE_MS: i64 = 60_000;
    /// An opening time long after any test log was written: nothing resumes.
    const LOG_LONG_QUIET: i64 = i64::MAX / 2;

    fn line(time: &str, text: &str) -> String {
        format!("2026/09/28 {time} 1000 abcd1234 [INFO Client 312] {text}\r\n")
    }

    fn area(time: &str, area_id: &str, seed: u64) -> String {
        format!(
            "2026/09/28 {time} 1000 2caa229f [DEBUG Client 312] Generating level 80 area \"{area_id}\" with seed {seed}\r\n"
        )
    }

    fn at(time: &str) -> i64 {
        poe2_log::parse_line(&area(time, "HideoutShoreline", 1), 0)
            .expect("valid line")
            .occurred_at_ms
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("poe2-source-{name}-{}", RecordingId::new()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        /// Pretend the game last wrote the log at `at_ms`.
        fn touch(&self, at_ms: i64) {
            let file = File::options()
                .append(true)
                .open(self.0.join(CLIENT_LOG))
                .expect("open log");
            file.set_modified(UNIX_EPOCH + std::time::Duration::from_millis(at_ms as u64))
                .expect("set mtime");
        }

        fn append(&self, text: &str) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.0.join(CLIENT_LOG))
                .expect("open log");
            file.write_all(text.as_bytes()).expect("append");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_map_run_becomes_a_begin_and_a_finished_draft() {
        let dir = TempDir::new("run");
        dir.append(&area("10:00:00", "MapOld", 5));
        let mut source =
            Poe2Source::open(GameFlavor::Poe2, &dir.0, 0, GRACE_MS, LOG_LONG_QUIET).expect("open");

        dir.append(&area("12:00:00", "MapHiddenGrotto", 7));
        dir.append(&line("12:01:00", ": TestExile has been slain."));
        dir.append(&area("12:02:00", "HideoutShoreline", 1));
        dir.append(&area("12:02:30", "MapHiddenGrotto", 7));
        dir.append(&area("12:05:00", "HideoutShoreline", 1));
        let actions = source.poll(at("12:05:00")).expect("poll");
        let [Poe2Action::Begin(begin)] = actions.as_slice() else {
            panic!("expected one begin, got {actions:?}");
        };
        assert_eq!(begin.title.as_deref(), Some("Hidden Grotto"));
        assert_eq!(begin.category, Category::MapRuns);
        assert!(source.is_running());

        let actions = source.poll(at("12:05:00") + GRACE_MS).expect("poll");
        let [Poe2Action::Complete(done)] = actions.as_slice() else {
            panic!("expected one completion, got {actions:?}");
        };
        assert_eq!(done.id, begin.id);
        assert_eq!(done.duration_ms, Some(300_000));
        assert_eq!(
            done.player.as_ref().map(|player| player.name.as_str()),
            Some("TestExile")
        );
        assert_eq!(done.timeline.len(), 2);
        assert_eq!(done.timeline[0].kind(), &TimelineKind::Death);
        assert_eq!(done.timeline[0].start_ms(), 60_000);
        assert_eq!(done.timeline[1].start_ms(), 120_000);
        assert_eq!(done.timeline[1].end_ms(), Some(150_000));
        assert!(matches!(
            done.details,
            ActivityDetails::MapRun {
                deaths: 1,
                portal_trips: 1,
                away_ms: 30_000,
                ..
            }
        ));
        assert!(!source.is_running());
    }

    #[test]
    fn a_path_of_exile_1_run_is_tagged_with_its_game() {
        let dir = TempDir::new("poe1");
        let mut source =
            Poe2Source::open(GameFlavor::Poe1, &dir.0, 0, GRACE_MS, LOG_LONG_QUIET).expect("open");

        dir.append(&area("21:46:23", "MapWorldsOrchard", 2_990_929_181));
        dir.append(&line("21:51:21", ": FortunaRFtwo has been slain."));
        dir.append(&area("21:51:22", "2_11_endgame_town", 1));
        dir.append(&area("21:51:31", "MapWorldsOrchard", 2_990_929_181));
        dir.append(&area("21:52:57", "2_11_endgame_town", 1));
        dir.append(&area("21:53:08", "HideoutRuinedTemple", 1));
        let actions = source.poll(at("21:53:08") + GRACE_MS).expect("poll");
        let [Poe2Action::Begin(begin), Poe2Action::Complete(done)] = actions.as_slice() else {
            panic!("expected a begin and a completion, got {actions:?}");
        };
        assert_eq!(begin.flavor, GameFlavor::Poe1);
        assert_eq!(done.flavor, GameFlavor::Poe1);
        assert_eq!(done.title.as_deref(), Some("Orchard"));
        assert!(matches!(
            done.details,
            ActivityDetails::MapRun {
                deaths: 1,
                portal_trips: 1,
                ..
            }
        ));
    }

    #[test]
    fn a_partly_written_line_waits_for_its_end() {
        let dir = TempDir::new("partial");
        let mut source =
            Poe2Source::open(GameFlavor::Poe2, &dir.0, 0, GRACE_MS, LOG_LONG_QUIET).expect("open");
        let text = area("12:00:00", "MapBluff", 7);
        let (first, rest) = text.split_at(30);
        dir.append(first);
        assert!(source.poll(at("12:00:00")).expect("poll").is_empty());
        dir.append(rest);
        assert!(matches!(
            source.poll(at("12:00:00")).expect("poll").as_slice(),
            [Poe2Action::Begin(_)]
        ));
    }

    #[test]
    fn a_replaced_log_is_read_from_the_top() {
        let dir = TempDir::new("replaced");
        dir.append(&line("09:00:00", "a long line from an earlier session"));
        let mut source =
            Poe2Source::open(GameFlavor::Poe2, &dir.0, 0, GRACE_MS, LOG_LONG_QUIET).expect("open");
        // Longer than what was read, so only the new inode gives it away.
        let fresh = dir.0.join("Client.new");
        std::fs::write(&fresh, area("12:00:00", "MapBluff", 7)).expect("write");
        std::fs::rename(&fresh, dir.0.join(CLIENT_LOG)).expect("replace");
        assert!(matches!(
            source.poll(at("12:00:00")).expect("poll").as_slice(),
            [Poe2Action::Begin(_)]
        ));
    }

    #[test]
    fn opening_mid_map_begins_recording_at_once() {
        let dir = TempDir::new("resume");
        dir.append(&area("11:00:00", "MapOld", 5));
        dir.append(&area("11:10:00", "HideoutShoreline", 1));
        dir.append(&area("12:00:00", "MapHiddenGrotto", 7));
        dir.append(&line("12:03:00", ": TestExile has been slain."));
        dir.touch(at("12:03:00"));
        let mut source =
            Poe2Source::open(GameFlavor::Poe2, &dir.0, 0, GRACE_MS, at("12:05:00")).expect("open");
        assert!(source.is_running());
        assert_eq!(source.last_read_ms(), Some(at("12:03:00")));

        let actions = source.poll(at("12:05:00")).expect("poll");
        let [Poe2Action::Begin(begin)] = actions.as_slice() else {
            panic!("expected one begin, got {actions:?}");
        };
        assert_eq!(begin.title.as_deref(), Some("Hidden Grotto"));
        assert_eq!(begin.started_at_ms, at("12:00:00"));

        // The run carries on: leaving and waiting out the grace completes it.
        dir.append(&area("12:06:00", "HideoutShoreline", 1));
        assert!(source.poll(at("12:06:00")).expect("poll").is_empty());
        let actions = source.poll(at("12:06:00") + GRACE_MS).expect("poll");
        let [Poe2Action::Complete(done)] = actions.as_slice() else {
            panic!("expected one completion, got {actions:?}");
        };
        assert_eq!(done.id, begin.id);
        assert_eq!(done.duration_ms, Some(360_000));
        // The death before opening is still the run's.
        assert_eq!(done.timeline[0].start_ms(), 180_000);
    }

    #[test]
    fn opening_in_the_hideout_waits_for_the_map() {
        let dir = TempDir::new("resume-away");
        dir.append(&area("12:00:00", "MapBluff", 7));
        dir.append(&area("12:04:00", "HideoutShoreline", 1));
        dir.touch(at("12:04:00"));
        let mut source =
            Poe2Source::open(GameFlavor::Poe2, &dir.0, 0, GRACE_MS, at("12:04:30")).expect("open");
        assert!(!source.is_running());
        assert!(source.poll(at("12:04:30")).expect("poll").is_empty());

        dir.append(&area("12:04:40", "MapBluff", 7));
        let actions = source.poll(at("12:04:40")).expect("poll");
        let [Poe2Action::Begin(begin)] = actions.as_slice() else {
            panic!("expected one begin, got {actions:?}");
        };
        assert_eq!(begin.started_at_ms, at("12:04:40"));
    }

    #[test]
    fn a_quiet_log_is_not_resumed() {
        let dir = TempDir::new("resume-quiet");
        dir.append(&area("12:00:00", "MapBluff", 7));
        dir.touch(at("12:00:00"));
        let mut source = Poe2Source::open(
            GameFlavor::Poe2,
            &dir.0,
            0,
            GRACE_MS,
            at("12:00:00") + QUIET_LOG_MS + 1,
        )
        .expect("open");
        assert!(!source.is_running());
        assert!(source.poll(at("12:30:00")).expect("poll").is_empty());
    }

    #[test]
    fn opening_a_missing_folder_fails() {
        assert!(
            Poe2Source::open(
                GameFlavor::Poe2,
                Path::new("/nonexistent/poe2/logs"),
                0,
                GRACE_MS,
                0
            )
            .is_err()
        );
    }
}
