# Terraform tests for azemu

Runs the `azurerm` Terraform provider against the emulator's Resource Manager, one case at a time (`cases/`): `base` (resource group, storage), `servicebus`,
`eventhubs`, `keyvault`, `appconfig`, `cosmos`, `eventgrid` and `template` (a template deployment). For each case `run.py` starts the emulator on a free port,
runs `terraform init`, `apply`, a second `plan -detailed-exitcode` (it must report no changes), checks with the data-plane SDKs that the resources exist and work, runs
`terraform destroy`, and checks that nothing is left. `provider.tf` shows the provider block to copy into your own configuration (see also the README).

## What you need

* Terraform 1.x on the `PATH`, and network access for `terraform init` (the provider download; pinned by `.terraform.lock.hcl`). The provider is cached in
  `$TF_PLUGIN_CACHE_DIR` (or `--cache`).
* Python with the packages in `tests/sdk-python/requirements.txt` (the checks use the Python SDKs).
* A built emulator binary (`--bin`, default `target/debug/azemu`).

## Run

```bash
.venv/bin/python tests/terraform/run.py                  # every case
.venv/bin/python tests/terraform/run.py base eventgrid   # some cases
.venv/bin/python tests/terraform/run.py --bin target/release/azemu --list
.venv/bin/python tests/terraform/run.py --keep base      # keep the work directory (state, logs) and print it
```

It prints one line per step and exits 0 when every step of every case passed. It is slow because the provider polls (about two minutes for a Key Vault); that is the
provider, not the emulator. Run one runner at a time: two `terraform init` on one plug-in cache collide. Terraform needs one file with the system certificates plus the
emulator's CA: the runner builds it and gives it to Terraform as `SSL_CERT_FILE` for the commands that talk to the emulator.
