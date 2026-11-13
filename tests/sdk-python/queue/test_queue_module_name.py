"""Guard: this directory is named "queue", the same as a standard library module.

If pytest ever puts this directory's parent on sys.path ahead of the stdlib, or an
__init__.py appears here, the stdlib queue module would be shadowed. This test
fails in that case.
"""

import queue as stdlib_queue


def test_stdlib_queue_module_is_not_shadowed_by_this_directory():
    assert hasattr(stdlib_queue, "Queue")
    assert stdlib_queue.Queue is not None
