# 0040 · Hub: the app's profile, sealed, store-and-forward

Node release: **0.45.0** (hub only). Status: proposed. Issue: [#50](https://github.com/TheRoben-Dev/nightgrid-node/issues/50). Decision N-096. Schemas: [PROTOCOL.md](../PROTOCOL.md#hub-socket-profile-1-0450).

Additive and versioned like [0020](0020-hub-fleet-sync.md). It changes nothing in 0001 to 0039 or in `fleet` 1. An app without this spec, against a hub with it, sees nothing new. A hub without it (no `profile` in `hello`) leaves the app on its LAN app-to-app sync (D-205 to D-209), exactly as today.

## What the person gets

- A fresh install at a site with a hub: add the hub host once, then **Import from hub** fills hosts, groups, services, dashboards, notes, alert rules and shared settings, with no other app open.
- Devices that are never open at the same time stay in sync through the hub: the laptop's edit waits on the hub until the desktop opens.
- Works on a plain LAN with no internet and no tailnet (the hub socket is reached over SSH to the hub's LAN address).

## 1. When to offer it

- The hub's `hello` lists capability `profile` ≥ 1.
- **Settings → Sync** shows the hub as a sync target per site ("Sync profile through hub pi"), off until the person turns it on and sets or enters the profile passphrase.
- The empty-hosts screen offers **Import from hub** once a hub host is added and `profile.list` returns at least one profile. Show `items`, `lastPush` and `lastDevice` from the list so the person can tell which profile is theirs.

## 2. Crypto and what leaves the app (app-owned)

- Seal every item before it leaves: for example XChaCha20-Poly1305 with a key derived from the profile passphrase (the same PBKDF2-HMAC-SHA256 210k derivation and salt the app uses for passphrase pairing, with a distinct info string such as `nightgrid-hub-profile-v1`). Put `tbl`, `uid` and `modified` in the associated data, so the hub can't move a blob to another item or change its time without the app noticing.
- `blob` is base64 or base64url of nonce + ciphertext. The hub never reads it.
- Recommended: send `uid` as-is (it is a random id) and `tbl` as-is, or both as keyed hashes if you want zero metadata on the hub; the hub only needs them to be stable.
- Keep the app's filters: no saved secrets, no per-device settings. Tombstones may carry a sealed blob (recommended, so a delete is authenticated) or none.
- `profileId`: derive it from the profile (for example the first 32 hex characters of a hash of the profile's public id and salt), 8-64 characters of `A-Z a-z 0-9 _ -`.

## 3. Syncing

Keep one cursor per (hub, profileId), like `events.sync`:

1. **Pull**: `profile.pull {profileId, after: cursor}` until `more` is false. Open each blob; drop any that fails to authenticate (log it, it means the hub is broken or tampered with). Take a snapshot before applying (D-209), then merge with the app's item-level last write wins. Store `next` as the cursor only after the page is applied.
2. **Push**: send local items changed since the last successful push, at most 1000 per call (and under the 4 MiB line), with `device` set to the app's device name. The hub answers `accepted` / `ignored`; ignored means the hub already has the same or a newer copy, which the next pull brings.
3. If a pull answers `resync: true`, pull again from 0 into a fresh view, then merge (deletes older than the hub's tombstone window may otherwise be missed).
4. Errors: `too large` (profile over `[profile] max_mb`, or the hub's profile slots full) → show the reason, keep syncing app-to-app. `bad request` "rate limited" → retry after a minute. `capability disabled` → the hub's root turned it off; fall back.

Sync when the app opens, after local edits (debounced, at most once a minute), and on a timer while open. The hub applies last write wins too (newer `modified`, then a tombstone at the same time, then the larger blob), so a replay of an old push changes nothing.

## 4. Showing it

- Settings → Sync: per hub, "Profile on hub pi: 412 items, last pushed 14:02 from desktop", and **Remove from hub** (`profile.drop`, after a confirmation).
- Hub card (0020 §6): a line with the profile's size against `maxBytes`.

## 5. Checks

1. A site with a hub; the desktop pushes its profile, then closes. On a fresh install on another machine on the same LAN (no internet, no tailnet), after adding the hub host, Import from hub restores all hosts, services, dashboards and notes.
2. Edit a host on the laptop and close it; open the desktop later. The edit arrives via the hub; the newer edit wins on conflicts.
3. The hub's database (`/var/lib/nightgrid-hub/hub.db`, table `profile_items`) holds no readable host names, addresses or notes.
4. Replaying an old `profile.push` returns `ignored` for every record and changes nothing.
5. An app build without this spec against a hub with it: no change. A hub without `profile`: the app keeps LAN app-to-app sync.

## Later

LAN discovery (UDP 47921) and passphrase pairing directly with the hub, so a fresh install finds it without adding the host first, come as their own opt-in hub unit in a later release, with its own spec. Until then the first hub host is added by hand.
