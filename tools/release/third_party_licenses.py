#!/usr/bin/env python3
"""Writes THIRD-PARTY-LICENSES.md: every external crate that is linked into the `azemu` binary (cargo's "normal"
dependencies of `-p azemu` for this machine's target), its version, its licence expression, and the licence texts that
the crates ship (each distinct text once, with the crates that carry it).

    python3 tools/release/third_party_licenses.py > THIRD-PARTY-LICENSES.md

Needs cargo and the crates in ~/.cargo/registry (a build, or `cargo fetch`, puts them there). Reads only; no network
when the registry is already filled (`--offline`)."""
import glob, hashlib, os, re, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FORMAT = "{p}|{l}|{r}"
# (crate-name prefixes, text files in tools/release/licence-texts/, where the text came from)
SUPPLEMENTS = [
    (["boa_", "small_btree", "tag_ptr"], ["boa-LICENSE-MIT.txt", "boa-LICENSE-UNLICENSE.txt"],
     "https://github.com/boa-dev/boa, tag v0.22 (`LICENSE-MIT`, `LICENSE-UNLICENSE`); the crates are `Unlicense OR MIT`"),
    (["asn1-rs-impl"], ["asn1-rs-LICENSE-MIT.txt"],
     "https://github.com/rusticata/asn1-rs (`LICENSE-MIT`; its `LICENSE-APACHE` is the standard Apache-2.0 text, see `LICENSE-APACHE` here)"),
]


def main():
    out = subprocess.run(
        ["cargo", "tree", "-p", "azemu", "-e", "normal", "--prefix", "none", "--format", FORMAT, "--locked", "--offline"],
        cwd=ROOT, capture_output=True, text=True, check=True).stdout
    crates = {}
    for line in out.splitlines():
        line = line.replace(" (*)", "").strip()
        if not line:
            continue
        pkg, lic, repo = (line.split("|") + ["", ""])[:3]
        m = re.match(r"(\S+) v(\S+)( \((.*)\))?$", pkg)
        if not m:
            continue
        name, version, _, source = m.groups()
        if source and source.startswith("/"):
            continue  # a crate of this workspace
        crates[(name, version)] = (lic or "(none declared)", repo)
    registry = glob.glob(os.path.expanduser("~/.cargo/registry/src/*"))
    texts = {}  # hash -> (text, [crate ids])
    for (name, version) in sorted(crates):
        for reg in registry:
            d = os.path.join(reg, f"{name}-{version}")
            if not os.path.isdir(d):
                continue
            for f in sorted(os.listdir(d)):
                if re.match(r"(LICEN[CS]E|COPYING|NOTICE|UNLICENSE)", f, re.I) and os.path.isfile(os.path.join(d, f)):
                    t = open(os.path.join(d, f), errors="replace").read().strip()
                    # a crate's own copyright line is part of the text, so two crates share a text only when it is identical
                    h = hashlib.sha256(t.encode()).hexdigest()
                    texts.setdefault(h, (t, []))[1].append(f"{name} {version}")
            break
    p = sys.stdout.write
    p("# Third-party licences\n\n")
    p("What is linked into the `azemu` binary (Linux x86-64; the `windows-*` and Apple crates that other targets add are\n")
    p("not in this list). Made by `tools/release/third_party_licenses.py` from `Cargo.lock`; "
      "`cargo deny check licenses` (see `deny.toml`) is the check that nothing outside the allowed list gets in.\n\n")
    p(f"{len(crates)} crates. Test data and recordings under `refs/` and the dependencies of the SDK test suites are not shipped and are not listed.\n\n")
    p("## Crates\n\n| Crate | Version | Licence (SPDX) |\n|---|---|---|\n")
    for (name, version), (lic, _) in sorted(crates.items()):
        p(f"| {name} | {version} | {lic} |\n")
    p("\n## Licence texts shipped by the crates\n\n")
    p("Each text once. Crates whose repository keeps the licence outside the published package carry no file and are "
      "covered by the SPDX expression above (MIT, Apache-2.0 and the other standard texts).\n\n")
    for h, (t, ids) in sorted(texts.items(), key=lambda kv: -len(kv[1][1])):
        shown = ", ".join(ids)
        p(f"### {shown}\n\n```\n{t}\n```\n\n")
    # Crates that publish no licence file in their package: the text comes from the crate's own repository
    # (tools/release/licence-texts/, fetched once, with the repository and tag it came from named here).
    shipped = {i for _, ids in texts.values() for i in ids}
    lacking = sorted(f"{n} {v}" for (n, v) in crates if f"{n} {v}" not in shipped)
    p("## Crates that ship no licence file\n\n")
    p("These crates declare their licence in `Cargo.toml` but their package holds no licence text. The texts below are "
      "from the crates' own repositories (listed with each), so that the copyright notice that the MIT licence asks to "
      "keep is kept. A crate that is not listed here has its licence file in its package, above.\n\n")
    for names, files, source in SUPPLEMENTS:
        have = [n for n in lacking if any(n.startswith(x) for x in names)]
        if not have:
            continue
        p(f"### {', '.join(have)}\n\nSource: {source}\n\n")
        for f in files:
            body = open(os.path.join(ROOT, "tools", "release", "licence-texts", f), errors="replace").read().strip()
            p(f"```\n{body}\n```\n\n")
        lacking = [n for n in lacking if n not in have]
    for n in lacking:
        if n.startswith("p12-keystore "):
            p(f"### {n}\n\nDeclares `MIT/Apache-2.0` in its `Cargo.toml` and README (author: Dmitry Pankratov, "
              "https://github.com/ancwrd1/p12-keystore). Neither its package nor its repository holds a licence text or a "
              "copyright line, so there is none to reproduce; the standard texts are `LICENSE-MIT` and `LICENSE-APACHE` of "
              "this repository.\n\n")
        else:
            sys.stderr.write(f"WARNING: no licence text for {n}\n")


main()
