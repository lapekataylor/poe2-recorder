// SPDX-License-Identifier: GPL-3.0-or-later

//! Path of Exile 2 `Client.txt` parsing into the few facts the recorder needs.
//!
//! A `Client.txt` line looks like:
//!
//! ```text
//! 2026/09/28 12:27:43 320000 aaa7659d [DEBUG Client 40464] Generating level 74 area "MapWillow" with seed 3003
//! ```
//!
//! That is: local date and time, milliseconds since the game started, an
//! opaque hex id, `[LEVEL Client <pid>]`, then the message. Only two messages
//! matter; every other line parses to `None`:
//!
//! - `Generating level <n> area "<id>" with seed <n>`: an area was entered.
//!   Going back into an existing instance (a portal, a checkpoint respawn)
//!   repeats the same seed.
//! - `: <name> has been slain.`: a character died.
//!
//! The formats are unconfirmed against real PoE2 logs; see `area_kind` and
//! the `*_PREFIX`/`*_SUFFIX` constants when they need adjusting.
//!
//! [`tracker`] turns the parsed events into map runs.

pub mod tracker;

const GENERATING_PREFIX: &str = "Generating level ";
const AREA_INFIX: &str = " area \"";
const SEED_INFIX: &str = "\" with seed ";
const SLAIN_PREFIX: &str = ": ";
const SLAIN_SUFFIX: &str = " has been slain.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLine {
    /// Unix time of the line, from its local timestamp and the UTC offset.
    pub occurred_at_ms: i64,
    /// Milliseconds since the game client started. Unaffected by clock or
    /// daylight-saving changes, so it is the better source for durations
    /// within one game session.
    pub uptime_ms: u64,
    pub event: LogEvent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogEvent {
    AreaEntered {
        area_id: String,
        area_level: u32,
        seed: u64,
        kind: AreaKind,
    },
    Slain {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AreaKind {
    /// A waystone map, such as `MapBluff`.
    Map,
    Hideout,
    Town,
    /// Campaign zones and anything else not recognised.
    Other,
}

/// Parse one line, without its trailing newline (a trailing `\r` is
/// accepted). `utc_offset_minutes` is the local offset the game wrote its
/// timestamps in, e.g. 120 for UTC+2.
pub fn parse_line(line: &str, utc_offset_minutes: i32) -> Option<ParsedLine> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let (date, rest) = line.split_once(' ')?;
    let (time, rest) = rest.split_once(' ')?;
    let (uptime, rest) = rest.split_once(' ')?;
    let (_id, rest) = rest.split_once(' ')?;
    let rest = rest.strip_prefix('[')?;
    let (_level_and_pid, message) = rest.split_once("] ")?;

    let event = parse_message(message)?;
    Some(ParsedLine {
        occurred_at_ms: unix_ms(date, time, utc_offset_minutes)?,
        uptime_ms: uptime.parse().ok()?,
        event,
    })
}

fn parse_message(message: &str) -> Option<LogEvent> {
    if let Some(rest) = message.strip_prefix(GENERATING_PREFIX) {
        let (level, rest) = rest.split_once(AREA_INFIX)?;
        let (area_id, seed) = rest.split_once(SEED_INFIX)?;
        if area_id.is_empty() {
            return None;
        }
        return Some(LogEvent::AreaEntered {
            kind: area_kind(area_id),
            area_id: area_id.to_owned(),
            area_level: level.parse().ok()?,
            seed: seed.trim_end().parse().ok()?,
        });
    }
    let name = message
        .strip_prefix(SLAIN_PREFIX)?
        .strip_suffix(SLAIN_SUFFIX)?;
    (!name.is_empty() && !name.contains(' ')).then(|| LogEvent::Slain {
        name: name.to_owned(),
    })
}

/// Classify an area id. Maps start with `Map` (Path of Exile 1 atlas maps
/// with `MapWorlds`), hideouts with `Hideout`, and towns end in `_town`
/// (`G1_town`, Path of Exile 1's `2_11_endgame_town`).
pub fn area_kind(area_id: &str) -> AreaKind {
    if area_id.starts_with("Map") {
        AreaKind::Map
    } else if area_id.starts_with("Hideout") {
        AreaKind::Hideout
    } else if area_id.to_ascii_lowercase().ends_with("_town") {
        AreaKind::Town
    } else {
        AreaKind::Other
    }
}

/// `2026/09/28` and `12:27:43` in local time to Unix milliseconds.
fn unix_ms(date: &str, time: &str, utc_offset_minutes: i32) -> Option<i64> {
    let mut date = date.split('/').map(str::parse::<i32>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut time = time.split(':').map(str::parse::<i64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if date.next().is_some()
        || time.next().is_some()
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(
        (days * 86_400 + hour * 3_600 + minute * 60 + second - i64::from(utc_offset_minutes) * 60)
            * 1_000,
    )
}

fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

/// Howard Hinnant's proleptic Gregorian days-from-civil conversion.
fn days_from_civil(year: i32, month: i32, day: i32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: &str = "2026/09/28 12:27:43 320000 aaa7659d [DEBUG Client 40464] Generating level 74 area \"MapWillow\" with seed 3003";
    const SLAIN: &str =
        "2026/09/28 12:28:43 380000 a528a289 [INFO Client 40464] : TestExile has been slain.";

    fn area(line: &str) -> (String, u32, u64, AreaKind) {
        match parse_line(line, 0).unwrap().event {
            LogEvent::AreaEntered {
                area_id,
                area_level,
                seed,
                kind,
            } => (area_id, area_level, seed, kind),
            other => panic!("expected an area, got {other:?}"),
        }
    }

    #[test]
    fn map_entry_is_parsed_with_level_seed_and_times() {
        let parsed = parse_line(MAP, 0).unwrap();
        // 2026-09-28 12:27:43 UTC.
        assert_eq!(parsed.occurred_at_ms, 1_790_598_463_000);
        assert_eq!(parsed.uptime_ms, 320_000);
        assert_eq!(area(MAP), ("MapWillow".to_owned(), 74, 3003, AreaKind::Map));
    }

    #[test]
    fn utc_offset_moves_the_timestamp_back() {
        let utc = parse_line(MAP, 0).unwrap().occurred_at_ms;
        let utc_plus_two = parse_line(MAP, 120).unwrap().occurred_at_ms;
        assert_eq!(utc - utc_plus_two, 2 * 3_600_000);
    }

    #[test]
    fn crlf_line_ending_is_accepted() {
        assert_eq!(parse_line(&format!("{MAP}\r"), 0), parse_line(MAP, 0));
    }

    #[test]
    fn area_kinds_are_recognised() {
        assert_eq!(area_kind("MapBluff"), AreaKind::Map);
        assert_eq!(area_kind("HideoutFelled"), AreaKind::Hideout);
        assert_eq!(area_kind("G1_town"), AreaKind::Town);
        assert_eq!(area_kind("G1_1"), AreaKind::Other);
        // Seen in a real Client.txt.
        assert_eq!(area_kind("HideoutShoreline"), AreaKind::Hideout);
        assert_eq!(area_kind("P2_Town"), AreaKind::Town);
        assert_eq!(area_kind("G_Endgame_Town"), AreaKind::Town);
        assert_eq!(area_kind("MapUberBoss_FallenStar"), AreaKind::Map);
        assert_eq!(area_kind("MapWorldsOrchard"), AreaKind::Map);
        assert_eq!(area_kind("HideoutRuinedTemple"), AreaKind::Hideout);
        assert_eq!(area_kind("2_11_endgame_town"), AreaKind::Town);
        assert_eq!(area_kind("Abyss_Depths1"), AreaKind::Other);
    }

    #[test]
    fn death_is_parsed() {
        assert_eq!(
            parse_line(SLAIN, 0).unwrap().event,
            LogEvent::Slain {
                name: "TestExile".to_owned()
            }
        );
    }

    #[test]
    fn chat_that_mentions_a_death_is_not_a_death() {
        let chat = "2026/09/28 12:28:43 380000 a528a289 [INFO Client 40464] #Global: Someone: lol has been slain.";
        assert_eq!(parse_line(chat, 0), None);
        let spaced =
            "2026/09/28 12:28:43 380000 a528a289 [INFO Client 40464] : two words has been slain.";
        assert_eq!(parse_line(spaced, 0), None);
    }

    #[test]
    fn unrelated_and_malformed_lines_are_ignored() {
        for line in [
            "",
            "garbage",
            "2026/09/28 12:27:43 320000 aaa7659d [INFO Client 40464] [SHADER] Delay: ON",
            "2026/09/28 12:27:43 320000 aaa7659d [DEBUG Client 40464] Generating level x area \"MapWillow\" with seed 3003",
            "2026/09/28 12:27:43 320000 aaa7659d [DEBUG Client 40464] Generating level 74 area \"\" with seed 3003",
            "2026/13/28 12:27:43 320000 aaa7659d [DEBUG Client 40464] Generating level 74 area \"MapWillow\" with seed 3003",
            "2026/02/30 12:27:43 320000 aaa7659d [DEBUG Client 40464] Generating level 74 area \"MapWillow\" with seed 3003",
            "2026/09/28 12:27:43 up aaa7659d [DEBUG Client 40464] Generating level 74 area \"MapWillow\" with seed 3003",
        ] {
            assert_eq!(parse_line(line, 0), None, "{line}");
        }
    }
}
