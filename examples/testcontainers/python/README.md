# Testcontainers, Python

pytest tests that start `ghcr.io/tirthpatell/azemu:0.3.0` with `testcontainers` and reach it on the published ports: Blob over plain http with the Azurite connection string
(`test_blob.py`), Key Vault over https with a token credential (`test_key_vault.py`), Service Bus send and receive (`test_service_bus.py`). `conftest.py` starts one container for the
whole run and empties it after each test. The page that explains them is [`docs/testcontainers.md`](../../../docs/testcontainers.md).

```bash
python -m venv .venv && .venv/bin/pip install -r requirements.txt
.venv/bin/python -m pytest      # Docker must be running; the first run pulls the image
```

Run with: Python 3.13, Docker 29.10, the versions in `requirements.txt`, azemu 0.3.0 on Linux x86-64.
