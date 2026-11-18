#!/usr/bin/env python3
"""Writes a release's manifest.json: version, protocol range, and per program
and arch the file name, size and SHA-256. The app reads this one file instead
of the release API plus SHA256SUMS. Unsigned for now (checksums and GitHub
build provenance, decision N-014); a minisign signature would cover it later.

    ci/manifest.py OUT_DIR TAG PROTO_RS > OUT_DIR/manifest.json
"""
import hashlib
import json
import os
import re
import sys

# `uname -m` to release arch (also in docs/app-integration/0022).
UNAME = {
    "x86_64": "x86_64", "amd64": "x86_64",
    "aarch64": "aarch64", "arm64": "aarch64",
    "armv7l": "armv7", "armv7": "armv7", "armv7hl": "armv7", "armv8l": "armv7",
    "armv6l": "armv6", "armv6": "armv6",
    "i386": "i686", "i486": "i686", "i586": "i686", "i686": "i686", "x86": "i686",
}


def main(out_dir, tag, proto_rs):
    src = open(proto_rs, encoding="utf-8").read()
    pmax = int(re.search(r"pub const PROTO_VERSION: u32 = (\d+);", src).group(1))
    pmin = int(re.search(r"pub const PROTO_MIN: u32 = (\d+);", src).group(1))
    programs = {}
    pat = re.compile(r"^(nightgrid-node|nightgrid-hub)-" + re.escape(tag) + r"-([a-z0-9_]+)-linux$")
    for name in sorted(os.listdir(out_dir)):
        m = pat.match(name)
        if not m:
            continue
        path = os.path.join(out_dir, name)
        with open(path, "rb") as f:
            digest = hashlib.sha256(f.read()).hexdigest()
        programs.setdefault(m.group(1), {})[m.group(2)] = {
            "file": name,
            "size": os.path.getsize(path),
            "sha256": digest,
        }
    if not programs.get("nightgrid-node"):
        sys.exit("no binaries for " + tag)
    manifest = {
        "manifest": 1,
        "version": tag.lstrip("v"),
        "tag": tag,
        "protocol": {"min": pmin, "max": pmax},
        "programs": programs,
        "uname": UNAME,
    }
    json.dump(manifest, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main(*sys.argv[1:4])
