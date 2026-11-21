# 0039 · Windows node W3f: jobs and backups

Node: **0.44.0** (W3f of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on the Linux jobs spec. Additive: no method, field or frame changed.

## What the person gets

Backup jobs and scheduled tasks on a Windows host are watched with the app closed, as on Linux. A failed or stale backup raises the same alerts, and each finished run lands in the timeline.

## What the app sees

- `hello` caps now include `jobs` on Windows. Task reads need `allow health`, because a task an administrator registers can be read only by administrators. Without it, a task job answers with an error that names the command; path jobs work either way. `jobs.set`, `jobs.get` and `jobs.latest` work as on Linux, with the same shapes.
- A backup job with `source: "unit"` names a scheduled task path, as the app already stores them for Windows hosts: `\Backups\Nightly`, or `\Microsoft\Windows\Backup\Microsoft-Windows-WindowsBackup` for Windows Server Backup. The status follows the app's own rules in `core/src/win/backups.rs`. A failure's `exitCode` is the task's `LastTaskResult`, and its `summary` is `Last result 0x…`.
- A `path` job takes `D:\Backups` or `\\nas\share`. A UNC path answers with an error, because the node has no network. On Windows, `jobs.set` refuses POSIX targets (`/srv/backup`) and systemd unit names, with the error text.
- `jobs.latest` `runs` lists the tasks in `\Nightgrid\`. `unit` is the full task path, and `description` is the task's description or its name. `started` and `finished` are both the task's last run time, because Task Scheduler keeps only the start.
- Events: `job` events read `backup <name> finished|failed` and `job <description> finished|failed (exit N)`, as on Linux.

## What to change in the app

- When a task job's error mentions `allow health`, offer that command in an elevated PowerShell.
- Send the Windows host's backup jobs to the node with `jobs.set` when `hello` lists `jobs`, and prefer `jobs.latest` over the SSH status script, as on Linux.
- For a UNC path job, keep reading it over SSH, since the node can't reach it.
