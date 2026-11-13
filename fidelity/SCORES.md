# Fidelity scores

- Date: 2026-11-13
- Emulator binary: `azemu-release`
- Services mounted (from `/_azemu/health`): blob, dfs, queue, table, keyvault, appconfig, cosmos, eventgrid, identity, servicebus, eventhubs, kafka, arm
- Produced by `tools/replay/run_all.py`; reports are in `fidelity/reports/`. Rules for every class: `tools/replay/README.md`.

Raw = every entry that is not a pass counts against. Adjusted = unreplayable entries removed from the denominator and ahead-async counted as passing. A recording passes (adjusted) when no hard failure is left after classification.

**Values that differ** is a third number next to the status and error code score: for every answer whose status matched, the values of the headers, of the body and the first line of error texts that exist on both sides, except those a rule excuses (ids, times, ETags, sanitized values...; the rules and their reasons are in `tools/replay/README.md`). It does not change the other columns. Most of what it finds is a property of Microsoft's test account in the recording (logging settings, access tier), not a mistake of the emulator.

| package | recordings passing (adjusted / total) | recordings passing (raw) | entries raw % | entries adjusted % | unreplayable | ahead-async | hard failures left | values that differ / compared |
|---|---|---|---|---|---|---|---|---|
| blob | 1594/1685 | 1501/1685 | 95.77 | 97.06 | 153 | 0 | 336 | 86/19491 (0.44%) |
| queue | 196/200 | 192/200 | 98.89 | 99.44 | 4 | 0 | 4 | 3/954 (0.31%) |
| table | 423/428 | 413/428 | 98.34 | 98.71 | 10 | 0 | 34 | 20/13188 (0.15%) |
| keyvault-secrets | 312/348 | 167/348 | 84.16 | 98.94 | 0 | 944 | 68 | 0/13607 (0.00%) |
| keyvault-keys | 782/810 | 486/810 | 87.64 | 99.43 | 336 | 788 | 52 | 2/48487 (0.00%) |
| keyvault-certificates | 457/510 | 324/510 | 86.90 | 96.61 | 0 | 685 | 239 | 33/65760 (0.05%) |
| appconfig | 272/304 | 269/304 | 93.30 | 99.38 | 431 | 0 | 41 | 4/37575 (0.01%) |
| eventgrid | 59/59 | 59/59 | 100.00 | 100.00 | 0 | 0 | 0 | 4/63 (6.35%) |
| servicebus | 115/120 | 115/120 | 97.93 | 97.93 | 0 | 0 | 19 | 38/10564 (0.36%) |
| datalake | 404/451 | 331/451 | 95.03 | 97.52 | 87 | 0 | 82 | 10/5790 (0.17%) |
| arm | 26/34 | 24/34 | 71.68 | 82.86 | 8 | 6 | 18 | 15/597 (2.51%) |

Classified entries (all packages above): ahead-async: 2423, unreplayable:auth-dependent: 108, unreplayable:azure-server-error: 8, unreplayable:generated-key: 336, unreplayable:leftover-state: 124, unreplayable:sanitized-copy-source: 22, unreplayable:throttled: 431

## Settings the emulator was started with, per package

- blob: `AZEMU_BLOB_VERSIONING=1` (the recordings come from storage accounts with blob versioning turned on (`x-ms-version-id` answers, versions in listings, deletes that keep a version); without it new accounts have versioning off, as on Azure)
- queue: none
- table: none
- keyvault-secrets: none
- keyvault-keys: none
- keyvault-certificates: none
- appconfig: none
- eventgrid: `AZEMU_EVENTGRID_AUTOCREATE=1` (the recorded publishes go to topics that were made outside the recordings (in the Azure portal); without auto-creation they answer 404)
- servicebus: none
- datalake: none
- arm: none

## Most frequent value differences, per package

Operation, status, then the header or the path of the value. `Azure` is the recorded value, `emulator` ours. The full lists are in `fidelity/reports/<package>.json` under `value_groups`.

**blob** (86 of 19491 values)

- 12 x `GET /{container}?comp=list&restype=container  200  header content-type`: Azure `application/vnd.apache.arrow.stream`, emulator `application/xml`
- 12 x `GET /{container}?comp=list&restype=container&include  200  header content-type`: Azure `application/vnd.apache.arrow.stream`, emulator `application/xml`
- 8 x `PUT /?comp=properties&restype=service  400  value /Error/LineNumber[]`: Azure `2`, emulator `1`
- 8 x `PUT /?comp=properties&restype=service  400  value /Error/LinePosition[]`: Azure `176`, emulator `1`
- 6 x `GET /{container}?comp=list&restype=container&maxresults  200  header content-type`: Azure `application/vnd.apache.arrow.stream`, emulator `application/xml`
- 6 x `HEAD /{container}/{blob}  200  header x-ms-access-tier`: Azure `Cool`, emulator `Hot`

**queue** (3 of 954 values)

- 2 x `GET /?comp=list&maxresults&prefix  200  value /EnumerationResults/Queues[]/Queue[]/Name[]`: Azure `listqueue0dcba29e4`, emulator `listqueue1cf1d2e5f`
- 1 x `GET /{queue}/messages?numofmessages  200  value /QueueMessagesList/QueueMessage[]/DequeueCount[]`: Azure `2`, emulator `1`

**table** (20 of 13188 values)

- 4 x `GET /?comp=properties&restype=service  200  value /StorageServiceProperties/Logging[]/Delete[]`: Azure `true`, emulator `false`
- 4 x `GET /?comp=properties&restype=service  200  value /StorageServiceProperties/Logging[]/Read[]`: Azure `true`, emulator `false`
- 4 x `GET /?comp=properties&restype=service  200  value /StorageServiceProperties/Logging[]/RetentionPolicy[]/Enabled[]`: Azure `true`, emulator `false`
- 2 x `GET /?comp=properties&restype=service  200  value /StorageServiceProperties/Logging[]/Write[]`: Azure `false`, emulator `true`
- 2 x `GET /Tables?$top  200  value /value[]/TableName`: Azure `listtable08f022b69`, emulator `listtable0b4a4292a`
- 2 x `GET /Tables?$top&NextTableName  200  value /value[]/TableName`: Azure `listtable18f022b69`, emulator `listtable2b4a4292a`

**keyvault-secrets**: none

**keyvault-keys** (2 of 48487 values)

- 1 x `POST /keys/{name}/create  200  value /key/x`: Azure `jQhc_CtdcMOyZPZKGSzmIxggYTcyxMmPQB-Nuzayn-w`, emulator `AIzaBW7jsnQq5n_Hy4n9Np4y9fQlVu4e1M_FYmcDV0w`
- 1 x `POST /keys/{name}/create  200  value /key/y`: Azure `HLDDFphpCphDvBxRoRWcClZYNxrLrANBZfTekTKjcto`, emulator `oieeYWGK3bMmtlnOmznMGMkXX33jrMuc8AWAtxvBd5o`

**keyvault-certificates** (33 of 65760 values)

- 11 x `DELETE /certificates/{name}  200  value /attributes/enabled`: Azure `False`, emulator `True`
- 11 x `GET /deletedcertificates/{name}  200  value /attributes/enabled`: Azure `False`, emulator `True`
- 2 x `GET /certificates/{name}/policy  200  value /issuer/name`: Azure `PlatformManaged`, emulator `Self`
- 2 x `GET /certificates/{name}/policy  200  value /lifetime_actions[]/trigger/lifetime_percentage`: Azure `50`, emulator `80`
- 2 x `GET /certificates/{name}/policy  200  value /x509_props/key_usage[]`: Azure `keyEncipherment`, emulator `digitalSignature`
- 2 x `GET /certificates/{name}/policy  200  value /x509_props/validity_months`: Azure `6`, emulator `12`

**appconfig** (4 of 37575 values)

- 1 x `DELETE /kv/{name}  200  value /content_type`: Azure `application/vnd.microsoft.appconfig.ff+json;charset=utf-8`, emulator `test content type`
- 1 x `DELETE /kv/{name}  200  value /key`: Azure `.appconfig.featureflag/key40c43ad9`, emulator `PYTHON_UNIT_test_key_a6af8952-54a6-11e9-b600-2816a84d0309`
- 1 x `DELETE /kv/{name}  200  value /value`: Azure `hello world`, emulator `test value`
- 1 x `GET /revisions?after&tags  200  value /etag`: Azure `vaRJCadaADxrVq_RYwHZN_CuL-MSABwSxQNlxCKeEFU`, emulator `S3YpyBWuG1yl7L9sQQ6h_s5l0YCDc_52qIJZM-YtbnE`

**eventgrid** (4 of 63 values)

- 2 x `POST /api/events  401  error text /error/details[]/message`: Azure `The request authorization key is not authorized for LLAWTOPIC.EASTUS-…`, emulator `The request authorization key is not authorized for SANITIZED.EVENTGR…`
- 2 x `POST /api/events  401  error text /error/message`: Azure `The request authorization key is not authorized for LLAWTOPIC.EASTUS-…`, emulator `The request authorization key is not authorized for SANITIZED.EVENTGR…`

**servicebus** (38 of 10564 values)

- 8 x `PUT /  401  error text /Error/Detail[]`: Azure `Generic: Provided resource identifier '/subscriptions/faa080af-c1d8-4…`, emulator `MissingToken: The authorization header was not found. TrackingId:d999…`
- 5 x `PUT /{entity}  400  error text /Error/Detail[]`: Azure `SubCode=40000. The entity description of type 'QueueDescription' has …`, emulator `SubCode=40000. The entity description of type 'QueueDescription' has …`
- 4 x `GET /$Resources/queues?$skip&$top  401  error text /Error/Detail[]`: Azure `InvalidSignature: The token has an invalid signature. TrackingId:a990…`, emulator `MissingToken: The authorization header was not found. TrackingId:cd5a…`
- 4 x `GET /{entity}/subscriptions/{subscription}?enrich  200  value /entry/content[]/SubscriptionDescription[]/AutoDeleteOnIdle[]`: Azure `P10675199DT2H48M5.4775807S`, emulator `PT10M`
- 4 x `PUT /{entity}/subscriptions/{subscription}  200  value /entry/content[]/SubscriptionDescription[]/AutoDeleteOnIdle[]`: Azure `P10675199DT2H48M5.4775807S`, emulator `PT10M`
- 2 x `PUT /{entity}/subscriptions  400  error text /Error/Detail[]`: Azure `The specified HTTP verb (PUT) is not valid. To know more visit https:…`, emulator `The specified HTTP verb (PUT) is not valid. To know more visit https:…`

**datalake** (10 of 5790 values)

- 4 x `PUT /?comp=properties&restype=service  400  value /Error/LineNumber[]`: Azure `2`, emulator `1`
- 4 x `PUT /?comp=properties&restype=service  400  value /Error/LinePosition[]`: Azure `89`, emulator `1`
- 1 x `GET /?comp=list&include&maxresults&prefix  200  value /EnumerationResults/Containers[]/Container[]/Name[]`: Azure `filesystem14713c3b`, emulator `filesystem236d33317`
- 1 x `GET /?comp=list&include&maxresults&prefix  200  value /EnumerationResults/Containers[]/Container[]/Properties[]/DefaultEncryptionScope[]`: Azure `hnstestscope1`, emulator `$account-encryption-key`

**arm** (15 of 597 values)

- 2 x `POST /subscriptions/{sub}/providers/Microsoft.EventHub/checkNameAvailability  200  value /nameAvailable`: Azure `False`, emulator `True`
- 1 x `GET /providers/Microsoft.DocumentDB/operations  200  header content-type`: Azure `application/json`, emulator `application/json; charset=utf-8`
- 1 x `GET /providers/Microsoft.Storage/operations  200  header content-type`: Azure `application/json`, emulator `application/json; charset=utf-8`
- 1 x `GET /subscriptions/{sub}  200  value /displayName`: Azure `Azure SDK Developer Playground`, emulator `azemu subscription`
- 1 x `GET /subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Resources/deploymentScripts/{name}  200  value /identity/type`: Azure `userAssigned`, emulator `UserAssigned`
- 1 x `GET /subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.Resources/deploymentScripts/{name}  200  value /properties/provisioningState`: Azure `Failed`, emulator `Succeeded`

