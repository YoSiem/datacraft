# Table action previews

These full-window images render the real DataCraft interface on Windows with
synthetic data in a disposable SQLite database. The offscreen test drives the
application and background worker, verifies that duplication preserves 1,200 rows,
and verifies that JSON export includes all 1,200 rows independently of pagination.
No personal database or saved credentials are used.

- [Shared table context menu](context-menu.png)
- [Duplicate table SQL preview](duplicate-table.png)
- [Model from declared foreign keys](table-model.png)

Regenerate with a working GPU, without opening or controlling a desktop window:

```sh
cargo test --locked app::table_actions::tests::render_table_context_menu_workflows -- --ignored --exact --nocapture
```

The default output directory is `target/table-action-snapshots`. Review the generated
PNGs, then copy the selected images here. Keep the complete 1400 × 900 frame,
including the application header. These project-generated screenshots are not
evidence of live remote-engine or native macOS/Linux testing.
