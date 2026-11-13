#!/usr/bin/env python3
"""Fails when a fidelity score is lower than the committed one.

    check_scores.py <new SCORES.md> [baseline SCORES.md]      (baseline default: fidelity/SCORES.md)

Both files are the table that `tools/replay/run_all.py` writes. For every package in the baseline the new file must have a
row, and its "entries raw %" and "entries adjusted %" must not be lower than the baseline's by more than 0.05 points (the
numbers are of a replay of fixed recordings, so a drop is a change in the emulator, not noise; the 0.05 is for a recording
that depends on the clock). Prints one line per package. Exit 0 when all hold, 1 otherwise."""
import os, re, sys

TOLERANCE = 0.05


def table(path):
    rows = {}
    head = None
    for line in open(path):
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if cells and cells[0] == "package":
            head = cells
            continue
        if head and len(cells) == len(head) and not set(cells[0]) <= {"-"}:
            rows[cells[0]] = dict(zip(head, cells))
    return rows


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    new_path = sys.argv[1]
    base_path = sys.argv[2] if len(sys.argv) > 2 else os.path.join(os.path.dirname(__file__), "..", "..", "fidelity", "SCORES.md")
    base, new = table(base_path), table(new_path)
    if not base:
        print(f"no table in the baseline {base_path}")
        return 1
    failed = False
    for pkg, b in base.items():
        n = new.get(pkg)
        if n is None:
            print(f"FAIL {pkg}: no row in {new_path}")
            failed = True
            continue
        parts = []
        bad = False
        for col in ("entries raw %", "entries adjusted %"):
            try:
                bv, nv = float(b[col]), float(n[col])
            except (KeyError, ValueError):
                parts.append(f"{col}: unreadable")
                bad = True
                continue
            if nv + TOLERANCE < bv:
                bad = True
            parts.append(f"{col} {nv:.2f} (baseline {bv:.2f})")
        print(("FAIL " if bad else "ok   ") + pkg + ": " + "; ".join(parts))
        failed |= bad
    return 1 if failed else 0


sys.exit(main())
