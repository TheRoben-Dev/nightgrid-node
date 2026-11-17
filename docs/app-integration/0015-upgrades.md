# 0015 · Upgrades

Node milestone: **N8a** (nightgrid-node 0.19.0). Status: **proposed** for the app. Builds on 0001 (install).

## What the person gets

- **"Update node" on a host,** with the same confirmation as install. The node installs the release only if its checksum matches, checks that the new version comes up and stays up, and goes back to the old one by itself if not.

## 1. When to offer it

Compare `node.info.node.version` with the newest release the app knows. Offer the update when the release is newer. For hosts on 0.18.0 or older, keep the current install flow (copy the binary, then run `sudo ./nightgrid-node install`), since `upgrade` doesn't exist there yet.

## 2. Flow (node 0.19.0 and later)

1. Download the binary for the host's `arch` (`node.info.node.arch`) and `SHA256SUMS` from the release, and check the build provenance as the install flow does.
2. Copy the binary to a temporary path on the host over SSH, for example `/tmp/nightgrid-node-<version>`.
3. Run `sudo /usr/local/bin/nightgrid-node upgrade /tmp/nightgrid-node-<version> --sha256 <hex> --json` with the app's existing sudo confirmation. It takes about 15 to 40 seconds.
4. Read the JSON (`ok`, `reverted`, `from`, `to`, `steps`, `error`):
   - **`ok: true`:** "Updated to 0.19.0". Reconnect: the node restarted, so streams resubscribe as on any reconnect.
   - **`reverted: true`:** "0.19.0 didn't start, so the node went back to 0.18.0", with `error` under it.
   - **otherwise:** show `error`. It ends with the command to run if even the revert failed.
5. Delete the temporary file.

A wrong checksum, an older version (unless the person picked "go back to this version", which passes `--allow-downgrade`) or a binary for the wrong architecture changes nothing. Show the error as is.

## 3. Checks

- On a host with 0.19.0, updating to a newer release shows "upgraded to …" in the timeline, and the host stays connected.
- A tampered file (any byte changed) is refused with "checksum mismatch", and the host keeps running the old version.
- Hosts on older nodes still update with the install flow.
