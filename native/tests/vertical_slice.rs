// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless vertical slice: config, `Client.txt` polling, map-run detection,
//! recorder control, storage, and media jobs behind the real coordinator.
//!
//! The recorder and FFmpeg are replaced by shell fakes; every
//! other type is the production one operating on a temp directory. The
//! coordinator core is stepped with `tick()`, so no test sleeps or timing
//! assertions are needed.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use poe_recorder::config::{
    ActivitySettings, AuthorizedPath, CaptureSettings, Config, FlavorConfig, LayoutSettings,
    ManualSettings, StorageSettings,
};
use poe_recorder::coordinator::{AppSnapshot, ClipRange, Command, Coordinator, Setup, start};
use poe_recorder::domain::{
    ActivityDetails, Category, Outcome, RecorderStatus, StorageLimit, TimelineKind,
};
use poe_recorder::media_jobs::MediaConfig;
use poe_recorder::recorder::Timeouts;
use poe_recorder::storage::{RECOVERY_DIR, now_unix_ms};

const PLAYER_NAME: &str = "TestExile";
/// Long enough for real process spawns and FFmpeg fakes, short enough to fail
/// fast. Nothing is asserted about how long a step actually takes.
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

// --- Harness ---

struct Harness {
    root: PathBuf,
    library: PathBuf,
    capture_root: PathBuf,
    log_file: PathBuf,
    coordinator: Coordinator,
    commands: SyncSender<Command>,
    snapshots: Receiver<Arc<AppSnapshot>>,
    latest: Arc<AppSnapshot>,
    replay_index: u32,
}

impl Harness {
    fn new(name: &str) -> Self {
        let (root, library, capture_root, log_file) = spawn_tree(name);
        Self::attach(root, library, capture_root, log_file)
    }

    /// Build a coordinator over an existing directory tree.
    fn attach(root: PathBuf, library: PathBuf, capture_root: PathBuf, log_file: PathBuf) -> Self {
        let (commands, commands_rx) = mpsc::sync_channel(64);
        let (snapshot_tx, snapshots) = mpsc::sync_channel(1);
        let mut coordinator =
            Coordinator::new(setup(&root), commands_rx, snapshot_tx, Box::new(|| {}));
        coordinator.startup();
        let mut harness = Self {
            root,
            library,
            capture_root,
            log_file,
            coordinator,
            commands,
            snapshots,
            latest: Arc::new(empty_snapshot()),
            replay_index: 0,
        };
        // Startup publishes the scanned library before it arms, so settle on
        // the post-arm snapshot rather than the first one out.
        harness.pump(|snapshot| {
            !matches!(
                snapshot.status,
                RecorderStatus::SetupRequired | RecorderStatus::WaitingForCapture
            )
        });
        harness
    }

    /// Step the coordinator until the condition holds. Panics on timeout so a
    /// broken flow fails loudly instead of asserting on a stale snapshot.
    fn pump(&mut self, mut done: impl FnMut(&AppSnapshot) -> bool) {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            self.coordinator.tick();
            while let Ok(snapshot) = self.snapshots.try_recv() {
                self.latest = snapshot;
            }
            if done(&self.latest) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out; status {:?}, problems {:?}",
                self.latest.status,
                self.latest.problems
            );
        }
    }

    /// Tear the coordinator down and rebuild over the same tree.
    fn restart(self) -> Self {
        let (root, library, capture_root, log_file) = (
            self.root.clone(),
            self.library.clone(),
            self.capture_root.clone(),
            self.log_file.clone(),
        );
        drop(self);
        Self::attach(root, library, capture_root, log_file)
    }

    fn send(&self, command: Command) {
        assert!(
            self.commands.try_send(command).is_ok(),
            "command queue full"
        );
    }

    fn log(&self, lines: &[String]) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&self.log_file)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    /// Stand in for gpu-screen-recorder saving its two artifacts and calling
    /// the `-sc` hook.
    fn emit_artifacts(&mut self, replay: bool) {
        self.replay_index += 1;
        let index = self.replay_index;
        let events = self.root.join("recorder/gsr-events.tsv");
        let mut file = fs::OpenOptions::new().append(true).open(&events).unwrap();
        if replay {
            let path = self.capture_root.join(format!("replay/Replay_{index}.mkv"));
            fs::write(&path, b"replay media").unwrap();
            writeln!(file, "{}\treplay\t{}", now_unix_ms(), path.display()).unwrap();
        }
        let path = self.capture_root.join(format!("regular/Video_{index}.mkv"));
        fs::write(&path, b"regular media").unwrap();
        // The regular hook fires after the subsequent stop signal; this
        // fixture emits both artifacts before it drives that signal.
        writeln!(
            file,
            "{}\tregular\t{}",
            now_unix_ms() + 1_000,
            path.display()
        )
        .unwrap();
    }

    fn entries_of(&self, category: &Category) -> Vec<&poe_recorder::domain::LibraryEntry> {
        self.latest
            .entries
            .iter()
            .filter(|entry| &entry.category == category)
            .collect()
    }
}

fn setup(root: &Path) -> Setup {
    Setup {
        config_path: root.join("config.json"),
        data_dir: root.join("recorder"),
        gsr_binary: fixture_bin("fake-gsr.sh"),
        media: MediaConfig {
            ffmpeg: fixture_bin("fake-ffmpeg.sh"),
            utc_offset_minutes: 0,
            poll_interval: Duration::from_millis(10),
            finalize_grace: Duration::from_secs(5),
            sigint_grace: Duration::from_millis(300),
        },
        recorder_timeouts: Timeouts {
            arm_stability: Duration::from_millis(150),
            replay_event: Duration::from_millis(400),
            // Ends are asynchronous now, so this budget spans however long a
            // test spends between the stop request and the fake hook's event,
            // including `Config::save`'s two fsyncs. Keep it far enough above
            // that work that a loaded filesystem cannot expire it: the failure
            // mode is a dropped recording and a 20 s `pump` timeout, not a
            // clear assertion. Only the missing-artifact test waits it out.
            regular_event: Duration::from_secs(2),
            exit_grace: Duration::from_millis(500),
            toggle_gap: Duration::from_millis(20),
        },
        poll_interval: Duration::from_millis(5),
        test_duration: Duration::from_millis(200),
    }
}

/// A fresh temp tree: empty directories, an empty `Client.txt`, and a config
/// pointing at them.
fn spawn_tree(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("wr-slice-{name}-{}", uuid::Uuid::new_v4()));
    let library = root.join("recordings with space");
    let capture_root = root.join("buffer");
    let log_dir = root.join("Path of Exile 2/logs");
    for directory in [&library, &capture_root, &log_dir] {
        fs::create_dir_all(directory).unwrap();
    }
    let log_file = log_dir.join("Client.txt");
    fs::write(&log_file, b"").unwrap();
    write_config(&root, &library, &capture_root, &log_dir);
    (root, library, capture_root, log_file)
}

fn fixture_bin(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/native/bin")
        .join(name)
}

fn write_config(root: &Path, library: &Path, capture_root: &Path, log_dir: &Path) {
    let config = Config {
        flavors: poe_recorder::config::FlavorSettings {
            poe2: FlavorConfig {
                enabled: true,
                log_dir: AuthorizedPath::authorized(log_dir),
            },
        },
        // Leaving a map ends the run on the next poll.
        activities: ActivitySettings {
            map_grace_seconds: 0,
        },
        storage: StorageSettings {
            recording_dir: AuthorizedPath::authorized(library),
            separate_buffer_dir: true,
            buffer_dir: AuthorizedPath::authorized(capture_root),
            limit: StorageLimit::Unlimited,
        },
        manual: ManualSettings {
            enabled: true,
            ..Default::default()
        },
        capture: CaptureSettings {
            extra_lead_in_seconds: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    config.save(&root.join("config.json")).unwrap();
}

fn empty_snapshot() -> AppSnapshot {
    AppSnapshot {
        entries: Arc::new(Vec::new()),
        category_counts: Vec::new(),
        status: RecorderStatus::SetupRequired,
        active: None,
        config: Config::default(),
        setup_problems: Vec::new(),
        problems: Vec::new(),
        work: None,
        queued_jobs: 0,
        storage_used_bytes: 0,
        protected_over_limit: false,
    }
}

// --- Client.txt helpers ---

/// `YYYY/MM/DD HH:MM:SS` in UTC, matching the harness's zero UTC offset.
fn stamp(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    let ms_of_day = unix_ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year}/{month:02}/{day:02} {:02}:{:02}:{:02}",
        ms_of_day / 3_600_000,
        (ms_of_day / 60_000) % 60,
        (ms_of_day / 1_000) % 60,
    )
}

/// Howard Hinnant's `civil_from_days`, the inverse of the parser's conversion.
fn civil_from_days(days: i64) -> (i32, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    ((year + i64::from(month <= 2)) as i32, month, day)
}

/// Whole seconds `ago_seconds` before now: log times have no milliseconds.
fn seconds_ago(ago_seconds: i64) -> i64 {
    (now_unix_ms().div_euclid(1_000) - ago_seconds) * 1_000
}

fn area(at_ms: i64, area_id: &str, seed: u64) -> String {
    format!(
        "{} 1000 2caa229f [DEBUG Client 312] Generating level 80 area \"{area_id}\" with seed {seed}",
        stamp(at_ms)
    )
}

fn enter_map(at_ms: i64, seed: u64) -> String {
    area(at_ms, "MapHiddenGrotto", seed)
}

fn hideout(at_ms: i64) -> String {
    area(at_ms, "HideoutShoreline", 1)
}

fn player_death(at_ms: i64) -> String {
    format!(
        "{} 1000 3ef23348 [INFO Client 312] : {PLAYER_NAME} has been slain.",
        stamp(at_ms)
    )
}

// --- Scenarios ---

#[test]
fn automatic_map_run_completes_and_survives_a_restart() {
    let mut harness = Harness::new("map-run");
    assert_eq!(harness.latest.status, RecorderStatus::Ready);

    let start_ms = seconds_ago(2);
    harness.log(&[enter_map(start_ms, 7)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    let active = harness.latest.active.clone().unwrap();
    // The request reaches back past the extra lead-in to the map entry.
    assert!(active.requested_replay_ms >= 10_000);
    assert_eq!(active.category, Category::MapRuns);
    assert_eq!(active.title, "Hidden Grotto");

    harness.emit_artifacts(true);
    harness.log(&[player_death(start_ms + 1_000)]);
    harness.log(&[hideout(now_unix_ms())]);
    harness.pump(|snapshot| !snapshot.entries.is_empty());

    let entry = harness.latest.entries[0].clone();
    assert_eq!(entry.category, Category::MapRuns);
    assert_eq!(entry.outcome, Outcome::Complete);
    assert_eq!(
        entry.player.as_ref().map(|player| player.name.as_str()),
        Some(PLAYER_NAME)
    );
    assert!(matches!(
        entry.details,
        ActivityDetails::MapRun {
            area_level: 80,
            seed: 7,
            deaths: 1,
            ..
        }
    ));
    assert!(entry.media_path.exists(), "media was not written");
    assert!(entry.sidecar_path.exists(), "sidecar was not written");
    // The replay is in front of the media, so the death sits later than its
    // one-second run offset and still inside the media.
    let death = entry
        .timeline
        .iter()
        .find(|item| item.kind() == &TimelineKind::Death)
        .expect("death marker");
    assert!(
        death.start_ms() > 1_000 && death.start_ms() <= entry.duration_ms,
        "death at {} ms, duration {} ms",
        death.start_ms(),
        entry.duration_ms
    );
    // The intermediates were consumed by finalization.
    assert!(read_dir_count(&harness.capture_root.join("replay")) == 0);
    assert!(read_dir_count(&harness.capture_root.join("regular")) == 0);

    // Tag and protect go through the real sidecar.
    harness.send(Command::SetTag {
        id: entry.id.clone(),
        tag: "  keeper  ".to_owned(),
    });
    harness.send(Command::SetProtected {
        ids: vec![entry.id.clone()],
        value: true,
    });
    harness.pump(|snapshot| {
        snapshot
            .entries
            .first()
            .is_some_and(|entry| entry.protected && entry.tag.as_deref() == Some("  keeper  "))
    });

    // Restart: a stray artifact is quarantined and the entry is rescanned.
    let stray = harness.capture_root.join("regular/Video_stray.mkv");
    fs::write(&stray, b"interrupted").unwrap();
    let mut restarted = harness.restart();
    assert_eq!(restarted.latest.entries.len(), 1);
    assert!(restarted.latest.entries[0].protected);
    assert_eq!(restarted.latest.entries[0].timeline, entry.timeline);
    assert!(!stray.exists(), "stray artifact was not swept");
    assert!(
        read_dir_count(&restarted.library.join(RECOVERY_DIR)) > 0,
        "nothing was quarantined"
    );
    // The restart replayed the log's end but found the player out of the
    // map, so nothing is recording.
    assert!(restarted.latest.active.is_none());

    let id = restarted.latest.entries[0].id.clone();
    restarted.send(Command::Delete { ids: vec![id] });
    restarted.pump(|snapshot| snapshot.entries.is_empty());
}

#[test]
fn force_ended_map_run_is_saved() {
    let mut harness = Harness::new("force-end");
    harness.log(&[enter_map(seconds_ago(1), 7)]);
    harness.pump(|snapshot| snapshot.active.is_some());

    harness.emit_artifacts(true);
    harness.send(Command::ForceEnd);
    harness.pump(|snapshot| !snapshot.entries.is_empty());

    let entry = &harness.latest.entries[0];
    assert_eq!(entry.category, Category::MapRuns);
    assert_eq!(entry.outcome, Outcome::Complete);
}

#[test]
fn manual_and_test_recordings_reuse_the_capture_pipeline() {
    let mut harness = Harness::new("manual");

    harness.send(Command::StartManual);
    harness.pump(|snapshot| snapshot.active.is_some());
    assert_eq!(
        harness.latest.active.as_ref().unwrap().category,
        Category::Manual
    );
    harness.emit_artifacts(true);
    harness.send(Command::StopManual);
    harness.pump(|snapshot| !snapshot.entries.is_empty());
    let manual = harness.latest.entries[0].clone();
    assert_eq!(manual.category, Category::Manual);
    assert_eq!(manual.title, "Manual recording");

    // The test recording simulates a short map run with one death.
    harness.send(Command::RunTest);
    harness.pump(|snapshot| {
        matches!(
            snapshot.status,
            RecorderStatus::Recording {
                test: true,
                manual: false,
                ..
            }
        )
    });
    harness.emit_artifacts(true);
    harness.pump(|snapshot| snapshot.entries.len() == 2);
    let test = harness.entries_of(&Category::MapRuns)[0].clone();
    assert_eq!(test.title, "Test Map");
    assert!(
        test.timeline
            .iter()
            .any(|item| item.kind() == &TimelineKind::Death)
    );
}

#[test]
fn finalization_precedes_queued_user_jobs() {
    let mut harness = Harness::new("queue");

    // One finished recording to clip against.
    harness.log(&[enter_map(seconds_ago(1), 1)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.emit_artifacts(true);
    harness.log(&[hideout(now_unix_ms())]);
    harness.pump(|snapshot| !snapshot.entries.is_empty());
    let source = harness.latest.entries[0].clone();

    // A second recording, then complete it and queue a clip before the tick's
    // single dispatch: both jobs are queued before dispatch chooses a job.
    harness.log(&[enter_map(seconds_ago(1), 2)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.emit_artifacts(true);
    harness.log(&[hideout(now_unix_ms())]);
    harness.send(Command::CreateClip(ClipRange {
        source: source.id.clone(),
        start_ms: 0,
        end_ms: source.duration_ms.min(2_000),
    }));

    let mut order: Vec<Category> = Vec::new();
    harness.pump(|snapshot| {
        for entry in snapshot.entries.iter() {
            if !order.contains(&entry.category) && entry.id != source.id {
                order.push(entry.category.clone());
            }
        }
        order.len() == 2
    });
    assert_eq!(order, vec![Category::MapRuns, Category::Clip]);
}

#[test]
fn commands_are_served_while_a_capture_is_ending() {
    let mut harness = Harness::new("ending");
    harness.log(&[enter_map(seconds_ago(1), 7)]);
    harness.pump(|snapshot| snapshot.active.is_some());

    // End the run without the hook reporting any artifact yet.
    harness.log(&[hideout(now_unix_ms())]);
    harness.pump(|snapshot| snapshot.active.is_none());
    assert!(
        matches!(harness.latest.status, RecorderStatus::Finalizing { .. }),
        "status {:?}",
        harness.latest.status
    );
    assert!(harness.latest.entries.is_empty());

    harness.send(Command::SetSelectedCategory {
        category: Category::Manual,
    });
    harness.pump(|snapshot| snapshot.config.interface.selected_category == Category::Manual);
    assert!(
        harness.latest.entries.is_empty(),
        "the capture must still be waiting on its artifacts"
    );

    // The artifacts finally land: the recording finalizes as usual.
    harness.emit_artifacts(true);
    harness.pump(|snapshot| !snapshot.entries.is_empty());
    assert_eq!(harness.latest.entries[0].category, Category::MapRuns);
}

/// Entering a different map ends the previous run at once: both must be
/// saved, each as its own recording.
#[test]
fn a_new_map_keeps_both_runs() {
    let mut harness = Harness::new("next-map");
    harness.log(&[enter_map(seconds_ago(1), 1)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    let first = harness.latest.active.clone().unwrap().id;
    harness.emit_artifacts(true);

    harness.log(&[enter_map(now_unix_ms(), 2)]);
    harness.pump(|snapshot| {
        snapshot
            .active
            .as_ref()
            .is_some_and(|active| active.id != first)
    });
    let second = harness.latest.active.clone().unwrap().id;
    harness.emit_artifacts(true);
    harness.log(&[hideout(now_unix_ms())]);
    harness.pump(|snapshot| snapshot.entries.len() == 2);

    let seed_of = |id| {
        harness
            .latest
            .entries
            .iter()
            .find(|entry| &entry.id == id)
            .and_then(|entry| match entry.details {
                ActivityDetails::MapRun { seed, .. } => Some(seed),
                _ => None,
            })
    };
    assert_eq!(seed_of(&first), Some(1));
    assert_eq!(seed_of(&second), Some(2));
}

#[test]
fn dismissing_the_release_notes_ends_them_for_good() {
    let harness = Harness::new("dismissals");
    let config_path = harness.root.join("config.json");
    let mut config = Config::load(&config_path).expect("load the harness config");
    // What an install updated from an earlier version looks like: the field
    // is missing from its config file, so it deserializes empty.
    config.last_seen_version = String::new();
    config
        .save(&config_path)
        .expect("seed the pending release notes");
    let mut harness = harness.restart();
    assert!(harness.latest.config.last_seen_version.is_empty());

    harness.send(Command::DismissReleaseNotes);
    harness.pump(|snapshot| snapshot.config.last_seen_version == poe_recorder::VERSION);
    let saved = Config::load(&config_path).expect("reload the saved config");
    assert_eq!(
        saved.last_seen_version,
        poe_recorder::VERSION,
        "the dismissal must outlive the process"
    );

    // Dialogs opened before the notes were closed carry a draft with the old
    // values. Applying it must not replay the notes, or they reappear on
    // every later start.
    let mut stale = harness.latest.config.clone();
    stale.last_seen_version = String::new();
    stale.capture.fps = 30;
    harness.send(Command::SaveConfig {
        draft: Box::new(stale),
    });
    harness.pump(|snapshot| snapshot.config.capture.fps == 30);
    assert_eq!(
        harness.latest.config.last_seen_version,
        poe_recorder::VERSION,
        "a settings save must not resurrect the dismissal"
    );
    let saved = Config::load(&config_path).expect("reload after the settings save");
    assert_eq!(
        saved.last_seen_version,
        poe_recorder::VERSION,
        "the resurrected notes must not reach disk either"
    );
}

#[test]
fn a_dragged_layout_outlives_the_process() {
    let mut harness = Harness::new("layout");
    assert_eq!(
        harness.latest.config.interface.layout,
        LayoutSettings::default(),
        "a clean start stores nothing, which is what lets the pane autoscale"
    );

    let layout = LayoutSettings {
        player_split: Some(612),
        column_widths: BTreeMap::from([("Map".to_owned(), 240)]),
    };
    harness.send(Command::SaveLayout {
        layout: layout.clone(),
    });
    harness.pump(|snapshot| snapshot.config.interface.layout == layout);

    let harness = harness.restart();
    assert_eq!(harness.latest.config.interface.layout, layout);
}

#[test]
fn missing_replay_falls_back_to_the_regular_recording() {
    let mut harness = Harness::new("regular-only");
    let start_ms = seconds_ago(2);
    harness.log(&[enter_map(start_ms, 7)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.emit_artifacts(false);
    harness.log(&[player_death(start_ms)]);
    harness.log(&[hideout(now_unix_ms())]);
    harness.pump(|snapshot| !snapshot.entries.is_empty());

    let entry = &harness.latest.entries[0];
    assert_eq!(entry.category, Category::MapRuns);
    assert!(entry.media_path.exists());
    assert!(
        !entry
            .timeline
            .iter()
            .any(|item| item.kind() == &TimelineKind::Death),
        "a death before the media start should be clipped: {:?}",
        entry.timeline
    );
}

#[test]
fn missing_regular_artifact_replaces_the_child_and_recovers() {
    let mut harness = Harness::new("failure");
    harness.log(&[enter_map(seconds_ago(1), 1)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    // GSR never reports either artifact.
    harness.log(&[hideout(now_unix_ms())]);
    harness.pump(|snapshot| !snapshot.problems.is_empty());
    assert!(harness.latest.entries.is_empty());
    assert!(
        harness
            .latest
            .problems
            .iter()
            .any(|problem| problem.summary.contains("no video file")),
        "problems: {:?}",
        harness.latest.problems
    );
    // A missing regular recording means the toggle desynced; replace the child.
    harness.pump(|snapshot| {
        snapshot
            .problems
            .iter()
            .any(|problem| problem.summary.contains("stopped unexpectedly"))
    });
    // The replacement re-arms, so the next recording still saves instead of
    // every later capture going silent.
    harness.pump(|snapshot| snapshot.status == RecorderStatus::Ready);
    harness.log(&[enter_map(seconds_ago(1), 2)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.emit_artifacts(true);
    harness.log(&[hideout(now_unix_ms())]);
    harness.pump(|snapshot| !snapshot.entries.is_empty());
    let entry = &harness.latest.entries[0];
    assert_eq!(entry.category, Category::MapRuns);
    assert!(entry.media_path.exists(), "media was not written");
}

/// A harness over a tree that already holds one scanned recording
/// (`canary-old`), optionally protected. Its media is removed right after
/// startup, so any full library rescan would drop it: the canary stays in the
/// index only while completion, mutation, and eviction update incrementally.
fn canary_harness(name: &str, protected: bool) -> (Harness, poe_recorder::domain::LibraryEntry) {
    let (root, library, capture_root, log_file) = spawn_tree(name);
    let start = now_unix_ms() - 61_000;
    // A native sidecar, exactly as finalize would write one: the canary must
    // scan like any recording the app itself produced.
    let sidecar = format!(
        r#"{{"schema_version":1,"media_file":"canary-old.mp4","id":"0d8a0e10-1a2b-4c3d-8e4f-aabbccddeeff","category":"map_runs","flavor":"poe2","title":"Canary","start_unix_ms":{start},"duration_ms":60000,"outcome":"complete","protected":{protected},"timeline":[],"details":{{"kind":"map_run","area_id":"MapBluff","map_name":"Bluff","area_level":80,"seed":1,"deaths":0,"portal_trips":0,"away_ms":0}},"media":{{"has_content":true}}}}"#
    );
    fs::write(library.join("canary-old.json"), sidecar).unwrap();
    fs::write(library.join("canary-old.mp4"), b"canary media").unwrap();

    let harness = Harness::attach(root, library, capture_root, log_file);
    assert_eq!(harness.latest.entries.len(), 1);
    let canary = harness.latest.entries[0].clone();
    fs::remove_file(&canary.media_path).unwrap();
    (harness, canary)
}

/// Media plus sidecar bytes for every entry still counted by the coordinator.
fn counted_usage(entries: &[&poe_recorder::domain::LibraryEntry]) -> u64 {
    entries
        .iter()
        .flat_map(|entry| [&entry.media_path, &entry.sidecar_path])
        .map(|path| fs::metadata(path).map(|meta| meta.len()).unwrap_or(0))
        .sum()
}

#[test]
fn completion_updates_the_library_without_a_full_rescan() {
    let (mut harness, canary) = canary_harness("incremental", false);

    harness.log(&[enter_map(seconds_ago(1), 2)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.emit_artifacts(true);
    harness.log(&[hideout(now_unix_ms())]);
    harness.pump(|snapshot| snapshot.entries.len() == 2);

    // The completed recording is the newest entry and the canary kept its
    // place despite its missing media, which a rescan would have dropped.
    let run = harness.latest.entries[0].clone();
    assert_eq!(run.category, Category::MapRuns);
    assert_eq!(harness.latest.entries[1].id, canary.id);
    // Usage tracks the real files, including the canary's missing media.
    assert_eq!(
        harness.latest.storage_used_bytes,
        counted_usage(&[&run, &canary])
    );

    // Tag and protect fold into the index the same way.
    harness.send(Command::SetTag {
        id: run.id.clone(),
        tag: "keeper".to_owned(),
    });
    harness.pump(|snapshot| snapshot.entries[0].tag.as_deref() == Some("keeper"));
    assert_eq!(harness.latest.entries[1].id, canary.id);

    // Deletion removes only the deleted entry.
    harness.send(Command::Delete {
        ids: vec![run.id.clone()],
    });
    harness.pump(|snapshot| snapshot.entries.len() == 1);
    assert_eq!(harness.latest.entries[0].id, canary.id);
    assert_eq!(
        harness.latest.storage_used_bytes,
        counted_usage(&[&harness.latest.entries[0]])
    );
    assert!(!run.media_path.exists());
    assert!(!run.sidecar_path.exists());
}

#[test]
fn completion_eviction_updates_the_index_without_a_full_rescan() {
    let (mut harness, canary) = canary_harness("eviction", true);

    // One unprotected map run, then a storage cap below its padded size.
    harness.log(&[enter_map(seconds_ago(1), 2)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.emit_artifacts(true);
    harness.log(&[hideout(now_unix_ms())]);
    harness.pump(|snapshot| snapshot.entries.len() == 2);
    let run = harness.latest.entries[0].clone();
    assert_eq!(run.category, Category::MapRuns);

    let mut draft = harness.latest.config.clone();
    draft.storage.limit = StorageLimit::Gib(NonZeroU64::new(1).expect("nonzero"));
    harness.send(Command::SaveConfig {
        draft: Box::new(draft),
    });
    harness.pump(|snapshot| {
        snapshot.config.storage.limit == StorageLimit::Gib(NonZeroU64::new(1).expect("nonzero"))
    });

    // Sparse-pad the run past the cap without touching its bytes.
    let padded = fs::OpenOptions::new()
        .write(true)
        .open(&run.media_path)
        .unwrap();
    padded.set_len(2 * 1024 * 1024 * 1024).unwrap();

    harness.log(&[enter_map(now_unix_ms(), 3)]);
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.emit_artifacts(true);
    harness.log(&[hideout(now_unix_ms())]);
    let run_id = run.id.clone();
    harness.pump(|snapshot| {
        snapshot.entries.len() == 2 && snapshot.entries.iter().all(|entry| entry.id != run_id)
    });

    // Eviction removed the oldest unprotected recording; the protected canary
    // survived and, with its media missing, only if no rescan ran.
    let newest = harness.latest.entries[0].clone();
    assert_eq!(newest.category, Category::MapRuns);
    assert_ne!(newest.id, run.id);
    assert_eq!(harness.latest.entries[1].id, canary.id);
    assert!(!run.media_path.exists(), "evicted media was not removed");
    assert_eq!(
        harness.latest.storage_used_bytes,
        counted_usage(&[&newest, &canary])
    );
}

#[test]
fn production_handle_starts_and_shuts_down() {
    let (root, ..) = spawn_tree("handle");

    let mut handle = start(setup(&root), Box::new(|| {}));
    let mut snapshot = handle.snapshots.recv_timeout(STEP_TIMEOUT);
    while let Ok(current) = &snapshot {
        if current.status == RecorderStatus::Ready {
            break;
        }
        snapshot = handle.snapshots.recv_timeout(STEP_TIMEOUT);
    }
    assert_eq!(
        snapshot.expect("armed snapshot").status,
        RecorderStatus::Ready
    );
    assert!(handle.send(Command::Disarm));
    handle.shutdown();
}

fn read_dir_count(path: &Path) -> usize {
    fs::read_dir(path)
        .map(|entries| entries.count())
        .unwrap_or(0)
}
