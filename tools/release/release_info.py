#!/usr/bin/env python3
"""What a release needs to know before it builds: the version, and the notes for the release page.

    python3 tools/release/release_info.py <tag or version> [--notes-out FILE] [--root DIR]

`<tag or version>` is a tag such as `v0.1.0` or a bare version `0.1.0`; `-` means "the version in Cargo.toml" (a trial run that
is not for a tag). The script
  * checks that it equals the version in the root Cargo.toml (`[workspace.package] version`), so a tag that does not match the
    code cannot be released;
  * writes the section of CHANGELOG.md for that version (the text under its `## 0.1.0` heading, up to the next `## `) to FILE;
    a missing or empty section is an error;
  * prints `version=<version>` on standard output (the form GitHub Actions reads from $GITHUB_OUTPUT).
Standard library only. Exit 0: fine; 1: the tag does not match or the changelog has no section."""
import argparse
import os
import re
import sys


def cargo_version(root):
    text = open(os.path.join(root, "Cargo.toml"), encoding="utf-8").read()
    m = re.search(r"^\[workspace\.package\](.*?)(?=^\[|\Z)", text, re.S | re.M)
    v = re.search(r'^version\s*=\s*"([^"]+)"', m.group(1) if m else "", re.M)
    if not v:
        sys.exit("no [workspace.package] version in Cargo.toml")
    return v.group(1)


def changelog_section(root, version):
    lines = open(os.path.join(root, "CHANGELOG.md"), encoding="utf-8").read().splitlines()
    out, inside = [], False
    head = re.compile(r"^##\s+\[?" + re.escape(version) + r"\]?(\s|$|\()")
    for line in lines:
        if line.startswith("## "):
            if inside:
                break
            inside = bool(head.match(line))
            continue
        if inside:
            out.append(line)
    text = "\n".join(out).strip()
    return text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ref")
    ap.add_argument("--notes-out")
    ap.add_argument("--root", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
    a = ap.parse_args()
    version = cargo_version(a.root) if a.ref == "-" else re.sub(r"^(refs/tags/)?v?", "", a.ref)
    if not re.fullmatch(r"\d+\.\d+\.\d+([-+][0-9A-Za-z.-]+)?", version):
        print(f"'{a.ref}' is not a version tag like v1.2.3", file=sys.stderr)
        return 1
    have = cargo_version(a.root)
    if have != version:
        print(f"the tag says {version} but the root Cargo.toml says {have}: change one of them", file=sys.stderr)
        return 1
    notes = changelog_section(a.root, version)
    if not notes:
        print(f"CHANGELOG.md has no section '## {version}' with text under it", file=sys.stderr)
        return 1
    if a.notes_out:
        with open(a.notes_out, "w", encoding="utf-8") as f:
            f.write(notes + "\n")
    print(f"version={version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
