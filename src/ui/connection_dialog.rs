//! DataGrip's "Data Sources and Drivers" window: the project's data sources on the left and the
//! selected one's settings on the right (General / Options / Advanced tabs), with Test
//! Connection, Cancel, Apply and OK at the bottom.

use crate::db::url::{apply_jdbc_url, jdbc_url};
use crate::db::{AuthMode, ConnectionConfig, Driver, is_secret_property, snowflake};
use crate::i18n::{tr, trf};
use crate::ui::icons::{self, Ico};
use crate::ui::theme::{self, Palette, palette};
use egui::{Align, Layout, Rect, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

pub enum TestState {
    Idle,
    Testing,
    /// `Ok` carries the server's product and version when it could be read.
    Done(Result<Option<String>, String>),
}

pub enum DialogResult {
    /// Apply (`close` false) or OK: data sources to save and ids to delete.
    Save { save: Vec<ConnectionConfig>, delete: Vec<u64>, close: bool },
    Test(Box<ConnectionConfig>),
    Cancel,
}

/// One data source of the list, as edited.
struct Item {
    cfg: ConnectionConfig,
    /// As last saved; None for a data source that doesn't exist yet.
    saved: Option<ConnectionConfig>,
}

impl Item {
    fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.cfg)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Options,
    Advanced,
}

pub struct ConnectionDialog {
    items: Vec<Item>,
    selected: usize,
    /// Saved data sources removed with "−", deleted on Apply/OK.
    removed: Vec<u64>,
    /// Test Connection of the data source with this id.
    test: (u64, TestState),
    error: Option<String>,
    tab: Tab,
    /// Text of the URL field. Rebuilt from the fields every frame unless it is being edited, when
    /// it is parsed back into them instead.
    url: String,
    /// Cancel / Escape with unapplied changes: asking before they are discarded.
    confirm_discard: bool,
}

const LIST_WIDTH: f32 = 290.0;
const LABEL_WIDTH: f32 = 118.0;
const TITLE_H: f32 = 40.0;
const FOOTER_H: f32 = 56.0;
const TEST_H: f32 = 40.0;

impl ConnectionDialog {
    /// The window with the existing data sources and a new one of `id`, selected.
    pub fn new_connection(id: u64, existing: &[ConnectionConfig]) -> Self {
        let mut d = Self::with(existing);
        d.add(id, Driver::Postgres);
        d
    }

    /// The window with `id` selected.
    pub fn edit(id: u64, existing: &[ConnectionConfig]) -> Self {
        let mut d = Self::with(existing);
        d.selected = d.items.iter().position(|i| i.cfg.id == id).unwrap_or(0);
        d
    }

    fn with(existing: &[ConnectionConfig]) -> Self {
        let items = existing.iter().map(|c| Item { cfg: c.clone(), saved: Some(c.clone()) }).collect();
        Self { items, selected: 0, removed: Vec::new(), test: (0, TestState::Idle), error: None, tab: Tab::General, url: String::new(), confirm_discard: false }
    }

    fn add(&mut self, id: u64, driver: Driver) {
        let mut cfg = ConnectionConfig::new(id, driver);
        // The id is a timestamp: it is not shown in the default name.
        cfg.name = driver.label().to_string();
        self.items.push(Item { cfg, saved: None });
        self.select(self.items.len() - 1);
    }

    fn select(&mut self, i: usize) {
        self.selected = i;
        self.error = None;
        if self.tab == Tab::Advanced && self.items.get(i).is_none_or(|it| it.cfg.driver != Driver::Snowflake) {
            self.tab = Tab::General;
        }
    }

    pub fn set_test_result(&mut self, id: u64, result: Result<Option<String>, String>) {
        if self.test.0 == id {
            self.test.1 = TestState::Done(result);
        }
    }

    /// An id not used by any data source of the list (they are millisecond timestamps).
    fn next_id(&self) -> u64 {
        let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let max = self.items.iter().map(|i| i.cfg.id).chain(self.removed.iter().copied()).max().unwrap_or(0);
        (max + 1).max(millis)
    }

    fn change_driver(cfg: &mut ConnectionConfig, new: Driver) {
        let old = cfg.driver;
        if old == new {
            return;
        }
        let old_def = ConnectionConfig::new(cfg.id, old);
        let new_def = ConnectionConfig::new(cfg.id, new);
        // Kusto starts without a host (a cluster address, not localhost).
        if cfg.host == old_def.host {
            cfg.host = new_def.host;
        }
        if cfg.port == old_def.port {
            cfg.port = new_def.port;
        }
        if cfg.database == old_def.database {
            cfg.database = new_def.database;
        }
        if cfg.user == old_def.user {
            cfg.user = new_def.user;
        }
        if cfg.name == old_def.name || cfg.name == old.label() {
            cfg.name = new.label().to_string();
        }
        // Authenticator is Snowflake's only, the Azure CLI Kusto's only; InfluxDB always takes a
        // token, and Kusto starts with the Azure CLI's account.
        if (new != Driver::Snowflake && cfg.auth == AuthMode::Authenticator)
            || (new != Driver::Kusto && cfg.auth == AuthMode::AzureCli)
            || new == Driver::InfluxDb
        {
            cfg.auth = AuthMode::UserPassword;
        }
        if new == Driver::Kusto && cfg.auth == AuthMode::UserPassword && cfg.user.trim().is_empty() {
            cfg.auth = AuthMode::AzureCli;
        }
        cfg.driver = new;
    }

    fn validate(cfg: &ConnectionConfig) -> Result<(), String> {
        if cfg.name.trim().is_empty() {
            return Err(tr("Name is required").into());
        }
        match cfg.driver {
            Driver::Sqlite if cfg.file_path.trim().is_empty() => Err(tr("Specify the database file").into()),
            Driver::Sqlite => Ok(()),
            _ if cfg.host.trim().is_empty() => Err(tr("Host is required").into()),
            Driver::Snowflake => snowflake::credentials(cfg).map(|_| ()).map_err(|e| e.to_string()),
            _ => Ok(()),
        }
    }

    /// The config to save or test: property rows without a name are dropped.
    fn finished(cfg: &ConnectionConfig) -> ConnectionConfig {
        let mut cfg = cfg.clone();
        cfg.properties.retain(|(k, _)| !k.trim().is_empty());
        for (k, _) in &mut cfg.properties {
            *k = k.trim().to_string();
        }
        cfg
    }

    /// Apply / OK: every changed data source must be valid; the first that isn't is selected.
    fn save(&mut self, close: bool) -> Option<DialogResult> {
        for i in 0..self.items.len() {
            if self.items[i].dirty()
                && let Err(e) = Self::validate(&self.items[i].cfg)
            {
                self.select(i);
                self.error = Some(e);
                return None;
            }
        }
        let mut save = Vec::new();
        for item in self.items.iter_mut().filter(|i| i.dirty()) {
            item.cfg = Self::finished(&item.cfg);
            item.saved = Some(item.cfg.clone());
            save.push(item.cfg.clone());
        }
        self.error = None;
        Some(DialogResult::Save { save, delete: std::mem::take(&mut self.removed), close })
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<DialogResult> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(egui::Margin::ZERO)
            .stroke(Stroke::new(1.0, p.popup_border));
        let screen = ctx.content_rect();
        let size = vec2((screen.width() - 80.0).clamp(640.0, 1000.0), (screen.height() - 60.0).clamp(480.0, 760.0));
        egui::Modal::new(egui::Id::new("connection_dialog")).frame(frame).show(ctx, |ui| {
            let (outer, _) = ui.allocate_exact_size(size, Sense::hover());
            let line = Stroke::new(1.0, p.popup_border);
            let body = Rect::from_min_max(pos2(outer.left(), outer.top() + TITLE_H), pos2(outer.right(), outer.bottom() - FOOTER_H));
            let list = Rect::from_min_max(body.min, pos2(body.left() + LIST_WIDTH, body.bottom()));
            let right = Rect::from_min_max(pos2(list.right() + 1.0, body.top()), body.max);

            ui.painter().text(
                pos2(outer.left() + 18.0, outer.top() + TITLE_H / 2.0),
                egui::Align2::LEFT_CENTER,
                tr("Data Sources and Drivers"),
                theme::bold(13.0),
                p.text,
            );
            ui.painter().hline(outer.x_range(), body.top(), line);
            ui.painter().vline(list.right() + 0.5, body.y_range(), line);
            ui.painter().hline(outer.x_range(), body.bottom(), line);

            let mut list_ui = ui.new_child(UiBuilder::new().max_rect(list.shrink2(vec2(10.0, 10.0))));
            self.data_source_list(&mut list_ui, &p);

            let test_rect = Rect::from_min_max(pos2(right.left(), right.bottom() - TEST_H), right.max);
            ui.painter().hline(right.x_range(), test_rect.top(), line);
            let form = Rect::from_min_max(right.min, pos2(right.right(), test_rect.top())).shrink2(vec2(18.0, 14.0));
            let mut form_ui = ui.new_child(UiBuilder::new().max_rect(form));
            if !self.items.is_empty() {
                self.details(&mut form_ui, &p);
            }

            let mut test_ui = ui.new_child(
                UiBuilder::new().max_rect(test_rect.shrink2(vec2(18.0, 0.0))).layout(Layout::left_to_right(Align::Center)),
            );
            if let Some(r) = self.test_row(&mut test_ui, &p) {
                result = Some(r);
            }

            let footer = Rect::from_min_max(pos2(outer.left(), body.bottom()), outer.max).shrink2(vec2(18.0, 0.0));
            let mut footer_ui = ui.new_child(UiBuilder::new().max_rect(footer).layout(Layout::right_to_left(Align::Center)));
            footer_ui.spacing_mut().item_spacing.x = 10.0;
            if theme::primary_button(&mut footer_ui, tr("OK")).clicked()
                && let Some(r) = self.save(true)
            {
                result = Some(r);
            }
            let dirty = !self.removed.is_empty() || self.items.iter().any(Item::dirty);
            if footer_ui.add_enabled(dirty, egui::Button::new(tr("Apply")).corner_radius(6).min_size(vec2(72.0, 28.0))).clicked()
                && let Some(r) = self.save(false)
            {
                result = Some(r);
            }
            if theme::secondary_button(&mut footer_ui, tr("Cancel")).clicked() {
                result = self.cancel(dirty);
            }
            if let Some(e) = &self.error {
                footer_ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(e).color(p.error)).truncate()).on_hover_text(e);
                });
            }
        });
        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if self.confirm_discard {
            let changed = self.items.iter().filter(|i| i.dirty()).count() + self.removed.len();
            let mut decision = None;
            egui::Modal::new(egui::Id::new("confirm_discard_data_sources")).frame(confirm_frame(ctx, &p)).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(tr("Unsaved changes")).font(theme::bold(15.0)));
                ui.add_space(8.0);
                ui.label(trf("Changes to {count} data source(s) have not been applied. Discard them?", &[("count", &changed)]));
                ui.add_space(16.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if theme::primary_button(ui, tr("Discard")).clicked() {
                        decision = Some(true);
                    }
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        decision = Some(false);
                    }
                });
            });
            // Escape here backs out of the question, not out of the window.
            match decision.or(escape.then_some(false)) {
                Some(true) => return Some(DialogResult::Cancel),
                Some(false) => self.confirm_discard = false,
                None => {}
            }
            return None;
        }
        if result.is_none() && escape {
            let dirty = !self.removed.is_empty() || self.items.iter().any(Item::dirty);
            result = self.cancel(dirty);
        }
        result
    }

    /// Cancel closes right away when nothing changed; otherwise it asks first.
    fn cancel(&mut self, dirty: bool) -> Option<DialogResult> {
        if dirty {
            self.confirm_discard = true;
            None
        } else {
            Some(DialogResult::Cancel)
        }
    }

    /// Left pane: "Data Sources", the +/− toolbar and the project's data sources.
    fn data_source_list(&mut self, ui: &mut Ui, p: &Palette) {
        ui.label(RichText::new(tr("Data Sources")).font(theme::bold(13.0)).color(p.text));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let plus = theme::icon_button(ui, Ico::Plus, true, tr("Add data source"));
            let mut add = None;
            egui::Popup::menu(&plus).show(|ui| {
                ui.set_min_width(180.0);
                for d in Driver::ALL {
                    let r = ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                        icons::paint(ui.painter(), r, Ico::Driver(d), p.icon, p, true);
                        ui.add(egui::Button::new(d.label()).frame(false))
                    });
                    if r.inner.clicked() {
                        add = Some(d);
                    }
                }
            });
            if let Some(d) = add {
                let id = self.next_id();
                self.add(id, d);
            }
            if theme::icon_button(ui, Ico::Minus, !self.items.is_empty(), tr("Remove")).clicked() {
                let item = self.items.remove(self.selected);
                if item.saved.is_some() {
                    self.removed.push(item.cfg.id);
                }
                let next = self.selected.min(self.items.len().saturating_sub(1));
                self.select(next);
            }
            if theme::icon_button(ui, Ico::Copy, !self.items.is_empty(), tr("Duplicate")).clicked() {
                let mut cfg = self.items[self.selected].cfg.clone();
                cfg.id = self.next_id();
                cfg.name = trf("{name} (copy)", &[("name", &cfg.name)]);
                self.items.push(Item { cfg, saved: None });
                self.select(self.items.len() - 1);
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Project Data Sources")).color(p.muted));
        ui.add_space(2.0);
        let mut clicked = None;
        egui::ScrollArea::vertical().id_salt("ds_list").auto_shrink([false, false]).show(ui, |ui| {
            for (i, item) in self.items.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
                if i == self.selected {
                    ui.painter().rect_filled(rect, 4.0, p.selection);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 4.0, p.row_hover);
                }
                let ico = Rect::from_min_size(pos2(rect.left() + 8.0, rect.center().y - 8.0), vec2(16.0, 16.0));
                icons::paint(ui.painter(), ico, Ico::Driver(item.cfg.driver), p.icon, p, true);
                let name = if item.cfg.name.trim().is_empty() { tr("<unnamed>") } else { item.cfg.name.as_str() };
                let text_rect = Rect::from_min_max(pos2(ico.right() + 8.0, rect.top()), rect.max);
                ui.painter().with_clip_rect(text_rect).text(
                    pos2(text_rect.left(), rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    name,
                    egui::FontId::proportional(13.0),
                    p.text,
                );
                if resp.clicked() {
                    clicked = Some(i);
                }
            }
        });
        if let Some(i) = clicked {
            self.select(i);
        }
    }

    /// Right pane: Name, the tabs and the selected tab.
    fn details(&mut self, ui: &mut Ui, p: &Palette) {
        let snowflake = self.items[self.selected].cfg.driver == Driver::Snowflake;
        egui::Grid::new("ds_name").num_columns(2).spacing([14.0, 10.0]).min_col_width(LABEL_WIDTH - 14.0).show(ui, |ui| {
            ui.label(tr("Name:"));
            text_field(ui, &mut self.items[self.selected].cfg.name, "");
            ui.end_row();
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let mut tabs = vec![(Tab::General, tr("General")), (Tab::Options, tr("Options"))];
            if snowflake {
                tabs.push((Tab::Advanced, tr("Advanced")));
            }
            for (tab, label) in tabs {
                if theme::tab(ui, self.tab == tab, None, label, false).0.clicked() {
                    self.tab = tab;
                }
            }
        });
        ui.add_space(14.0);
        egui::ScrollArea::vertical().id_salt("ds_form").auto_shrink([false, false]).show(ui, |ui| match self.tab {
            Tab::General => self.general(ui, p),
            Tab::Options => self.options(ui),
            Tab::Advanced => self.properties(ui, p),
        });
    }

    fn general(&mut self, ui: &mut Ui, p: &Palette) {
        let item = &mut self.items[self.selected];
        let before = item.cfg.driver;
        egui::Grid::new("ds_general").num_columns(2).spacing([14.0, 10.0]).min_col_width(LABEL_WIDTH - 14.0).show(ui, |ui| {
            ui.label(tr("Driver:"));
            driver_combo(ui, &mut item.cfg, p);
            ui.end_row();

            if item.cfg.driver == Driver::Sqlite {
                ui.label(tr("File:"));
                sqlite_file(ui, &mut item.cfg, &mut self.error);
                ui.end_row();
            } else {
                ui.label(tr("Connection type:"));
                segmented(ui, &mut item.cfg.url_only, [(false, tr("default")), (true, tr("URL only"))], p);
                ui.end_row();

                if item.cfg.url_only {
                    url_row(ui, &mut item.cfg, &mut self.url, p);
                } else {
                    ui.label(tr("Host:"));
                    ui.horizontal(|ui| {
                        let host_w = (ui.available_width() - 190.0).max(160.0);
                        let hint = match item.cfg.driver {
                            Driver::Snowflake => "<account>.snowflakecomputing.com",
                            Driver::Kusto => "<cluster>.<region>.kusto.windows.net",
                            _ => "localhost",
                        };
                        ui.add(egui::TextEdit::singleline(&mut item.cfg.host).hint_text(hint).margin(egui::vec2(8.0, 5.0)).desired_width(host_w));
                        ui.add_space(14.0);
                        ui.label(tr("Port:"));
                        ui.add_sized([ui.available_width(), 28.0], egui::DragValue::new(&mut item.cfg.port).range(1..=65535).speed(0));
                    });
                    ui.end_row();
                }
                auth_rows(ui, &mut item.cfg);
            }

            if !item.cfg.url_only {
                match item.cfg.driver {
                    Driver::Sqlite => {}
                    Driver::Snowflake => {
                        for (label, value) in [
                            (tr("Role:"), &mut item.cfg.role),
                            (tr("Database:"), &mut item.cfg.database),
                            (tr("Schema:"), &mut item.cfg.schema),
                            (tr("Warehouse:"), &mut item.cfg.warehouse),
                        ] {
                            ui.label(label);
                            text_field(ui, value, "");
                            ui.end_row();
                        }
                    }
                    d => {
                        ui.label(match d {
                            Driver::Oracle => tr("Service:"),
                            Driver::InfluxDb => tr("Bucket:"),
                            _ => tr("Database:"),
                        });
                        text_field(ui, &mut item.cfg.database, "");
                        ui.end_row();
                    }
                }
                // DataGrip leaves a gap above the URL.
                ui.label("");
                ui.allocate_space(vec2(0.0, 4.0));
                ui.end_row();
                url_row(ui, &mut item.cfg, &mut self.url, p);
            }
        });
        if item.cfg.driver != before && item.cfg.driver != Driver::Snowflake && self.tab == Tab::Advanced {
            self.tab = Tab::General;
        }
    }

    fn options(&mut self, ui: &mut Ui) {
        let cfg = &mut self.items[self.selected].cfg;
        ui.checkbox(&mut cfg.read_only, tr("Read-only"))
            .on_hover_text(tr("Only queries can run; the data editor and the create, drop and rename actions are disabled"));
        if cfg.driver == Driver::MsSql {
            ui.add_space(6.0);
            ui.checkbox(&mut cfg.trust_cert, tr("Trust server certificate"));
        }
        // Redis / Valkey over TLS (`rediss://`); MongoDB takes `?tls=true` in its URL. ClickHouse
        // has its own secure port, followed when it was on the default one; InfluxDB serves
        // HTTPS on the same port. Kusto is HTTPS unless turned off (the emulator serves HTTP).
        if matches!(cfg.driver, Driver::Redis | Driver::ClickHouse | Driver::InfluxDb | Driver::Kusto) {
            ui.add_space(6.0);
            let kusto = cfg.driver == Driver::Kusto;
            let mut tls = cfg.property("tls").map_or(kusto, |v| v.eq_ignore_ascii_case("true"));
            if ui.checkbox(&mut tls, tr("Use TLS")).changed() {
                cfg.set_property("tls", match (tls, kusto) {
                    (true, false) => "true",
                    (false, true) => "false",
                    _ => "",
                });
                if cfg.driver == Driver::ClickHouse {
                    let (plain, secure) = (Driver::ClickHouse.default_port(), crate::db::clickhouse::TLS_PORT);
                    match (tls, cfg.port) {
                        (true, p) if p == plain => cfg.port = secure,
                        (false, p) if p == secure => cfg.port = plain,
                        _ => {}
                    }
                }
            }
            if tls && matches!(cfg.driver, Driver::ClickHouse | Driver::InfluxDb) {
                let mut insecure = cfg.property("tls_insecure").is_some_and(|v| v.eq_ignore_ascii_case("true"));
                if ui.checkbox(&mut insecure, tr("Trust server certificate")).changed() {
                    cfg.set_property("tls_insecure", if insecure { "true" } else { "" });
                }
            }
        }
    }

    /// Advanced tab: DataGrip's Name / Value table of user-defined driver properties. The last
    /// row is always empty; typing in it adds a property, and "−" removes one.
    fn properties(&mut self, ui: &mut Ui, p: &Palette) {
        const ROW_H: f32 = 30.0;
        let cfg = &mut self.items[self.selected].cfg;
        let line = Stroke::new(1.0, p.grid);
        let width = ui.available_width();
        let name_w = (width * 0.5).floor();
        let mut remove = None;

        let header = |ui: &mut Ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
            ui.painter().rect_filled(rect, 0.0, p.header_bg);
            for (x, text) in [(rect.left(), tr("Name")), (rect.left() + name_w, tr("Value"))] {
                ui.painter().text(pos2(x + 10.0, rect.center().y), egui::Align2::LEFT_CENTER, text, egui::FontId::proportional(13.0), p.text);
            }
            ui.painter().vline(rect.left() + name_w, rect.y_range(), line);
            ui.painter().hline(rect.x_range(), rect.bottom(), line);
        };
        // One row: two frameless editors split at the column line, and "−" on hover.
        let row = |ui: &mut Ui, i: usize, name: &mut String, value: &mut String, placeholder: bool| -> (bool, bool) {
            let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
            let name_rect = Rect::from_min_size(rect.min, vec2(name_w, ROW_H));
            let value_rect = Rect::from_min_max(pos2(rect.left() + name_w, rect.top()), pos2(rect.right() - 28.0, rect.bottom()));
            let edit = |ui: &mut Ui, r: Rect, id: (&str, usize), v: &mut String, hint: &str, secret: bool| {
                ui.put(
                    r.shrink2(vec2(2.0, 3.0)),
                    egui::TextEdit::singleline(v)
                        .id_salt(id)
                        .frame(egui::Frame::NONE)
                        .hint_text(hint)
                        .password(secret)
                        .margin(egui::vec2(8.0, 4.0))
                        .vertical_align(Align::Center),
                )
            };
            let (nh, vh) = if placeholder { (tr("<user defined>"), tr("<value>")) } else { ("", "") };
            // Secret values (kept in the keychain) are masked like the Password field.
            let secret = is_secret_property(name);
            let a = edit(ui, name_rect, ("prop_name", i), name, nh, false);
            let b = edit(ui, value_rect, ("prop_value", i), value, vh, secret);
            ui.painter().vline(rect.left() + name_w, rect.y_range(), line);
            ui.painter().hline(rect.x_range(), rect.bottom(), line);
            let mut removed = false;
            if !placeholder && (resp.hovered() || a.has_focus() || b.has_focus() || ui.rect_contains_pointer(rect)) {
                let r = Rect::from_center_size(pos2(rect.right() - 14.0, rect.center().y), vec2(22.0, 22.0));
                removed = ui.put(r, |ui: &mut Ui| theme::icon_button_sized(ui, Ico::Minus, true, tr("Remove"), 22.0)).clicked();
            }
            (a.changed() || b.changed(), removed)
        };

        ui.spacing_mut().item_spacing.y = 0.0;
        let top = ui.cursor().top();
        header(ui);
        for (i, (name, value)) in cfg.properties.iter_mut().enumerate() {
            if row(ui, i, name, value, false).1 {
                remove = Some(i);
            }
        }
        // The new row has the ids the property will have once added, so focus stays in it.
        let n = cfg.properties.len();
        let (mut name, mut value) = (String::new(), String::new());
        if row(ui, n, &mut name, &mut value, true).0 && !(name.is_empty() && value.is_empty()) {
            cfg.properties.push((name, value));
        }
        let table = Rect::from_min_max(pos2(ui.min_rect().left(), top), pos2(ui.min_rect().left() + width, ui.cursor().top()));
        ui.painter().rect_stroke(table, 4.0, line, egui::StrokeKind::Inside);
        if let Some(i) = remove {
            cfg.properties.remove(i);
        }
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.add_space(16.0);
        ui.label(RichText::new(tr("User defined driver property. Key pair: set private_key_file (and private_key_file_pwd for an encrypted key) with Authentication → Authenticator. Secret values (passwords, passphrases, tokens) are kept in the system keychain, not in the connections file.")).size(12.0).color(p.muted));
    }

    /// "Test Connection" and its result, as in DataGrip's footer of the details pane.
    fn test_row(&mut self, ui: &mut Ui, p: &Palette) -> Option<DialogResult> {
        if self.items.is_empty() {
            return None;
        }
        let cfg = &self.items[self.selected].cfg;
        let mine = self.test.0 == cfg.id;
        let testing = mine && matches!(self.test.1, TestState::Testing);
        let mut result = None;
        let link = ui.add_enabled(!testing, egui::Button::new(RichText::new(tr("Test Connection")).color(p.accent)).frame(false));
        if link.clicked() {
            match Self::validate(cfg) {
                Ok(()) => {
                    self.error = None;
                    self.test = (cfg.id, TestState::Testing);
                    result = Some(DialogResult::Test(Box::new(Self::finished(cfg))));
                }
                Err(e) => self.error = Some(e),
            }
        }
        if !mine {
            return result;
        }
        match &self.test.1 {
            TestState::Idle => {}
            TestState::Testing => {
                ui.add(egui::Spinner::new().size(14.0));
            }
            TestState::Done(Ok(version)) => {
                mark(ui, true, p);
                ui.label(RichText::new(version.as_deref().unwrap_or(tr("Succeeded"))).color(p.muted));
            }
            TestState::Done(Err(e)) => {
                mark(ui, false, p);
                let first = e.lines().next().unwrap_or(e);
                ui.add(egui::Label::new(RichText::new(first).color(p.error)).truncate()).on_hover_text(e);
            }
        }
        result
    }
}

fn confirm_frame(ctx: &egui::Context, p: &Palette) -> egui::Frame {
    egui::Frame::popup(&ctx.global_style())
        .fill(p.popup_bg)
        .corner_radius(12)
        .inner_margin(egui::Margin::same(20))
        .stroke(Stroke::new(1.0, p.popup_border))
}

/// DataGrip's green check / red cross after Test Connection (painted: the UI font has neither).
fn mark(ui: &mut Ui, ok: bool, p: &Palette) {
    let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
    let at = |x: f32, y: f32| pos2(r.left() + x, r.top() + y);
    if ok {
        ui.painter().add(egui::Shape::line(vec![at(2.5, 8.5), at(6.5, 12.5), at(13.5, 4.0)], Stroke::new(2.0, p.ok)));
    } else {
        let st = Stroke::new(2.0, p.error);
        ui.painter().line_segment([at(3.5, 3.5), at(12.5, 12.5)], st);
        ui.painter().line_segment([at(12.5, 3.5), at(3.5, 12.5)], st);
    }
}

/// IntelliJ's chevron instead of egui's triangle on combo boxes.
fn chevron(ui: &Ui, rect: Rect, _: &egui::style::WidgetVisuals, _open: bool) {
    let p = theme::pal(ui);
    icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(14.0, 14.0)), Ico::ChevronDown, p.muted, &p, true);
}

fn text_field(ui: &mut Ui, value: &mut String, hint: &str) -> egui::Response {
    ui.add(egui::TextEdit::singleline(value).hint_text(hint).margin(egui::vec2(8.0, 5.0)).desired_width(f32::INFINITY))
}

/// Full-width driver combo with the driver's icon, as DataGrip's.
fn driver_combo(ui: &mut Ui, cfg: &mut ConnectionConfig, p: &Palette) {
    let mut driver = cfg.driver;
    let resp = egui::ComboBox::from_id_salt("driver_combo")
        .icon(chevron)
        .selected_text(format!("      {}", driver.label()))
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for d in Driver::ALL {
                ui.selectable_value(&mut driver, d, d.label());
            }
        });
    let r = resp.response.rect;
    icons::paint(ui.painter(), Rect::from_min_size(pos2(r.left() + 8.0, r.center().y - 8.0), vec2(16.0, 16.0)), Ico::Driver(cfg.driver), p.icon, p, true);
    ConnectionDialog::change_driver(cfg, driver);
}

/// DataGrip's two-button segmented control ("default | URL only").
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: [(T, &str); 2], p: &Palette) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, (v, label)) in options.into_iter().enumerate() {
            let galley = ui.painter().layout_no_wrap(label.to_owned(), egui::FontId::proportional(13.0), p.text);
            let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 32.0, 28.0), Sense::click());
            let radius = if i == 0 {
                egui::CornerRadius { nw: 6, sw: 6, ne: 0, se: 0 }
            } else {
                egui::CornerRadius { nw: 0, sw: 0, ne: 6, se: 6 }
            };
            let fill = if *value == v { p.tab_selected_bg } else if resp.hovered() { p.hover } else { p.control_bg };
            let stroke = if *value == v { p.tab_selected_border } else { p.control_border };
            ui.painter().rect(rect, radius, fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
            ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text);
            if resp.clicked() {
                *value = v;
            }
        }
    });
}

/// Authentication and the fields it shows: User & Password, Authenticator (Snowflake) or No auth.
fn auth_rows(ui: &mut Ui, cfg: &mut ConnectionConfig) {
    let influx = cfg.driver == Driver::InfluxDb;
    let kusto = cfg.driver == Driver::Kusto;
    let label = |m: AuthMode| match m {
        AuthMode::UserPassword if influx => tr("Token"),
        AuthMode::UserPassword if kusto => tr("Application (client secret)"),
        AuthMode::UserPassword => tr("User & Password"),
        AuthMode::Authenticator => tr("Authenticator"),
        AuthMode::NoAuth => tr("No auth"),
        AuthMode::AzureCli => tr("Azure CLI"),
    };
    let modes: &[AuthMode] = if cfg.driver == Driver::Snowflake {
        &[AuthMode::UserPassword, AuthMode::Authenticator, AuthMode::NoAuth]
    } else if influx {
        &[AuthMode::UserPassword]
    } else if kusto {
        &[AuthMode::AzureCli, AuthMode::UserPassword, AuthMode::NoAuth]
    } else {
        &[AuthMode::UserPassword, AuthMode::NoAuth]
    };
    ui.label(tr("Authentication:"));
    egui::ComboBox::from_id_salt("ds_auth").icon(chevron).selected_text(label(cfg.auth)).width(ui.available_width()).show_ui(ui, |ui| {
        for &m in modes {
            ui.selectable_value(&mut cfg.auth, m, label(m));
        }
    });
    ui.end_row();

    if cfg.auth == AuthMode::Authenticator {
        ui.label(tr("Authenticator:"));
        text_field(ui, &mut cfg.authenticator, "");
        ui.end_row();
    }
    // Kusto: the Entra ID tenant (empty: the Azure CLI's own).
    if kusto && cfg.auth != AuthMode::NoAuth {
        ui.label(tr("Tenant:"));
        let mut tenant = cfg.property("tenant").unwrap_or_default().to_string();
        let hint = if cfg.auth == AuthMode::AzureCli { tr("<default>") } else { "<tenant>.onmicrosoft.com" };
        if text_field(ui, &mut tenant, hint).changed() {
            cfg.set_property("tenant", tenant.trim());
        }
        ui.end_row();
    }
    if !matches!(cfg.auth, AuthMode::NoAuth | AuthMode::AzureCli) {
        // InfluxDB: the organization (empty: the token's only one) and the API token. Kusto: an
        // Entra ID application's client ID and secret.
        ui.label(if influx { tr("Organization:") } else if kusto { tr("Client ID:") } else { tr("User:") });
        text_field(ui, &mut cfg.user, "");
        ui.end_row();

        ui.label(if influx { tr("Token:") } else if kusto { tr("Client secret:") } else { tr("Password:") });
        let hint = if cfg.password.is_empty() { tr("<empty>") } else { "" };
        ui.add(egui::TextEdit::singleline(&mut cfg.password).password(true).hint_text(hint).margin(egui::vec2(8.0, 5.0)).desired_width(f32::INFINITY));
        ui.end_row();
    }
}

/// The URL field, synced both ways with the fields above it.
fn url_row(ui: &mut Ui, cfg: &mut ConnectionConfig, url: &mut String, p: &Palette) {
    ui.label("URL:");
    ui.vertical(|ui| {
        let edit = text_field(ui, url, "");
        // A URL that doesn't parse (another driver's, a port over 65535…) leaves the fields as
        // they were; say so while it is being edited.
        let valid = !edit.has_focus() || apply_jdbc_url(&mut cfg.clone(), url);
        if edit.changed() {
            apply_jdbc_url(cfg, url);
        }
        if !edit.has_focus() {
            *url = jdbc_url(cfg);
        }
        if valid {
            ui.label(RichText::new(tr("Overrides settings above")).size(12.0).color(p.muted));
        } else {
            ui.label(RichText::new(trf("Not a valid {driver} URL: the fields keep their values", &[("driver", &cfg.driver.label())])).size(12.0).color(p.error));
        }
    });
    ui.end_row();
}

fn sqlite_file(ui: &mut Ui, cfg: &mut ConnectionConfig, error: &mut Option<String>) {
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 150.0).max(160.0);
        ui.add(egui::TextEdit::singleline(&mut cfg.file_path).margin(egui::vec2(8.0, 5.0)).desired_width(w));
        if ui.button(tr("Browse…")).clicked()
            && let Some(p) = rfd::FileDialog::new()
                .set_title(tr("Open SQLite Database"))
                .add_filter("SQLite", &["db", "sqlite", "sqlite3", "db3"])
                .add_filter(tr("All files"), &["*"])
                .pick_file()
        {
            cfg.file_path = p.to_string_lossy().into_owned();
        }
        if ui.button(tr("New…")).on_hover_text(tr("Create a new file")).clicked()
            && let Some(p) = rfd::FileDialog::new()
                .set_title(tr("New SQLite Database"))
                .add_filter("SQLite", &["db", "sqlite", "sqlite3"])
                .set_file_name(tr("new.db"))
                .save_file()
        {
            // The driver no longer creates files: it is created empty here (SQLite
            // initializes it on open). An existing file is reused.
            match std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&p) {
                Ok(_) => {
                    cfg.file_path = p.to_string_lossy().into_owned();
                    *error = None;
                }
                Err(e) => *error = Some(trf("Could not create {path}: {error}", &[("path", &p.display()), ("error", &e)])),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    //! Renders the window to PNGs in `target/dialog-snapshots/` (needs a GPU, hence ignored):
    //! `cargo test render_dialog -- --ignored`.

    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn switching_to_kusto_and_back() {
        let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
        ConnectionDialog::change_driver(&mut cfg, Driver::Kusto);
        // A cluster address and the Azure CLI's account, not localhost and postgres.
        assert_eq!((cfg.host.as_str(), cfg.port, cfg.user.as_str(), cfg.auth), ("", 443, "", AuthMode::AzureCli));
        ConnectionDialog::change_driver(&mut cfg, Driver::MySql);
        assert_eq!((cfg.host.as_str(), cfg.port, cfg.user.as_str(), cfg.auth), ("localhost", 3306, "root", AuthMode::UserPassword));
        // What was typed stays.
        cfg.host = "db.internal".into();
        cfg.user = "app".into();
        ConnectionDialog::change_driver(&mut cfg, Driver::Kusto);
        assert_eq!((cfg.host.as_str(), cfg.user.as_str(), cfg.auth), ("db.internal", "app", AuthMode::UserPassword));
    }

    fn snowflake_source() -> ConnectionConfig {
        let mut cfg = ConnectionConfig::new(3, Driver::Snowflake);
        cfg.name = "@ACME-XY12345.snowflakecomputing.com [3]".into();
        cfg.host = "ACME-XY12345.snowflakecomputing.com".into();
        cfg.user = "JDOE".into();
        cfg.password = "secret".into();
        cfg.auth = AuthMode::Authenticator;
        cfg.set_property("private_key_file", "/Users/jdoe/.ssh/snowflake_key.p8");
        cfg
    }

    fn render(name: &str, dark: bool, dialog: ConnectionDialog, setup: impl FnOnce(&mut ConnectionDialog)) {
        let mut dialog = dialog;
        setup(&mut dialog);
        // Fonts installed in one pass are only there from the next one.
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(vec2(1100.0, 840.0)).with_pixels_per_point(2.0).build_ui_state(
            move |ui, d: &mut ConnectionDialog| {
                let ctx = ui.ctx().clone();
                if std::mem::take(&mut first) {
                    theme::install_fonts(&ctx);
                    theme::apply(&ctx, dark);
                } else {
                    d.show(&ctx);
                }
            },
            dialog,
        );
        harness.run_steps(6);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/dialog-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        harness.render().expect("render").save(dir.join(format!("{name}.png"))).unwrap();
    }

    #[test]
    #[ignore]
    fn render_dialog() {
        let others = |sf: ConnectionConfig| {
            let mut pg = ConnectionConfig::new(1, Driver::Postgres);
            pg.name = "orders@localhost".into();
            pg.host = "localhost".into();
            pg.database = "orders".into();
            let mut my = ConnectionConfig::new(2, Driver::MySql);
            my.name = "inventory@mysql.example.com".into();
            vec![pg, my, sf]
        };
        let list = others(snowflake_source());
        render("snowflake_general", true, ConnectionDialog::edit(3, &list), |d| {
            d.test = (3, TestState::Done(Ok(Some("Snowflake 10.37.102".into()))));
        });
        let mut user_password = snowflake_source();
        user_password.auth = AuthMode::UserPassword;
        user_password.properties.clear();
        render("snowflake_user_password", true, ConnectionDialog::edit(3, &others(user_password)), |_| {});
        render("snowflake_advanced", true, ConnectionDialog::edit(3, &list), |d| d.tab = Tab::Advanced);
        render("snowflake_light", false, ConnectionDialog::edit(3, &list), |_| {});
        render("postgres_general", true, ConnectionDialog::edit(1, &list), |_| {});
        render("new_url_only", true, ConnectionDialog::new_connection(9, &list), |d| d.items.last_mut().unwrap().cfg.url_only = true);
    }

    fn harness(dialog: ConnectionDialog) -> egui_kittest::Harness<'static, ConnectionDialog> {
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1100.0, 840.0)).build_ui_state(
            move |ui, d: &mut ConnectionDialog| {
                let ctx = ui.ctx().clone();
                if std::mem::take(&mut first) {
                    theme::install_fonts(&ctx);
                    theme::apply(&ctx, true);
                } else {
                    d.show(&ctx);
                }
            },
            dialog,
        );
        h.run_steps(3);
        h
    }

    /// Drives the real widgets: the Authentication dropdown shows and hides its fields, the URL
    /// writes back into the fields, and the Advanced tab's empty row adds a property.
    #[test]
    fn fields_follow_authentication_and_url() {
        let mut cfg = snowflake_source();
        cfg.auth = AuthMode::UserPassword;
        cfg.properties.clear();
        let mut h = harness(ConnectionDialog::edit(3, &[cfg]));
        let cfg = |h: &egui_kittest::Harness<'_, ConnectionDialog>| h.state().items[0].cfg.clone();

        assert!(h.query_by_label("Authenticator:").is_none());
        assert!(h.query_by_label("User:").is_some());

        // The second combo box is Authentication (the first is Driver).
        let auth_combo = |h: &egui_kittest::Harness<'_, ConnectionDialog>| h.get_all_by_role(egui::accesskit::Role::ComboBox).nth(1).unwrap().click();
        auth_combo(&h);
        h.run_steps(3);
        h.get_by_label("Authenticator").click();
        h.run_steps(3);
        assert_eq!(cfg(&h).auth, AuthMode::Authenticator);
        assert!(h.query_by_label("Authenticator:").is_some());
        assert!(h.query_by_label("Password:").is_some());

        auth_combo(&h);
        h.run_steps(3);
        h.get_by_label("No auth").click();
        h.run_steps(3);
        assert_eq!(cfg(&h).auth, AuthMode::NoAuth);
        for gone in ["Authenticator:", "User:", "Password:"] {
            assert!(h.query_by_label(gone).is_none(), "{gone} still shown");
        }

        // URL → fields.
        let url = "jdbc:snowflake://ACME-XY12345.snowflakecomputing.com:443";
        h.get_all_by_value(url).next().unwrap().focus();
        h.run_steps(2);
        h.get_all_by_value(url).next().unwrap().type_text("?warehouse=COMPUTE_WH&role=ANALYST");
        h.run_steps(3);
        assert_eq!((cfg(&h).warehouse.as_str(), cfg(&h).role.as_str()), ("COMPUTE_WH", "ANALYST"));
        assert!(h.query_all_by_value("COMPUTE_WH").next().is_some(), "Warehouse field not updated");

        // Fields → URL: typing in Warehouse rebuilds the URL once it loses focus.
        h.get_all_by_value("COMPUTE_WH").next().unwrap().focus();
        h.run_steps(2);
        h.get_all_by_value("COMPUTE_WH").next().unwrap().type_text("2");
        h.run_steps(3);
        assert!(h.query_all_by_value(&format!("{url}?warehouse=COMPUTE_WH2&role=ANALYST")).next().is_some(), "URL not rebuilt");

        // Advanced: typing in the empty row adds a property.
        h.get_by_label("Advanced").click();
        h.run_steps(3);
        let n = h.get_all_by_role(egui::accesskit::Role::TextInput).count();
        assert!(n >= 2, "no property editors");
        h.get_all_by_role(egui::accesskit::Role::TextInput).nth(n - 2).unwrap().focus();
        h.run_steps(2);
        h.get_all_by_role(egui::accesskit::Role::TextInput).nth(n - 2).unwrap().type_text("private_key_file");
        h.run_steps(3);
        assert_eq!(cfg(&h).properties, vec![("private_key_file".to_string(), String::new())]);
        // Focus stayed in the new row: more typing extends the same property.
        h.get_all_by_value("private_key_file").next().unwrap().type_text("_pwd");
        h.run_steps(3);
        assert_eq!(cfg(&h).properties[0].0, "private_key_file_pwd");
    }

    /// Cancel / Escape with unapplied edits asks before discarding them (found in review of the
    /// Snowflake PR); with nothing changed it closes at once.
    #[test]
    fn cancel_asks_before_discarding_edits() {
        struct State {
            dialog: ConnectionDialog,
            closed: bool,
        }
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1100.0, 840.0)).build_ui_state(
            move |ui, st: &mut State| {
                let ctx = ui.ctx().clone();
                if std::mem::take(&mut first) {
                    theme::install_fonts(&ctx);
                    theme::apply(&ctx, true);
                } else if let Some(DialogResult::Cancel) = st.dialog.show(&ctx) {
                    st.closed = true;
                }
            },
            State { dialog: ConnectionDialog::edit(3, &[snowflake_source()]), closed: false },
        );
        h.run_steps(3);

        // Untouched: Escape closes.
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(h.state().closed);
        h.state_mut().closed = false;

        // Edited: Escape asks; Escape again goes back to editing.
        h.state_mut().dialog.items[0].cfg.warehouse = "WH".into();
        h.run_steps(2);
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(!h.state().closed);
        assert!(h.query_by_label("Unsaved changes").is_some());
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(!h.state().closed && h.query_by_label("Unsaved changes").is_none());

        // Cancel → Discard closes without saving.
        h.get_by_label("Cancel").click();
        h.run_steps(2);
        h.get_by_label("Discard").click();
        h.run_steps(2);
        assert!(h.state().closed);
    }
}
