//! "Execute…" modal: a value for each input parameter (or NULL, the default, an SQL
//! expression), the call it generates, and a console that runs it.

use crate::db::Driver;
use crate::db::routines::{self, ArgValue, ParamMode, RoutineDefinition, RoutineRef, RoutineSpec};
use crate::i18n::{tr, trf};
use crate::ui::create_dialog::sql_preview;
use crate::ui::theme::{self, palette};
use egui::{Margin, RichText, vec2};

pub struct ExecuteDialog {
    pub routine: RoutineRef,
    pub driver: Driver,
    /// The definition request in flight (`None` once it arrived).
    pub loading: Option<u64>,
    pub spec: Option<RoutineSpec>,
    pub values: Vec<ArgValue>,
    pub error: Option<String>,
    focus: bool,
}

pub enum ExecuteResult {
    /// Open a console with the script and run it (`run`) or just show it.
    Console { script: String, run: bool },
    Cancel,
}

impl ExecuteDialog {
    /// `values`: what was typed the last time this routine was executed.
    pub fn new(routine: RoutineRef, driver: Driver, request_id: u64, values: Vec<ArgValue>) -> Self {
        Self { routine, driver, loading: Some(request_id), spec: None, values, error: None, focus: true }
    }

    pub fn definition_loaded(&mut self, result: Result<RoutineDefinition, String>) {
        self.loading = None;
        match result {
            Ok(RoutineDefinition { spec: Some(spec), .. }) => {
                if !routines::callable(&spec) {
                    self.error = Some(tr("Trigger functions run from their trigger; they can't be called directly.").into());
                }
                self.values.resize(spec.params.len(), ArgValue::default());
                for (v, p) in self.values.iter_mut().zip(&spec.params) {
                    // A fresh form starts from the defaults.
                    if v == &ArgValue::default() && !p.default.trim().is_empty() {
                        v.use_default = true;
                    }
                }
                self.spec = Some(spec);
            }
            Ok(RoutineDefinition { spec: None, .. }) => {
                self.error = Some(trf("The parameters of “{name}” couldn't be read; call it from a console.", &[("name", &self.routine.name)]));
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn script(&self) -> Option<String> {
        let spec = self.spec.as_ref().filter(|s| routines::callable(s))?;
        Some(routines::call_sql(self.driver, spec, &self.values))
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<ExecuteResult> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));
        egui::Modal::new(egui::Id::new("routine_execute")).frame(frame).show(ctx, |ui| {
            ui.set_width(680.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(trf("Execute {name}", &[("name", &self.routine.name)])).font(theme::bold(15.0)).color(p.text));
                ui.add_space(6.0);
                let place = if self.driver.flat_schema() { self.routine.db.clone() } else { format!("{}.{}", self.routine.db, self.routine.schema) };
                ui.label(RichText::new(trf("in {db}", &[("db", &place)])).color(p.muted));
            });
            ui.add_space(12.0);
            if self.loading.is_some() {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                    ui.label(RichText::new(tr("Loading the parameters…")).color(p.muted));
                });
            }
            if let Some(spec) = &self.spec {
                let driver = self.driver;
                if spec.params.is_empty() {
                    ui.label(RichText::new(tr("No parameters.")).color(p.muted));
                } else {
                    egui::ScrollArea::vertical().id_salt("execute_params").max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
                        egui::Grid::new("execute_grid").num_columns(5).spacing([10.0, 6.0]).show(ui, |ui| {
                            for h in [tr("Parameter"), tr("Type"), tr("Value"), "", ""] {
                                ui.label(RichText::new(h).small().color(p.muted));
                            }
                            ui.end_row();
                            for (i, (param, v)) in spec.params.iter().zip(self.values.iter_mut()).enumerate() {
                                let mode = match param.mode {
                                    ParamMode::In => String::new(),
                                    m => format!(" {}", crate::ui::routine_dialog::mode_label(driver, m)),
                                };
                                let name = if param.name.trim().is_empty() { format!("${}", i + 1) } else { param.name.clone() };
                                ui.label(RichText::new(format!("{name}{mode}")).monospace());
                                ui.label(RichText::new(&param.data_type).monospace().color(p.muted));
                                // Output-only parameters take nothing (SQL Server OUTPUT ones may take a value).
                                let input = param.mode.takes_input() || driver == Driver::MsSql;
                                let has_default = !param.default.trim().is_empty();
                                if input {
                                    let enabled = !v.null && !(v.use_default && has_default);
                                    let hint = if v.use_default && has_default { param.default.trim().to_string() } else { String::new() };
                                    let field = ui.add_enabled_ui(enabled, |ui| {
                                        ui.add_sized(
                                            [240.0, 26.0],
                                            egui::TextEdit::singleline(&mut v.value)
                                                .id_salt(("execute_value", i))
                                                .font(egui::TextStyle::Monospace)
                                                .hint_text(hint)
                                                .margin(vec2(6.0, 4.0)),
                                        )
                                    })
                                    .inner;
                                    if std::mem::take(&mut self.focus) && enabled {
                                        field.request_focus();
                                    }
                                    ui.horizontal(|ui| {
                                        ui.checkbox(&mut v.null, "NULL");
                                        if has_default {
                                            ui.checkbox(&mut v.use_default, tr("Default"));
                                        }
                                    });
                                    ui.checkbox(&mut v.expression, "SQL").on_hover_text(tr("Write the value as an SQL expression, not as text"));
                                } else {
                                    ui.label(RichText::new(tr("returned")).color(p.muted));
                                    ui.label("");
                                    ui.label("");
                                }
                                ui.end_row();
                            }
                        });
                    });
                }
            }
            if let Some(script) = self.script() {
                ui.add_space(10.0);
                sql_preview(ui, "execute", &script);
            }
            if let Some(e) = &self.error {
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(e).color(p.error)).wrap());
            }
            ui.add_space(14.0);
            let script = self.script();
            ui.horizontal(|ui| {
                ui.add_enabled_ui(script.is_some(), |ui| {
                    if theme::secondary_button(ui, tr("Copy SQL")).clicked()
                        && let Some(s) = &script
                    {
                        ui.ctx().copy_text(s.clone());
                    }
                    if theme::secondary_button(ui, tr("Open in Console")).clicked()
                        && let Some(s) = &script
                    {
                        result = Some(ExecuteResult::Console { script: s.clone(), run: false });
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.add_enabled_ui(script.is_some(), |ui| {
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
                        if (theme::primary_button(ui, tr("Run")).clicked() || enter)
                            && let Some(s) = &script
                        {
                            result = Some(ExecuteResult::Console { script: s.clone(), run: true });
                        }
                    });
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        result = Some(ExecuteResult::Cancel);
                    }
                });
            });
        });
        if result.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(ExecuteResult::Cancel);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::routines::{Param, RoutineKind};
    use crate::ui::theme;
    use egui::vec2;

    fn routine() -> RoutineRef {
        RoutineRef { conn: 1, db: "shop".into(), schema: "public".into(), name: "f".into(), kind: RoutineKind::Function, signature: "a integer".into() }
    }

    #[test]
    fn defaults_start_selected_and_values_are_kept() {
        let mut d = ExecuteDialog::new(routine(), Driver::Postgres, 4, Vec::new());
        assert!(d.script().is_none());
        let spec = RoutineSpec {
            kind: RoutineKind::Function,
            schema: "public".into(),
            name: "f".into(),
            params: vec![
                Param { name: "a".into(), data_type: "integer".into(), ..Default::default() },
                Param { name: "b".into(), data_type: "integer".into(), default: "2".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        d.definition_loaded(Ok(RoutineDefinition { ddl: String::new(), spec: Some(spec.clone()) }));
        assert!(!d.values[0].use_default && d.values[1].use_default);
        d.values[0].value = "7".into();
        assert_eq!(d.script().as_deref(), Some("select public.f(7);"));

        let kept = vec![ArgValue { value: "1".into(), ..Default::default() }, ArgValue { value: "5".into(), ..Default::default() }];
        let mut again = ExecuteDialog::new(routine(), Driver::Postgres, 5, kept);
        again.definition_loaded(Ok(RoutineDefinition { ddl: String::new(), spec: Some(spec) }));
        assert_eq!(again.script().as_deref(), Some("select public.f(1, 5);"));
    }

    #[test]
    fn the_form_runs_the_call_in_a_console() {
        use egui_kittest::kittest::Queryable;
        let spec = crate::db::routines::parse_definition(
            Driver::MsSql,
            "create procedure dbo.add_order @user_id int, @note nvarchar(20) = N'none', @new_id bigint output as select 1",
        )
        .unwrap();
        let mut d = ExecuteDialog::new(routine(), Driver::MsSql, 4, Vec::new());
        d.definition_loaded(Ok(RoutineDefinition { ddl: String::new(), spec: Some(spec) }));
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(900.0, 700.0)).build_ui_state(
            move |ui, s: &mut (ExecuteDialog, Option<(String, bool)>)| {
                let ctx = ui.ctx().clone();
                if std::mem::take(&mut first) {
                    theme::install_fonts(&ctx);
                    theme::apply(&ctx, false);
                } else if let Some(ExecuteResult::Console { script, run }) = s.0.show(&ctx) {
                    s.1 = Some((script, run));
                }
            },
            (d, None),
        );
        h.run_steps(3);
        h.state_mut().0.values[0].value = "5".into();
        h.run_steps(2);
        assert!(h.query_by_label("@new_id OUTPUT").is_some());
        h.get_by_label(tr("Run")).click();
        h.run_steps(2);
        let (script, run) = h.state().1.clone().unwrap();
        assert!(run);
        assert!(script.contains("exec @dc_return = dbo.add_order @user_id = 5, @new_id = @new_id output;"), "{script}");
    }

    #[test]
    #[ignore = "Requires a GPU; writes the Execute dialog preview to target/routine-snapshots"]
    fn render_execute_dialog() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/routine-snapshots");
        std::fs::create_dir_all(&directory).unwrap();
        let spec = crate::db::routines::parse_definition(
            Driver::Postgres,
            "CREATE PROCEDURE public.archive_orders(IN before_date date, IN batch integer DEFAULT 500, INOUT note text DEFAULT 'manual'::text, OUT moved bigint)\n LANGUAGE plpgsql\nAS $$ begin end $$",
        )
        .unwrap();
        let mut r = routine();
        r.name = "archive_orders".into();
        let mut d = ExecuteDialog::new(r, Driver::Postgres, 4, Vec::new());
        d.definition_loaded(Ok(RoutineDefinition { ddl: String::new(), spec: Some(spec) }));
        d.values[0].value = "2026-01-01".into();
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1000.0, 640.0)).build_ui_state(
            move |ui, d: &mut ExecuteDialog| {
                let ctx = ui.ctx().clone();
                if std::mem::take(&mut first) {
                    theme::install_fonts(&ctx);
                    theme::apply(&ctx, true);
                } else {
                    d.show(&ctx);
                }
            },
            d,
        );
        h.run_steps(3);
        h.render().unwrap().save(directory.join("execute.png")).unwrap();
    }

    #[test]
    fn unreadable_definitions_and_triggers_are_not_called() {
        let mut d = ExecuteDialog::new(routine(), Driver::Postgres, 4, Vec::new());
        d.definition_loaded(Ok(RoutineDefinition { ddl: "x".into(), spec: None }));
        assert!(d.error.is_some() && d.script().is_none());
        let mut t = ExecuteDialog::new(routine(), Driver::Postgres, 4, Vec::new());
        let mut spec = RoutineSpec { kind: RoutineKind::Function, name: "t".into(), ..Default::default() };
        spec.returns.data_type = "trigger".into();
        t.definition_loaded(Ok(RoutineDefinition { ddl: String::new(), spec: Some(spec) }));
        assert!(t.error.is_some() && t.script().is_none());
    }
}
