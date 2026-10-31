//! Find values in every table of a database, with bounded result previews.

use crate::app::{ConnState, ConnStatus};
use crate::db::schema_search::{SearchReport, SearchRequest};
use crate::i18n::{tr, trf};
use crate::ui::explorer::{ExplorerAction, TableRef};
use crate::ui::object_browser::{BrowserAction, ObjectScope};
use crate::ui::results::{self, ResultView};
use crate::worker::Request;
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Default)]
pub struct SchemaSearch {
    pub open: bool,
    conn: u64,
    db: String,
    /// Database of the shown results (the combo can change after the search).
    searched_db: String,
    schema: String,
    value: String,
    exact: bool,
    case_sensitive: bool,
    request_id: u64,
    cancel: Option<Arc<Notify>>,
    report: Option<Result<SearchReport, String>>,
    preview: Option<ResultView>,
    selected: Option<usize>,
}

impl SchemaSearch {
    pub fn open(&mut self, scope: &ObjectScope) {
        self.open = true;
        if self.conn != scope.conn || self.db != scope.db {
            self.stop();
            self.request_id = self.request_id.wrapping_add(1);
            self.report = None;
            self.preview = None;
            self.selected = None;
        }
        self.conn = scope.conn;
        self.db.clone_from(&scope.db);
        if self.report.is_none() {
            self.searched_db.clone_from(&scope.db);
        }
        self.schema = scope.schema.clone().unwrap_or_default();
    }

    fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.notify_one();
        }
    }

    pub fn receive(&mut self, id: u64, result: Result<SearchReport, String>) {
        if id != self.request_id {
            return;
        }
        self.cancel = None;
        self.report = Some(result);
    }

    pub fn show(&mut self, ctx: &egui::Context, connections: &[ConnState]) -> Vec<BrowserAction> {
        let mut actions = Vec::new();
        if !self.open {
            return actions;
        }
        let mut close = false;
        egui::Modal::new(egui::Id::new("schema_search")).show(ctx, |ui| {
            ui.set_width(860.0_f32.min(ctx.content_rect().width() - 60.0).max(300.0));
            ui.heading(tr("Find in Schema…"));
            let connection = connections.iter().find(|c| c.cfg.id == self.conn);
            let running = self.cancel.is_some();
            let connected = connection.is_some_and(|c| c.status == ConnStatus::Connected);
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal(|ui| {
                    if let Some(c) = connection { ui.label(&c.cfg.name); }
                    ui.label(tr("Database"));
                    egui::ComboBox::from_id_salt("search_database").selected_text(&self.db).show_ui(ui, |ui| {
                        if let Some(c) = connection {
                            for db in &c.databases {
                                if ui.selectable_value(&mut self.db, db.name.clone(), &db.name).changed() { self.schema.clear(); }
                            }
                        }
                    });
                    ui.label(tr("Schema"));
                    ui.add(egui::TextEdit::singleline(&mut self.schema).hint_text(tr("All schemas")).desired_width(150.0));
                });
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.value).hint_text(tr("Text or value to find")).desired_width(330.0));
                    ui.checkbox(&mut self.exact, tr("Exact value"));
                    ui.checkbox(&mut self.case_sensitive, tr("Case sensitive"));
                    if ui.add_enabled(connected && !self.value.is_empty(), egui::Button::new(tr("Search"))).clicked() {
                        self.request_id = self.request_id.wrapping_add(1);
                        let cancel = Arc::new(Notify::new());
                        actions.push(BrowserAction::Search(Box::new(Request::SchemaSearch {
                            conn: self.conn, request_id: self.request_id,
                            request: SearchRequest { db: self.db.clone(), schema: (!self.schema.trim().is_empty()).then(|| self.schema.trim().to_string()),
                                value: self.value.clone(), exact: self.exact, case_sensitive: self.case_sensitive },
                            cancel: cancel.clone(),
                        })));
                        self.cancel = Some(cancel);
                        self.searched_db.clone_from(&self.db);
                        self.report = None;
                        self.preview = None;
                        self.selected = None;
                    }
                });
            });
            ui.label(tr("Searches all tables. Preview: up to 100 matching rows per table. Binary columns are skipped."));
            ui.label(tr("Each table is read in full on the server (15 s per table, 5 minutes in all): avoid busy production databases."));
            if connection.is_some_and(|c| c.cfg.driver == crate::db::Driver::Snowflake) {
                ui.colored_label(ui.visuals().warn_fg_color, tr("Snowflake: the search runs on the warehouse and uses credits."));
            }
            if running {
                ui.horizontal(|ui| { ui.spinner(); ui.label(tr("Searching tables…"));
                    if ui.button(tr("Cancel")).clicked() { self.stop(); }
                });
            }
            if let Some(report) = &self.report {
                match report {
                    Err(error) => { ui.colored_label(ui.visuals().error_fg_color, error); }
                    Ok(report) => {
                        ui.label(trf("{scanned} / {total} tables searched", &[("scanned", &report.scanned), ("total", &report.total)]));
                        if let Some(reason) = &report.stopped { ui.colored_label(ui.visuals().warn_fg_color, reason); }
                        if report.matches.is_empty() && report.stopped.is_none() { ui.label(tr("No matches")); }
                        egui::ScrollArea::vertical().id_salt("search_tables").max_height(160.0).show(ui, |ui| {
                            for (i, entry) in report.matches.iter().enumerate() {
                                let suffix = if entry.result.rows.len() == crate::db::schema_search::PREVIEW_ROWS { "+" } else { "" };
                                if ui.selectable_label(self.selected == Some(i), format!("{}.{}   ({}{suffix})", entry.schema, entry.table, entry.result.rows.len())).clicked() {
                                    self.selected = Some(i);
                                    let mut view = ResultView::new(entry.result.clone(), self.request_id.wrapping_mul(10000).wrapping_add(i as u64));
                                    view.edit_checked = true;
                                    self.preview = Some(view);
                                }
                            }
                        });
                        if !report.errors.is_empty() {
                            ui.collapsing(trf("{n} tables could not be searched", &[("n", &report.errors.len())]), |ui| {
                                egui::ScrollArea::vertical().max_height(100.0).show(ui, |ui| {
                                    for error in &report.errors { ui.label(error); }
                                });
                            });
                        }
                        if let Some(i) = self.selected && let Some(entry) = report.matches.get(i)
                            && ui.button(tr("Open table")).clicked() {
                            actions.push(BrowserAction::Explorer(ExplorerAction::OpenTable(TableRef {
                                conn: self.conn, db: self.searched_db.clone(), schema: entry.schema.clone(), table: entry.table.clone(),
                            })));
                            close = true;
                        }
                    }
                }
            }
            if let Some(view) = &mut self.preview {
                ui.separator();
                ui.allocate_ui(egui::vec2(ui.available_width(), 240.0), |ui| {
                    results::show_result(ui, view, false, None, false, true);
                });
            }
            if ui.button(tr("Close")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
        });
        if close {
            self.open = false;
            self.stop();
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema_search::TableMatch;
    use crate::db::{ConnectionConfig, DatabaseInfo, Driver, ResultSet};
    use egui_kittest::kittest::Queryable;

    fn dialog_scenario(render: bool) {
        let mut search = SchemaSearch::default();
        search.open(&ObjectScope {
            conn: 7,
            db: "shop".into(),
            schema: Some("public".into()),
            kind: None,
        });
        search.value = "alice".into();
        let request_id = search.request_id;
        search.receive(request_id.wrapping_sub(1), Err("stale".into()));
        assert!(search.report.is_none());
        search.receive(
            request_id,
            Ok(SearchReport {
                scanned: 12,
                total: 12,
                matches: vec![TableMatch {
                    schema: "public".into(),
                    table: "customers".into(),
                    result: ResultSet {
                        columns: vec!["id".into(), "name".into(), "email".into()],
                        rows: vec![vec![
                            Some("1".into()),
                            Some("Alice".into()),
                            Some("alice@example.test".into()),
                        ]],
                        ..Default::default()
                    },
                }],
                ..Default::default()
            }),
        );
        let mut connection = ConnState::new(ConnectionConfig::new(7, Driver::Postgres));
        connection.status = ConnStatus::Connected;
        connection.databases.push(DatabaseInfo {
            name: "shop".into(),
            current: true,
            schemas: None,
        });
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1050.0, 750.0))
            .build_ui_state(
                move |ui, state: &mut (SchemaSearch, Vec<ConnState>)| {
                    if std::mem::take(&mut first) {
                        crate::ui::theme::install_fonts(ui.ctx());
                        crate::ui::theme::apply(ui.ctx(), true);
                        return;
                    }
                    state.0.show(ui.ctx(), &state.1);
                },
                (search, vec![connection]),
            );
        h.run_steps(3);
        h.get_by_label("public.customers   (1)").click();
        h.run_steps(3);
        assert_eq!(
            h.state().0.preview.as_ref().unwrap().rs.rows[0][1],
            Some("Alice".into())
        );
        assert!(h.state().0.preview.as_ref().unwrap().edit.is_none());
        if render {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/navigation-snapshots");
            std::fs::create_dir_all(&dir).unwrap();
            h.render()
                .unwrap()
                .save(dir.join("schema-search.png"))
                .unwrap();
        }
    }

    #[test]
    fn results_preview_is_read_only_and_ignores_stale_requests() {
        dialog_scenario(false);
    }

    #[test]
    #[ignore = "Requires a GPU; renders the schema search dialog on test data"]
    fn render_schema_search() {
        dialog_scenario(true);
    }
}
