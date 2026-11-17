# 0013 · Backups and scheduled jobs

Node milestone: **N7b** (nightgrid-node 0.17.0, `jobs` 1). Status: **proposed** for the app. Builds on 0002 (events and alerts), the app's backup board (M10g), scheduled jobs (M10v) and power schedules (M10k).

## What the person gets

- **Backup alerts with the app closed.** A failed or stale backup alerts in "while you were away", not only when the board is open.
- **Every run in the timeline.** Each run of a backup unit or of a job the app scheduled shows up as it finishes: "backup nightly finished · snapshot 1a2b saved · 5.2 GiB" or "job nightly sync failed (exit 23)".
- **Scheduled reboots on record.** "scheduled reboot ran" appears after the host is back.

The app's schedules don't change: they are still systemd timers the app installs, and systemd runs them. The node only watches.

## 1. Availability

With `jobs` in `caps` and `node.info.access.jobs == "ok"`, use the node (sections 2 and 3). `off` means root disabled `[jobs]`; behave as before.

Summaries, sizes and error lines come from the journal. If `access.journal` isn't `ok`, show the existing journal offer (0005) next to the backup board: "Allow reading the journal to see backup totals and errors."

## 2. Backup jobs

- On connect, and whenever a host's backup jobs change, call `jobs.set` on that host's node with `{backups: [{id, name, tool, source, target, staleHours}]}`: the app's `BackupJob` without `hostId`. Push the whole list each time.
- A `bad_request` names the job. Show it on that job and keep reading it from the app.
- **Board:** call `jobs.latest` when the board opens and on reconnect. `backups[]` entries are the app's own `BackupStatus`, so render them with the existing row, with "read by the node 14:02" from `at`. The app's own read over SSH stays as the refresh button.
- **Alerts:** the node raises `backupFailed` and `backupStale` with key `backup:<jobId>` (app: `backup:<hostId>:<jobId>`). Push `backupStale` with the rest of `alerts.rules.set`, which the app already does. While a node reads a host's jobs, the app should not raise those alerts itself for them.

## 3. Scheduled jobs and the timeline

- **Timeline:** `job` events render like `action` events. Older app builds see them as `other`, which already shows title and detail. Add a filter chip "Jobs".
- **Scheduled jobs view:** `jobs.latest.runs` has the last run of each `nightgrid-job-*` and `nightgrid-power-*` unit (`result`, `started`, `finished`, `exitCode`, `summary`). Where the view shows a timer's last result, it can add the summary line, for example "rsync error: some files vanished".
- **Test restores:** these stay on request in the app. If the person wants them scheduled, the app can install the restore script as a `nightgrid-job-*`; the node then reports it like any other job.

## 4. Checks

- On a host with node 0.17.0 and `allow journal`:
  - Add a backup job for a restic timer. After its next run, the timeline shows "backup … finished" with the snapshot line, and `jobs.latest` matches the app's own board.
  - Make it fail (for example, lock the repository). "BACKUP FAILED" arrives with the app closed.
  - A path job whose newest file is older than its limit shows "BACKUP STALE".
- A `nightgrid-job-*` job made in the app shows "job … finished" or "failed (exit N)" after each run.
- After a scheduled reboot, "scheduled reboot ran" appears (needs a persistent journal).
- Hosts without the node, or with an older node, behave exactly as before.
