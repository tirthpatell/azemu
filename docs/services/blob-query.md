# Blob quick query (`Query Blob Contents`)

**What it emulates.** `POST /{container}/{blob}?comp=query` on the Blob endpoint, and the same URL on the Data Lake (`dfs`) endpoint for a file: one SQL
`SELECT` over a block blob, CSV and JSON in and out, answered as an Avro stream of data, progress and error records, as the service does. The Avro
answer was checked byte for byte against seven decoded real answers.

**How to reach it.** Like Blob (`docs/services/blob.md`): `http(s)://{account}.blob.core.localhost:4577`, or `http://azemu:10000/{account}/...` with
`--standard-ports`.

**Not built.** Parquet input and Arrow output (both answer 501, see "Refused"), `sys.split`, `CASE`. Error names and some formatting rules are guesses
(see "Behaviour that could not be confirmed against Azure"); one real account would settle them.

## What it does

`POST /{container}/{blob}?comp=query` (and the same URL on the `dfs` endpoint for a file) runs one `SELECT` over the
data of a block blob and answers `200` with an Avro object container file (`Content-Type: avro/binary`) whose
records are the query's messages. Python (`BlobClient.query_blob`, `DataLakeFileClient.query_file`), JavaScript
(`BlockBlobClient.query`) and Java (`BlobClient.openQueryInputStream`, `queryWithResponse`) read the answer; the
Go SDK has no query call. The code is `crates/services/blob/src/query/`.

## The request

Headers: `Authorization` (SAS needs `r`), `x-ms-lease-id` (checked when sent; a leased blob can be read without it,
as for Get Blob), `If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`, `x-ms-if-tags`, the
customer-provided key headers (`cpk.rs`: a blob written with a key needs the same key, `409
BlobUsesCustomerSpecifiedEncryption` otherwise, and the answer carries `x-ms-encryption-key-sha256`), `snapshot`
and `versionid` in the query. Ranges and `timeout` are ignored. Only block blobs: another type is `409
InvalidBlobType`; an archived blob is `409 BlobArchived`; a blob larger than the limit is `400 InvalidInput`.

Body: `QueryRequest` with `QueryType` (`SQL` only), `Expression` (up to 256 KiB), `InputSerialization` and
`OutputSerialization` (`Format` > `Type` and the configuration). Errors, all before the stream starts:

| Problem | Answer |
|---|---|
| not XML, wrong root | `400 InvalidXmlDocument` |
| `QueryType`, `Expression`, `Format` or `Type` missing | `400 MissingRequiredXmlNode` (`XmlNodeName`) |
| `QueryType` not `SQL`, a format type that does not exist (`arrow` as input, `parquet` as output), `HasHeaders` not a boolean, a separator longer than 16 bytes | `400 InvalidXmlNodeValue` |
| expression over 256 KiB | `400 InvalidInput` |
| `parquet` as input, `arrow` as output | `501 NotImplemented`, the message names it |

Defaults when something is not given: no `InputSerialization` is delimited text, `,` `"` `\n`, no escape character,
no header row. No `OutputSerialization`: the same format and separators as the input, no header row (the Python SDK
sends the input dialect as the output when none is given, so a header row in its input dialect is also asked for
in the output). An empty `FieldQuote`, `ColumnSeparator` or `RecordSeparator` means the default (the JavaScript SDK
sends an empty `FieldQuote` when none is set); an empty `EscapeChar` means none. `csv` is accepted for `delimited`.

## The answer

Headers: those of Get Blob (`ETag`, `Last-Modified`, `x-ms-blob-type`, `x-ms-creation-time`, lease, metadata,
`x-ms-resource-type: file` for a Data Lake file, `x-ms-server-encrypted` and so on) with `Content-Type:
avro/binary`. Like Azure, the emulator sends it chunked (HTTP/1.1) or as data frames (HTTP/2), with no `Content-Length`: the
answer is made while the client reads it.

The body, as in the recordings (`avro.rs` has the layout):

* header: `Obj\x01`, a metadata map with the one entry `avro.schema` (1959 bytes of JSON, copied from the answers;
  no `avro.codec`), the 16 byte sync marker (random per answer);
* one block per message (count 1, size, record, sync marker). A record is the index of its type in the schema's
  union and its fields: 0 `resultData` (`bytes`), 1 `error` (`fatal`, `name`, `description`, `position`), 2
  `progress` (`bytesScanned`, `totalBytes`), 3 `end` (`totalBytes`);
* order: result data (as many records as the data needs), then the non-fatal errors found since the last data
  record, then `progress`; at the end the last data, the errors, a fatal error if there was one, `progress` with
  both numbers equal to the blob size (also after a fatal error, as recorded), `end`. A header line of the output
  (`HasHeaders` true) is a data record of its own before the first row. When there is no data at all one empty data
  record is sent (recorded for a fatal error at the first record; for a query without results it is a guess).

Result data is cut into records of at most 256 KiB (`data_chunk_bytes`) with a `progress` after each full one;
`bytesScanned` is the blob offset reached. The real service's chunk size is not in any recording.

## Formats

**Delimited text in.** Column separator, field quote, record separator and escape character may be any string of
up to 16 bytes. A field that starts with the quote ends at the closing quote; a doubled quote is a quote; with an
escape character set, escape + quote is a quote and escape + escape is the escape character; a record separator
inside quotes is data. With the default record separator `\n`, `\r\n` ends a record as well (the SDK tests' blobs
have `\r\n` and the real answers have `\n`). Empty lines are skipped. `HasHeaders` makes the first record the names
of the columns, which are then not returned. A quote that is not closed before the end is a fatal `ParseError`.
All values are text.

**Delimited text out.** A field is quoted when it holds the column separator, the record separator (recorded: `'1.0.1'`
with `.` as record separator), the quote, the escape character or a line break; inside quotes the quote is written as
escape + quote (doubled when there is no escape character) and the escape character is doubled. Empty values (NULL,
MISSING, empty text) are written as nothing. Numbers have no `.0` (`3`, `2.5`).

**JSON in.** The blob is read as JSON documents one after the other. The record separator (default `\n`) and
white space between two documents are skipped and are not needed: the real service reads `{"a":1}{"b":2}` as two
records (recorded: `test_quick_query_with_only_input_json_serialization_setting`, whose blob has no line breaks). An
object or array ends where its brackets close, so a document may span lines and a separator inside a string is
data; a scalar ends at white space or the separator. A document that is not valid JSON is a fatal `ParseError` with
the offset of the document and the parser's message in the form the real service uses, for example `Unexpected
token 'n' at [byte: 1]. Expecting token '"'. ` for `{name: owner}` (recorded; the byte is counted from the start of
the document). Nesting is limited to 100 levels. Objects keep the order of their fields. `FROM
BlobStorage[*].a.b[*]` walks into the document and makes one row of every element of an array (`[*]` as the first
step is the same as none).

**JSON out.** One object per row, `{"name":value,...}` followed by the record separator; MISSING fields are left out
(recorded: `{}` for a record without the field); `SELECT *` writes the record as it was read (compact, same order).
Values from delimited text are strings (`"1"`); `CAST` makes numbers.

**Names of output columns** (JSON output, and header rows): the alias; for a column the name in the header row (or
`_N` for an ordinal); for a JSON field the name of the last step; for anything else `_N` with N the place in the
select list. This is a guess (see below).

## The SQL dialect

`SELECT * | item [, item]... FROM BlobStorage[...] [alias] [WHERE expr] [LIMIT n]` (a trailing `;` is fine). From the
Microsoft reference "Query acceleration SQL language reference":

| Built | Notes |
|---|---|
| columns by name (case-insensitive when not quoted, exact in `"double quotes"`), by position `_1`, `_2`, ... | a name that is not in the header row, or any name when there is no header row, is a fatal `InvalidColumnName`; a row with fewer fields than the position is a non-fatal `InvalidColumnOrdinal` (recorded) and the row is skipped |
| `*`, expressions, `AS alias` or a bare alias, table alias (`s.name`, `BlobStorage.name`) | |
| JSON: `a.b`, `a[0].b`, `FROM BlobStorage[*].path` | |
| literals: integers, decimals, `'text'` (`''` is a quote), `TRUE`, `FALSE`, `NULL` | |
| `= != <> < <= > >=`, `AND OR NOT`, `+ - * / %`, `\|\|` | text next to a number is read as a number, next to a timestamp as a timestamp; integer `/` is integer division; overflow and division by zero are non-fatal row errors |
| `BETWEEN`, `IN (...)`, `LIKE ... [ESCAPE c]` (`%`, `_`), `IS [NOT] NULL`, `IS [NOT] MISSING` | `LIKE` is matched without backtracking (see "Time and cancellation") |
| `CAST(x AS INT\|FLOAT\|STRING\|BOOLEAN\|TIMESTAMP)` | also `INTEGER`, `BIGINT`, `DOUBLE`, `DECIMAL(p,s)`, `NUMERIC`, `VARCHAR`, `BOOL`; `DECIMAL` is a FLOAT here |
| `CHAR_LENGTH`, `CHARACTER_LENGTH`, `LOWER`, `UPPER`, `SUBSTRING(s, start[, len])` and `SUBSTRING(s FROM start [FOR len])`, `TRIM([LEADING\|TRAILING\|BOTH] [chars] FROM s)` and `TRIM(s)` | `SUBSTRING` counts from 0, because the reference's example is `SUBSTRING('123456789', 1, 5)` = `23456` |
| `DATE_ADD(part, n, ts)`, `DATE_DIFF(part, ts1, ts2)`, `EXTRACT(part FROM ts)`, `TO_STRING(ts, pattern)`, `TO_TIMESTAMP(s)`, `UTCNOW()` | parts: year, month, day, hour, minute, second (and timezone_hour, timezone_minute for `EXTRACT`); the part may be quoted or not; `TO_STRING` patterns as in the table of the reference (`yy y yyyy M MM MMM MMMM d dd a h hh H HH m mm s ss S..SSS X XX XXX x xx xxx`) |
| `COALESCE`, `NULLIF` | |
| `COUNT(*)`, `COUNT(x)`, `SUM`, `AVG`, `MIN`, `MAX` | every select item must be an aggregate, or none; not in `WHERE`; text is read as a number for `SUM` and `AVG`; one row comes out at the end |
| `LIMIT n` | stops the scan after n result rows |

Timestamps are read as ISO 8601 (`2007T`, `2007-02`, `2007-02-23`, `...T12:14Z`, `...T12:14:33.079+05:30`) and written
as `2007-02-23T12:14:33.079Z` (the fraction only when there is one). Adding a number to a timestamp adds days.

**Not built** (each is a fatal `ParseError` that names it): `CASE`, `sys.split`, subqueries, joins, `GROUP BY`,
`ORDER BY`, `DISTINCT`, any function not in the table. A syntax error, an unknown function and a wrong table name all
end the same way: HTTP 200 and a fatal `ParseError` with a description that names the place (see "Behaviour that could not be confirmed against Azure").

Bounds of the parser: the text is at most 256 KiB; at most 100 levels of nesting (parentheses, prefix operators and
operator chains each count, so `1+1+1+...` ten thousand times is refused); at most 100,000 nodes; at most 4,096 items
in the select list; paths at most 64 steps. The evaluator does no recursion deeper than that.

## Refused

* **Parquet input**: `501 NotImplemented` ("Query Blob Contents with parquet input is not implemented by this
  emulator"). No Parquet reader that is small, pure Rust and covers what real files use (dictionary pages, several
  compression codecs) was available without a large dependency, and a reader that is wrong is worse than none.
  The two `test_quick_query_input_in_parquet_format` recordings (blob and Data Lake) therefore still fail in replay.
* **Arrow output**: `501 NotImplemented`. The recorded answer (`test_quick_query_output_in_arrow_format`) is two Arrow
  IPC messages built by Azure's own writer, one with a second batch of unexplained content; with one recording the
  rules (batching, null handling, decimal storage) cannot be told from the quirks, and the SDK tests compare bytes.
  The two recordings still fail in replay.
* `sys.split`, `CASE`, other SQL: see above.

## Limits (`Limits` in `exec.rs`, environment variables read at start)

| Limit | Default | Variable | When it is passed |
|---|---|---|---|
| blob size | 256 MiB | `AZEMU_BLOB_QUERY_MAX_BLOB_BYTES` | `400 InvalidInput` before anything is read |
| one record (a CSV line, a JSON document) | 4 MiB | `AZEMU_BLOB_QUERY_MAX_RECORD_BYTES` | fatal `RecordTooLarge` at the record's offset |
| size of the answer | 128 MiB | `AZEMU_BLOB_QUERY_MAX_ANSWER_BYTES` | fatal `ResultTooLarge` |
| time of the scan | 30 s | `AZEMU_BLOB_QUERY_SECONDS` | fatal `QueryTimeout` (looked at inside every record, expression and match, see below; time spent waiting for a slow client counts) |
| non-fatal errors reported | 1000 | `AZEMU_BLOB_QUERY_MAX_ERRORS` | fatal `TooManyErrors` |
| queries at once | 4 | `AZEMU_BLOB_QUERY_MAX_CONCURRENT` | the fifth waits 10 s for a place, then `503 ServerBusy`. A place is held until the answer has been sent or the client is gone |
| a string built by `\|\|` | 16 MiB | | row error `ValueTooLarge` |

Memory and the lock: the state lock is held only to find the blob, check conditions, lease and key, and take the
blob's data out as shared `Bytes` pieces (a handle per block, no copy; `blob_headers` and the checks are the same
as for Get Blob). The expression is parsed before the lock. The scan runs on a blocking thread after the lock is
released, reading the pieces through a window of 64 KiB (never a joined copy of the blob). Because the scan owns shared
handles, a blob that is overwritten or deleted while its query runs does not change the answer: the query sees the
version it started with.

## The answer is streamed

The scan writes the answer into a channel that holds two pieces (each a data record of at most 256 KiB with its progress
message); core (`azemu_core::stream`) takes a piece when the connection can send it. So memory per query is about
three pieces whatever the size of the answer (a 70 KB blob and a 16 KB expression that make a 134 MB answer use 35 MB of
process memory in all), and a client that reads slowly makes the scan wait. The answer is still limited to 128 MiB
(`ResultTooLarge`), which is now a limit on time and bytes sent, not on memory.

The place in the gate belongs to the answer: it is given back when core has dropped the answer (it was sent completely, the
client closed the connection or reset the HTTP/2 stream, or the client took no data for the body idle time of 60 s and core
closed the connection) and the scan has ended. Five clients that never read hold five places for at most 60 s; they do not
hold memory. The place is also given back when the request is cancelled while the query waits for a place.

## Time and cancellation

`Budget` (`budget.rs`) counts work and reads the clock every 256 units. One expression evaluated is one unit, a text
costs one unit per 4 KiB it holds, every record is a unit plus its size, so the limit is looked at between records, inside
a record with a long select list, inside `LIKE`, and while a big value is copied, compared, cast or changed by a string
function. A query stops within milliseconds of its limit (`QueryTimeout`, fatal, in the answer). The same check looks at the
cancellation token of the request (`azemu_core::CancelToken`): when the client goes away the scan ends at the next check, writes
nothing more, and gives its place back.

`LIKE` (`like.rs`) cuts the pattern at `%` into segments and finds each one from where the one before ended. A segment of
plain characters is found with `str::find` (linear), a segment with `_` with the shift-and method (the value's length times
the segment's length divided by 64, counted against the budget as it goes). The value is read in place. There is no step limit and no `InvalidLikePattern` error for a long value: `'aaaa...(4 million)' LIKE '%aaaab'` takes
milliseconds.

## Behaviour that could not be confirmed against Azure (not in a recording or in Microsoft's pages)

* A **bad expression** is HTTP 200 and a fatal `ParseError` in the stream; the REST page says errors found while
  streaming are part of the body, and the one recorded `ParseError` (JSON) is in-band, but no recording has a bad
  SQL expression. The description is `<what> (position N)`, position 0.
* Names of errors other than `ParseError` and `InvalidColumnOrdinal` (recorded): `InvalidColumnName`, `CastFailed`,
  `DivisionByZero`, `ArithmeticOverflow`, `InvalidLikePattern`, `InvalidFormat`, `ValueTooLarge` (non-fatal, the row
  is skipped) and `RecordTooLarge`, `ResultTooLarge`, `QueryTimeout`, `TooManyErrors` (fatal) are the emulator's own.
  `InvalidColumnOrdinal` carries `Column ordinal N does not exist in row.` with N the place in the select list (the
  recorded N is 0 for a one column select list; whether Azure counts the select list or something else is unknown).
* An empty CSV field is empty text, not NULL. Output column names for JSON output (above). `SUBSTRING` from 0.
  Numbers are written without `.0`. `AVG` always gives a FLOAT. A number next to text that is not a number is a
  row error, not "false".
* The chunk size (256 KiB), a progress after each full data record, and one empty data record for an answer with no
  data when no error was found.
* `bytesScanned` of the closing progress is the blob size even when `LIMIT` stopped the scan.
* The HTTP statuses for a wrong blob type (`409 InvalidBlobType`) and a blob over the size limit (`400 InvalidInput`).
