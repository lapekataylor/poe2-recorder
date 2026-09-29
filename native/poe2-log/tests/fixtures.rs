// SPDX-License-Identifier: GPL-3.0-or-later

//! Parses the simulated `Client.txt` files in `tests/fixtures/`, written by
//! `scripts/fake-poe2-log.py --instant --fresh --scenario <name>`. Regenerate
//! a fixture after changing that script's line formats.

use poe2_log::{AreaKind, LogEvent, ParsedLine, parse_line};

fn parse_fixture(name: &str) -> Vec<ParsedLine> {
    let path = format!("{}/tests/fixtures/{name}.txt", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    assert!(text.contains("\r\n"), "{path} should keep the game's CRLF");
    text.lines()
        .filter_map(|line| parse_line(line, 0))
        .collect()
}

/// One short label per event, e.g. `MapWillow:3003`, `hideout`, `death`.
fn labels(lines: &[ParsedLine]) -> Vec<String> {
    lines
        .iter()
        .map(|line| match &line.event {
            LogEvent::AreaEntered {
                kind: AreaKind::Map,
                area_id,
                seed,
                ..
            } => format!("{area_id}:{seed}"),
            LogEvent::AreaEntered {
                kind: AreaKind::Hideout,
                ..
            } => "hideout".to_owned(),
            LogEvent::AreaEntered {
                kind: AreaKind::Town,
                ..
            } => "town".to_owned(),
            LogEvent::AreaEntered { area_id, .. } => area_id.clone(),
            LogEvent::Slain { .. } => "death".to_owned(),
        })
        .collect()
}

/// Seconds between the first and last event, by both clocks in the line.
fn span_seconds(lines: &[ParsedLine]) -> (i64, u64) {
    let (first, last) = (lines.first().unwrap(), lines.last().unwrap());
    (
        (last.occurred_at_ms - first.occurred_at_ms) / 1_000,
        (last.uptime_ms - first.uptime_ms) / 1_000,
    )
}

#[test]
fn simple() {
    let lines = parse_fixture("simple");
    assert_eq!(labels(&lines), ["hideout", "MapBluff:1001", "hideout"]);
    // 5s in the hideout, then a 240s map.
    assert_eq!(span_seconds(&lines), (245, 245));
}

#[test]
fn death() {
    let lines = parse_fixture("death");
    assert_eq!(
        labels(&lines),
        [
            "hideout",
            "MapHiddenGrotto:2002",
            "death",
            "MapHiddenGrotto:2002",
            "hideout"
        ]
    );
    let LogEvent::Slain { name } = &lines[2].event else {
        unreachable!()
    };
    assert_eq!(name, "TestExile");
}

#[test]
fn portal_returns_to_the_same_seed() {
    let lines = parse_fixture("portal");
    assert_eq!(
        labels(&lines),
        [
            "hideout",
            "MapWillow:3003",
            "hideout",
            "MapWillow:3003",
            "hideout"
        ]
    );
    // The hideout visit in the middle lasts 20s.
    assert_eq!(lines[3].uptime_ms - lines[2].uptime_ms, 20_000);
}

#[test]
fn abandon_ends_in_the_hideout() {
    let lines = parse_fixture("abandon");
    assert_eq!(labels(&lines), ["hideout", "MapCrypt:4004", "hideout"]);
}

#[test]
fn back_to_back_maps_have_different_seeds() {
    let lines = parse_fixture("back-to-back");
    assert_eq!(
        labels(&lines),
        [
            "hideout",
            "MapBluff:5005",
            "hideout",
            "MapSwamp:6006",
            "hideout"
        ]
    );
}

#[test]
fn demo() {
    let lines = parse_fixture("demo");
    assert_eq!(
        labels(&lines),
        [
            "town",
            "hideout",
            "MapWillow:7007",
            "death",
            "MapWillow:7007",
            "hideout",
            "MapWillow:7007",
            "hideout",
            "MapBluff:8008",
            "hideout"
        ]
    );
    let LogEvent::AreaEntered { area_level, .. } = lines[2].event else {
        unreachable!()
    };
    assert_eq!(area_level, 74);
    assert_eq!(span_seconds(&lines), (305, 305));
}
