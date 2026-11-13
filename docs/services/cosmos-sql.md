# Cosmos DB SQL query engine (`azemu-cosmos-sql`)

**What it emulates.** The query language of Cosmos DB for NoSQL (`SELECT ... FROM c ... WHERE ... ORDER BY ... GROUP BY ... OFFSET LIMIT`, joins,
subqueries, aggregates, string, math, array, date and type functions) and the "query plan" the SDKs ask for before a cross-partition query.

**How to reach it.** Not by itself: the Cosmos service (`docs/services/cosmos.md`) runs queries with it, so every Cosmos endpoint and SDK query goes through it.

**Not built.** Spatial `ST_*`, `VectorDistance`, full text search, `DOCUMENTID`, `ORDER BY RANK`, computed properties; each answers a clear 400 "not supported".
The section "Behaviour that could not be confirmed against Azure" below lists every place where the documentation did not decide the behaviour and no real account was available.

A pure library (no HTTP, no async, no I/O, no `unsafe`) that parses and runs the Azure Cosmos DB
for NoSQL query language over `serde_json::Value` documents, and answers the questions a
gateway must answer for the SDK "query plan". The Cosmos service crate builds on it.

Reference: Microsoft Learn, "Query language in Cosmos DB" pages
(`learn.microsoft.com/azure/cosmos-db/nosql/query/*`, fetched 2026-11 for each page named in
the tests). No real Cosmos DB account was available: everything below is "as documented", and
the section "Behaviour that could not be confirmed against Azure" lists where the documentation did not decide the behaviour.

## Supported language

* `SELECT [DISTINCT] [TOP n|@n] * | VALUE expr | item [[AS] alias], ...`; unnamed items become `$1`, `$2`, ...
  (path expressions use their last name, as in `SELECT c.a.b` -> `b`).
* `FROM c`, `FROM root AS c`, `FROM c IN root.children`, `FROM c.address`, `FROM (SELECT ...) t`,
  `JOIN x IN c.arr`, `JOIN c.address a`, `JOIN (SELECT VALUE ...) [AS] s`; any number of joins.
* `WHERE`, `GROUP BY`, `ORDER BY expr [ASC|DESC], ...`, `OFFSET n LIMIT m` (literals or parameters).
* Expressions: property paths, `c["a b"]`, `c.arr[0]`, arithmetic, comparison, `AND OR NOT`,
  bitwise `& | ^ ~ << >> >>>` (JavaScript 32-bit semantics), `||`, `?:`, `??`, `IN`, `NOT IN`,
  `BETWEEN`, `LIKE` (`%`, `_`, `[abc]`, `[a-f]`, `[^abc]`, `ESCAPE`), `EXISTS(...)`, `ARRAY(...)`,
  scalar subqueries, parameters, array/object literals (also unquoted keys), `NaN`, `Infinity`.
* Constants: `undefined null true false`, integers, decimals, exponents, hex (`0x1F`), strings with
  `'` or `"` and the documented escapes (`\uXXXX`, surrogate pairs).
* Aggregates `COUNT SUM AVG MIN MAX`, with or without GROUP BY, also inside expressions and subqueries.
* Keywords and function names are case-insensitive, property names and aliases case-sensitive.
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
  integer (`10/5` is `2`); integer overflow falls back to double; `NaN`/`Infinity` exist inside a query
  and become `null` in output.
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

`udf.name(args)` parses to `ExprKind::Udf` anywhere an expression may stand (SELECT, WHERE, ORDER BY, GROUP BY, subqueries; the prefix `udf` is
not case sensitive, the function name is). The evaluator calls `UdfHost::call(name, args)` once per evaluation of the call, with the arguments as
JSON (`None` is an undefined argument) and takes `Option<Value>` back (`None` is undefined, so the row drops out of `SELECT VALUE` like any
undefined). `EvalOptions::udf` holds a `UdfFactory` (`Send + Sync`) that makes the host on the thread that runs the query, because a script engine
cannot move between threads. `Query::udf_names()` lists the functions a query calls, so a service can refuse an unknown one before it starts.
An `Err` from the host is a 400 query error (`SqlError::udf`, code `SC2103`). This crate has no JavaScript: `azemu-cosmos` brings the engine
(`docs/services/cosmos-scripts.md`). A UDF in the query plan: the plan request only parses, and the rewritten query keeps the call as written; the
plan has no flag for UDFs (none exists in the SDKs' `QueryInfo`), so every SDK path works as for any other expression.

## Not supported (clear error, `ErrorKind::Unsupported` unless noted)

* User-defined functions are parsed and evaluated through a host the caller provides (see "User-defined functions" below); without a host `udf.name(..)` is
  `ErrorKind::Unsupported`, as before.
* Spatial `ST_*`, `VECTORDISTANCE`, `FULLTEXT*`, `RRF`, `ORDER BY RANK` (the latter is a syntax error).
* `DOCUMENTID` (needs the internal id of the physical partition).
* `COUNT(DISTINCT ..)`, `HAVING`, `IS NULL`/`IS NOT NULL` syntax, `UNION`, `CASE`, `CAST`: not in the
  documented dialect, so they are syntax errors. Use `IS_NULL()`, subqueries, `IIF`.
* `has_non_streaming_order_by` is always false (no vector search).
* Computed properties, `TOP` together with `OFFSET LIMIT` is rejected (a guess that matches T-SQL; unverified on Cosmos).
* Other `Applies to` pages not fetched: LINQ translation, pagination/continuation tokens (service concern).
* At most 64 sources in FROM (the first plus 63 JOINs; Azure documents 10): the evaluator walks the sources recursively, so the parser refuses more (`ErrorKind::LimitExceeded`; a 64 source query with one row each runs on a 2 MiB stack in `tests/limits.rs`).
* Limits (tested with 10,000 levels of every kind on a 2 MiB stack, `tests/limits.rs`): expressions nest up to 24 levels (parentheses, subqueries, brackets, unary operators, function
  arguments) and expression trees are at most 100 high, so a left-associative chain such as `1 + 1 + ...`
  is limited to about 100 terms (`AND`/`OR` chains are flat and unlimited; a `??` chain is right associative and is built in a loop, limited to about 100 terms the same way). These numbers keep the
  recursion inside a 2 MB thread stack of a debug build (measured). Exceeding them gives
  `ErrorKind::LimitExceeded`.

## Behaviour that could not be confirmed against Azure (the documentation did not decide, or no real server was available)

1. ORDER BY with undefined values: excluded by default (`order_by_excludes_undefined = true`); with `false` they sort
   first (ascending) / last (descending). Settled from Microsoft Learn, "Indexing policies": Azure
   serves ORDER BY from the index; under the default policy (`/*`) an item without the property is not in the index
   of that path, so it is left out; "If an explicitly indexed path doesn't exist in an item, a value is added to the
   index to indicate that the path is undefined" (also for composite indexes), so such items are returned when the path
   is indexed explicitly and when several fields are sorted (a composite index is required). The library only has the
   switch; the service (`azemu-cosmos`, `doc_query.rs`) sets it per query from the container's indexing policy. Not run
   against a real account or the vnext image.
2. ORDER BY on arrays and objects is allowed here with a total order (arrays element-wise, objects by sorted
   key then value). Real Cosmos may reject or order them differently. Mixed-type order undefined < null <
   boolean < number < string < array < object is as documented.
3. Group order of GROUP BY and DISTINCT results: first appearance. Real order is unspecified.
4. A group of rows with an undefined key is its own group (shows as `{}`) as in the group-by page's example.
5. Aggregate without GROUP BY next to non-aggregate select items: those items are evaluated on the first row.
6. SUM/AVG: a string, boolean, null, array or object among the values gives undefined (documented for
   string/boolean/null; arrays and objects assumed). MIN/MAX use the type order; MIN over `[null, 5]` is `null`.
   SUM of integers stays an integer until it overflows.
7. Scalar subquery returning several rows: the first row is used. Zero rows: undefined.
8. `x IN (...)`: undefined only when `x` is undefined; undefined list members never match; no match is
   `false`. `BETWEEN` is `x >= lo AND x <= hi` with the three-valued AND, so `15 BETWEEN 'a' AND 10` is `false`.
9. `LIKE`: case-sensitive, `ESCAPE` must be one character (else undefined), a `[` without a closing `]` is
   literal, `]` first in a set is a member. Matching is by Unicode scalar value.
10. `REGEXMATCH` uses the Rust `regex` crate (no look-around or back-references; .NET/PCRE syntax differences
    are possible). Modifiers `i m s x` are supported; any other modifier or an invalid pattern gives undefined.
11. Date/time: only `YYYY-MM-DD`, `YYYY-MM-DD[T ]hh:mm:ss[.f{1,7}]` with an optional trailing `Z` are
    accepted, as in the `IS_DATETIME` page; time zone offsets are not. Years 1 to 9999. `DATETIMEDIFF`
    counts boundaries crossed (floor division of ticks per unit; years and months by calendar fields).
    `DATETIMEADD` of months clamps the day. `DATETIMEBIN` for month/year uses the calendar from the bin start.
    `NOW`/`AGO` accept `("dd", n)` and `("3d")` forms. `DATETIMEFORMAT` follows the specifier table of its page
    (unlisted characters are copied). Non-static current-time functions use the fixed clock when one is set.
12. `ROUND` rounds half away from zero; with a precision it scales by a power of ten in floating point, so
    values like `2.675` may round like the binary double does. `LOG(x, base)` is `ln x / ln base`.
13. Number to text (`TOSTRING`) uses `serde_json`/Rust formatting (for example `1e+20` where Cosmos may print
    `1E+20`).
14. `ARRAY_CONTAINS(..., true)` partial match is applied recursively to nested objects.
15. `SETINTERSECT` returns the elements in the order of the second array (matches the doc example
    `SETINTERSECT([1,2,"A","B"], ["A",1])` = `["A",1]`); `SETUNION`/`SETDIFFERENCE` keep first-seen order.
16. `STRINGSPLIT` with an empty separator returns the whole string as one element (as documented);
    `LASTINDEXOF` with an empty needle returns the string length.
17. `RAND()` uses a seeded xorshift generator; each execution seeds from the clock unless `rand_seed` is set.
18. Error codes other than `SC1001` (syntax) are invented: `SC2001` unresolved identifier, `SC2002` duplicate alias,
    `SC2005` unknown function, `SC2008` wrong argument count, `SC2102`/`SC2104` aggregate misplaced or nested,
    `SC2103` bad `SELECT *`, `SC2100` evaluation (missing parameter, bad TOP/OFFSET), `SC2101` limit, `SC9999`
    unsupported. Positions are in characters and point at the offending token; for end of input the position
    is the end of the text. The message text of `SC1001` is `Syntax error, incorrect syntax near '<token>'.`
    Real Cosmos messages for the other cases differ.
19. The text of the rewritten queries is modeled on what the gateway sends (from memory of the SDK protocol and
    the SDK code); only the result shapes were checked against SDK code. `{documentdb-formattableorderbyquery-filter}`
    appears in the WHERE of ORDER BY rewrites. `_rid` is selected so documents carrying it round-trip.
20. Documentation errors worked around: the `CONTAINS_ALL_CS` page shows `case1: true` for a case-sensitive
    search of `"have"` in `"Have a nice day!"` (a copy of the `_CI` page); the emulator returns `false`, which the
    function's own definition requires. Other doc examples (113 more) pass unchanged, except that the
    floating-point tests compare with a relative tolerance of 1e-12 (last-digit differences in `EXP`, `LOG`,
    `COT`, `TAN` between the docs and Rust's libm).
21. `FROM c.address a` style sources use the value as is (no iteration); `FROM x IN c.arr` iterates only
    arrays; a path that is undefined skips the document.
22. A scalar subquery in `FROM` that starts at the container (`FROM (SELECT ...)`) is container-scoped and
    runs over the whole container once.
