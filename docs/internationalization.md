# Interface languages and Unicode

Choose **Language → العربية** in Settings for the Arabic interface. Arabic system
locales (including `ar-SA` and `ar-AE`) select Arabic on first launch. An explicit
language choice is remembered. The translation uses Modern Standard Arabic.

Noto Sans Arabic is bundled for the interface, SQL editor and results, including
when another interface language is selected. Arabic text is shaped and ordered
for display; SQL, clipboard text, exported files and database values retain their
original logical Unicode character order. The workspace keeps its existing panel
layout rather than mirroring the SQL tools.

## Text rendering dependency

Released egui 0.36.2 incorrectly orders multi-word Arabic text and can produce
more rendered characters than source characters, breaking the editing cursor.
The egui crates are temporarily pinned to commit
`c93e8863e958507056415e2a6dace56e23ba5f3a` of
[egui PR #8577](https://github.com/emilk/egui/pull/8577), which adds bidi ordering,
direction-aware cursor positions and composed glyph clusters. All egui crates
use the same revision to preserve their internal API compatibility.

This is an unreleased dependency patch. Replace the Cargo patches with a released
version containing these fixes once available, and rerun the Arabic rendering,
editing and full application tests. The patch does not implement every part of
bidi editing: trailing whitespace follows the patch's existing limitation, and
visual arrow-key navigation and selections across mixed-direction boundaries
still need care. It also introduces upstream API deprecation warnings in existing
callers; those are separate from the RTL fix.

## Database and file encoding

DataCraft keeps text as Rust UTF-8 strings and preserves Unicode in grid edits,
imports and exports. SQL Server generated values use `N'…'` Unicode literals.
The MySQL/MariaDB driver negotiates `utf8mb4` with servers that support it.
SQLite text supports Unicode without changing a database's schema.

The server's schema and encoding still determine which values can be stored:

- Use `utf8mb4` columns in MySQL/MariaDB for Arabic and supplementary characters
  such as emoji; the older `utf8` character set cannot store all Unicode.
- Use Unicode-capable SQL Server columns (`nvarchar`/`nchar`, or appropriately
  configured UTF-8 `varchar`). A Unicode literal cannot add Unicode support to
  an incompatible destination column.
- PostgreSQL and Oracle databases need a suitable server encoding (typically
  UTF8 and AL32UTF8 respectively).
- CSV/TSV and SQL text files are UTF-8. Legacy code pages are not auto-detected.

DataCraft does not migrate existing columns, change collations, normalize stored
text or repair data previously saved as question marks or replacement characters.
Other scripts and additional emoji use the installed system fallback fonts;
system color emoji fonts are enabled on Windows and macOS. No single bundled font
covers every Unicode character. A missing glyph does not change the stored text.

Regression tests cover Arabic and combining marks, Latin, CJK, Hangul, Thai,
Devanagari, Hebrew and supplementary characters in SQLite insert/update/read and
reopen operations, as well as Arabic identifiers, CSV round trips, JSON export,
SQL literals and MySQL text decoding. External engines require separate tests
against disposable databases.
