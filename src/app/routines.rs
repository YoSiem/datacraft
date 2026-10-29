//! Procedures and functions: loading them for the explorer, "New / Edit Procedure…",
//! "Execute…", "Generate DDL" and dropping them. The app side of `ui::routine_dialog`,
//! `ui::routine_execute` and `db::routines`.

use super::{ConnStatus, DataCraftApp};
use crate::db::{Driver, SchemaInfo};
use crate::db::routines::{self, ArgValue, RoutineDefinition, RoutineInfo, RoutineKind, RoutineList, RoutineRef, RoutineSpec};
use crate::i18n::trf;
use crate::ui::create_dialog;
use crate::ui::drop_dialog::DropDialog;
use crate::ui::explorer;
use crate::ui::routine_dialog::{RoutineDialog, RoutineResult};
use crate::ui::routine_execute::{ExecuteDialog, ExecuteResult};
use crate::worker::{ConnId, Request};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct RoutineState {
    pub dialog: Option<RoutineDialog>,
    pub execute: Option<ExecuteDialog>,
    /// "Generate DDL" waiting for the definition, by request id.
    ddl: HashMap<u64, RoutineRef>,
    /// What was typed in "Execute…" for each routine (this session only).
    last_values: HashMap<RoutineRef, Vec<ArgValue>>,
    next_request: u64,
}

impl RoutineState {
    fn request_id(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
    }

    pub fn is_open(&self) -> bool {
        self.dialog.is_some() || self.execute.is_some()
    }
}

impl DataCraftApp {
    /// Loads (or reloads) the procedures and functions of `db` in the background.
    pub(super) fn load_routines(&mut self, conn: ConnId, db: &str) {
        let Some(c) = self.conn_mut(conn) else { return };
        if c.status != ConnStatus::Connected || !routines::supported(c.cfg.driver) {
            return;
        }
        let list = c.routines.entry(db.to_string()).or_default();
        if list.loading {
            return;
        }
        list.loading = true;
        self.worker.send(Request::LoadRoutines { conn, db: db.to_string() });
    }

    pub(super) fn routines_loaded(&mut self, conn: ConnId, db: String, result: Result<Vec<RoutineInfo>, String>) {
        // A reply that arrives after a disconnect belongs to the old session.
        let Some(c) = self.conn_mut(conn).filter(|c| c.status == ConnStatus::Connected) else { return };
        if let Ok(items) = &result {
            add_routine_schemas(c.cfg.driver, c.databases.iter_mut().find(|d| d.name == db).and_then(|d| d.schemas.as_mut()), items);
        }
        let list = c.routines.entry(db).or_insert_with(RoutineList::default);
        list.loading = false;
        match result {
            Ok(items) => {
                list.items = Some(items);
                list.error = None;
            }
            Err(e) => list.error = Some(e),
        }
    }

    fn send_routine_definition(&mut self, r: &RoutineRef) -> u64 {
        let request_id = self.routines.request_id();
        self.worker.send(Request::RoutineDefinition { routine: r.clone(), request_id });
        request_id
    }

    fn routine_driver(&self, conn: ConnId) -> Option<Driver> {
        self.conn(conn).filter(|c| c.status == ConnStatus::Connected && routines::supported(c.cfg.driver)).map(|c| c.cfg.driver)
    }

    pub(super) fn open_new_routine(&mut self, conn: ConnId, db: String, schema: Option<String>, kind: RoutineKind) {
        if self.refuse_read_only(conn) {
            return;
        }
        let Some(driver) = self.routine_driver(conn) else { return };
        let Some(c) = self.conn(conn) else { return };
        let schemas: Vec<String> = c.schemas(Some(&db)).unwrap_or_default().iter().map(|s| s.name.clone()).collect();
        let default = match driver {
            Driver::MySql | Driver::MariaDb => db.clone(),
            Driver::Oracle => c.cfg.user.to_uppercase(),
            _ => create_dialog::default_schema(driver).to_string(),
        };
        let schema = schema
            .or_else(|| schemas.contains(&default).then_some(default.clone()))
            .or_else(|| schemas.first().cloned())
            .unwrap_or(default);
        self.ensure_db_loaded(conn, Some(&db));
        self.routines.dialog = Some(RoutineDialog::new_routine(conn, driver, db, schema, kind));
    }

    pub(super) fn open_edit_routine(&mut self, r: RoutineRef) {
        if self.refuse_read_only(r.conn) {
            return;
        }
        let Some(driver) = self.routine_driver(r.conn) else { return };
        let request_id = self.send_routine_definition(&r);
        let placeholder = RoutineSpec { kind: r.kind, schema: r.schema.clone(), name: r.name.clone(), ..Default::default() };
        self.routines.dialog = Some(RoutineDialog::edit_routine(r.conn, driver, r.db, placeholder, request_id));
    }

    pub(super) fn open_execute_routine(&mut self, r: RoutineRef) {
        let Some(driver) = self.routine_driver(r.conn) else { return };
        let request_id = self.send_routine_definition(&r);
        let values = self.routines.last_values.get(&r).cloned().unwrap_or_default();
        self.routines.execute = Some(ExecuteDialog::new(r, driver, request_id, values));
    }

    pub(super) fn request_routine_ddl(&mut self, r: RoutineRef) {
        if self.routine_driver(r.conn).is_none() {
            return;
        }
        let id = self.send_routine_definition(&r);
        self.routines.ddl.insert(id, r);
    }

    pub(super) fn open_drop_routine(&mut self, r: RoutineRef) {
        if self.refuse_read_only(r.conn) {
            return;
        }
        let Some(driver) = self.routine_driver(r.conn) else { return };
        self.drop = Some(DropDialog::routine(driver, &r));
    }

    pub(super) fn routine_definition_received(&mut self, conn: ConnId, request_id: u64, result: Result<RoutineDefinition, String>) {
        if let Some(d) = self.routines.dialog.as_mut().filter(|d| d.conn == conn && d.loading == Some(request_id)) {
            d.definition_loaded(result);
            return;
        }
        if let Some(d) = self.routines.execute.as_mut().filter(|d| d.routine.conn == conn && d.loading == Some(request_id)) {
            d.definition_loaded(result);
            return;
        }
        let Some(r) = self.routines.ddl.remove(&request_id) else { return };
        match result {
            Ok(def) => {
                let i = self.new_console(Some(r.conn), Some(format!("{}.sql", r.name)), def.ddl);
                self.consoles[i].db = Some(r.db);
            }
            Err(e) => self.error(e),
        }
    }

    pub(super) fn routine_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(d) = self.routines.dialog.as_ref() {
            let schemas: Vec<String> =
                self.conn(d.conn).and_then(|c| c.schemas(Some(&d.db))).unwrap_or_default().iter().map(|s| s.name.clone()).collect();
            if let Some(d) = self.routines.dialog.as_mut() {
                match d.show(ctx, &schemas) {
                    Some(RoutineResult::Run(run)) => {
                        d.running = true;
                        d.error = None;
                        let (conn, db) = (d.conn, Some(d.db.clone()));
                        self.worker.send(Request::RunDdl { conn, db, statements: run.statements, close_db: None, transaction: run.transaction });
                    }
                    Some(RoutineResult::Cancel) => self.routines.dialog = None,
                    None => {}
                }
            }
        }
        let Some(d) = self.routines.execute.as_mut() else { return };
        match d.show(ctx) {
            Some(ExecuteResult::Console { script, run }) => {
                let Some(d) = self.routines.execute.take() else { return };
                self.routines.last_values.insert(d.routine.clone(), d.values);
                let r = d.routine;
                let i = self.new_console(Some(r.conn), Some(format!("{}.sql", r.name)), script);
                self.consoles[i].db = Some(r.db);
                if run {
                    self.run_console(i, true);
                }
            }
            Some(ExecuteResult::Cancel) => {
                if let Some(d) = self.routines.execute.take() {
                    self.routines.last_values.insert(d.routine, d.values);
                }
            }
            None => {}
        }
    }

    /// Reply to the routine dialog's statements. false if they weren't the dialog's.
    pub(super) fn routine_ddl_done(&mut self, conn: ConnId, result: &Result<(), String>) -> bool {
        let Some(d) = self.routines.dialog.as_mut().filter(|d| d.conn == conn && d.running) else { return false };
        if let Err(e) = result {
            d.running = false;
            d.error = Some(e.clone());
            // Oracle: the routine may exist now, invalid; show it in the explorer.
            let db = d.db.clone();
            self.load_routines(conn, &db);
            return true;
        }
        let Some(d) = self.routines.dialog.take() else { return true };
        let edit = d.is_edit();
        let kind = d.kind();
        let name = routines::final_name(d.driver, &d.spec);
        let schema = match d.driver {
            Driver::MySql | Driver::MariaDb => d.db.clone(),
            _ => d.spec.schema.trim().to_string(),
        };
        self.explorer.expanded.insert(format!("c:{conn}"));
        self.explorer.expanded.insert(explorer::db_key(conn, &d.db));
        self.explorer.expanded.insert(format!("s:{conn}:{}:{schema}", d.db));
        let gk = if kind == RoutineKind::Procedure { "p" } else { "f" };
        self.explorer.expanded.insert(format!("g:{conn}:{}:{schema}:{gk}", d.db));
        if let Some(c) = self.conn_mut(conn) {
            c.routines.entry(d.db.clone()).or_default().loading = false;
        }
        self.load_routines(conn, &d.db);
        let msg = match (kind, edit) {
            (RoutineKind::Procedure, false) => "Procedure “{name}” created",
            (RoutineKind::Procedure, true) => "Procedure “{name}” saved",
            (RoutineKind::Function, false) => "Function “{name}” created",
            (RoutineKind::Function, true) => "Function “{name}” saved",
        };
        self.info(trf(msg, &[("name", &name)]));
        true
    }

    /// After "Drop…" of a routine.
    pub(super) fn routine_dropped(&mut self, r: &RoutineRef) {
        if self.explorer.selected.as_deref() == Some(explorer::routine_key(r).as_str()) {
            self.explorer.selected = None;
        }
        if let Some(c) = self.conn_mut(r.conn)
            && let Some(list) = c.routines.get_mut(&r.db)
        {
            if let Some(items) = list.items.as_mut() {
                items.retain(|i| !(i.schema == r.schema && i.name == r.name && i.kind == r.kind && i.signature == r.signature));
            }
            list.loading = false;
        }
        self.load_routines(r.conn, &r.db);
        let msg = if r.kind == RoutineKind::Procedure { "Procedure “{name}” dropped" } else { "Function “{name}” dropped" };
        self.info(trf(msg, &[("name", &r.name)]));
    }
}

/// The schema introspection only lists schemas with tables or views: schemas that only hold
/// routines are added so their routines show in the explorer.
fn add_routine_schemas(driver: Driver, schemas: Option<&mut Vec<SchemaInfo>>, routines: &[RoutineInfo]) {
    let Some(schemas) = schemas else { return };
    if driver.flat_schema() {
        return;
    }
    for r in routines {
        if schemas.iter().any(|s| s.name == r.schema) {
            continue;
        }
        let at = schemas.iter().position(|s| s.name > r.schema).unwrap_or(schemas.len());
        // Oracle lists the connected user's schema first.
        let at = if driver == Driver::Oracle { at.max(1).min(schemas.len()) } else { at };
        schemas.insert(at, SchemaInfo { name: r.schema.clone(), tables: Vec::new() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_with_only_routines_are_added() {
        let schema = |name: &str| SchemaInfo { name: name.into(), tables: Vec::new() };
        let routine = |schema: &str| RoutineInfo { schema: schema.into(), name: "f".into(), ..Default::default() };
        let mut schemas = vec![schema("app"), schema("public")];
        add_routine_schemas(Driver::Postgres, Some(&mut schemas), &[routine("api"), routine("public"), routine("zeta"), routine("api")]);
        assert_eq!(schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["api", "app", "public", "zeta"]);
        let mut oracle = vec![schema("HR"), schema("APP")];
        add_routine_schemas(Driver::Oracle, Some(&mut oracle), &[routine("AAA")]);
        assert_eq!(oracle.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["HR", "AAA", "APP"]);
        let mut flat = vec![schema("shop")];
        add_routine_schemas(Driver::MySql, Some(&mut flat), &[routine("other")]);
        assert_eq!(flat.len(), 1);
    }
}
