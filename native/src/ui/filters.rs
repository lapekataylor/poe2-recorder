// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure suggestion-chip and date-range filtering. No GTK here: this module is
//! table-driven and unit tested on its own.
//!
//! A chip is a numeric grouping plus its label; icon and colour are a
//! deterministic function of the grouping. A row passes only when every
//! selected chip occurs in its entry (AND), and, when both endpoints exist,
//! its start falls inside the inclusive date range.

use std::collections::BTreeSet;

use poe_recorder::domain::{ActivityDetails, LibraryEntry, Outcome};

// Chip groupings. Distinct groupings keep otherwise-equal labels from
// colliding in matching.
const GROUP_PROTECTION: u16 = 101;
const GROUP_TAGGED: u16 = 102;
const GROUP_NAME: u16 = 200;
const GROUP_MAP: u16 = 202;
const GROUP_LEVEL: u16 = 206;
const GROUP_RESULT: u16 = 50;

/// One selectable/encoded search suggestion. `Ord` gives the suggestion list a
/// stable display order (grouping, then label).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chip {
    pub group: u16,
    pub label: String,
}

impl Chip {
    fn new(group: u16, label: impl Into<String>) -> Self {
        Self {
            group,
            label: label.into(),
        }
    }

    /// A stock symbolic icon for the chip's grouping. Game-specific art is not
    /// redistributable, so chips are icon+text.
    pub fn icon_name(&self) -> &'static str {
        match self.group {
            GROUP_PROTECTION => "starred-symbolic",
            GROUP_TAGGED => "tag-symbolic",
            GROUP_NAME => "avatar-default-symbolic",
            GROUP_MAP => "mark-location-symbolic",
            GROUP_LEVEL => "security-high-symbolic",
            GROUP_RESULT => "emblem-ok-symbolic",
            _ => "system-search-symbolic",
        }
    }

    /// A per-grouping tint class for the chip pill.
    pub fn css_class(&self) -> &'static str {
        match self.group {
            GROUP_NAME => "wr-chip-name",
            GROUP_MAP => "wr-chip-place",
            GROUP_LEVEL => "wr-chip-difficulty",
            GROUP_RESULT => "wr-chip-result",
            _ => "wr-chip-generic",
        }
    }
}

/// Every suggestion the entry contributes.
pub fn suggestions_for_entry(entry: &LibraryEntry) -> Vec<Chip> {
    let mut chips = Vec::new();

    // Generic suggestions (category independent).
    chips.push(Chip::new(
        GROUP_PROTECTION,
        if entry.protected {
            "Starred"
        } else {
            "Not Starred"
        },
    ));
    if entry.tag.is_some() {
        chips.push(Chip::new(GROUP_TAGGED, "Tagged"));
    }
    if let Some(player) = &entry.player
        && !player.name.is_empty()
    {
        chips.push(Chip::new(GROUP_NAME, player.name.clone()));
    }

    // Category-specific suggestions.
    match &entry.details {
        ActivityDetails::MapRun {
            map_name,
            area_level,
            deaths,
            ..
        } => {
            if !map_name.is_empty() {
                chips.push(Chip::new(GROUP_MAP, map_name.clone()));
            }
            chips.push(Chip::new(GROUP_LEVEL, format!("Level {area_level}")));
            chips.push(Chip::new(
                GROUP_RESULT,
                if *deaths == 0 { "Deathless" } else { "Died" },
            ));
        }
        ActivityDetails::Clip { .. } | ActivityDetails::Manual => {
            if entry.outcome == Outcome::Abandoned {
                chips.push(Chip::new(GROUP_RESULT, "Abandoned"));
            }
        }
    }

    chips
}

/// Narrow the available suggestions: case-insensitive substring on the label,
/// excluding already-selected labels.
/// Typing narrows only; it does not filter rows until a suggestion is chosen.
pub fn narrow(available: &[Chip], query: &str, selected: &[Chip], limit: usize) -> Vec<Chip> {
    let needle = query.trim().to_lowercase();
    available
        .iter()
        .filter(|chip| {
            !selected.iter().any(|s| s.label == chip.label)
                && (needle.is_empty() || chip.label.to_lowercase().contains(&needle))
        })
        .take(limit)
        .cloned()
        .collect()
}

/// A row passes when every selected chip occurs in its suggestion set and its
/// start is inside the date range.
pub fn row_matches(
    combined: &BTreeSet<Chip>,
    start_unix_ms: i64,
    selected: &[Chip],
    range: Option<(i64, i64)>,
) -> bool {
    range.is_none_or(|(start, end)| start_unix_ms >= start && start_unix_ms <= end)
        && selected.iter().all(|chip| combined.contains(chip))
}

/// The union of suggestions over a set of entries.
pub fn combined_suggestions<'a>(
    entries: impl IntoIterator<Item = &'a LibraryEntry>,
) -> BTreeSet<Chip> {
    entries
        .into_iter()
        .flat_map(suggestions_for_entry)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use poe_recorder::domain::{Category, PlayerSummary};

    fn map_run(deaths: u32) -> LibraryEntry {
        let mut entry = crate::ui::window::tests::entry(Category::MapRuns, "T", 1_000);
        entry.details = ActivityDetails::MapRun {
            area_id: "MapHiddenGrotto".to_owned(),
            map_name: "Hidden Grotto".to_owned(),
            area_level: 80,
            seed: 7,
            deaths,
            portal_trips: 0,
            away_ms: 0,
        };
        entry.outcome = Outcome::Complete;
        entry
    }

    fn labels(chips: &[Chip]) -> Vec<&str> {
        chips.iter().map(|chip| chip.label.as_str()).collect()
    }

    #[test]
    fn map_run_suggestions_cover_map_level_character_and_deaths() {
        let mut entry = map_run(1);
        entry.protected = true;
        entry.player = Some(PlayerSummary {
            name: "Alice".to_owned(),
        });
        let got = suggestions_for_entry(&entry);
        let got = labels(&got);
        for expected in ["Starred", "Alice", "Hidden Grotto", "Level 80", "Died"] {
            assert!(got.contains(&expected), "missing {expected}: {got:?}");
        }
        let clean = suggestions_for_entry(&map_run(0));
        assert!(labels(&clean).contains(&"Deathless"));
    }

    #[test]
    fn and_matching_and_dates() {
        let mut entry = map_run(0);
        entry.player = Some(PlayerSummary {
            name: "Alice".to_owned(),
        });
        let combined = combined_suggestions([&entry]);
        let selected = vec![
            Chip::new(GROUP_NAME, "Alice"),
            Chip::new(GROUP_MAP, "Hidden Grotto"),
        ];
        assert!(row_matches(&combined, 1_000, &selected, None));

        // A chip the entry does not have fails.
        let missing = vec![Chip::new(GROUP_NAME, "Carol")];
        assert!(!row_matches(&combined, 1_000, &missing, None));

        // Date range is inclusive and only applied when both endpoints exist.
        assert!(row_matches(&combined, 1_000, &[], Some((1_000, 2_000))));
        assert!(!row_matches(&combined, 999, &[], Some((1_000, 2_000))));
    }

    #[test]
    fn narrowing_excludes_selected_and_matches_substring() {
        let available = vec![
            Chip::new(GROUP_NAME, "Alice"),
            Chip::new(GROUP_NAME, "Bob"),
            Chip::new(GROUP_MAP, "Frozen Falls"),
        ];
        let selected = vec![Chip::new(GROUP_NAME, "Bob")];
        let narrowed = narrow(&available, "o", &selected, usize::MAX);
        // "Bob" excluded (selected); "Frozen Falls" matches the "o".
        assert_eq!(labels(&narrowed), vec!["Frozen Falls"]);
    }

    #[test]
    fn combined_suggestions_deduplicate_repeated_labels() {
        let a = map_run(0);
        let b = map_run(0);
        let combined = combined_suggestions([&a, &b]);
        assert_eq!(combined.len(), suggestions_for_entry(&a).len());
    }
}
