"""Paging: every page of a query or of a document feed arrives exactly once, whatever happens
to the data in between, and a token keeps working when the service forgot the result.

(The service keeps the result of a query behind the token for a short time instead of running the
query again for every page; the Rust tests count executions. These tests check what the SDK sees.)
"""

import json
import time

import pytest

from cosmosutil import idlist


def fill(container, n, parts=3):
    for i in range(n):
        container.create_item({"id": f"{i:05d}", "pk": f"p{i % parts}", "n": i})


def test_order_by_pages_cover_the_result_once_in_order(container):
    fill(container, 300)
    pager = container.query_items("SELECT * FROM c ORDER BY c.n", enable_cross_partition_query=True, max_item_count=10).by_page()
    pages = [list(p) for p in pager]
    assert [len(p) for p in pages] == [10] * 30
    assert [d["n"] for p in pages for d in p] == list(range(300))


def test_plain_cross_partition_query_pages_have_no_duplicates(container):
    fill(container, 100)
    pager = container.query_items("SELECT * FROM c WHERE c.n >= 10", enable_cross_partition_query=True, max_item_count=7).by_page()
    got = [d["n"] for p in pager for d in p]
    assert sorted(got) == list(range(10, 100))
    assert len(got) == len(set(got))


def test_resume_from_a_token_with_another_client(make_client, db_id, container):
    fill(container, 60)
    q = "SELECT * FROM c ORDER BY c.n"
    pager = container.query_items(q, partition_key="p1", max_item_count=5).by_page()
    first = list(next(pager))
    token = pager.continuation_token
    assert token
    other = make_client().get_database_client(db_id).get_container_client(container.id)
    rest = [d for page in other.query_items(q, partition_key="p1", max_item_count=5).by_page(token) for d in page]
    expected = [i for i in range(60) if i % 3 == 1]
    assert [d["n"] for d in first] + [d["n"] for d in rest] == expected


def test_a_page_after_a_change_in_between_is_still_a_full_page_without_repeats(container):
    fill(container, 40)
    q = "SELECT * FROM c WHERE c.pk = 'p0' ORDER BY c.n"
    pager = container.query_items(q, partition_key="p0", max_item_count=5).by_page()
    seen = [d["n"] for d in next(pager)]
    container.delete_item("00000", "p0")  # already delivered
    container.create_item({"id": "00001a", "pk": "p0", "n": -1})  # sorts before the position
    for page in pager:
        seen += [d["n"] for d in page]
    assert len(seen) == len(set(seen)), "a row was delivered twice"
    assert seen[:5] == [0, 3, 6, 9, 12]
    assert set(range(15, 40, 3)) <= set(seen)


# The continuation token is the emulator's own JSON; Azure's token has another form and a token it does not know is a 400.
@pytest.mark.emulator_only
def test_a_token_the_service_does_not_know_still_resumes(endpoint, ca_file, db, container):
    # the opaque token is a JSON object; a token with an unknown kept-result id (expired, evicted
    # or from before a restart) is only a row count
    from cosmosutil import RawCosmos

    fill(container, 30)
    raw = RawCosmos(endpoint, ca_file)
    path = f"dbs/{db.id}/colls/{container.id}/docs"
    h = {
        "x-ms-documentdb-isquery": "True",
        "content-type": "application/query+json",
        "x-ms-documentdb-query-enablecrosspartition": "True",
        "x-ms-documentdb-partitionkeyrangeid": "0",
        "x-ms-max-item-count": "10",
    }
    body = {"query": "SELECT * FROM c ORDER BY c.n"}
    r = raw.post(path, body=body, headers=h)
    assert r.status_code == 200, r.text
    token = r.headers["x-ms-continuation"]
    forged = json.dumps({**json.loads(token), "token": json.loads(token)["token"].replace("#Q:", "#Q:9")})
    r2 = raw.post(path, body=body, headers={**h, "x-ms-continuation": forged})
    assert [d["n"] for d in r2.json()["Documents"]] == list(range(10, 20))
    r3 = raw.post(path, body=body, headers={**h, "x-ms-continuation": token})
    assert [d["n"] for d in r3.json()["Documents"]] == list(range(10, 20))
    # a token that is not ours at all is refused (see the next test)
    r4 = raw.post(path, body=body, headers={**h, "x-ms-continuation": "garbage"})
    assert r4.status_code == 400


MALFORMED = "The continuation token specified in the request is malformed. Re-run the query without it and iterate."


@pytest.mark.parametrize("token", ["garbage", "{}", '{"token":"x"}', '{"token":"-RID:~abc"}'])
def test_a_continuation_token_that_is_no_token_is_a_400(endpoint, ca_file, db, container, token):
    # Azure (checked in November 2026): text, `{}` and the token of another resource are a 400 on a read feed, on a query of
    # one partition and on the list of containers or databases; an empty token is "from the start"
    from cosmosutil import RawCosmos

    fill(container, 5)
    raw = RawCosmos(endpoint, ca_file)
    docs = f"dbs/{db.id}/colls/{container.id}/docs"
    pk = {"x-ms-documentdb-partitionkey": json.dumps(["p0"])}
    r = raw.get(docs, headers={**pk, "x-ms-continuation": token, "x-ms-max-item-count": "2"})
    assert r.status_code == 400 and MALFORMED in r.text, r.text
    q = {"x-ms-documentdb-isquery": "True", "content-type": "application/query+json", **pk}
    r = raw.post(docs, body={"query": "SELECT * FROM c"}, headers={**q, "x-ms-continuation": token})
    assert r.status_code == 400 and MALFORMED in r.text, r.text
    r = raw.get(f"dbs/{db.id}/colls", headers={"x-ms-continuation": token})
    assert r.status_code == 400 and MALFORMED in r.text, r.text
    r = raw.get("dbs", headers={"x-ms-continuation": token})
    assert r.status_code == 400 and MALFORMED in r.text, r.text
    for h in ({**pk, "x-ms-continuation": ""}, {**pk}):
        assert raw.get(docs, headers={**h, "x-ms-max-item-count": "2"}).status_code == 200


def test_a_token_the_service_made_goes_back_to_it_unchanged(endpoint, ca_file, db, container):
    from cosmosutil import RawCosmos

    fill(container, 6)
    raw = RawCosmos(endpoint, ca_file)
    docs = f"dbs/{db.id}/colls/{container.id}/docs"
    pk = {"x-ms-documentdb-partitionkey": json.dumps(["p0"])}
    seen, token = [], None
    for _ in range(10):
        h = {**pk, "x-ms-max-item-count": "1"}
        if token:
            h["x-ms-continuation"] = token
        r = raw.get(docs, headers=h)
        assert r.status_code == 200, r.text
        seen += [d["id"] for d in r.json()["Documents"]]
        token = r.headers.get("x-ms-continuation")
        if not token:
            break
    assert len(seen) == 2 and len(set(seen)) == 2


@pytest.mark.parametrize("value", ["abc", "-2", "1.5", "2147483648"])
def test_a_page_size_that_is_no_page_size_is_a_400(endpoint, ca_file, db, container, value):
    from cosmosutil import RawCosmos

    raw = RawCosmos(endpoint, ca_file)
    docs = f"dbs/{db.id}/colls/{container.id}/docs"
    msg = f"The input PageSize {value} is invalid. Ensure to pass a valid page size which must be a positive integer or -1 for a dynamic page size."
    pk = {"x-ms-documentdb-partitionkey": json.dumps(["p"])}
    for method, path, h in (("GET", docs, pk), ("GET", f"dbs/{db.id}/colls/{container.id}", {}), ("GET", "dbs", {}),
                            ("GET", docs + "/x", pk)):
        r = raw.request(method, path, headers={**h, "x-ms-max-item-count": value})
        assert r.status_code == 400 and msg in r.text, (path, r.text)
    # the list of partition key ranges only refuses a number below -1; 0, -1 and an empty value mean the default
    r = raw.get(f"dbs/{db.id}/colls/{container.id}/pkranges", headers={"x-ms-max-item-count": value})
    assert r.status_code == (400 if value == "-2" else 200), r.text
    for ok in ("0", "-1", "", "2147483647"):
        assert raw.get(docs, headers={**pk, "x-ms-max-item-count": ok}).status_code == 200, ok


def test_read_all_items_pages_cover_every_document_once(container):
    fill(container, 50)
    pager = container.read_all_items(max_item_count=7).by_page()
    pages = [list(p) for p in pager]
    assert [len(p) for p in pages][:-1] == [7] * 7
    ids = [d["id"] for p in pages for d in p]
    assert sorted(ids) == [f"{i:05d}" for i in range(50)]
    assert len(set(ids)) == 50


def test_read_all_items_survives_deletes_and_inserts_between_pages(container):
    fill(container, 30)
    pager = container.read_all_items(max_item_count=7).by_page()
    seen = [d["id"] for d in next(pager)]
    first = seen[0]
    container.delete_item(first, f"p{int(first) % 3}")
    container.create_item({"id": "zzz", "pk": "p2", "n": 999})  # sorts behind everything
    for page in pager:
        seen += [d["id"] for d in page]
    assert len(seen) == len(set(seen)), "a document was delivered twice"
    for i in range(1, 30):
        assert f"{i:05d}" in seen
    assert "zzz" in seen


def test_read_all_items_of_one_partition(container):
    fill(container, 30)
    pages = list(container.read_all_items(max_item_count=4).by_page())
    assert sum(len(list(p)) for p in pages) == 30
    got = []
    pager = container.query_items("SELECT * FROM c", partition_key="p1", max_item_count=4).by_page()
    for p in pager:
        got += [d["n"] for d in p]
    assert sorted(got) == [i for i in range(30) if i % 3 == 1]


def test_a_page_ends_after_the_document_that_takes_it_to_four_megabytes(container):
    # Azure (checked in November 2026): five documents of 1.5 MB came in pages of 3 and 2 (not 2, 2 and 1) the first time and as
    # 2, 2 and 1 the second time, so Azure does not promise more than this: a page is cut once it holds about 4 MB, and a page
    # of 1.39 MB documents had 4 (the page ends after the document that crosses the line). The emulator always answers 3 and 2
    # (`crates/services/cosmos/tests/paging.rs` checks it exactly).
    big = "x" * 1_500_000
    for i in range(5):
        container.create_item({"id": str(i), "pk": "p", "b": big})
    for read in (lambda: container.read_all_items(max_item_count=100),
                 lambda: container.query_items("SELECT * FROM c", partition_key="p"),
                 lambda: container.query_items("SELECT * FROM c", enable_cross_partition_query=True)):
        sizes = [len(list(p)) for p in read().by_page()]
        assert sum(sizes) == 5 and 2 <= sizes[0] <= 3 and max(sizes) <= 3, sizes


def test_many_pages_of_a_big_result_are_quick(container):
    # 2000 documents in pages of 5 would run the query 400 times if each page ran it again
    for i in range(2000):
        container.create_item({"id": f"{i:05d}", "pk": "p", "n": i, "pad": "x" * 100})
    t = time.time()
    got = 0
    for page in container.query_items("SELECT * FROM c ORDER BY c.n", partition_key="p", max_item_count=5).by_page():
        got += len(list(page))
    assert got == 2000
    assert time.time() - t < 30
