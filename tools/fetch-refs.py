#!/usr/bin/env python3
"""Fetch the recordings that `tools/replay` replays, from Microsoft's public repositories, into `refs/recordings/`.

    python3 tools/fetch-refs.py                    # everything the replay needs (about 100 MB download, 254 MB on disk)
    python3 tools/fetch-refs.py --only blob        # packages whose name contains "blob"
    python3 tools/fetch-refs.py --dest /some/dir   # another directory than refs/ (the replay reads refs/recordings/)
    python3 tools/fetch-refs.py --list             # print the pinned sources and stop

Run it once; running it again fetches only what is missing or what the pins changed. Standard library only, Python 3.9 or newer.

What it fetches. The replay (`tools/replay/run_all.py`) sends the real Azure traffic that the Azure SDK for Python's own test
suite recorded (each test's requests and Azure's answers, as JSON) to the emulator and compares the answers. Microsoft keeps those
recordings in https://github.com/Azure/azure-sdk-assets (MIT licence, Copyright Microsoft), one tag per package of the SDK. Each
package below is pinned to the commit of its tag, so the download is the same every time. Downloaded: the tarball of that commit
from codeload.github.com, of which only `python/sdk/<service>/<package>/tests/recordings/` is kept. Nothing else is fetched; the
REST API specifications (Azure/azure-rest-api-specs) are not needed by the replay and are not fetched.

The pins. The tags are what the Azure SDK for Python named in its `assets.json` files on 2026-11-13 (commit 3e59835146b8 of
Azure/azure-sdk-for-python) for the data-plane packages; the `azure-mgmt-*` packages no longer have an `assets.json`, so their tags
were found by comparing every tag of the package with the recordings that `fidelity/SCORES.md` was measured on (2026-11-13): the
files are byte for byte the same. To move to newer recordings, put the new tag and its commit (`git ls-remote --tags
https://github.com/Azure/azure-sdk-assets '<tag>'`) into PINS, run the replay, and update `fidelity/SCORES.md` if the numbers moved.

The recordings are Microsoft's, not part of azemu, and not in the azemu repository (`refs/` is ignored by git). Their licence is the
MIT licence of azure-sdk-assets. See NOTICE."""
import argparse
import json
import os
import shutil
import sys
import tarfile
import time
import urllib.error
import urllib.request

ASSETS_REPO = "Azure/azure-sdk-assets"

# (directory below python/sdk/, tag of Azure/azure-sdk-assets, commit of that tag)
PINS = [
    ("appconfiguration/azure-appconfiguration", "python/appconfiguration/azure-appconfiguration_34c93ff839", "34c93ff8392802fa884d45ce42029ea4943de6d1"),
    ("cosmos/azure-mgmt-cosmosdb", "python/cosmosdb/azure-mgmt-cosmosdb_b5ac1c9e19", "b5ac1c9e19dd6c994b96d258ac37a32d1259b5fe"),
    ("eventgrid/azure-eventgrid", "python/eventgrid/azure-eventgrid_3ae14bceff", "3ae14bceff5ccb0bc89d339ba796bb55e42d35d4"),
    ("eventgrid/azure-mgmt-eventgrid", "python/eventgrid/azure-mgmt-eventgrid_cf1f9ef41d", "cf1f9ef41d156c8e1d191c2defecf6e241fc3fd4"),
    ("eventhub/azure-mgmt-eventhub", "python/eventhub/azure-mgmt-eventhub_97ea34d10b", "97ea34d10b55ad702d8c07012d8d1007ca8bc666"),
    ("keyvault/azure-keyvault-certificates", "python/keyvault/azure-keyvault-certificates_de4a448a91", "de4a448a91a379f2c8f464a58df8cea57821ba12"),
    ("keyvault/azure-keyvault-keys", "python/keyvault/azure-keyvault-keys_ebec9cd9be", "ebec9cd9be4cebbb76a4df49df123ee6ae5d7523"),
    ("keyvault/azure-keyvault-secrets", "python/keyvault/azure-keyvault-secrets_6d5d3a0d1e", "6d5d3a0d1ee14fe6def280562332f1039693bcd1"),
    ("keyvault/azure-mgmt-keyvault", "python/keyvault/azure-mgmt-keyvault_0b4d646e03", "0b4d646e034a6b74feb8bc564cadf3f38a27738a"),
    ("resources/azure-mgmt-resource", "python/resources/azure-mgmt-resource_89dd6d2402", "89dd6d240264da5a3201017239b0ebac9669b318"),
    ("servicebus/azure-mgmt-servicebus", "python/servicebus/azure-mgmt-servicebus_1027d31b47", "1027d31b4748079f618e0132aa5ba67f91e4c283"),
    ("servicebus/azure-servicebus", "python/servicebus/azure-servicebus_54b26ba299", "54b26ba2999f38a810138eab1ea0b5990d1799b0"),
    ("storage/azure-mgmt-storage", "python/storage/azure-mgmt-storage_745b7ea500", "745b7ea5005360e10d6f1e2984f42a6f90a444c6"),
    ("storage/azure-storage-blob", "python/storage/azure-storage-blob_e151c28f39", "e151c28f39b921502cd04b2216711560dbefd3f8"),
    ("storage/azure-storage-file-datalake", "python/storage/azure-storage-file-datalake_77d87fd024", "77d87fd02444bc15b17ebdfcbe0dbc09064b82c3"),
    ("storage/azure-storage-queue", "python/storage/azure-storage-queue_5064253f11", "5064253f11426fa10e48b743602120fa5990eb4a"),
    ("tables/azure-data-tables", "python/tables/azure-data-tables_97970da74f", "97970da74f147e7b92839ce15557a7e682fcaf7b"),
]


def download(url, path, tries=4):
    for attempt in range(1, tries + 1):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "azemu-fetch-refs"})
            with urllib.request.urlopen(req, timeout=120) as r, open(path, "wb") as f:
                shutil.copyfileobj(r, f, 1 << 20)
            return
        except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
            if attempt == tries:
                raise SystemExit(f"download failed: {url}: {e}")
            wait = 2 ** attempt
            print(f"  retry in {wait} s ({e})", flush=True)
            time.sleep(wait)


def extract(tgz, wanted_prefix, dest):
    """Unpack the members under `<root>/python/sdk/<package>/` into dest/python/sdk/<package>/. Refuses any path that would
    leave dest. Returns the number of files."""
    n = 0
    with tarfile.open(tgz, "r:gz") as tf:
        for m in tf:
            parts = m.name.split("/", 1)
            if len(parts) < 2:
                continue
            rel = parts[1]
            if not rel.startswith(wanted_prefix + "/"):
                continue
            if not m.isfile():
                continue
            target = os.path.normpath(os.path.join(dest, rel))
            if not target.startswith(os.path.normpath(dest) + os.sep):
                raise SystemExit(f"refusing a path outside the destination: {m.name}")
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with tf.extractfile(m) as src, open(target, "wb") as out:
                shutil.copyfileobj(src, out)
            n += 1
    return n


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--dest", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "refs"),
                    help="directory that holds recordings/ (default: refs/ of this repository)")
    ap.add_argument("--only", action="append", default=[], help="only packages whose directory contains this text (repeatable)")
    ap.add_argument("--force", action="store_true", help="fetch again even when the pinned commit is already there")
    ap.add_argument("--list", action="store_true", help="print the pins and stop")
    a = ap.parse_args()
    pins = [p for p in PINS if not a.only or any(o in p[0] for o in a.only)]
    if a.list:
        for d, tag, sha in pins:
            print(f"{d:45s} {tag}  {sha}")
        return 0
    if not pins:
        print("no package matches --only", file=sys.stderr)
        return 1
    dest = os.path.abspath(a.dest)
    rec = os.path.join(dest, "recordings")
    tmp = os.path.join(dest, ".fetch-tmp")
    stamps = os.path.join(rec, ".pinned")
    os.makedirs(stamps, exist_ok=True)
    os.makedirs(tmp, exist_ok=True)
    total = 0
    try:
        for i, (d, tag, sha) in enumerate(pins, 1):
            stamp = os.path.join(stamps, d.replace("/", "__"))
            have = open(stamp).read().strip() if os.path.exists(stamp) else ""
            pkg_dir = os.path.join(rec, "python", "sdk", d)
            if have == sha and os.path.isdir(pkg_dir) and not a.force:
                print(f"[{i}/{len(pins)}] {d}: already at {sha[:10]}")
                continue
            print(f"[{i}/{len(pins)}] {d}: {tag}", flush=True)
            tgz = os.path.join(tmp, "a.tar.gz")
            download(f"https://codeload.github.com/{ASSETS_REPO}/tar.gz/{sha}", tgz)
            if os.path.isdir(pkg_dir):
                shutil.rmtree(pkg_dir)
            n = extract(tgz, f"python/sdk/{d}", rec)
            if n == 0:
                raise SystemExit(f"{d}: the tarball of {sha} holds no python/sdk/{d} files")
            with open(stamp, "w") as f:
                f.write(sha + "\n")
            total += n
            print(f"    {n} files", flush=True)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    manifest = {"source": f"https://github.com/{ASSETS_REPO}", "packages": {d: {"tag": t, "commit": s} for d, t, s in PINS}}
    with open(os.path.join(rec, "PINNED.json"), "w") as f:
        json.dump(manifest, f, indent=1)
    print(f"done: {total} files fetched into {rec}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
