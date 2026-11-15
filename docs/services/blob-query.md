# Blob quick query (`Query Blob Contents`)

**What it emulates.** `POST /{container}/{blob}?comp=query` on the Blob endpoint, and the same URL on the Data Lake (`dfs`) endpoint for a file: one SQL
`SELECT` over a block blob, CSV and JSON in and out, answered as an Avro stream of data, progress and error records, as the service does. The Avro
answer was checked byte for byte against seven decoded real answers. Only the Blob endpoint has it: the `dfs` host reads the `POST` as a lease and answers
`400 MissingRequiredHeader` (as Azure does).

**How to reach it.** Like Blob (`docs/services/blob.md`): `http(s)://{account}.blob.core.localhost:4577`, or `http://azemu:10000/{account}/...` with
`--standard-ports`.

**Not built.** Parquet input of a real Parquet file and Arrow output (501, see "Refused"), `sys.split`, `CASE`, decimal arithmetic. Error names, status codes, number and
timestamp text were checked against a real account in November 2026 (about 130 queries, see "What was checked"); what is still a guess is listed at the end.

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
BlobUsesCustomerSpecifiedEncryption` or `BlobCustomerSpecifiedEncryptionMismatch` otherwise, and the answer carries `x-ms-encryption-key-sha256`), `snapshot`
and `versionid` in the query. Ranges and `timeout` are ignored. Only block blobs: a page blob or an append blob is `400
QueryBlobContentsNotSupportedForPageBlobsOrAppendBlobs` (the message is the code); an archived blob is `409 BlobArchived`; a blob larger than the limit is `400 InvalidInput`.
The order is: blob and container found, conditions, blob type, then the request body (so a bad expression for a missing blob is `404`, and for a page blob the `400` above).

Body: `QueryRequest` with `QueryType` (any value is read as `SQL`), `Expression` (up to 256 KiB), `InputSerialization` and
`OutputSerialization` (`Format` > `Type` and the configuration). Errors, all before the stream starts:

| Problem | Answer |
|---|---|
| not XML, wrong root, `QueryType` or `Expression` missing, `Expression` empty | `400 InvalidXmlDocument` |
| `Format` or `Type` missing | `400 MissingRequiredXmlNode` (`XmlNodeName`; not tried on Azure) |
| a format type that does not exist (`arrow` as input, `parquet` as output), `HasHeaders` not a boolean, a separator longer than 16 bytes | `400 InvalidXmlNodeValue` (not tried on Azure) |
| expression over 256 KiB | `400 InvalidInput` (not tried on Azure) |
| `parquet` as input | `400 ErrorProcessingQueryExpression` "Invalid parquet file. Corrupt footer." for a blob that is not Parquet; `501 NotImplemented` for one that starts and ends with `PAR1` |
| `arrow` as output | `501 NotImplemented`, the message names it |
| the expression is not valid, or does not fit the data | `400 ErrorProcessingQueryExpression`, see "Bad expressions" |

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
  record is sent (recorded for a fatal error at the first record, and seen on Azure for a query without results).
* **An empty blob and `LIMIT 0`** are only `end` (no data record, no progress), as on Azure.
* **An aggregate** (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`) is an empty data record, the errors, a `progress` for the finished scan, then one data record with
  the result, then `end`. A `SUM` or `AVG` over no rows is an empty record without a record separator in delimited output and `{"_1":null}` in JSON output. `LIMIT n` counts the
  rows that are added up (`COUNT(*) ... LIMIT 1` is 1). The Python SDK's `records()` therefore yields an empty first item for an aggregate.
* **A row that fails** (a non-fatal or a fatal error in the select list) leaves the items before the failing one on the output line, each followed by the column separator,
  and no record separator (delimited output): `SELECT _1, CAST(_2 AS INT)` over `alice,x` writes `alice,` and reports the error. In JSON output nothing is written for the row.

Result data is cut into records of at most 4 MiB (`data_chunk_bytes`) with a `progress` after each full one;
`bytesScanned` is the blob offset reached. Azure sent a result of 1,020,000 bytes as one record (tried in November 2026); how it cuts a larger one was not tried,
so the 4 MiB is a choice. (It used to be 256 KiB.)

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
MISSING, empty text) are written as nothing. A header row (`HasHeaders` true in the output) is written only when every select item is a column or has an alias
(and the input has a header row, or the select list is `*`): `SELECT name, age + 1` writes none, as Azure does.

**Numbers and timestamps** (checked against Azure in November 2026). A number written in the query is written back as it was typed when it is a select item by itself
(`2.0`, `1.50`, `100000000000`; also in JSON output); the language has no exponent (`1e3` is the number `1` with the alias `e3`). Computed numbers are doubles written with
15 significant digits and no trailing zeros (`0.1 + 0.2` is `0.3`, `10.0/4` is `2.5`, `10/4` is `2`). Azure computes with decimals that keep their scale (`2 * 3.5` is `7.0`,
`1/3.0` is `0.333333`, `0.000001 * 0.5` is `5E-7`); that is not copied. A timestamp is written as `2007-02-23T12:14:33+00:00`: no fraction, and `+00:00` for UTC; another offset is kept.

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
select list (checked against Azure in November 2026).

An empty field of delimited text is empty text (`WHERE age = ''` finds it, `COALESCE(age, 'none')` gives the empty text) and NULL when it is read as a number (`age + 1`, `age > 5`, `CAST(age AS INT)`): no error, and the row is
left out of a comparison or written as an empty value.

## The SQL dialect

`SELECT * | item [, item]... FROM BlobStorage[...] [WHERE expr] [LIMIT n]` (a trailing `;` is fine). From the
Microsoft reference "Query acceleration SQL language reference", with what Azure refused when it was tried (November 2026) taken out:

| Built | Notes |
|---|---|
| columns by name (case-insensitive when not quoted, exact in `"double quotes"`), by position `_1`, `_2`, ... | a name that is not in the header row, or any name when there is no header row, is a `400` before any data (see "Bad expressions"); a row with fewer fields than the position is a non-fatal `InvalidColumnOrdinal` (recorded) and the row is skipped |
| `*`, expressions, `AS alias` (a plain word; `AS "n m"` is refused) or a bare alias, `BlobStorage.name` | no alias for the table (`FROM BlobStorage s` is refused) |
| JSON: `a.b`, `a[0].b`, `FROM BlobStorage[*].path` | |
| literals: integers, decimals, `'text'` (`''` is a quote), `TRUE`, `FALSE`, `NULL` | |
| `= != <> < <= > >=`, `AND OR NOT`, `+ - * / %` | text next to a number is read as a number (an empty field is NULL), next to a timestamp as a timestamp; integer `/` is integer division; text that is not a number is a non-fatal `InvalidTypeConversion`; overflow and division by zero end the query (`ClientError`). No `\|\|` |
| `BETWEEN`, `IN (...)`, `LIKE ... [ESCAPE c]` (`%`, `_`), `IS [NOT] MISSING` (JSON input only) | `LIKE` is matched without backtracking (see "Time and cancellation"). No `IS NULL` |
| `CAST(x AS INT\|FLOAT\|STRING\|BOOLEAN\|TIMESTAMP)` | also `INTEGER`, `BIGINT`, `DOUBLE`, `DECIMAL(p,s)`, `NUMERIC`, `VARCHAR`, `BOOL`; `DECIMAL` is a FLOAT here. Text that is not a number is `InvalidTypeConversion` (non-fatal), an empty field is NULL, text that is not `true` is `false` for `BOOLEAN`, text that is not a timestamp ends the query |
| `CHAR_LENGTH`, `CHARACTER_LENGTH`, `LOWER`, `UPPER`, `SUBSTRING(s, start[, len])`, `TRIM([LEADING\|TRAILING\|BOTH] [chars] FROM s)` and `TRIM(s)` | `SUBSTRING` counts from 0, because the reference's example is `SUBSTRING('123456789', 1, 5)` = `23456`; the `FROM` form of `SUBSTRING` is refused |
| `DATE_ADD(part, n, ts)`, `DATE_DIFF(part, ts1, ts2)`, `EXTRACT(part FROM ts)`, `TO_STRING(ts, pattern)`, `TO_TIMESTAMP(s)`, `UTCNOW()` | parts: year, month, day, hour, minute, second (and timezone_hour, timezone_minute for `EXTRACT`); the part may be quoted or not; `TO_STRING` patterns as in the table of the reference (`yy y yyyy M MM MMM MMMM d dd a h hh H HH m mm s ss S..SSS X XX XXX x xx xxx`) |
| `COALESCE`, `NULLIF` | |
| `COUNT(*)`, `COUNT(x)`, `SUM`, `AVG`, `MIN`, `MAX` | one aggregate is the whole select list (`COUNT(*), MAX(x)` is a syntax error); not in `WHERE`; `SUM`, `AVG`, `MIN` and `MAX` take numbers only: text, even `7`, ends the query (`ClientError` "Not a numeric type."), use `CAST`; one row comes out at the end |
| `LIMIT n` | stops the scan after n rows |

Timestamps are read as ISO 8601 with a `T` (`2007T`, `2007-02T`, `2007-02-23T`, `...T12:14Z`, `...T12:14:33.079+05:30`); `2007-02-23` and `2007-02` without the `T` are refused
(`ClientError` "String can't be converted to TimeStamp: 2007-02-23"). Adding a number to a timestamp adds days.

**Not built** (each is `400 ErrorProcessingQueryExpression`, a syntax error, as on Azure): `CASE`, `sys.split`, subqueries, joins, `GROUP BY`,
`ORDER BY`, `DISTINCT`, any function not in the table.

Bounds of the parser: the text is at most 256 KiB; at most 100 levels of nesting (parentheses, prefix operators and
operator chains each count, so `1+1+1+...` ten thousand times is refused); at most 100,000 nodes; at most 4,096 items
in the select list; paths at most 64 steps. The evaluator does no recursion deeper than that.

## Refused

* **Parquet input**: a blob that is not a Parquet file gets Azure's `400 ErrorProcessingQueryExpression` "Invalid parquet file. Corrupt footer."; a blob that starts and
  ends with `PAR1` gets `501 NotImplemented` ("Query Blob Contents with parquet input is not implemented by this
  emulator"). No Parquet reader that is small, pure Rust and covers what real files use (dictionary pages, several
  compression codecs) was available without a large dependency, and a reader that is wrong is worse than none.
  The two `test_quick_query_input_in_parquet_format` recordings (blob and Data Lake) therefore still fail in replay.
* **Arrow output**: `501 NotImplemented`. The recorded answer (`test_quick_query_output_in_arrow_format`) is two Arrow
  IPC messages built by Azure's own writer, one with a second batch of unexplained content; with one recording the
  rules (batching, null handling, decimal storage) cannot be told from the quirks, and the SDK tests compare bytes.
  The two recordings still fail in replay.
* `sys.split`, `CASE`, other SQL: see above.

## Bad expressions

Azure answers these with `400 ErrorProcessingQueryExpression` before any data, in the XML error form (checked in November 2026). The emulator does the same.

| Problem | Message |
|---|---|
| a syntax error, an unknown function, an unclosed string, a construct Azure does not have (see "Not built" and the dialect table) | `Syntax error in query: <the expression>` |
| a table name other than `BlobStorage`, or a path (`[*]`, `.a`), over CSV | `Record paths are not valid for queries over CSV-formatted input.` |
| a table name other than `BlobStorage`, or `[0]`, over JSON | `Invalid path for JSON-formatted input.` |
| a column name when the input has no header row (also an empty blob; `_0`; a word where `DATE_ADD` wants a date part) | `Column names can be used only if 'hasHeader' is set in the config and there is a header row.` |
| a column name that is not in the header row (quoted names match exactly) | `Column name(s) does not exist in the header row.` |
| `_N` beyond the columns of the header row | `Invalid column ordinal(s).` (without a header row it is a non-fatal `InvalidColumnOrdinal` per row, recorded) |
| `ESCAPE` of more than one character | `An invalid escape character was specified in the LIKE predicate.` |
| `IS MISSING` over CSV | `Missing Expression is not valid for this format!` |

The names and the ordinals are checked against the first record of the blob before the answer starts. What is wrong in the data stays in the stream (HTTP 200): a JSON document that
cannot be read is a fatal `ParseError` at position 0 whose text counts bytes from the start of the blob (`Unexpected token 'bad' at [byte: 9]. Expecting token '"'. `), an unclosed quote in
CSV is `ParseError` "Unexpected end-of-input within record at [byte: 20]. " (the size of the blob). Row errors are non-fatal `InvalidTypeConversion` ("Invalid type conversion.") at the
offset of the row; arithmetic that cannot be done, a bad timestamp, a bad `TO_STRING` pattern and `SUM` over text are fatal `ClientError` (`Divided 30 by zero.`, `Division by zero in decimal arithmetic`,
`Integer addition overflow/underflow for ints 9223372036854775807, 1`, `String can't be converted to TimeStamp: alice`, `String alice can't be converted to TimeStamp.` for `CAST`,
`TimeStamp 2007-02-23T12:14:33+00:00 can't be formatted with given formatString`, `Not a numeric type.`).

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

## What was checked, and what was not

In November 2026 about 130 queries, 50 more on number and error text, and a dozen body variants were sent to a real storage account and to the emulator, and the answers were
compared (status, error name, message, the data, the order of the messages). The emulator now gives the same answer for all of them except the cases below.

Differences that are not copied:

* **Decimal arithmetic.** Azure computes with decimals: `2 * 3.5` is `7.0`, `1/3.0` is `0.333333`, `1000000.0 * 1000000.0` is `1000000000000.00`, `0.000001 * 0.5` is `5E-7`. Here the numbers are doubles
  written with 15 significant digits (`7`, `0.333333333333333`, `1000000000000`, `0.0000005`). The literal as typed, `0.1 + 0.2` and `10.0/4` do match. Azure closes the connection for `5.5/0` and `-2.0`
  (a failure on its side); here `5.5/0` is the `ClientError` above and `-2.0` is `-2`.
* `age NOT BETWEEN 1 AND 10` over an empty field gives Azure an extra non-fatal `NullValue` ("Null value.") error; not copied.
* The `dfs` headers of a served path (Azure does not send `x-ms-blob-type` there) are not touched.

Not tried on Azure, and therefore choices (they are marked "not tried" above where they sit in a table):

* The size at which a result is cut into several data records (4 MiB here; a 1,020,000 byte result was one record on Azure), `ValueTooLarge`, `RecordTooLarge`, `ResultTooLarge`, `QueryTimeout`,
  `TooManyErrors` and the answer for a blob over the size limit (they need blobs of many MiB).
* The names `InvalidColumnName`, `ValueTooLarge`, `RecordTooLarge`, `ResultTooLarge`, `QueryTimeout`, `TooManyErrors` and `InvalidFormat` are the emulator's own (the limits are the emulator's too).
  `InvalidColumnOrdinal` carries `Column ordinal N does not exist in row.` with N the place in the select list (checked: it is the 0-based place of the failing item).
* `bytesScanned` of the closing progress is the blob size even when `LIMIT` stopped the scan (checked with a result of 1,020,000 bytes and `LIMIT 1`).
* The HTTP status for a blob over the size limit (`400 InvalidInput`), and the errors of the body for a missing `Format` or `Type`.
* The token in the JSON error text for a bad word is the whole word (`bad`), except where it could start `null`, `true` or `false`, where it is the first character (`n`, recorded for `{name: owner}`).
