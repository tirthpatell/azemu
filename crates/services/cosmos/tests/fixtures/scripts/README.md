# Sample stored procedures, triggers and functions

These files are **not** written by the azemu authors. They are Microsoft's published samples, kept here as test input for the
Cosmos DB script engine (`../../scripts.rs`). They are not part of the shipped binary. Their licence is the MIT licence, with
Microsoft's copyright; it is not changed by the licence of this repository. The licence text is in
[`LICENSES/MIT-Microsoft.txt`](../../../../../../LICENSES/MIT-Microsoft.txt) at the root of the repository.

| File | Source | Licence | Changes |
|---|---|---|---|
| `hello_world.js`, `create_todo_items.js`, `array_parameter.js`, `trade_players.js`, `bulk_import.js`, `async_sample.js`, `pre_trigger_timestamp.js`, `post_trigger_metadata.js`, `udf_tax.js` | Microsoft Learn, "How to write stored procedures, triggers, and user-defined functions in Azure Cosmos DB" (https://learn.microsoft.com/azure/cosmos-db/how-to-write-stored-procedures-triggers-udfs); the section is named in the first line of each file. The article was published from the public repository `MicrosoftDocs/azure-docs` (`articles/cosmos-db/...`), whose `LICENSE-CODE` gives its code samples the MIT licence (Copyright (c) Microsoft Corporation) and whose `LICENSE` gives its prose CC BY 4.0. | MIT | A `// Source:` comment line added at the top. `hello_world.js` is the `serverScript` of the article's `helloWorldStoredProc` example. Nothing else was changed. |
| `bulk_delete.js`, `bulk_import_repo.js`, `count.js`, `sum.js`, `update.js` | https://github.com/Azure/azure-cosmosdb-js-server, `samples/stored-procedures/` (`bulkDelete.js`, `BulkImport.js`, `Count.js`, `sum.js`, `update.js`) | MIT, Copyright (c) 2022 Microsoft | Byte for byte the upstream files (checked against the repository's `master` branch). Renamed to snake case. |

The current source of the Learn article is a Microsoft repository that is not public; the sample code was checked to be in the
public `azure-docs` repository under the licence above (`articles/cosmos-db/sql/how-to-write-stored-procedures-triggers-udfs.md`,
commit a6dcba06). The same samples are quoted, with small changes, in the tests of `tests/sdk-python/cosmos`, `tests/sdk-js` and
`tests/sdk-dotnet` (`CosmosScriptTests.cs`).
