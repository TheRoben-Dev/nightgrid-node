# 0022 · Old and 32-bit hosts, the release manifest, clearer failures

Node: **0.28.0**. Status: **proposed** for the app. Additive: it widens 0001 (install) and 0015 (upgrades), and changes nothing in the protocol, the methods or the link. An app that ignores it keeps working exactly as today on x86_64 and aarch64 hosts.

## What the person gets

- Nightgrid Node on older and smaller boxes: a ReadyNAS RN102 (armv7, 512 MB, ReadyNAS OS 6 with systemd 215), a Raspberry Pi 1 or Zero (armv6), an old 32-bit PC (i686), and hosts without systemd (Alpine and other OpenRC hosts, sysvinit Debian or Devuan, BusyBox userlands).
- A clear reason instead of "not supported" or a raw error when something can't be done, such as "this host has no systemd, so the extra access isn't available".

## 1. Picking the file

Release files are still `nightgrid-node-<tag>-<arch>-linux` and `nightgrid-hub-<tag>-<arch>-linux`, all static (musl), all in `SHA256SUMS`. The arches are now:

| `uname -m` | asset arch |
|---|---|
| `x86_64`, `amd64` | `x86_64` |
| `aarch64`, `arm64` | `aarch64` |
| `armv7l`, `armv7`, `armv7hl`, `armv8l` | `armv7` |
| `armv6l`, `armv6` | `armv6` |
| `i386`, `i486`, `i586`, `i686`, `x86` | `i686` |

Anything else is "not supported". `armv8l` is a 32-bit userland on an ARMv8 CPU, so it gets `armv7`, never `aarch64`. The `armv6` build also runs on armv7, but it's slower, so only use it for `armv6l`. The `i686` build needs an i686-class CPU (Pentium Pro or later, with `cmov`), which covers everything that reports `i586` or newer in practice.

The table is also in every release's `manifest.json` (below) as `uname`, so the app can read it instead of hard-coding it. A host's own arch is in `node.info.node.arch` (unchanged) and in the install and doctor reports (§3).

## 2. `manifest.json`

Each release from 0.28.0 has one more file, `manifest.json`, so the app reads one small file instead of the release API plus `SHA256SUMS`:

```json
{
  "manifest": 1,
  "version": "0.28.0",
  "tag": "v0.28.0",
  "protocol": {"min": 1, "max": 2},
  "programs": {
    "nightgrid-node": {
      "armv7": {"file": "nightgrid-node-v0.28.0-armv7-linux", "size": 4925424, "sha256": "…64 hex"},
      "x86_64": {"file": "…", "size": 0, "sha256": "…"}
    },
    "nightgrid-hub": {"x86_64": {"file": "…", "size": 0, "sha256": "…"}}
  },
  "uname": {"armv7l": "armv7", "x86_64": "x86_64"}
}
```

- `manifest.json` itself is listed in `SHA256SUMS`, and both have GitHub build provenance. It is **not signed** (decision of 2026-11-16: checksums plus provenance). If a `manifest.json.minisig` ever appears, verify it against a key pinned in the app, as 0001 says for binaries.
- `protocol` is the range the binaries in this release speak (`hello`'s `protoMin`/`protoMax`), so the app can tell before uploading that a release is too new or too old for it.
- Unknown fields are ignored. A release without `manifest.json` (0.27.0 and older) goes the old way: `SHA256SUMS` and the file name pattern.
- Always verify the downloaded binary's SHA-256 against the manifest or `SHA256SUMS`, as today.

## 3. Install and doctor reports

`install --json` and `doctor --json` now also carry the host's facts at the top level:

```json
{"action": "install", "version": "0.28.0", …,
 "arch": "armv7l", "assetArch": "armv7", "libc": "glibc", "init": "systemd", "initVersion": 215,
 "kernel": "3.0.101.RN_ARM.13", "busybox": false,
 "limits": ["no cgroup v2: no per-service and per-container usage"],
 "unsupported": ["ProtectSystem=strict (systemd 232)", "IPAddressDeny=any (systemd 235)", "…"]}
```

- `init`: `systemd`, `openrc`, `sysvinit` or `other`. `initVersion` only with systemd. `libc`: `glibc`, `musl`, `uclibc` or `unknown` (what the host's userland uses; the node is static either way). `busybox`: whether the shell tools are BusyBox. `assetArch` is missing when no build fits.
- `limits` (install and doctor): short sentences on what this host can't do. `unsupported` (install, only on systemd older than 247): the unit settings this systemd doesn't know, which the node commented out of its unit (it uses the older spelling where there is one, such as `ProtectSystem=full` and `MemoryLimit=`). Show both as notes under "Installed". They are never errors.
- `doctor` adds findings `host` (arch, libc, init, kernel), `sandbox` (which unit settings this systemd ignores, or "no systemd: the service runs from an init script") and `limits`. Same `check`/`level`/`detail` shape as before.

## 4. Hosts without systemd

`install` works on OpenRC (`/etc/init.d/nightgrid-node`, `rc-update add … default`) and sysvinit with `start-stop-daemon` (`update-rc.d`, `chkconfig`, or rc links). The service runs as the same unprivileged account, with the same socket, group and directory modes, so the app's session, `bridge` and the 0021 probe work unchanged. The probe's `systemctl is-active` reads as missing there. When `unit` is missing and the socket isn't visible, use `init` from the last install report, or run `nightgrid-node doctor --json`, before you call the node "not running".

What needs systemd is refused with a JSON error (with `--json`), not a raw message:

```json
{"error": {"code": "no_systemd", "message": "this needs systemd, and this host's init is openrc (…)"}}
```

That is every `allow`, `deny` and `serve` command, `upgrade` (on such a host, install the new binary with `install`, which keeps the history), and the whole hub. Show "Not available on this host (no systemd)" and hide those switches when `init` isn't `systemd`.

Other failed commands with `--json` (`install`, `uninstall`, `allow`, `deny`, `serve`, `upgrade`, and the hub's) print `{"error": {"code": "not_root" | "failed", "message"}}` with exit status 1 instead of a bare line on stderr. Older nodes print plain text, so keep the old fallback: if stdout isn't JSON, show stderr.

## 5. `bridge` failures

From 0.28.0, `nightgrid-node bridge` and `nightgrid-hub bridge` print one JSON line and exit 1 when they can't reach the socket ([PROTOCOL.md](../PROTOCOL.md#when-the-socket-cant-be-reached-bridge-0280)):

- `{"error": {"code": "no_access", …}}`: running, but this login can't use the socket yet. Try the `sg nightgrid-node -c 'nightgrid-node bridge'` route, then "Reconnect to finish setup".
- `{"error": {"code": "not_running", …}}`: no socket, or nothing answers. Show "installed, not running".
- Exit status 127 with nothing on stdout: no bridge on this host (not installed).

So the app can run the bridge first and run the probe only when the first line is an error. A first line with `"proto"` is the normal `hello` answer, and an older node that fails prints only to stderr.

## 6. Checks

- An RN102-class host (armv7l, systemd 215): install, the report shows `initVersion: 215` and the `unsupported` note, metrics sync, uninstall.
- An Alpine (OpenRC) host and a sysvinit Debian host: install, sync, and "Not available on this host" on the access switches.
- `uname -m` = `mips`: "not supported", nothing uploaded.
- A release with and without `manifest.json`: the same binary chosen and verified.
- Bridge as a login not in the group yet: the `sg` route is taken without a probe round trip.
