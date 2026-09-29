// SPDX-License-Identifier: GPL-3.0-or-later

//! Feeds the simulated `Client.txt` fixtures through the map tracker, as the
//! app will: every parsed line in order, then a tick once the grace period
//! after the last line has passed.

use poe2_log::parse_line;
use poe2_log::tracker::{Away, MapAction, MapRun, MapTracker};

const GRACE_MS: i64 = 60_000;

struct Outcome {
    begins: usize,
    runs: Vec<MapRun>,
    /// Time of the first line, so runs can be checked in scenario seconds.
    origin_ms: i64,
}

fn run_fixture(name: &str) -> Outcome {
    let path = format!("{}/tests/fixtures/{name}.txt", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    let lines: Vec<_> = text
        .lines()
        .filter_map(|line| parse_line(line, 0))
        .collect();
    let origin_ms = lines[0].occurred_at_ms;
    let last_ms = lines.last().unwrap().occurred_at_ms;

    let mut tracker = MapTracker::new(GRACE_MS);
    let mut actions = Vec::new();
    for line in &lines {
        actions.extend(tracker.handle(line));
    }
    actions.extend(tracker.tick(last_ms + GRACE_MS));
    assert!(tracker.is_idle(), "{name}: a run is still open");

    let mut begins = 0;
    let mut runs = Vec::new();
    for action in actions {
        match action {
            MapAction::Begin(_) => begins += 1,
            MapAction::Complete(run) => runs.push(run),
        }
    }
    assert_eq!(begins, runs.len(), "{name}: every begin completes");
    Outcome {
        begins,
        runs,
        origin_ms,
    }
}

impl Outcome {
    /// (map id, start second, end second, deaths) per run.
    fn summary(&self) -> Vec<(&str, i64, i64, usize)> {
        self.runs
            .iter()
            .map(|run| {
                (
                    run.start.area_id.as_str(),
                    (run.start.started_at_ms - self.origin_ms) / 1_000,
                    (run.ended_at_ms - self.origin_ms) / 1_000,
                    run.deaths.len(),
                )
            })
            .collect()
    }

    fn away_seconds(&self, run: usize) -> Vec<(i64, i64)> {
        self.runs[run]
            .away
            .iter()
            .map(
                |&Away {
                     left_at_ms,
                     returned_at_ms,
                 }| {
                    (
                        (left_at_ms - self.origin_ms) / 1_000,
                        (returned_at_ms - self.origin_ms) / 1_000,
                    )
                },
            )
            .collect()
    }
}

#[test]
fn simple() {
    let outcome = run_fixture("simple");
    assert_eq!(outcome.summary(), [("MapBluff", 5, 245, 0)]);
    assert_eq!(outcome.runs[0].duration_ms(), 240_000);
    assert_eq!(outcome.runs[0].start.area_level, 70);
}

#[test]
fn death_and_checkpoint_respawn_stay_one_run() {
    let outcome = run_fixture("death");
    assert_eq!(outcome.summary(), [("MapHiddenGrotto", 5, 160, 1)]);
    let death = &outcome.runs[0].deaths[0];
    assert_eq!((death.at_ms - outcome.origin_ms) / 1_000, 65);
    assert_eq!(death.name, "TestExile");
    assert!(outcome.away_seconds(0).is_empty());
}

#[test]
fn portal_mid_map_does_not_split_the_run() {
    let outcome = run_fixture("portal");
    assert_eq!(outcome.begins, 1);
    assert_eq!(outcome.summary(), [("MapWillow", 5, 175, 0)]);
    assert_eq!(outcome.away_seconds(0), [(65, 85)]);
}

#[test]
fn abandoned_map_ends_when_the_player_left() {
    let outcome = run_fixture("abandon");
    assert_eq!(outcome.summary(), [("MapCrypt", 5, 50, 0)]);
}

#[test]
fn back_to_back_maps_are_two_runs() {
    let outcome = run_fixture("back-to-back");
    assert_eq!(
        outcome.summary(),
        [("MapBluff", 5, 85, 0), ("MapSwamp", 95, 175, 0)]
    );
}

#[test]
fn demo() {
    let outcome = run_fixture("demo");
    assert_eq!(
        outcome.summary(),
        [("MapWillow", 15, 200, 1), ("MapBluff", 215, 305, 0)]
    );
    assert_eq!(outcome.away_seconds(0), [(120, 140)]);
}
