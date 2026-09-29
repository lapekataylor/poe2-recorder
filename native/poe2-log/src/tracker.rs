// SPDX-License-Identifier: GPL-3.0-or-later

//! Map-run state: when a recording should begin and end.
//!
//! - Entering a waystone map while idle begins a run.
//! - Leaving the map (hideout, town, anywhere else) starts a grace period.
//!   Coming back to the same seed within it continues the run and records the
//!   time away, so portalling out to sell loot does not split the video.
//! - The run completes when the grace period runs out, or at once when a map
//!   with a different seed is entered. Either way it ends at the moment the
//!   player left, not when the grace period expired.
//! - Deaths inside the map are recorded with the character's name.
//!
//! Like the WoW activity engine, this is deterministic: no clocks, files or
//! threads. All times come from events or from `tick`/`force_end` arguments,
//! in Unix milliseconds.

use crate::{AreaKind, LogEvent, ParsedLine};

/// Used when the settings do not say otherwise.
pub const DEFAULT_GRACE_MS: i64 = 60_000;

/// A run that has begun: enough to title and start a recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapStart {
    pub area_id: String,
    pub area_level: u32,
    pub seed: u64,
    pub started_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Death {
    pub at_ms: i64,
    pub name: String,
}

/// Time spent outside the map in the middle of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Away {
    pub left_at_ms: i64,
    pub returned_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapRun {
    pub start: MapStart,
    pub ended_at_ms: i64,
    pub deaths: Vec<Death>,
    pub away: Vec<Away>,
}

impl MapRun {
    /// Wall time from entering the map to leaving it the last time,
    /// including any time away.
    pub fn duration_ms(&self) -> i64 {
        self.ended_at_ms - self.start.started_at_ms
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapAction {
    /// Start recording.
    Begin(MapStart),
    /// Stop recording. The run ended at `run.ended_at_ms`, which can be up
    /// to the grace period before now.
    Complete(MapRun),
}

#[derive(Debug)]
enum State {
    Idle,
    InMap(MapRun),
    /// Left the map at `left_at_ms` and has not come back yet.
    Away {
        run: MapRun,
        left_at_ms: i64,
    },
}

#[derive(Debug)]
pub struct MapTracker {
    grace_ms: i64,
    state: State,
}

impl MapTracker {
    pub fn new(grace_ms: i64) -> Self {
        Self {
            grace_ms: grace_ms.max(0),
            state: State::Idle,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    pub fn handle(&mut self, line: &ParsedLine) -> Vec<MapAction> {
        let at_ms = line.occurred_at_ms;
        // Events can arrive in a batch after a quiet spell with no `tick` in
        // between, so an expired grace period is settled first.
        let mut actions = self.tick(at_ms);
        let state = std::mem::replace(&mut self.state, State::Idle);
        self.state = match (state, &line.event) {
            (
                state,
                LogEvent::AreaEntered {
                    kind: AreaKind::Map,
                    area_id,
                    area_level,
                    seed,
                },
            ) => match state {
                State::InMap(run) if run.start.seed == *seed => State::InMap(run),
                State::Away {
                    mut run,
                    left_at_ms,
                } if run.start.seed == *seed => {
                    run.away.push(Away {
                        left_at_ms,
                        returned_at_ms: at_ms,
                    });
                    State::InMap(run)
                }
                previous => {
                    // A different map: whatever was running ended when the
                    // player left it (or now, if they never did).
                    match previous {
                        State::Idle => {}
                        State::InMap(run) => actions.push(complete(run, at_ms)),
                        State::Away { run, left_at_ms } => {
                            actions.push(complete(run, left_at_ms));
                        }
                    }
                    let start = MapStart {
                        area_id: area_id.clone(),
                        area_level: *area_level,
                        seed: *seed,
                        started_at_ms: at_ms,
                    };
                    actions.push(MapAction::Begin(start.clone()));
                    State::InMap(MapRun {
                        start,
                        ended_at_ms: at_ms,
                        deaths: Vec::new(),
                        away: Vec::new(),
                    })
                }
            },
            (State::InMap(run), LogEvent::AreaEntered { .. }) => State::Away {
                run,
                left_at_ms: at_ms,
            },
            (State::InMap(mut run), LogEvent::Slain { name }) => {
                run.deaths.push(Death {
                    at_ms,
                    name: name.clone(),
                });
                State::InMap(run)
            }
            // Moving between hideout and town while away, or anything while
            // idle, changes nothing.
            (state, _) => state,
        };
        actions
    }

    /// Complete the run if its grace period has run out by `now_ms`. Call
    /// this regularly; log events alone never arrive while the player sits
    /// in the hideout.
    pub fn tick(&mut self, now_ms: i64) -> Vec<MapAction> {
        match self.state {
            State::Away { left_at_ms, .. } if now_ms - left_at_ms >= self.grace_ms => {}
            _ => return Vec::new(),
        }
        let State::Away { run, left_at_ms } = std::mem::replace(&mut self.state, State::Idle)
        else {
            unreachable!()
        };
        vec![complete(run, left_at_ms)]
    }

    /// Complete any run now, e.g. when the app quits or the game stops
    /// writing its log. A run already away ends when the player left.
    pub fn force_end(&mut self, now_ms: i64) -> Vec<MapAction> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => Vec::new(),
            State::InMap(run) => vec![complete(run, now_ms)],
            State::Away { run, left_at_ms } => vec![complete(run, left_at_ms)],
        }
    }
}

impl Default for MapTracker {
    fn default() -> Self {
        Self::new(DEFAULT_GRACE_MS)
    }
}

fn complete(mut run: MapRun, ended_at_ms: i64) -> MapAction {
    run.ended_at_ms = ended_at_ms.max(run.start.started_at_ms);
    MapAction::Complete(run)
}

/// A readable map name from its area id: `MapHiddenGrotto` → `Hidden Grotto`.
pub fn map_display_name(area_id: &str) -> String {
    let base = area_id.strip_prefix("Map").unwrap_or(area_id);
    let base = if base.is_empty() { area_id } else { base };
    let mut name = String::with_capacity(base.len() + 4);
    let mut previous: Option<char> = None;
    for character in base.chars() {
        if character == '_' {
            name.push(' ');
        } else {
            let starts_word = previous.is_some_and(|previous| {
                previous != '_'
                    && ((character.is_uppercase() && previous.is_lowercase())
                        || (character.is_ascii_digit() && !previous.is_ascii_digit()))
            });
            if starts_word {
                name.push(' ');
            }
            name.push(character);
        }
        previous = Some(character);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: i64 = 60_000;

    fn at(seconds: i64, event: LogEvent) -> ParsedLine {
        ParsedLine {
            occurred_at_ms: seconds * 1_000,
            uptime_ms: 0,
            event,
        }
    }

    fn map(seconds: i64, seed: u64) -> ParsedLine {
        at(
            seconds,
            LogEvent::AreaEntered {
                area_id: "MapBluff".to_owned(),
                area_level: 70,
                seed,
                kind: AreaKind::Map,
            },
        )
    }

    fn hideout(seconds: i64) -> ParsedLine {
        at(
            seconds,
            LogEvent::AreaEntered {
                area_id: "HideoutFelled".to_owned(),
                area_level: 1,
                seed: 1,
                kind: AreaKind::Hideout,
            },
        )
    }

    fn completed(actions: &[MapAction]) -> Vec<&MapRun> {
        actions
            .iter()
            .filter_map(|action| match action {
                MapAction::Complete(run) => Some(run),
                MapAction::Begin(_) => None,
            })
            .collect()
    }

    #[test]
    fn hideout_before_any_map_does_nothing() {
        let mut tracker = MapTracker::new(GRACE);
        assert!(tracker.handle(&hideout(0)).is_empty());
        assert!(tracker.tick(1_000_000).is_empty());
        assert!(tracker.is_idle());
    }

    #[test]
    fn grace_expires_exactly_at_the_boundary() {
        let mut tracker = MapTracker::new(GRACE);
        tracker.handle(&map(0, 7));
        tracker.handle(&hideout(100));
        assert!(tracker.tick(100_000 + GRACE - 1).is_empty());
        let actions = tracker.tick(100_000 + GRACE);
        assert_eq!(completed(&actions)[0].ended_at_ms, 100_000);
        assert!(tracker.is_idle());
    }

    #[test]
    fn late_event_settles_an_expired_grace_period_first() {
        let mut tracker = MapTracker::new(GRACE);
        tracker.handle(&map(0, 7));
        tracker.handle(&hideout(100));
        // No tick ran; the same map is re-entered long after the grace period.
        let actions = tracker.handle(&map(500, 7));
        assert_eq!(completed(&actions)[0].ended_at_ms, 100_000);
        assert!(
            matches!(actions.last(), Some(MapAction::Begin(start)) if start.started_at_ms == 500_000)
        );
    }

    #[test]
    fn a_new_seed_ends_the_previous_run_even_without_a_hideout() {
        let mut tracker = MapTracker::new(GRACE);
        tracker.handle(&map(0, 7));
        let actions = tracker.handle(&map(90, 8));
        assert_eq!(completed(&actions)[0].ended_at_ms, 90_000);
        assert!(matches!(&actions[1], MapAction::Begin(start) if start.seed == 8));
    }

    #[test]
    fn deaths_outside_a_map_are_ignored() {
        let mut tracker = MapTracker::new(GRACE);
        let death = at(
            5,
            LogEvent::Slain {
                name: "TestExile".to_owned(),
            },
        );
        tracker.handle(&death);
        tracker.handle(&map(10, 7));
        tracker.handle(&hideout(20));
        tracker.handle(&death);
        let actions = tracker.force_end(30_000);
        assert!(completed(&actions)[0].deaths.is_empty());
    }

    #[test]
    fn force_end_while_in_the_map_ends_now() {
        let mut tracker = MapTracker::new(GRACE);
        tracker.handle(&map(0, 7));
        let actions = tracker.force_end(42_000);
        assert_eq!(completed(&actions)[0].duration_ms(), 42_000);
        assert!(tracker.force_end(50_000).is_empty());
    }

    #[test]
    fn display_names_split_words() {
        assert_eq!(map_display_name("MapHiddenGrotto"), "Hidden Grotto");
        assert_eq!(map_display_name("MapBluff"), "Bluff");
        assert_eq!(map_display_name("MapSwampTower2"), "Swamp Tower 2");
        assert_eq!(map_display_name("MapUber_Boss"), "Uber Boss");
        assert_eq!(map_display_name("Map"), "Map");
    }
}
