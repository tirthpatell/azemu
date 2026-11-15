# Cosmos DB SQL query engine (`azemu-cosmos-sql`)

**What it emulates.** The query language of Cosmos DB for NoSQL (`SELECT ... FROM c ... WHERE ... ORDER BY ... GROUP BY ... OFFSET LIMIT`, joins,
subqueries, aggregates, string, math, array, date and type functions) and the "query plan" the SDKs ask for before a cross-partition query.

**How to reach it.** Not by itself: the Cosmos service (`docs/services/cosmos.md`) runs queries with it, so every Cosmos endpoint and SDK query goes through it.

**Not built.** Spatial `ST_*`, full text search (`FullTextContains` and the others), `ORDER BY RANK`, computed properties; each answers a clear 400 "not supported".
**Checked against Azure.** In November 2026 the query language was compared with a real account (API for NoSQL, serverless) through the Python SDK: about 1,500 queries, each run on the account and
on the emulator. The section "Checked against Azure in November 2026" below lists what that settled; "Behaviour that could not be confirmed against Azure" lists what it did not, and "Known differences" what is left.

A pure library (no HTTP, no async, no I/O, no `unsafe`) that parses and runs the Azure Cosmos DB
for NoSQL query language over `serde_json::Value` documents, and answers the questions a
gateway must answer for the SDK "query plan". The Cosmos service crate builds on it.

Reference: Microsoft Learn, "Query language in Cosmos DB" pages
(`learn.microsoft.com/azure/cosmos-db/nosql/query/*`, fetched 2026-11 for each page named in
the tests). Where a statement below says "checked against Azure", a real account gave that answer in November 2026; the other statements are
"as documented", and "Behaviour that could not be confirmed against Azure" lists where the documentation did not decide.

## Supported language

* `SELECT [DISTINCT] [TOP n|@n] * | VALUE expr | item [[AS] alias], ...`; unnamed items become `$1`, `$2`, ...
  (path expressions use their last name, as in `SELECT c.a.b` -> `b`).
* `FROM c`, `FROM root AS c`, `FROM c IN root.children`, `FROM c.address`, `FROM (SELECT ...) t`,
  `JOIN x IN c.arr`, `JOIN c.address a`, `JOIN (SELECT VALUE ...) [AS] s`; at most 10 joins in a query (the subqueries count too).
* `WHERE`, `GROUP BY`, `ORDER BY path [ASC|DESC], ...` (a path of the document, see below), `OFFSET n LIMIT m` and `TOP n` (a whole number or a parameter, nothing else: `TOP (1)`, `TOP -1` and `OFFSET -1` are syntax errors).
* `-- comment` to the end of the line. Reserved words (`select`, `value`, `from`, `top`, `order`, `left`, `udf`, `null`, ... the list is in `parser.rs`) cannot be property names after a dot, aliases or unquoted object keys; `c["select"]` works.
* Expressions: property paths, `c["a b"]`, `c.arr[0]`, arithmetic, comparison, `AND OR NOT`,
  bitwise `& | ^ ~ << >> >>>` (JavaScript 32-bit semantics), `||`, `?:`, `??`, `IN`, `NOT IN`,
  `BETWEEN`, `LIKE` (`%`, `_`, `[abc]`, `[a-f]`, `[^abc]`, `ESCAPE`), `EXISTS(...)`, `ARRAY(...)`,
  scalar subqueries, parameters, array/object literals (also unquoted keys), `NaN`, `Infinity`.
* Constants: `undefined null true false`, integers, decimals, exponents, hex (`0x1F`), strings with
  `'` or `"` and the documented escapes (`\uXXXX`, surrogate pairs).
* Aggregates `COUNT SUM AVG MIN MAX`, with or without GROUP BY, also inside expressions and subqueries.
* Keywords and function names are case-insensitive, property names and aliases case-sensitive (`udf` is a keyword in lower case only).
* Functions: `IS_DEFINED IS_NULL IS_ARRAY IS_BOOL IS_NUMBER IS_OBJECT IS_STRING IS_PRIMITIVE IS_INTEGER
  IS_FINITE_NUMBER IS_DATETIME`; `CONCAT CONTAINS STARTSWITH ENDSWITH STRINGEQUALS` (case flag), `INDEX_OF
  LASTINDEXOF LEFT RIGHT LENGTH LOWER UPPER LTRIM RTRIM TRIM REPLACE REPLICATE REVERSE SUBSTRING TOSTRING
  STRINGJOIN STRINGSPLIT STRING_SPLIT STRINGTOARRAY STRINGTOOBJECT STRINGTONUMBER STRINGTOBOOLEAN STRINGTONULL
  REGEXMATCH CONTAINS_ALL_CI CONTAINS_ALL_CS CONTAINS_ANY_CI CONTAINS_ANY_CS`; `ABS ACOS ASIN ATAN ATN2 CEILING COS
  COT DEGREES EXP FLOOR LOG LOG10 PI POWER RADIANS RAND ROUND SIGN SIN SQRT SQUARE TAN TRUNC NUMBERBIN INTADD
  INTSUB INTMUL INTDIV INTMOD INTBITAND INTBITOR INTBITXOR INTBITNOT INTBITLEFTSHIFT INTBITRIGHTSHIFT`;
  `ARRAY_CONCAT ARRAY_CONTAINS ARRAY_CONTAINS_ALL ARRAY_CONTAINS_ANY ARRAY_LENGTH ARRAY_SLICE ARRAY_SUM ARRAY_AVG
  ARRAY_MEDIAN ARRAY_MIN ARRAY_MAX SETUNION SETINTERSECT SETDIFFERENCE SETEQUAL CHOOSE OBJECTTOARRAY IIF`;
  `GETCURRENTDATETIME GETCURRENTTIMESTAMP GETCURRENTTICKS` (+ `STATIC`), `NOW AGO DATETIMEADD DATETIMEDIFF
  DATETIMEFROMPARTS DATETIMEPART DATETIMEBIN DATETIMETOTICKS DATETIMETOTIMESTAMP DATETIMEFORMAT TICKSTODATETIME
  TIMESTAMPTODATETIME YEAR MONTH DAY`.
* Undefined rules as documented: comparison across types or with undefined is undefined; WHERE keeps
  only `true`; undefined properties are omitted from objects and arrays; `SELECT VALUE undefined`
  returns no row; a function called with an undefined argument is not called and gives undefined.
* Numbers: integers stay integers (`1` is `1`); an integral float result below 2^53 is written as an
  integer (`10/5` is `2`); integer overflow falls back to double; `NaN`/`Infinity` exist inside a query,
  and an answer that holds one is a 400 with sub-status 4001 (checked against Azure; JSON cannot write them).
* Safety: no panics on any input (fuzzed with truncations, mutations, random bytes, random token
  soup), nesting depth limit (see below), a row budget for the work of a query and a byte budget for what it holds.

## Memory and work

* **Shared documents**: `execute_shared` takes `&[Arc<Value>]`. A row of the FROM stage holds the document as `Val::Root` (no conversion);
  `c.a.b[0]` reads the JSON in place and converts only what it ends at; a bare `c` in an expression is converted when needed; `SELECT *`,
  `SELECT VALUE c` and filters return the same `Arc` (`Item::Shared`). `execute` (borrowed `&Value`) copies one document at a time.
* **Streaming**: FROM and JOIN produce one row at a time into WHERE and then the projection or the group (`eval/query.rs`, `stream`).
  The product of a JOIN is never held; `max_rows` (5 million) counts the rows produced, whether kept or not.
* **Aggregates** are running totals (`aggregate.rs`, `Acc`); a group keeps its first row and its accumulators, not its rows.
* **Early stop**: no ORDER BY / GROUP BY / aggregate: TOP, OFFSET LIMIT end the scan when enough rows are there (DISTINCT is decided as
  rows arrive); `EXISTS` and scalar subqueries stop after one row; ORDER BY with TOP / LIMIT keeps at most about twice the rows it needs.
* **Byte budget** `max_bytes`: counts ORDER BY / result rows, DISTINCT keys, groups; a subquery gives back what it used; over it:
  `ErrorKind::LimitExceeded` with a message that says what to do. Arguments of one function call may total 48 MB, `CONCAT`, `||`,
  `REPLACE`, `STRINGJOIN` may not build a string over 4 MB, `ARRAY_CONCAT` an array over a million elements, array and object literals
  and select lists check their running size (reading the same big property many times in one expression copies it each time).
  Variadic functions take any number of arguments.

## User-defined functions

`udf.name(args)` parses to `ExprKind::Udf` anywhere an expression may stand (SELECT, WHERE, GROUP BY, subqueries; not ORDER BY, which takes a path; the prefix `udf` is
lower case only on Azure, the function name is case sensitive). The evaluator calls `UdfHost::call(name, args)` once per evaluation of the call, with the arguments as
JSON (`None` is an undefined argument) and takes `Option<Value>` back (`None` is undefined, so the row drops out of `SELECT VALUE` like any
undefined). `EvalOptions::udf` holds a `UdfFactory` (`Send + Sync`) that makes the host on the thread that runs the query, because a script engine
cannot move between threads. `Query::udf_names()` lists the functions a query calls, so a service can refuse an unknown one before it starts.
An `Err` from the host is a 400 query error (`SqlError::udf`, code `SC2103`). This crate has no JavaScript: `azemu-cosmos` brings the engine
(`docs/services/cosmos-scripts.md`). A UDF in the query plan: the plan request only parses, and the rewritten query keeps the call as written; the
plan has no flag for UDFs (none exists in the SDKs' `QueryInfo`), so every SDK path works as for any other expression.

## Not supported (clear error, `ErrorKind::Unsupported` unless noted)

* User-defined functions are parsed and evaluated through a host the caller provides (see "User-defined functions" above); without a host `udf.name(..)` is
  `ErrorKind::Unsupported`.
* Spatial `ST_*` (Azure answers them; its distance on the ellipsoid could not be matched to the last digit), `FULLTEXT*` (Azure answers `FullTextContains` with no rows on a
  container without a full text policy, and `FullTextScore` outside `ORDER BY RANK` is `SC2240`), `ORDER BY RANK` (a syntax error).
* `COUNT(DISTINCT ..)`, `HAVING`, `IS NULL`/`IS NOT NULL` syntax, `UNION`, `CASE`, `CAST`, `CONVERT`, `CROSS`/`INNER JOIN`, `ON`: not in the language, syntax errors at the keyword (as on Azure).
* `has_non_streaming_order_by` is always false (no vector search ordering).
* Computed properties.
* Other `Applies to` pages not fetched: LINQ translation, pagination/continuation tokens (service concern).
* `REGEXMATCH` uses the Rust `regex` crate: no look-around, back-references, atomic groups or conditionals (Azure has them, with the `.NET`-like details below).
* Limits: queries nest as deep as Azure takes them (about 990 parentheses, about 500 nested calls, `SC1030` beyond); texts of more than 400 bytes, or with brackets more than twelve
  levels deep, run their recursive work on a thread with a 256 MiB stack (`lib.rs`, `big_stack`), so a 2 MiB worker thread is enough. The tests in `tests/limits.rs` use 10,000 levels of every kind.

## Checked against Azure in November 2026

A real account (API for NoSQL, serverless, Session consistency, one region) was asked about 1,500 queries through the Python SDK (`azure-cosmos` 4.17.1) and the raw REST interface for the plan; the
emulator answers them the same way now (`tests/sdk-python/cosmos/test_cosmos_query_azure_rules.py` and `cosmos_query_cases.json` hold 440 of them with the answers Azure gave, and run on
both). Settled by that:

* **ORDER BY** returns the documents that lack the field, whatever the indexing policy (default, explicit path, composite): undefined first ascending, last descending, ahead of null. Rows with equal keys
  come in creation order (reversed when all keys are DESC). Only a path is allowed (`2206`; JOIN paths are refused), and the path must be in the index (`The index path ... is excluded.`; indexing mode none
  is refused too). The details are in `cosmos.md`.
* **Aggregates**: `SUM` of no values is 0 (also in a subquery: `(SELECT VALUE SUM(a) FROM a IN c.arr)` is 0 for an empty array), `AVG`, `MIN` and `MAX` of none are undefined; `SUM` and `AVG` of a string, boolean, null, array or object
  are undefined; `MIN`/`MAX` over arrays or objects, or with one among the values, are undefined; `AVG` adds integers exactly and divides once. A select list that has an aggregate may only hold
  aggregates, constants, parameters, GROUP BY expressions (or paths below them): `Property reference 'c.id' is invalid in the select list because it is not contained in either an aggregate function or the GROUP BY clause.`
  (`SC2102`; `SC2103` for ORDER BY). An aggregate in WHERE is `SC2104`, one inside an aggregate `SC2101`, `SELECT *` with GROUP BY `SC2041`, an aggregate or subquery in GROUP BY `SC2111`, a GROUP BY expression with no property `SC2110`.
* **GROUP BY** with one expression and only `COUNT` to compute comes in ascending order of the key (undefined first); with several expressions, or with `SUM`, `AVG`, `MIN`, `MAX`, in the order the groups first appeared.
  **DISTINCT** over one value (`DISTINCT VALUE c.t`, `DISTINCT c.t`, one select item, an object literal) comes ascending; over several items, or a function in `VALUE`, in arrival order. A group of rows with an undefined key is its own group.
* **Scalar subqueries**: `(SELECT ...)` that can return several rows is refused when the query is compiled (`SC2201`), also when no document would give two: it has no FROM, or is an aggregate without GROUP BY, or reads only single values
  (`FROM c.o x`, not `x IN c.arr`). `TOP`, `OFFSET LIMIT` and `ORDER BY` are refused in any subquery (`SC2203`, `SC2204`, `SC2202`).
* **Numbers**: division by zero, `5 % 0`, `LOG(0)`, `SQRT(-1)`, `ACOS(2)`, `EXP(1000)`, an overflow: the query works inside, but an answer that holds NaN or an infinity is a 400, sub-status 4001
  (`The response content contains a numeric value that cannot be represented in JSON (e.g. Infinity and NaN).`); `1e400` is `SC1011`. `a[1.5]` reads `a[1]`.
* **Comparison**: values of different types are never equal (`=` is false, `!=` true) and have no order (undefined). The ternary takes the second branch for anything but `true`. A parameter that was not supplied is undefined (no error);
  names are case sensitive; a parameter name must start with `@` and a letter or `_` (`Invalid query. Specified parameter name 'x' is invalid...`), and none may repeat.
* **Syntax**: `--` comments; `.5` and `5.` are numbers; `SELECT *` without FROM is `SC1021`, `SC1002` at the end of the text, `SC1010` for a character that is no token (`;`, `#`, `` ` ``, a lone `@`), `SC1012` for a string that does not end;
  `SC1011` for a number that is not one; reserved words (see above); `udf` only in lower case; the member indexer takes a number, a string or a parameter (`c.a[c.i]` is `SC2990`); a duplicate name in a select list or an object literal
  is `SC2022`, a duplicate alias `SC2021`; a wrong number of arguments `SC2050` (`The UPPER function requires 1 argument(s).`); a TOP with OFFSET LIMIT `SC2205`; `TOP 1.5` `SC2060`; a count that is not a whole number from 0 up
  (or a parameter that is not) is `SC2060` (TOP), `SC2062` (OFFSET), `SC2061` (LIMIT); more than 10 joins `SC3002`; a LIKE `ESCAPE` that is not one character `SC2207`, and it must be a string literal; an unknown function `SC2005`.
* **Functions**: `SETINTERSECT` answers in the order of the second array when that one is smaller and all of it is in the first, else in the order of the first; `LASTINDEXOF(s, '')` is `length - 1`; `SUBSTRING` with a
  negative start is `""` (no clamping); `REPLACE` with an empty search string, and `LEFT`, `RIGHT`, `REPLICATE`, `SUBSTRING` with a fraction as a count, are undefined; `TOSTRING(1.5e-7)` is `1.5e-07`; `DATETIMEBIN` takes days and
  smaller units only (`mm` and `yyyy` are undefined); `DATETIMEFORMAT` with an empty result is undefined, and a run of `d` or `M` is one field (`dddd` is `15`); a UTC offset (`+05:30`, `-0800`, `+05`) is accepted in a date time and converted
  to UTC (`IS_DATETIME`, `DATETIMEPART`, `DATETIMEADD`, ...; offsets up to 14 hours); `DOCUMENTID(c)` is the number inside the `_rid` of the item; `VectorDistance(a, b [, brute, {distanceFunction}])` works for `cosine` (default),
  `dotproduct` and `euclidean` on arrays of numbers of the same length.
* **The query plan** (the `queryInfo` the SDKs read, taken with the Java feature list; see `cosmos.md` for the requests): MIN and MAX are `{"item": v, "item2": {"min": v, "count": n}}`, an empty partition `{"item2": {"count": 0}}`; AVG is
  `{"item": {"sum": s, "count": n}}` (`{"sum": 0, "count": 0}` when empty); `SELECT VALUE <aggregate> ... GROUP BY` has no aliases and names its operator in `aggregates`; a select list of aggregates (no VALUE) is
  `SELECT {"n": {"item": COUNT(1)}, ...} AS payload` with the aliases in `groupByAliases` and `aggregates` empty; ORDER BY plans `SELECT c._rid, [{"item": ...}] AS orderByItems, ... AS payload FROM c WHERE (<where> AND (<filter>)) ORDER BY <as written>`;
  DISTINCT is `Ordered` only when the distinct value is the one sorted by, and then the filter adds `IS_DEFINED(<value>)` and the partition sends every row; OFFSET LIMIT is `OFFSET 0 LIMIT <offset + limit>`.

## Behaviour that could not be confirmed against Azure (the documentation did not decide, or the account could not show it)

1. GROUP BY and DISTINCT order when the rule above does not apply (several expressions: "first appearance" was seen for two expressions and for the aggregates named; the
   DISTINCT rule between `VALUE <function>` and the other forms is a guess from a handful of queries). Cross partition, the SDK merges and the order is the SDK's.
2. `SUM`/`AVG` with strings, booleans, nulls: undefined (checked); with arrays and objects: undefined (checked for SUM; MIN/MAX over them undefined too, checked).
3. `x IN (...)`: undefined only when `x` is undefined; undefined list members never match; no match is `false`. `BETWEEN` across types is undefined (checked). `15 BETWEEN 'a' AND 10` is `false`.
4. `LIKE`: case-sensitive, `[` without a closing `]` is literal, `]` first in a set is a member (the sets were checked; a two-character `ESCAPE` is `SC2207`).
5. `REGEXMATCH` modifiers `i m s x`; any other modifier or an invalid pattern gives undefined (checked for `z` and an invalid pattern).
6. Date/time: `YYYY-MM-DD`, `YYYY-MM-DD[T ]hh:mm:ss[.f{1,7}]` with `Z` or a UTC offset; years 1 to 9999; `DATETIMEDIFF` counts boundaries crossed. Offsets beyond 14 hours are refused (a guess from `+99:00`).
7. `ROUND` rounds half away from zero (checked); `LOG(x, base)` is `ln x / ln base`; the last digit of `SIN`, `COS`, `TAN`, `COT`, `ACOS` can differ from Azure's by one unit.
8. `RAND()` uses a seeded xorshift generator (a random value cannot be compared).
9. A scalar subquery in `FROM` that starts at the container (`FROM (SELECT ...)`) is container-scoped and runs over the whole container once.
10. TOP, OFFSET and LIMIT accept counts up to 4,294,967,295 (Azure took 2,147,483,648 and refused 99,999,999,999 in the same place).
11. The text of the `where` in a rewritten query is the text as written; Azure writes it normalised (`(c.id = "none")`). The SDKs do not read it.
12. A literal that is an integer larger than 2^63 is a float (`SC1011` only when it is infinite).

## Known differences from Azure

* `ORDER BY` an array or an object field: Azure sorts, but not by value (the order looks hashed: `[1]`, `[2]`, `[1,2]`, `[]` came out as no value order), and the same for GROUP BY of arrays and objects. The emulator orders them
  element by element and objects by sorted key.
* `ORDER BY` with `GROUP BY` across partitions (Java/JavaScript plans): Azure plans it with the order items; the emulator's plan has `orderBy` but no `orderByItems` in the rows.
* `REGEXMATCH`: look-around, back-references, atomic groups, conditionals, `\Z` give undefined here; `\d` here matches Unicode digits and `$` here does not match before a final line break, Azure the other way round.
* `ST_*` and `FULLTEXT*` functions are refused; `ISNULL(x)` (an undocumented function Azure answers) is unknown here.
* `SIN`, `TAN`, `COT`, `ACOS` can differ in the last digit; `TOSTRING(-0.0)` is `0` here and `-0` on Azure.
* An AND chain of about 3,000 terms is `SC1030` on Azure (a chain of `+` terms of 3,000 is fine there), and a chain of 20,000 `+` made its gateway time out; both run or fail differently here.
* Azure lists every problem it finds in one error (`[SC2001, SC2001, SC2022]`); the emulator lists the first, and the extra errors it knows (`SC2042` next to `SC2021`, `SC2201` next to `SC2203`, `SC2110` next to `SC2111`).
  The position of a subquery in parentheses includes the parentheses here, and not on Azure.
