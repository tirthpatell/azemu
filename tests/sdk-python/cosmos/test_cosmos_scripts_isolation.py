"""Scripts run in a worker process, so no script can end the emulator, hold a script slot, or use unbounded memory.

These are the hostile-script cases (parser overflow, a call that never returns, work in callbacks, memory) as SDK calls: each script is bounded in size, and each test ends by running an
ordinary script and an ordinary query, which must still work. The emulator is used with its default limits (5 s for a script, 4 at a time).
"""

import time

import pytest
from azure.cosmos.exceptions import CosmosHttpResponseError

pytestmark = pytest.mark.not_on_vnext(reason="vnext has no server-side JavaScript")

HELLO = "function () { getContext().getResponse().setBody('Hello, World'); }"
FIB = "function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }"


def sproc(container, id, body):
    return container.scripts.create_stored_procedure({"id": id, "serverScript": body})


def run(container, id):
    return container.scripts.execute_stored_procedure(id, partition_key="p")


def still_works(container):
    sproc(container, "after", HELLO)
    assert run(container, "after") == "Hello, World"
    container.upsert_item({"id": "alive", "pk": "p"})
    assert container.read_item("alive", partition_key="p")["id"] == "alive"


def test_nesting_hidden_in_a_template_literal_is_a_400_and_the_service_goes_on(container):
    body = "function () { var x = `${" + "(" * 12000 + "1" + ")" * 12000 + "}`; }"
    sproc(container, "deep", body)
    with pytest.raises(CosmosHttpResponseError) as e:
        run(container, "deep")
    assert e.value.status_code == 400
    still_works(container)


def test_a_catastrophic_regular_expression_is_a_408_and_frees_its_slot(container):
    sproc(container, "regex", "function () { /^(a+)+$/.test('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!'); }")
    started = time.time()
    with pytest.raises(CosmosHttpResponseError) as e:
        run(container, "regex")
    assert e.value.status_code == 408
    assert time.time() - started < 8
    # the slot is free again at once: more than four such runs in a row would have used them all up
    started = time.time()
    still_works(container)
    assert time.time() - started < 3


@pytest.mark.parametrize(
    "name,body",
    [
        ("promise", "Promise.resolve().then(function () { fib(70); });"),
        ("sort", "[2, 1].sort(function (a, b) { fib(70); return a - b; });"),
        ("replace", "'a'.replace(/a/, function () { fib(70); return 'b'; });"),
        ("tojson", "JSON.stringify({ toJSON: function () { fib(70); return 1; } });"),
    ],
)
def test_work_in_callbacks_outside_the_engine_clock_is_a_408(container, name, body):
    sproc(container, name, "function () { " + FIB + " " + body + " }")
    with pytest.raises(CosmosHttpResponseError) as e:
        run(container, name)
    assert e.value.status_code == 408
    still_works(container)


def test_memory_used_in_a_promise_job_is_limited(container):
    double = "var s = 'xxxxxxxx'; for (var i = 0; i < 27; i++) { s = s + s; } getContext().getResponse().setBody(s.length);"
    sproc(container, "job", "function () { Promise.resolve().then(function () { " + double + " }); }")
    sproc(container, "main", "function () { " + double + " }")
    for name in ("main", "job"):
        with pytest.raises(CosmosHttpResponseError) as e:
            run(container, name)
        assert e.value.status_code == 400
        assert "memory" in e.value.message
    still_works(container)
