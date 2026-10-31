//! SQL task manager. Persistence and service operations run outside the UI thread.

use super::{Job, Run, Store, database_path, now, service, validate};
use crate::db::ConnectionConfig;
use crate::i18n::tr;
use std::sync::mpsc::{self, Receiver};

mod view;

enum Action {
    Load,
    Save(Job),
    Delete(i64),
    Pause(i64),
    EnableService,
}
enum DraftUpdate {
    Keep,
    Replace,
    Clear,
    Pause,
}
enum Navigation {
    New,
    Select(i64),
    Close,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Configuration,
    History,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IntervalUnit {
    Seconds,
    Minutes,
    Hours,
    Days,
}
impl IntervalUnit {
    const ALL: [Self; 4] = [Self::Seconds, Self::Minutes, Self::Hours, Self::Days];
    fn seconds(self) -> i64 {
        match self {
            Self::Seconds => 1,
            Self::Minutes => 60,
            Self::Hours => 3600,
            Self::Days => 86400,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Seconds => tr("Seconds"),
            Self::Minutes => tr("Minutes"),
            Self::Hours => tr("Hours"),
            Self::Days => tr("Days"),
        }
    }
}
#[derive(Clone, Copy)]
struct Interval {
    value: f64,
    unit: IntervalUnit,
}
impl Interval {
    fn from_seconds(seconds: i64) -> Self {
        let unit = IntervalUnit::ALL
            .into_iter()
            .rev()
            .find(|unit| seconds >= unit.seconds() && seconds % unit.seconds() == 0)
            .unwrap_or(IntervalUnit::Seconds);
        Self {
            value: seconds as f64 / unit.seconds() as f64,
            unit,
        }
    }
    fn seconds(self) -> Option<i64> {
        let seconds = self.value * self.unit.seconds() as f64;
        (seconds.is_finite() && (1.0..=31_536_000.0).contains(&seconds))
            .then(|| seconds.round() as i64)
    }
    fn change_unit(&mut self, unit: IntervalUnit) {
        self.value = self.value * self.unit.seconds() as f64 / unit.seconds() as f64;
        self.unit = unit;
    }
}

struct Snapshot {
    jobs: Vec<Job>,
    runs: Vec<Run>,
    selected: Option<i64>,
    update: DraftUpdate,
    notice: Option<&'static str>,
    running: bool,
}

pub struct Dialog {
    jobs: Vec<Job>,
    runs: Vec<Run>,
    selected: Option<i64>,
    draft: Job,
    original: Job,
    interval: Interval,
    tab: Tab,
    pending_navigation: Option<Navigation>,
    receiver: Option<Receiver<Result<Snapshot, String>>>,
    message: Option<(String, bool)>,
    running: bool,
    confirm_save: bool,
    confirm_delete: bool,
}

impl Dialog {
    fn empty_job(connections: &[ConnectionConfig]) -> Job {
        Job {
            id: 0,
            name: String::new(),
            connection: connections
                .iter()
                .find(|c| !c.driver.is_nosql())
                .map_or(0, |c| c.id),
            database: String::new(),
            sql: String::new(),
            interval_seconds: 300,
            enabled: true,
            next_run: 0,
        }
    }
    pub fn new(ctx: &egui::Context, connections: &[ConnectionConfig]) -> Self {
        let draft = Self::empty_job(connections);
        let mut dialog = Self {
            jobs: Vec::new(),
            runs: Vec::new(),
            selected: None,
            original: draft.clone(),
            interval: Interval::from_seconds(draft.interval_seconds),
            draft,
            tab: Tab::Configuration,
            pending_navigation: None,
            receiver: None,
            message: None,
            running: false,
            confirm_save: false,
            confirm_delete: false,
        };
        dialog.dispatch(ctx, Action::Load);
        dialog
    }
    fn dirty(&self) -> bool {
        let (a, b) = (&self.draft, &self.original);
        a.name != b.name
            || a.connection != b.connection
            || a.database != b.database
            || a.sql != b.sql
            || a.enabled != b.enabled
            || self.interval.seconds() != Some(b.interval_seconds)
    }
    fn valid(&self, connections: &[ConnectionConfig]) -> bool {
        !self.draft.name.trim().is_empty()
            && !self.draft.sql.trim().is_empty()
            && self.interval.seconds().is_some()
            && connections
                .iter()
                .any(|c| c.id == self.draft.connection && !c.driver.is_nosql())
    }
    fn set_draft(&mut self, draft: Job) {
        self.interval = Interval::from_seconds(draft.interval_seconds);
        self.original = draft.clone();
        self.draft = draft;
    }
    fn apply_navigation(
        &mut self,
        connections: &[ConnectionConfig],
        action: &mut Option<Action>,
    ) -> bool {
        match self.pending_navigation.take() {
            Some(Navigation::Close) => return true,
            Some(Navigation::New) => {
                self.selected = None;
                self.runs.clear();
                self.set_draft(Self::empty_job(connections));
                self.tab = Tab::Configuration;
            }
            Some(Navigation::Select(id)) => {
                if let Some(job) = self.jobs.iter().find(|job| job.id == id).cloned() {
                    self.selected = Some(id);
                    self.runs.clear();
                    self.set_draft(job);
                    *action = Some(Action::Load);
                }
            }
            None => {}
        }
        self.message = None;
        false
    }
    fn dispatch(&mut self, ctx: &egui::Context, action: Action) {
        if self.receiver.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        let selected = self.selected;
        let spawned = std::thread::Builder::new()
            .name("sql-scheduler-ui".into())
            .spawn(move || {
                let result = (|| -> anyhow::Result<Snapshot> {
                    let mut store = Store::open(&database_path()?)?;
                    let (selected, update, notice) = match action {
                        Action::Load => (selected, DraftUpdate::Keep, None),
                        Action::Save(job) => {
                            validate(&job, now())?;
                            if job.enabled {
                                service::enable()?;
                            }
                            (
                                Some(store.save(&job, now())?),
                                DraftUpdate::Replace,
                                Some("Task saved"),
                            )
                        }
                        Action::Delete(id) => {
                            store.delete(id)?;
                            (None, DraftUpdate::Clear, Some("Task deleted"))
                        }
                        Action::Pause(id) => {
                            store.pause(id)?;
                            (Some(id), DraftUpdate::Pause, Some("Task paused"))
                        }
                        Action::EnableService => {
                            service::enable()?;
                            (
                                selected,
                                DraftUpdate::Keep,
                                Some("Background scheduler enabled"),
                            )
                        }
                    };
                    Ok(Snapshot {
                        jobs: store.jobs()?,
                        runs: if let Some(id) = selected {
                            store.runs(id)?
                        } else {
                            Vec::new()
                        },
                        selected,
                        update,
                        notice,
                        running: service::is_running()?,
                    })
                })();
                let _ = sender.send(result.map_err(|error| format!("{error:#}")));
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.receiver = Some(receiver);
                self.message = None;
            }
            Err(error) => {
                self.message = Some((
                    format!("{}: {error}", tr("Could not start scheduler operation")),
                    true,
                ))
            }
        }
    }
    fn poll(&mut self, connections: &[ConnectionConfig]) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(tr("Scheduler operation stopped unexpectedly").into())
            }
        };
        self.receiver = None;
        match result {
            Ok(snapshot) => {
                self.jobs = snapshot.jobs;
                self.runs = snapshot.runs;
                self.selected = snapshot.selected;
                self.running = snapshot.running;
                match snapshot.update {
                    DraftUpdate::Replace => {
                        if let Some(job) = self
                            .jobs
                            .iter()
                            .find(|job| Some(job.id) == self.selected)
                            .cloned()
                        {
                            self.set_draft(job);
                        }
                    }
                    DraftUpdate::Clear => {
                        self.set_draft(Self::empty_job(connections));
                        self.tab = Tab::Configuration;
                    }
                    DraftUpdate::Pause => {
                        self.original.enabled = false;
                        self.draft.enabled = false;
                    }
                    DraftUpdate::Keep => {}
                }
                self.message = snapshot
                    .notice
                    .map(|notice| (tr(notice).to_string(), false));
            }
            Err(error) => self.message = Some((error, true)),
        }
    }
    pub fn show(&mut self, ctx: &egui::Context, connections: &[ConnectionConfig]) -> bool {
        self.poll(connections);
        let mut action = None;
        self.render(ctx, connections, &mut action);
        let mut close = false;
        if self.pending_navigation.is_some() {
            if self.dirty() {
                let mut discard = false;
                egui::Modal::new(egui::Id::new("discard_scheduled_sql")).show(ctx, |ui| {
                    ui.heading(tr("Unsaved changes"));
                    ui.label(tr("Discard changes to this task?"));
                    ui.horizontal(|ui| {
                        if crate::ui::theme::primary_button(ui, tr("Keep editing")).clicked() {
                            self.pending_navigation = None;
                        }
                        if ui.button(tr("Discard changes")).clicked() {
                            discard = true;
                        }
                    });
                });
                if discard {
                    close = self.apply_navigation(connections, &mut action);
                }
            } else {
                close = self.apply_navigation(connections, &mut action);
            }
        }
        if self.confirm_save {
            egui::Modal::new(egui::Id::new("confirm_scheduled_sql")).show(ctx, |ui| {
                ui.set_max_width(620.0_f32.min(ctx.content_rect().width() - 64.0));
                ui.heading(tr("Confirm task"));
                ui.label(tr("This SQL will run automatically and may change data."));
                egui::ScrollArea::both().max_height(220.0).show(ui, |ui| {
                    ui.label(egui::RichText::new(&self.draft.sql).monospace());
                });
                ui.horizontal(|ui| {
                    if crate::ui::theme::primary_button(ui, tr("Save task")).clicked() {
                        if let Some(seconds) = self.interval.seconds() {
                            let mut job = self.draft.clone();
                            job.interval_seconds = seconds;
                            action = Some(Action::Save(job));
                        }
                        self.confirm_save = false;
                    }
                    if ui.button(tr("Keep editing")).clicked() {
                        self.confirm_save = false;
                    }
                });
            });
        }
        if self.confirm_delete {
            egui::Modal::new(egui::Id::new("delete_scheduled_sql")).show(ctx, |ui| {
                ui.heading(tr("Delete task"));
                ui.label(tr("Delete this task and its history?"));
                ui.horizontal(|ui| {
                    if ui.button(tr("Confirm deletion")).clicked() {
                        self.confirm_delete = false;
                        action = Some(Action::Delete(self.draft.id));
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        self.confirm_delete = false;
                    }
                });
            });
        }
        if let Some(action) = action {
            self.dispatch(ctx, action);
        }
        close
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Driver;
    use egui_kittest::kittest::Queryable;

    fn fixture() -> Dialog {
        Dialog {
            jobs: Vec::new(),
            runs: Vec::new(),
            selected: None,
            draft: Job {
                id: 0,
                name: "task".into(),
                connection: 1,
                database: String::new(),
                sql: "DELETE FROM records".into(),
                interval_seconds: 60,
                enabled: true,
                next_run: 0,
            },
            receiver: None,
            message: None,
            original: Dialog::empty_job(&[ConnectionConfig::new(1, Driver::Sqlite)]),
            interval: Interval::from_seconds(60),
            tab: Tab::Configuration,
            pending_navigation: None,
            running: false,
            confirm_save: false,
            confirm_delete: false,
        }
    }

    #[test]
    fn footer_actions_stay_visible_on_small_viewports() {
        for size in [
            egui::vec2(400.0, 300.0),
            egui::vec2(780.0, 560.0),
            egui::vec2(1000.0, 700.0),
        ] {
            let connection = ConnectionConfig::new(1, Driver::Sqlite);
            let mut first = true;
            let mut harness = egui_kittest::Harness::builder()
                .with_size(size)
                .build_ui_state(
                    move |ui, dialog: &mut Dialog| {
                        if std::mem::take(&mut first) {
                            crate::ui::theme::install_fonts(ui.ctx());
                            return;
                        }
                        dialog.show(ui.ctx(), std::slice::from_ref(&connection));
                    },
                    fixture(),
                );
            harness.run_steps(3);
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            for label in ["Save task", "Cancel"] {
                assert!(
                    screen.contains_rect(harness.get_by_label(label).rect()),
                    "{label} is outside {size:?}"
                );
            }
        }
    }

    #[test]
    fn saving_a_task_requires_confirmation() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let dialog = fixture();
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 900.0))
            .build_ui_state(
                move |ui, dialog: &mut Dialog| {
                    if std::mem::take(&mut first) {
                        crate::ui::theme::install_fonts(ui.ctx());
                        return;
                    }
                    dialog.show(ui.ctx(), std::slice::from_ref(&connection));
                },
                dialog,
            );
        harness.run_steps(2);
        harness.get_by_label("Save task").click();
        harness.run_steps(2);
        assert!(
            harness
                .query_by_label("This SQL will run automatically and may change data.")
                .is_some()
        );
        assert!(harness.state().receiver.is_none());
        harness.get_by_label("Keep editing").click();
        harness.run_steps(3);
        assert!(!harness.state().confirm_save);
    }

    #[test]
    fn interval_units_roundtrip() {
        let mut interval = Interval::from_seconds(300);
        assert_eq!(interval.unit, IntervalUnit::Minutes);
        assert_eq!(interval.value, 5.0);
        interval.change_unit(IntervalUnit::Hours);
        assert_eq!(interval.seconds(), Some(300));
        interval.value = f64::INFINITY;
        assert_eq!(interval.seconds(), None);
    }

    #[test]
    fn navigation_preserves_unsaved_sql() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 760.0))
            .build_ui_state(
                move |ui, dialog: &mut Dialog| {
                    if std::mem::take(&mut first) {
                        crate::ui::theme::install_fonts(ui.ctx());
                        return;
                    }
                    dialog.show(ui.ctx(), std::slice::from_ref(&connection));
                },
                fixture(),
            );
        harness.run_steps(3);
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "New task")
            .click();
        harness.run_steps(3);
        assert!(
            harness
                .query_by_label("Discard changes to this task?")
                .is_some()
        );
        assert_eq!(harness.state().draft.sql, "DELETE FROM records");
        harness.get_by_label("Keep editing").click();
        harness.run_steps(3);
        assert!(harness.state().pending_navigation.is_none());
        assert_eq!(harness.state().draft.sql, "DELETE FROM records");
    }

    #[test]
    fn refresh_and_pause_keep_the_edited_draft() {
        let connections = [ConnectionConfig::new(1, Driver::Sqlite)];
        let mut dialog = fixture();
        let edited = dialog.draft.sql.clone();
        for update in [DraftUpdate::Keep, DraftUpdate::Pause] {
            let (sender, receiver) = mpsc::channel();
            dialog.receiver = Some(receiver);
            sender
                .send(Ok(Snapshot {
                    jobs: Vec::new(),
                    runs: Vec::new(),
                    selected: None,
                    update,
                    notice: None,
                    running: true,
                }))
                .unwrap();
            dialog.poll(&connections);
            assert_eq!(dialog.draft.sql, edited);
            assert!(dialog.dirty());
        }
        assert!(!dialog.draft.enabled);
    }

    #[test]
    fn confirmation_actions_fit_with_long_sql_on_small_viewports() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut dialog = fixture();
        dialog.draft.sql = "SELECT 1;\n".repeat(100);
        dialog.confirm_save = true;
        let size = egui::vec2(400.0, 300.0);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(size)
            .build_ui_state(
                move |ui, dialog: &mut Dialog| {
                    if std::mem::take(&mut first) {
                        crate::ui::theme::install_fonts(ui.ctx());
                        return;
                    }
                    dialog.show(ui.ctx(), std::slice::from_ref(&connection));
                },
                dialog,
            );
        harness.run_steps(3);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(screen.contains_rect(harness.get_by_label("Keep editing").rect()));
        assert!(screen.contains_rect(harness.get_by_label("Confirm task").rect()));
    }

    #[test]
    fn long_task_name_keeps_the_compact_new_button_visible() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut dialog = fixture();
        dialog.draft.id = 1;
        dialog.draft.name = "Very long task name ".repeat(30);
        dialog.jobs = vec![dialog.draft.clone()];
        dialog.set_draft(dialog.draft.clone());
        let size = egui::vec2(600.0, 560.0);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(size)
            .build_ui_state(
                move |ui, dialog: &mut Dialog| {
                    if std::mem::take(&mut first) {
                        crate::ui::theme::install_fonts(ui.ctx());
                        return;
                    }
                    dialog.show(ui.ctx(), std::slice::from_ref(&connection));
                },
                dialog,
            );
        harness.run_steps(3);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(
            screen.contains_rect(
                harness
                    .get_by_role_and_label(egui::accesskit::Role::Button, "New task")
                    .rect()
            )
        );
    }

    #[test]
    #[ignore = "Requires a GPU; renders the scheduler on synthetic data without running SQL"]
    fn render_scheduler_manager() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/scheduler-ui-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for (dark, history) in [(false, false), (true, false), (false, true)] {
            let connection = ConnectionConfig::new(1, Driver::Sqlite);
            let mut dialog = fixture();
            dialog.draft.id = 1;
            dialog.draft.name = "Refresh daily sales".into();
            dialog.draft.sql = "INSERT INTO daily_sales (day, total)\nSELECT date(created_at), SUM(amount)\nFROM orders\nGROUP BY date(created_at);".into();
            dialog.draft.interval_seconds = 300;
            dialog.draft.next_run = 1793448300;
            dialog.set_draft(dialog.draft.clone());
            dialog.selected = Some(1);
            dialog.jobs = vec![
                dialog.draft.clone(),
                Job {
                    id: 2,
                    name: "Archive old events".into(),
                    enabled: false,
                    ..dialog.draft.clone()
                },
            ];
            dialog.running = true;
            dialog.runs = vec![
                Run {
                    id: 3,
                    started: 1793448000,
                    finished: Some(1793448002),
                    status: "succeeded".into(),
                    message: String::new(),
                },
                Run {
                    id: 2,
                    started: 1793447700,
                    finished: Some(1793447701),
                    status: "failed".into(),
                    message:
                        "The database was unavailable. Check the saved data source and try again."
                            .into(),
                },
            ];
            if history {
                dialog.tab = Tab::History;
            }
            let mut first = true;
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(1400.0, 900.0))
                .build_ui_state(
                    move |ui, dialog: &mut Dialog| {
                        if std::mem::take(&mut first) {
                            crate::ui::theme::install_fonts(ui.ctx());
                            crate::ui::theme::apply(ui.ctx(), dark);
                            return;
                        }
                        dialog.show(ui.ctx(), std::slice::from_ref(&connection));
                    },
                    dialog,
                );
            harness.run_steps(4);
            harness
                .render()
                .unwrap()
                .save(dir.join(format!(
                    "scheduler-{}-{}.png",
                    if dark { "dark" } else { "light" },
                    if history { "history" } else { "configuration" }
                )))
                .unwrap();
        }
    }
}
