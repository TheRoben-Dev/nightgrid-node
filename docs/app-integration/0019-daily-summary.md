# 0019 · Daily summary facts for DRIFT

Node milestone: **N9** (nightgrid-node 0.23.0). Status: **proposed** for the app. Builds on 0001 (the node session) and the app's daily summary skill (M10p, D-108).

## What the person gets

- **"What happened overnight?"** gets a real answer. DRIFT's daily summary today only knows what the app saw while it was open. With the node, each host adds what it recorded while the app was closed: outages and restarts, alerts that came and went, crashing containers, failed backups, logins, changed files, new listeners, and disks filling up.

## 1. Where it goes

In `summaryInput()` (`ui/src/lib/stores/helper.svelte.ts`), add a field for the node's facts per host:

```ts
nodes: { host: string; level: "ok" | "warn" | "critical"; complete: boolean; facts: string[] }[];
```

Fill it with `summary.daily` (no params, the last 24 h) from each host whose **node session is already open** and whose welcome has capability `summary` ≥ 1. Like the rest of the summary (D-108), don't open SSH connections or wake hosts for it. Use at most a 2 s timeout per host and run them in parallel. A host that doesn't answer in time is left out, and the facts say so.

In `summaryFacts()`, add one block per host after the app's own lines, worst `level` first, with every fact prefixed by the host name:

```
nas: Uptime: down (crash or power loss) for 2 h from 7 h ago.
nas: Jobs and backups: backup nightly failed (exit 1); 3 other runs fine.
pi: Active alerts: none.
```

To keep the prompt small, give at most 8 facts per host and 40 in all. Keep a host's `Coverage` line if there is one, because it tells the model that the night isn't fully covered.

Drop node lines that repeat what the app already has: the node's `Active alerts` when the app's alert list already covers that host, and `Updates` when the app has a fresher update check for that host.

## 2. Wording for DRIFT

Add one sentence to the summary skill's instructions: "Lines that start with a host name come from that host's node, which watched it while the app was closed." Nothing else changes. The facts are plain sentences and need no parsing.

## 3. Checks

- Close the app overnight and stop a container on a host with the node. In the morning, "daily summary" mentions the container on that host.
- A host without the node, or one on an older node, gives the same summary as before.
- Asking for a summary opens no new SSH connections.
