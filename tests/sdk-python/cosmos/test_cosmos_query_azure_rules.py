"""The query language as a real account answers it.

`cosmos_query_cases.json` holds queries, each with the rows or the error a real Cosmos DB account (API for NoSQL, serverless)
gave in November 2026 through the Python SDK: ORDER BY over undefined values and its limits, SUM of no values, the ordering of
groups and DISTINCT values, scalar subqueries that can return several rows, division by zero, reserved words, comments,
TOP/OFFSET/LIMIT, error codes and texts, a few function results. The same cases run against the emulator, so the two agree.

Rows are compared in any order unless the query has ORDER BY or the case says it is ordered; an error is compared by its
status, sub-status, code and leading sentence (and position, when the compiler gives one).
"""

import json
import os

import pytest
from azure.cosmos import PartitionKey
from azure.cosmos.exceptions import CosmosHttpResponseError

from cosmosutil import uid

with open(os.path.join(os.path.dirname(__file__), "cosmos_query_cases.json")) as f:
    DATA = json.load(f)

KINDS = ["c", "mixed", "explicit", "composite", "noindex"]


def _policy(kind):
    return {"explicit": DATA["explicit_policy"], "composite": DATA["composite_policy"], "noindex": DATA["noindex_policy"]}.get(kind)


def _load(make_container, kind):
    kw = {"indexing_policy": _policy(kind)} if _policy(kind) else {}
    c = make_container("/pk", **kw)
    for d in DATA["mixed"] if kind == "mixed" else DATA["docs"]:
        c.create_item(d)
    return c


def _run(c, case):
    kw = {}
    if "partition_key" in case:
        kw["partition_key"] = case["partition_key"]
    else:
        kw["enable_cross_partition_query"] = True
    return list(c.query_items(case["sql"], parameters=case.get("parameters"), **kw))


def _check(c, case):
    """None when the answer is the recorded one, else a description."""
    want_error = case.get("error")
    try:
        rows = _run(c, case)
    except CosmosHttpResponseError as e:
        if not want_error:
            return f"error {e.status_code}: {e.message[:200]}"
        w = want_error
        problems = []
        if e.status_code != w["status"]:
            problems.append(f"status {e.status_code} != {w['status']}")
        if (e.sub_status or None) != (w.get("sub") or None):
            problems.append(f"sub-status {e.sub_status} != {w.get('sub')}")
        if w.get("code") is not None and str(w["code"]) not in e.message:
            problems.append(f"code {w['code']} not in {e.message[:160]!r}")
        text = w["text"].replace("\\", "")
        if text not in e.message.replace("\\", ""):
            problems.append(f"text {text!r} not in {e.message[:300]!r}")
        if w.get("pos") and f'"start":{w["pos"][0]},"end":{w["pos"][1]}' not in e.message:
            problems.append(f"position {w['pos']} not in {e.message[:160]!r}")
        return "; ".join(problems) or None
    if want_error:
        return f"rows {rows!r:.200} instead of the error {want_error['text']!r}"
    want = case["rows"]
    if not case.get("ordered") and "ORDER BY" not in case["sql"].upper():
        key = lambda v: json.dumps(v, sort_keys=True)
        rows, want = sorted(rows, key=key), sorted(want, key=key)
    return None if rows == want else f"rows {rows!r:.300} != {want!r:.300}"


@pytest.mark.parametrize("kind", KINDS)
def test_queries_answer_as_a_real_account_does(make_container, kind):
    cases = [c for c in DATA["cases"] if c["container"] == kind]
    c = _load(make_container, kind)
    failures = []
    for case in cases:
        problem = _check(c, case)
        if problem:
            failures.append(f"{case['name']}: {case['sql'][:120]}\n    {problem}")
    assert not failures, f"{len(failures)} of {len(cases)} cases differ:\n" + "\n".join(failures)
