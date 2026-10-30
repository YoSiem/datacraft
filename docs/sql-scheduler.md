# Scheduled SQL tasks

Open **File → SQL scheduler…** in the DataCraft application menu. Add a task,
choose a saved SQL data source, enter its SQL and interval in seconds, then
confirm the SQL before saving. Leave the database field blank to use the data
source's default database. A task can be paused without stopping an execution
that is already in progress. Editing or deleting an active task is refused until
that execution has finished; pausing prevents its next execution.

An enabled task starts a separate background process and registers it to start
when the user signs in: the current user's Run entry on Windows, a LaunchAgent
on macOS, or a user systemd service on Linux. When the systemd user manager is
unavailable, Linux uses the desktop's XDG autostart mechanism, which requires
an XDG-compatible desktop session. The process does not open a window and continues after DataCraft
closes. It runs only while the computer and user session are available; it does
not wake a sleeping computer or run while the user is signed out. Keep the
DataCraft executable in its registered location. If it moves, open the scheduler
and enable the background scheduler again. System startup registration failures
are shown in the scheduler window. The background process and its startup entry
remain installed when every task is paused or deleted; remove the per-user
startup entry through the system's service or startup settings if desired.

The first execution is due after the configured interval. Missed intervals are
not replayed individually, and one task never runs concurrently with itself.
If an execution takes longer than its interval, its next run is scheduled after
it finishes. A maximum of four tasks run at once. The history shows the last
100 runs per task and records failures and interrupted runs. If the process or
computer stops during a statement, the database may have completed it even if
DataCraft could not record the outcome; inspect the database before retrying.
SQL statements in a task run in order and stop at the first error. They are not
automatically wrapped in a transaction.

Background tasks use the saved data source settings and system credential
store. They cannot answer password or SSH host-key prompts: save the required
credentials and trust the SSH host key through the connection dialog first.
An unavailable or locked credential store may prevent a run. Read-only data
sources retain their existing restrictions. The task's SQL and execution
history are stored locally; do not put passwords or tokens in SQL text.
