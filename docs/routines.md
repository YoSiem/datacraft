# Procedures and functions

PostgreSQL, MySQL, MariaDB, SQL Server, Oracle and Snowflake databases show
**Procedures** and **Functions** folders next to Tables and Views. SQLite has no stored
routines; Oracle packages are not listed (standalone procedures and functions are).

The routines of a database load in their own query right after its schema, so the tree
appears first. Each routine shows its arguments; hover for the return type and language.
Oracle routines that failed to compile are shown in red. Filtering the explorer by name
also finds routines.

## Actions

| Action | Where | What it does |
| --- | --- | --- |
| New Procedure… / New Function… | database, schema, folder or routine menu | Opens the routine dialog with a template for the engine |
| Edit… | routine menu, or double-click | Reads the definition from the server into the dialog |
| Execute… | routine menu | Asks for argument values and runs the call in a new console |
| Generate DDL | routine menu | Opens the server's own `CREATE` statement in a console |
| Drop… | routine menu | Confirms and drops it; PostgreSQL and Snowflake name the overload by its argument types |

## Routine dialog

- **Parameters**: name, mode (`IN`, `OUT`, `INOUT`, `VARIADIC` or SQL Server `OUTPUT`, as
  the engine allows), type with common choices, default value (not on MySQL / MariaDB) and
  order.
- **Returns** (functions, and Snowflake procedures): a value, PostgreSQL `SETOF`, or a
  table with its columns. On SQL Server a table without columns is an inline function
  (`RETURNS TABLE AS RETURN (SELECT …)`), and with columns a multi-statement one.
- **Options**: the clauses of each engine, for example PostgreSQL volatility, `STRICT`,
  `SECURITY DEFINER` and `PARALLEL`; MySQL `DEFINER`, `DETERMINISTIC`, data access and
  `SQL SECURITY`; SQL Server `EXECUTE AS`, `SCHEMABINDING` and `RECOMPILE`; Oracle
  `AUTHID`, `DETERMINISTIC`, `PARALLEL_ENABLE` and `PIPELINED`; Snowflake `SECURE`,
  `EXECUTE AS` and the runtime, packages and handler of Python, Java or Scala routines.
  **Other clauses** are written as typed (PostgreSQL `COST`, `SET search_path`…).
- **Body** in a highlighted editor, and a live preview of the statements that will run.

Saving an edited routine replaces it in place when the engine allows it:

| Engine | Same name | Renamed | Parameters or return type changed |
| --- | --- | --- | --- |
| PostgreSQL | `CREATE OR REPLACE` | `ALTER … RENAME`, then replace | drop and create, in one transaction |
| MariaDB | `CREATE OR REPLACE` | create the new one, drop the old | same as same name |
| MySQL | compile under a scratch name, then drop and create | create the new one, drop the old | same as same name |
| SQL Server | `ALTER` | create the new one, drop the old | drop and create in one transaction when the function kind changes |
| Oracle | compile under a scratch name, then `CREATE OR REPLACE` | compile under a scratch name, create the new one, drop the old | same as same name |
| Snowflake | `CREATE OR REPLACE` | create the new one, drop the old | create the new overload, drop the old one |

MySQL cannot replace a routine, so edits and new routines with **OR REPLACE** are first
compiled under a scratch name. Oracle also checks a scratch routine before an edit or
replacement, reporting compilation errors (line, position and message) and removing the
scratch routine. A name collision stops the operation without replacing the scratch object.
The check cannot guarantee the final creation will succeed: for example, the database state
can change between statements, or the routine may refer to its own name. Review the SQL
preview and keep a backup before replacing a routine. PostgreSQL signature changes and
SQL Server function-kind changes drop and recreate the object, removing its grants; the
dialog warns when this happens. A definition the dialog can't split into fields (for example
a PostgreSQL `BEGIN ATOMIC` body or a SQL Server routine `WITH ENCRYPTION` options it doesn't
know) opens as an editable statement instead; its SQL is run directly without the scratch check.

## Execute

Each input parameter gets a value field. **NULL** passes `NULL`, **Default** leaves the
argument out so its default applies (with named arguments where needed), and **SQL**
writes the value as an expression instead of a text literal. Plain decimal numbers and
booleans are written as typed; other values are quoted. PostgreSQL and Snowflake values are
cast to the parameter type so the right overload is called.

**Run** opens a console with the call and runs it; **Open in Console** only opens it. Output
parameters come back as rows:

| Engine | How output parameters are shown |
| --- | --- |
| PostgreSQL | `CALL` returns them; functions with OUT parameters are queried with `SELECT * FROM` |
| MySQL / MariaDB | session variables (`@name`) selected after the `CALL` |
| SQL Server | declared variables and the return value selected after `EXEC` |
| Oracle | an anonymous block prints them with `DBMS_OUTPUT`, shown as a result |
| Snowflake | `CALL` returns the procedure's result |

The values typed for a routine are kept for the rest of the session. PostgreSQL trigger
functions can't be executed directly.

The Oracle console also shows `DBMS_OUTPUT` lines written by any anonymous block, once the
block enables it with `DBMS_OUTPUT.ENABLE`.
