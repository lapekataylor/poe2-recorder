// SPDX-License-Identifier: GPL-3.0-or-later

//! The sidebar status card: recorder state, elapsed time, Force end, and
//! the bounded recovered problem list. The Linux recorder never reports microphone state, so there
//! is no microphone badge.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;

use poe_recorder::config::Config;
use poe_recorder::coordinator::AppSnapshot;
use poe_recorder::domain::{Problem, RecorderStatus, RecoveryAction};
use poe_recorder::storage::now_unix_ms;

use super::{ActionSink, ShellAction};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ready,
    Recording,
    Waiting,
    Overrunning,
    Finalizing,
    Invalid,
}

impl Tone {
    fn css_class(self) -> &'static str {
        match self {
            Self::Ready => "tone-ready",
            Self::Recording => "tone-recording",
            Self::Waiting => "tone-waiting",
            Self::Overrunning => "tone-overrunning",
            Self::Finalizing => "tone-finalizing",
            Self::Invalid => "tone-invalid",
        }
    }
}

/// What the card shows, derived from one snapshot. `elapsed_anchor_ms` is the
/// wall-clock anchor the card's own one-second timeout renders from; the
/// coordinator never publishes per-second snapshots for this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusView {
    pub title: String,
    pub detail: String,
    pub tone: Tone,
    pub elapsed_anchor_ms: Option<i64>,
    pub show_force_end: bool,
    pub show_spinner: bool,
}

/// Enabled flavour labels in config order, for the Ready detail line.
pub fn enabled_flavors(config: &Config) -> Vec<&'static str> {
    config
        .flavors
        .iter()
        .into_iter()
        .filter(|(_, settings)| settings.enabled)
        .map(|(flavor, _)| flavor.name())
        .collect()
}

pub fn view(snapshot: &AppSnapshot) -> StatusView {
    match &snapshot.status {
        RecorderStatus::SetupRequired => StatusView {
            title: "Setup required".to_owned(),
            detail: snapshot
                .setup_problems
                .first()
                .map(|problem| problem.message.clone())
                .unwrap_or_else(|| "Finish setup in Settings.".to_owned()),
            tone: Tone::Invalid,
            elapsed_anchor_ms: None,
            show_force_end: false,
            show_spinner: false,
        },
        RecorderStatus::WaitingForCapture => StatusView {
            title: "Waiting".to_owned(),
            detail: "Waiting for screen capture to start.".to_owned(),
            tone: Tone::Waiting,
            elapsed_anchor_ms: None,
            show_force_end: false,
            show_spinner: false,
        },
        RecorderStatus::Ready => StatusView {
            title: "Ready".to_owned(),
            detail: format!(
                "Watching game logs: {}.",
                enabled_flavors(&snapshot.config).join(", ")
            ),
            tone: Tone::Ready,
            elapsed_anchor_ms: None,
            show_force_end: false,
            show_spinner: false,
        },
        RecorderStatus::Recording {
            title,
            started_unix_ms,
            manual,
            test,
            ..
        } => StatusView {
            title: if *manual {
                "Manual recording".to_owned()
            } else if *test {
                "Test recording".to_owned()
            } else {
                "Recording".to_owned()
            },
            detail: title.clone(),
            tone: Tone::Recording,
            elapsed_anchor_ms: Some(*started_unix_ms),
            // Force end is automatic-only; manual recordings get their own Stop.
            show_force_end: !manual,
            show_spinner: false,
        },
        RecorderStatus::Overrunning {
            title,
            started_unix_ms,
        } => StatusView {
            title: "Overrunning".to_owned(),
            detail: format!("{title}: recording a few seconds of aftermath."),
            tone: Tone::Overrunning,
            elapsed_anchor_ms: Some(*started_unix_ms),
            show_force_end: false,
            show_spinner: false,
        },
        RecorderStatus::Finalizing { title } => StatusView {
            title: "Saving".to_owned(),
            detail: match &snapshot.work {
                Some(work) => match work.total {
                    Some(total) if total > 0 => {
                        format!("{title}: {}%", work.completed.saturating_mul(100) / total)
                    }
                    _ => title.clone(),
                },
                None => title.clone(),
            },
            tone: Tone::Finalizing,
            elapsed_anchor_ms: None,
            show_force_end: false,
            show_spinner: true,
        },
    }
}

/// Format the elapsed anchor as `m:ss` (or `h:mm:ss`) against `now_unix_ms`.
pub fn elapsed_label(anchor_unix_ms: i64, now_unix_ms: i64) -> String {
    // `saturating_sub` on i64 clamps at i64::MIN, not zero; clamp the elapsed
    // difference so a now-before-anchor snapshot never renders a negative time.
    let seconds = now_unix_ms.saturating_sub(anchor_unix_ms).max(0) / 1_000;
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub(crate) fn recovery_label(action: RecoveryAction) -> &'static str {
    match action {
        RecoveryAction::OpenSettings => "Open Settings",
        RecoveryAction::ReselectCaptureTarget => "Reselect capture target",
        RecoveryAction::Retry => "Try again",
        RecoveryAction::OpenLogs => "Open logs",
        RecoveryAction::Quit => "Quit",
    }
}

pub(crate) fn shell_action(action: RecoveryAction) -> ShellAction {
    match action {
        RecoveryAction::OpenSettings => ShellAction::OpenSettings,
        RecoveryAction::ReselectCaptureTarget => {
            ShellAction::Command(poe_recorder::coordinator::Command::ReselectCaptureTarget)
        }
        RecoveryAction::Retry => ShellAction::Retry,
        RecoveryAction::OpenLogs => ShellAction::OpenLogs,
        RecoveryAction::Quit => ShellAction::Quit,
    }
}

/// The status card widget. All behavior arrives through `apply`; all outgoing
/// intent goes through the one `ActionSink`.
pub struct StatusCard {
    pub widget: gtk4::Box,
    light: gtk4::Box,
    title: gtk4::Label,
    elapsed: gtk4::Label,
    spinner: libadwaita::Spinner,
    detail: gtk4::Label,
    force_end: gtk4::Button,
    problems_expander: gtk4::Expander,
    problems: gtk4::Box,
    tray_note: gtk4::Label,
    sink: ActionSink,
    elapsed_anchor: Rc<Cell<Option<i64>>>,
    timer_running: Rc<Cell<bool>>,
    /// What the problem list was last built from. Rebuilding it per snapshot
    /// throws away GTK objects, relayouts the rail, and collapses any problem
    /// row the user had expanded.
    rendered_problems: RefCell<Vec<Problem>>,
    rendered_tone: Cell<Option<Tone>>,
}

impl StatusCard {
    pub fn new(sink: ActionSink) -> Self {
        let widget = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        widget.add_css_class("card");
        widget.add_css_class("wr-status-card");
        widget.set_margin_top(12);
        widget.set_margin_bottom(12);
        widget.set_margin_start(12);
        widget.set_margin_end(12);

        let light = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        light.add_css_class("status-light");
        light.add_css_class("tone-waiting");
        light.set_valign(gtk4::Align::Center);
        light.set_tooltip_text(Some("Recorder status"));

        let title = gtk4::Label::new(Some("Starting"));
        title.add_css_class("heading");
        title.set_hexpand(true);
        title.set_xalign(0.0);

        let elapsed = gtk4::Label::new(None);
        elapsed.add_css_class("monospace");
        elapsed.set_tooltip_text(Some("Elapsed recording time"));

        let spinner = libadwaita::Spinner::new();
        spinner.set_visible(false);

        let title_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        title_row.append(&light);
        title_row.append(&title);
        title_row.append(&elapsed);
        title_row.append(&spinner);

        let detail = gtk4::Label::new(None);
        detail.set_xalign(0.0);
        detail.set_wrap(true);
        detail.add_css_class("dim-label");
        detail.add_css_class("caption");

        let force_end = gtk4::Button::with_label("Force end");
        force_end.add_css_class("destructive-action");
        force_end.set_halign(gtk4::Align::End);
        force_end.set_tooltip_text(Some(
            "End the current recording now. Normally this is not required.",
        ));
        force_end.set_visible(false);
        {
            let sink = Rc::clone(&sink);
            let force_end_widget = force_end.clone();
            force_end.connect_clicked(move |_| {
                if !sink(ShellAction::Command(
                    poe_recorder::coordinator::Command::ForceEnd,
                )) {
                    force_end_widget.set_sensitive(false);
                    let force_end_widget = force_end_widget.clone();
                    gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_secs(2),
                        move || force_end_widget.set_sensitive(true),
                    );
                }
            });
        }

        let problems = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        let problems_expander = gtk4::Expander::new(Some("Problems"));
        problems_expander.set_child(Some(&problems));
        problems_expander.set_visible(false);

        let tray_note = gtk4::Label::new(Some(
            "No system tray found: closing the window quits PoE Recorder.",
        ));
        tray_note.set_xalign(0.0);
        tray_note.set_wrap(true);
        tray_note.add_css_class("dim-label");
        tray_note.add_css_class("caption");
        tray_note.set_visible(false);

        widget.append(&title_row);
        widget.append(&detail);
        widget.append(&force_end);
        widget.append(&problems_expander);
        widget.append(&tray_note);

        Self {
            widget,
            light,
            title,
            elapsed,
            spinner,
            detail,
            force_end,
            problems_expander,
            problems,
            tray_note,
            sink,
            elapsed_anchor: Rc::new(Cell::new(None)),
            timer_running: Rc::new(Cell::new(false)),
            rendered_problems: RefCell::new(Vec::new()),
            rendered_tone: Cell::new(None),
        }
    }

    pub fn apply(&self, snapshot: &AppSnapshot, now_unix_ms: i64) {
        let view = view(snapshot);

        if self.rendered_tone.replace(Some(view.tone)) != Some(view.tone) {
            for class in [
                "tone-ready",
                "tone-recording",
                "tone-waiting",
                "tone-overrunning",
                "tone-finalizing",
                "tone-invalid",
            ] {
                self.light.remove_css_class(class);
            }
            self.light.add_css_class(view.tone.css_class());
        }

        self.title.set_label(&view.title);
        self.detail.set_label(&view.detail);
        self.detail.set_visible(!view.detail.is_empty());
        self.spinner.set_visible(view.show_spinner);
        self.force_end.set_visible(view.show_force_end);

        self.elapsed_anchor.set(view.elapsed_anchor_ms);
        if let Some(anchor) = view.elapsed_anchor_ms {
            self.elapsed.set_label(&elapsed_label(anchor, now_unix_ms));
            self.elapsed.set_visible(true);
            self.ensure_timer();
        } else {
            self.elapsed.set_visible(false);
        }

        self.apply_problems(&snapshot.problems);
    }

    /// The bounded recovered-problem list: one expandable row per problem
    /// with its technical detail and one recovery action. Unchanged lists keep
    /// their rows, so an expanded problem stays expanded.
    fn apply_problems(&self, problems: &[Problem]) {
        if *self.rendered_problems.borrow() == problems {
            return;
        }
        self.rendered_problems.borrow_mut().clear();
        self.rendered_problems
            .borrow_mut()
            .extend_from_slice(problems);
        self.problems_expander.set_visible(!problems.is_empty());
        self.problems_expander
            .set_label(Some(&format!("Problems ({})", problems.len())));
        while let Some(child) = self.problems.first_child() {
            self.problems.remove(&child);
        }
        for problem in problems {
            let row = gtk4::Expander::new(Some(&problem.summary));
            let body = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            if let Some(detail) = &problem.safe_detail {
                let detail_label = gtk4::Label::new(Some(detail));
                detail_label.add_css_class("problem-detail");
                detail_label.set_selectable(true);
                detail_label.set_wrap(true);
                detail_label.set_xalign(0.0);
                body.append(&detail_label);
            }
            if let Some(action) = problem.recovery_action {
                let button = gtk4::Button::with_label(recovery_label(action));
                button.set_halign(gtk4::Align::End);
                let sink = Rc::clone(&self.sink);
                button.connect_clicked(move |_| {
                    sink(shell_action(action));
                });
                body.append(&button);
            }
            row.set_child(Some(&body));
            self.problems.append(&row);
        }
    }

    pub fn set_tray_available(&self, available: bool) {
        self.tray_note.set_visible(!available);
    }

    /// One one-second timeout renders the elapsed anchor while it is visible.
    fn ensure_timer(&self) {
        if self.timer_running.replace(true) {
            return;
        }
        let anchor = Rc::clone(&self.elapsed_anchor);
        let timer_running = Rc::clone(&self.timer_running);
        let elapsed = self.elapsed.downgrade();
        gtk4::glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            let Some(elapsed) = elapsed.upgrade() else {
                timer_running.set(false);
                return gtk4::glib::ControlFlow::Break;
            };
            let Some(anchor_ms) = anchor.get() else {
                timer_running.set(false);
                return gtk4::glib::ControlFlow::Break;
            };
            elapsed.set_label(&elapsed_label(anchor_ms, now_unix_ms()));
            gtk4::glib::ControlFlow::Continue
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poe_recorder::domain::Category;

    fn snapshot_with(status: RecorderStatus) -> AppSnapshot {
        crate::ui::window::tests::snapshot_with(status, Config::default(), Vec::new())
    }

    #[test]
    fn force_end_is_visible_only_for_automatic_recording() {
        let recording = |manual, test| RecorderStatus::Recording {
            category: Category::MapRuns,
            title: "Bluff".to_owned(),
            started_unix_ms: 1_000,
            manual,
            test,
        };
        assert!(view(&snapshot_with(recording(false, false))).show_force_end);
        assert!(view(&snapshot_with(recording(false, true))).show_force_end);
        assert!(!view(&snapshot_with(recording(true, false))).show_force_end);
        assert!(
            !view(&snapshot_with(RecorderStatus::Overrunning {
                title: "Bluff".to_owned(),
                started_unix_ms: 1_000,
            }))
            .show_force_end
        );
    }

    #[test]
    fn elapsed_format_stays_compact() {
        assert_eq!(elapsed_label(1_000, 61_500), "1:00");
        assert_eq!(elapsed_label(0, 3_661_000), "1:01:01");
        assert_eq!(elapsed_label(5_000, 1_000), "0:00");
    }
}
