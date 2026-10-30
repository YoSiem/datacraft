//! Non-blocking editor for SQL schedules.

use super::{Job, Run, Store, database_path, now, service, validate};
use crate::db::ConnectionConfig;
use crate::i18n::tr;
use std::sync::mpsc::{self, Receiver};

enum Action {
    Load,
    Save(Job),
    Delete(i64),
    Pause(i64),
    EnableService,
}

struct Snapshot {
    jobs: Vec<Job>,
    runs: Vec<Run>,
    selected: Option<i64>,
    update_draft: bool,
    notice: Option<&'static str>,
    running: bool,
}

pub struct Dialog {
    jobs: Vec<Job>,
    runs: Vec<Run>,
    selected: Option<i64>,
    draft: Job,
    receiver: Option<Receiver<Result<Snapshot, String>>>,
    message: Option<String>,
    running: bool,
    confirm_save: bool,
    confirm_delete: bool,
}

impl Dialog {
    pub fn new(ctx: &egui::Context, connections: &[ConnectionConfig]) -> Self {
        let mut dialog = Self {
            jobs: Vec::new(),
            runs: Vec::new(),
            selected: None,
            draft: Job {
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
            },
            receiver: None,
            message: None,
            running: false,
            confirm_save: false,
            confirm_delete: false,
        };
        dialog.dispatch(ctx, Action::Load);
        dialog
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
                    let path = database_path()?;
                    let mut store = Store::open(&path)?;
                    let (selected, update_draft, notice) = match action {
                        Action::Load => (selected, false, None),
                        Action::Save(job) => {
                            validate(&job, now())?;
                            if job.enabled {
                                service::enable()?;
                            }
                            (Some(store.save(&job, now())?), true, Some("Task saved"))
                        }
                        Action::Delete(id) => {
                            store.delete(id)?;
                            (None, true, Some("Task deleted"))
                        }
                        Action::Pause(id) => {
                            store.pause(id)?;
                            (Some(id), true, Some("Task paused"))
                        }
                        Action::EnableService => {
                            service::enable()?;
                            (selected, false, Some("Background scheduler enabled"))
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
                        update_draft,
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
                self.message = Some(format!(
                    "{}: {error}",
                    tr("Could not start scheduler operation")
                ))
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, connections: &[ConnectionConfig]) -> bool {
        if let Some(receiver) = &self.receiver
            && let result @ (Ok(_) | Err(mpsc::TryRecvError::Disconnected)) = receiver.try_recv()
        {
            self.receiver = None;
            let result = result
                .unwrap_or_else(|_| Err(tr("Scheduler operation stopped unexpectedly").into()));
            match result {
                Ok(snapshot) => {
                    self.jobs = snapshot.jobs;
                    self.runs = snapshot.runs;
                    self.selected = snapshot.selected;
                    self.running = snapshot.running;
                    if snapshot.update_draft {
                        if let Some(job) =
                            self.jobs.iter().find(|job| Some(job.id) == self.selected)
                        {
                            self.draft = job.clone();
                        } else {
                            self.draft.id = 0;
                        }
                    }
                    self.message = snapshot.notice.map(|notice| tr(notice).to_string());
                }
                Err(error) => self.message = Some(error),
            }
        }
        let mut close = false;
        let mut action = None;
        egui::Window::new(tr("SQL scheduler"))
            .id(egui::Id::new("sql_scheduler"))
            .default_width(720.0)
            .max_height((ctx.content_rect().height() - 40.0).max(200.0))
            .vscroll(true)
            .show(ctx, |ui| {
                if self.receiver.is_some() {
                    ui.label(tr("Loading…"));
                    if ui.button(tr("Close")).clicked() {
                        close = true;
                    }
                    return;
                }
                ui.label(tr(
                    "Runs while you are signed in, including when DataCraft is closed.",
                ));
                ui.label(tr(
                    "SQL is saved on this computer. Do not put passwords or tokens in SQL.",
                ));
                ui.label(if self.running {
                    tr("Background scheduler is running")
                } else {
                    tr("Background scheduler is not running")
                });
                if ui.button(tr("Enable background scheduler")).clicked() {
                    action = Some(Action::EnableService);
                }
                if let Some(message) = &self.message {
                    ui.label(message);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_min_width(170.0);
                        ui.heading(tr("Tasks"));
                        for job in &self.jobs {
                            if ui
                                .selectable_label(self.selected == Some(job.id), &job.name)
                                .clicked()
                            {
                                self.selected = Some(job.id);
                                self.draft = job.clone();
                                self.confirm_save = false;
                                self.confirm_delete = false;
                                action = Some(Action::Load);
                            }
                        }
                        if ui.button(tr("New task")).clicked() {
                            self.selected = None;
                            self.runs.clear();
                            self.confirm_save = false;
                            self.confirm_delete = false;
                            self.draft = Job {
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
                            };
                        }
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.label(tr("Name"));
                        ui.text_edit_singleline(&mut self.draft.name);
                        ui.label(tr("Data source"));
                        let label = connections
                            .iter()
                            .find(|c| c.id == self.draft.connection)
                            .map_or(tr("Select a data source"), |c| c.name.as_str());
                        egui::ComboBox::from_id_salt("scheduler_connection")
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                for connection in
                                    connections.iter().filter(|c| !c.driver.is_nosql())
                                {
                                    ui.selectable_value(
                                        &mut self.draft.connection,
                                        connection.id,
                                        &connection.name,
                                    );
                                }
                            });
                        ui.label(tr("Database (blank uses the default)"));
                        ui.text_edit_singleline(&mut self.draft.database);
                        ui.label(tr("SQL"));
                        ui.add(
                            egui::TextEdit::multiline(&mut self.draft.sql)
                                .desired_rows(6)
                                .desired_width(470.0),
                        );
                        ui.horizontal(|ui| {
                            ui.label(tr("Interval (seconds)"));
                            ui.add(
                                egui::DragValue::new(&mut self.draft.interval_seconds)
                                    .range(1..=31_536_000),
                            );
                            ui.checkbox(&mut self.draft.enabled, tr("Enabled"));
                        });
                        ui.horizontal(|ui| {
                            let valid_connection = connections
                                .iter()
                                .any(|c| c.id == self.draft.connection && !c.driver.is_nosql());
                            if ui
                                .add_enabled(
                                    valid_connection && self.receiver.is_none(),
                                    egui::Button::new(tr("Save task")),
                                )
                                .clicked()
                            {
                                self.confirm_save = true;
                            }
                            if self.draft.id != 0
                                && self.draft.enabled
                                && ui.button(tr("Pause task")).clicked()
                            {
                                action = Some(Action::Pause(self.draft.id));
                            }
                            if self.draft.id != 0 && ui.button(tr("Delete task")).clicked() {
                                self.confirm_delete = true;
                            }
                            if ui.button(tr("Refresh history")).clicked() {
                                action = Some(Action::Load);
                            }
                        });
                        ui.separator();
                        if let Some(job) = self
                            .jobs
                            .iter()
                            .find(|job| job.id == self.draft.id && job.enabled)
                            && let Some(time) = chrono::DateTime::from_timestamp(job.next_run, 0)
                        {
                            ui.label(format!(
                                "{}: {}",
                                tr("Next run"),
                                time.with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M:%S")
                            ));
                        }
                        ui.heading(tr("Recent runs"));
                        egui::ScrollArea::vertical()
                            .max_height(150.0)
                            .show(ui, |ui| {
                                for run in &self.runs {
                                    let time = chrono::DateTime::from_timestamp(run.started, 0)
                                        .map(|t| {
                                            t.with_timezone(&chrono::Local)
                                                .format("%Y-%m-%d %H:%M:%S")
                                                .to_string()
                                        })
                                        .unwrap_or_default();
                                    let duration = run
                                        .finished
                                        .map(|finished| {
                                            format!(" · {} s", finished.saturating_sub(run.started))
                                        })
                                        .unwrap_or_default();
                                    let status = match run.status.as_str() {
                                        "running" => tr("Running"),
                                        "succeeded" => tr("Succeeded"),
                                        "failed" => tr("Failed"),
                                        "interrupted" => tr("Interrupted"),
                                        _ => run.status.as_str(),
                                    };
                                    ui.label(format!(
                                        "#{}, {time} · {status}{duration} · {}",
                                        run.id, run.message
                                    ));
                                }
                            });
                    });
                });
                ui.separator();
                if ui.button(tr("Close")).clicked() {
                    close = true;
                }
            });
        if self.confirm_save {
            egui::Modal::new(egui::Id::new("confirm_scheduled_sql")).show(ctx, |ui| {
                ui.label(tr("This SQL will run automatically and may change data."));
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&self.draft.sql).monospace());
                    });
                ui.horizontal(|ui| {
                    if ui.button(tr("Confirm task")).clicked() {
                        self.confirm_save = false;
                        action = Some(Action::Save(self.draft.clone()));
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        self.confirm_save = false;
                    }
                });
            });
        }
        if self.confirm_delete {
            egui::Modal::new(egui::Id::new("delete_scheduled_sql")).show(ctx, |ui| {
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

    #[test]
    fn saving_a_task_requires_confirmation() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let dialog = Dialog {
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
            running: false,
            confirm_save: false,
            confirm_delete: false,
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 900.0))
            .build_ui_state(
                move |ui, dialog: &mut Dialog| {
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
        harness.get_by_label("Cancel").click();
        harness.run_steps(3);
        assert!(!harness.state().confirm_save);
    }
}
