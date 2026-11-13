//! Running a query: read records, filter, project, write results, and frame everything as the Avro answer.
//!
//! Nothing here holds the state lock: the job owns shared handles to the blob's data. Work is bounded by
//! [`Limits`] (record size, result size, time, errors), so a blob or a query cannot use the machine up.

use std::time::Duration;

use azemu_core::CancelToken;

use super::avro::{Message, Writer};
use super::budget::{Budget, CANCELLED};
use super::datetime::Ts;
use super::eval::{eval, is_true, Acc, Columns, Ctx, Row};
use super::input::{next_json_record, Stream};
use super::json;
use super::output::{self, OutRow};
use super::request::{InputFormat, OutputFormat, Plan};
use super::sql::{Expr, FromStep, Projection, Select, Step};
use super::value::{EvalError, Value};
use crate::state::Piece;

/// What one query may use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// A blob larger than this is refused before anything is read.
    pub max_blob_bytes: u64,
    /// A record (a line of CSV, a JSON document) larger than this ends the query with a fatal error.
    pub max_record_bytes: usize,
    /// Result data goes out in records of at most this many bytes, with a progress message after each.
    pub data_chunk_bytes: usize,
    /// The whole answer may be this large; past it the query ends with a fatal error.
    pub max_answer_bytes: usize,
    /// Wall time for the scan.
    pub time_budget: Duration,
    /// Non-fatal errors reported; one more ends the query.
    pub max_errors: usize,
    /// Queries that run at the same time.
    pub max_concurrent: usize,
    /// How long a query waits for a place before it is refused with `503 ServerBusy`.
    pub queue_wait: Duration,
}

impl Limits {
    /// The defaults, changed by `AZEMU_BLOB_QUERY_MAX_BLOB_BYTES`, `AZEMU_BLOB_QUERY_MAX_RECORD_BYTES`,
    /// `AZEMU_BLOB_QUERY_MAX_ANSWER_BYTES`, `AZEMU_BLOB_QUERY_SECONDS`, `AZEMU_BLOB_QUERY_MAX_ERRORS`,
    /// `AZEMU_BLOB_QUERY_MAX_CONCURRENT` (read through `azemu_core::settings`: a value that is not valid is reported
    /// once and the default is used).
    pub fn from_env() -> Limits {
        use azemu_core::settings;
        settings::describe("AZEMU_BLOB_QUERY_MAX_BLOB_BYTES", "256M", "largest blob a Query Blob Contents request will scan");
        settings::describe("AZEMU_BLOB_QUERY_MAX_RECORD_BYTES", "4M", "largest single record (CSV row or JSON object) a query accepts");
        settings::describe(
            "AZEMU_BLOB_QUERY_MAX_ANSWER_BYTES",
            "128M",
            "largest answer of one query; past it the query ends with a fatal error",
        );
        settings::describe("AZEMU_BLOB_QUERY_SECONDS", "30 s", "time one query may scan before it ends with a fatal error");
        settings::describe("AZEMU_BLOB_QUERY_MAX_ERRORS", "1000", "non-fatal errors a query reports before it ends");
        settings::describe(
            "AZEMU_BLOB_QUERY_MAX_CONCURRENT",
            "4",
            "queries that run at the same time; further ones wait, then get 503 ServerBusy",
        );
        let d = Limits::default();
        let size = |n: u64| n.min(usize::MAX as u64) as usize;
        Limits {
            max_blob_bytes: settings::bytes("AZEMU_BLOB_QUERY_MAX_BLOB_BYTES", d.max_blob_bytes),
            max_record_bytes: size(settings::bytes("AZEMU_BLOB_QUERY_MAX_RECORD_BYTES", d.max_record_bytes as u64)),
            max_answer_bytes: size(settings::bytes("AZEMU_BLOB_QUERY_MAX_ANSWER_BYTES", d.max_answer_bytes as u64)),
            time_budget: settings::duration("AZEMU_BLOB_QUERY_SECONDS", d.time_budget),
            max_errors: size(settings::number("AZEMU_BLOB_QUERY_MAX_ERRORS", d.max_errors as u64)),
            max_concurrent: settings::number("AZEMU_BLOB_QUERY_MAX_CONCURRENT", d.max_concurrent as u64).min(1024) as usize,
            ..d
        }
    }
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_blob_bytes: 256 * 1024 * 1024,
            max_record_bytes: 4 * 1024 * 1024,
            data_chunk_bytes: 256 * 1024,
            max_answer_bytes: 128 * 1024 * 1024,
            time_budget: Duration::from_secs(30),
            max_errors: 1000,
            max_concurrent: 4,
            queue_wait: Duration::from_secs(10),
        }
    }
}

struct Fatal {
    name: String,
    description: String,
    position: u64,
}

impl Fatal {
    fn new(name: &str, description: impl Into<String>, position: u64) -> Fatal {
        Fatal { name: name.to_string(), description: description.into(), position }
    }

    /// Nobody is waiting for the answer any more: stop without writing anything more.
    fn cancelled() -> Fatal {
        Fatal::new(CANCELLED, "", 0)
    }

    fn is_cancelled(&self) -> bool {
        self.name == CANCELLED
    }
}

/// Where finished parts of the answer go. Returns false when nobody takes them any more.
type Out<'a> = Box<dyn FnMut(Vec<u8>) -> bool + 'a>;

/// Builds the answer: result data in bounded records, errors, progress, end. The answer is handed to `out` in pieces as it
/// is made (each time a full data record and its progress message are written), so only about one piece is in memory.
struct Sink<'a> {
    out: Out<'a>,
    /// Bytes already handed to `out`.
    sent: usize,
    w: Writer,
    data: Vec<u8>,
    errors: Vec<(String, String, u64)>,
    total: u64,
    sent_data: bool,
    chunk: usize,
    max_answer: usize,
    max_errors: usize,
    reported: usize,
}

impl Sink<'_> {
    /// Hand what is written so far to `out`.
    fn push(&mut self) -> Result<(), Fatal> {
        let part = std::mem::take(&mut self.w.out);
        self.sent += part.len();
        if (self.out)(part) {
            Ok(())
        } else {
            Err(Fatal::cancelled())
        }
    }

    fn flush_errors(&mut self) {
        for (name, description, position) in std::mem::take(&mut self.errors) {
            self.w.write(&Message::Error { fatal: false, name: &name, description: &description, position: position as i64 });
        }
    }

    /// Cut full records off the pending data. `scanned` is the blob offset reached.
    fn drain(&mut self, scanned: u64) -> Result<(), Fatal> {
        while self.data.len() >= self.chunk {
            let rest = self.data.split_off(self.chunk);
            let part = std::mem::replace(&mut self.data, rest);
            self.w.write(&Message::Data(&part));
            self.sent_data = true;
            self.flush_errors();
            self.w.write(&Message::Progress { bytes_scanned: scanned as i64, total_bytes: self.total as i64 });
            self.push()?;
        }
        if self.sent + self.w.len() + self.data.len() > self.max_answer {
            return Err(Fatal::new("ResultTooLarge", format!("The result is larger than {} bytes.", self.max_answer), scanned));
        }
        Ok(())
    }

    /// The header line of the output is a data record of its own (as in the recorded answers).
    fn header(&mut self, line: &[u8]) {
        self.w.write(&Message::Data(line));
        self.sent_data = true;
    }

    fn error(&mut self, name: &str, description: String, position: u64) -> Result<(), Fatal> {
        self.reported += 1;
        if self.reported > self.max_errors {
            return Err(Fatal::new("TooManyErrors", format!("More than {} records could not be processed.", self.max_errors), position));
        }
        self.errors.push((name.to_string(), description, position));
        Ok(())
    }

    /// Everything pending, a fatal error if there is one, the closing progress and the end message.
    fn finish(mut self, fatal: Option<Fatal>) {
        if fatal.as_ref().is_some_and(Fatal::is_cancelled) {
            return;
        }
        if !self.data.is_empty() || !self.sent_data {
            let d = std::mem::take(&mut self.data);
            self.w.write(&Message::Data(&d));
        }
        self.flush_errors();
        if let Some(f) = fatal {
            self.w.write(&Message::Error { fatal: true, name: &f.name, description: &f.description, position: f.position as i64 });
        }
        let t = self.total as i64;
        self.w.write(&Message::Progress { bytes_scanned: t, total_bytes: t });
        self.w.write(&Message::End { total_bytes: t });
        let _ = self.push();
    }
}

/// The name of an output column: the alias, else the name of the field, else `_` and the position.
fn column_name(i: usize, item: &super::sql::Item, cols: &Columns) -> String {
    if let Some(a) = &item.alias {
        return a.clone();
    }
    if let Expr::Ref(steps) = &item.expr {
        match steps.last() {
            Some(Step::Ordinal(n)) if steps.len() == 1 => return cols.names.get(n - 1).cloned().unwrap_or_else(|| format!("_{n}")),
            Some(Step::Field { name, quoted }) => {
                if steps.len() == 1 {
                    if let Some(idx) = cols.find(name, *quoted) {
                        return cols.names[idx].clone();
                    }
                }
                return name.clone();
            }
            _ => {}
        }
    }
    format!("_{}", i + 1)
}

fn path_rows(root: Value, steps: &[FromStep]) -> Vec<Value> {
    let mut cur = vec![root];
    for s in steps {
        let mut next = Vec::new();
        for v in cur {
            match (s, v) {
                (FromStep::Field(n), Value::Object(o)) => {
                    let hit = o.iter().position(|(k, _)| k == n).or_else(|| o.iter().position(|(k, _)| k.eq_ignore_ascii_case(n)));
                    if let Some(i) = hit {
                        next.push(o.into_iter().nth(i).map(|(_, v)| v).unwrap_or(Value::Missing));
                    }
                }
                (FromStep::All, Value::Array(a)) => next.extend(a),
                (FromStep::All, other) => next.push(other),
                (FromStep::Index(i), Value::Array(mut a)) if *i < a.len() => next.push(a.swap_remove(*i)),
                _ => {}
            }
        }
        cur = next;
    }
    cur
}

struct Runner<'a> {
    sel: &'a Select,
    plan: &'a Plan,
    budget: &'a Budget,
    cols: Columns,
    now: Ts,
    names: Vec<String>,
    accs: Vec<(Acc, Option<&'a Expr>)>,
    emitted: u64,
    wrote_header: bool,
    scratch: Vec<u8>,
}

impl<'a> Runner<'a> {
    /// A row that failed: a non-fatal error record, or the end of the query.
    fn failed(&self, e: EvalError, index: usize, position: u64, sink: &mut Sink<'_>) -> Result<(), Fatal> {
        let message = if e.name == "InvalidColumnOrdinal" { format!("Column ordinal {index} does not exist in row.") } else { e.message };
        if e.fatal {
            return Err(Fatal::new(e.name, message, position));
        }
        sink.error(e.name, message, position)
    }

    fn emit(&mut self, row: OutRow, sink: &mut Sink<'_>, scanned: u64) -> Result<(), Fatal> {
        self.scratch.clear();
        match &self.plan.output {
            OutputFormat::Delimited(c) => {
                if c.headers && !self.wrote_header {
                    output::csv_header(c, &output::names_of(&row), &mut self.scratch);
                    sink.header(&self.scratch);
                    self.scratch.clear();
                }
                self.wrote_header = true;
                output::csv_row(c, &row, &mut self.scratch)
            }
            OutputFormat::Json { record } => output::json_row(record, &row, &mut self.scratch),
        }
        sink.data.extend_from_slice(&self.scratch);
        self.emitted += 1;
        sink.drain(scanned)
    }

    /// One input row through WHERE and the projection. `Ok(true)` when LIMIT is reached.
    fn row(&mut self, row: Row, position: u64, sink: &mut Sink<'_>, scanned: u64) -> Result<bool, Fatal> {
        if self.sel.limit == Some(0) && !self.sel.aggregate {
            return Ok(true);
        }
        let ctx = Ctx { row: &row, columns: &self.cols, now: self.now, budget: self.budget };
        if let Some(f) = &self.sel.filter {
            match eval(f, &ctx).and_then(|v| is_true(&v)) {
                Ok(true) => {}
                Ok(false) => return Ok(false),
                Err(e) => {
                    self.failed(e, 0, position, sink)?;
                    return Ok(false);
                }
            }
        }
        if self.sel.aggregate {
            for (i, (acc, arg)) in self.accs.iter_mut().enumerate() {
                let v = match arg {
                    Some(x) => match eval(x, &ctx) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            let fatal = e.fatal;
                            let message = e.message.clone();
                            let name = e.name;
                            if fatal {
                                return Err(Fatal::new(name, message, position));
                            }
                            sink.error(name, message, position)?;
                            continue;
                        }
                    },
                    None => None,
                };
                if let Err(e) = acc.add(v.as_ref()) {
                    if e.fatal {
                        return Err(Fatal::new(e.name, e.message, position));
                    }
                    sink.error(e.name, e.message, position)?;
                }
                let _ = i;
            }
            return Ok(false);
        }
        let out = match &self.sel.projection {
            Projection::Star => match &row {
                Row::Csv(f) => {
                    let names =
                        if self.cols.names.is_empty() { (1..=f.len()).map(|i| format!("_{i}")).collect() } else { self.cols.names.clone() };
                    OutRow::Fields { names, values: f.iter().map(|s| Value::Str(s.clone())).collect() }
                }
                Row::Json(v) => OutRow::Whole(v.clone()),
            },
            Projection::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for (i, it) in items.iter().enumerate() {
                    match eval(&it.expr, &ctx) {
                        Ok(v) => values.push(v),
                        Err(e) => {
                            self.failed(e, i, position, sink)?;
                            return Ok(false);
                        }
                    }
                }
                OutRow::Fields { names: self.names.clone(), values }
            }
        };
        self.emit(out, sink, scanned)?;
        Ok(self.sel.limit.map(|l| self.emitted >= l).unwrap_or(false))
    }
}

/// Run a parsed request over the blob's data and return the whole Avro answer (tests and small uses; the server
/// uses [`run_streaming`]).
#[cfg(test)]
pub fn run(plan: &Plan, pieces: Vec<Piece>, limits: &Limits) -> Vec<u8> {
    let mut answer = Vec::new();
    run_streaming(plan, pieces, limits, &CancelToken::never(), |part| {
        answer.extend_from_slice(&part);
        true
    });
    answer
}

/// Run a parsed request over the blob's data and hand the Avro answer to `out` in pieces as it is made. `out` returns
/// false when nobody takes the answer any more; the query stops then, and also when `cancel` fires or the time limit passes
/// (the limit is looked at inside records, expressions and pattern matching, see `Budget`).
pub fn run_streaming(plan: &Plan, pieces: Vec<Piece>, limits: &Limits, cancel: &CancelToken, out: impl FnMut(Vec<u8>) -> bool) {
    let budget = Budget::new(limits.time_budget, cancel.clone());
    let mut stream = Stream::new(pieces);
    let mut sink = Sink {
        out: Box::new(out),
        sent: 0,
        w: Writer::new(),
        data: Vec::new(),
        errors: Vec::new(),
        total: stream.total(),
        sent_data: false,
        chunk: limits.data_chunk_bytes.max(1),
        max_answer: limits.max_answer_bytes,
        max_errors: limits.max_errors,
        reported: 0,
    };
    let fatal = match &plan.select {
        Err(e) => Some(Fatal::new("ParseError", format!("{} (position {})", e.message, e.pos), 0)),
        Ok(sel) => scan(plan, sel, &mut stream, &mut sink, limits, &budget).err(),
    };
    sink.finish(fatal);
}

fn scan(plan: &Plan, sel: &Select, stream: &mut Stream, sink: &mut Sink<'_>, limits: &Limits, budget: &Budget) -> Result<(), Fatal> {
    let mut r = Runner {
        sel,
        plan,
        budget,
        cols: Columns::default(),
        now: Ts::utc_now(),
        names: Vec::new(),
        accs: Vec::new(),
        emitted: 0,
        wrote_header: false,
        scratch: Vec::new(),
    };
    let read_err = |e: super::input::ReadError| Fatal::new(e.name, e.message, e.position);
    // every record counts as work (more for a big one), so the time limit and the cancellation are looked at between records
    // as well as inside them (`Budget` is also used by every expression evaluated)
    let mut last = stream.offset();
    let mut tick = |scanned: u64| -> Result<(), Fatal> {
        let units = 1 + scanned.saturating_sub(last) / 4096;
        last = scanned;
        budget.work(units).map_err(|e| Fatal::new(e.name, e.message, scanned))
    };
    budget.check().map_err(|e| Fatal::new(e.name, e.message, 0))?;

    let mut header_names: Option<Vec<String>> = None;
    if let InputFormat::Delimited(c) = &plan.input {
        if c.headers {
            if let Some((_, names)) = c.next(stream, limits.max_record_bytes).map_err(read_err)? {
                header_names = Some(names);
            }
        }
    }
    if let Some(names) = header_names {
        r.cols = Columns::new(names);
    }
    // the output column names are known now for a select list
    if let Projection::List(items) = &sel.projection {
        r.names = items.iter().enumerate().map(|(i, it)| column_name(i, it, &r.cols)).collect();
        if sel.aggregate {
            r.accs = items
                .iter()
                .filter_map(|it| match &it.expr {
                    Expr::Agg(f, arg) => Some((Acc::new(*f), arg.as_deref())),
                    _ => None,
                })
                .collect();
        }
    }
    // a header line is written even when no row follows, when its names are known
    if let OutputFormat::Delimited(c) = &plan.output {
        let known = match (&sel.projection, sel.aggregate) {
            (Projection::List(_), _) => Some(r.names.clone()),
            (Projection::Star, _) if matches!(plan.input, InputFormat::Delimited(_)) && !r.cols.names.is_empty() => {
                Some(r.cols.names.clone())
            }
            _ => None,
        };
        if c.headers {
            if let Some(names) = known {
                let mut line = Vec::new();
                output::csv_header(c, &names, &mut line);
                sink.header(&line);
                r.wrote_header = true;
            }
        }
    }

    match &plan.input {
        InputFormat::Delimited(c) => {
            let mut done = false;
            while !done {
                let Some((pos, fields)) = c.next(stream, limits.max_record_bytes).map_err(read_err)? else { break };
                tick(stream.offset())?;
                done = r.row(Row::Csv(fields), pos, sink, stream.offset())?;
            }
        }
        InputFormat::Json { record } => {
            let mut done = false;
            while !done {
                let Some((pos, bytes)) = next_json_record(stream, record, limits.max_record_bytes).map_err(read_err)? else { break };
                tick(stream.offset())?;
                let value = json::parse(&bytes).map_err(|m| Fatal::new("ParseError", m, pos))?;
                for v in path_rows(value, &sel.from) {
                    if r.row(Row::Json(v), pos, sink, stream.offset())? {
                        done = true;
                        break;
                    }
                }
            }
        }
    }
    if sel.aggregate {
        let values: Vec<Value> = r.accs.iter().map(|(a, _)| a.result()).collect();
        let names = r.names.clone();
        r.emit(OutRow::Fields { names, values }, sink, stream.total())?;
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::super::avro::tests::{read, Msg};
    use super::super::request;
    use super::*;
    use std::time::Duration;

    pub fn request(expr: &str, input: &str, output: &str) -> Plan {
        let body = format!(
            "<?xml version='1.0' encoding='utf-8'?>\n<QueryRequest><QueryType>SQL</QueryType><Expression>{}</Expression>{input}{output}</QueryRequest>",
            expr.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        );
        request::parse(body.as_bytes()).unwrap()
    }

    pub fn query(expr: &str, input: &str, output: &str, data: &[u8]) -> (Vec<Msg>, Vec<u8>) {
        query_with(expr, input, output, data, &Limits::default())
    }

    pub fn query_with(expr: &str, input: &str, output: &str, data: &[u8], limits: &Limits) -> (Vec<Msg>, Vec<u8>) {
        let plan = request(expr, input, output);
        let file = run(&plan, vec![Piece::Data(azemu_core::bytes::Bytes::copy_from_slice(data))], limits);
        let (_, _, msgs) = read(&file);
        let mut all = vec![];
        for m in &msgs {
            if let Msg::Data(d) = m {
                all.extend_from_slice(d);
            }
        }
        (msgs, all)
    }

    pub fn csv_cfg(col: &str, quote: &str, rec: &str, esc: &str, headers: bool, which: &str) -> String {
        format!(
            "<{which}Serialization><Format><Type>delimited</Type><DelimitedTextConfiguration><ColumnSeparator>{col}</ColumnSeparator><FieldQuote>{quote}</FieldQuote><RecordSeparator>{rec}</RecordSeparator><EscapeChar>{esc}</EscapeChar><HasHeaders>{headers}</HasHeaders></DelimitedTextConfiguration></Format></{which}Serialization>"
        )
    }

    pub fn json_cfg(rec: &str, which: &str) -> String {
        format!("<{which}Serialization><Format><Type>json</Type><JsonTextConfiguration><RecordSeparator>{rec}</RecordSeparator></JsonTextConfiguration></Format></{which}Serialization>")
    }

    const CSV: &[u8] = include_bytes!("testdata/readall.csv");

    #[test]
    fn select_star_on_the_recorded_blob_gives_the_recorded_result() {
        let (msgs, data) = query("SELECT * from BlobStorage", "", "", CSV);
        // data of the recording (readall): the lines with \n instead of \r\n
        let (_, _, rec) = read(include_bytes!("testdata/readall.avro"));
        let Msg::Data(want) = &rec[0] else { panic!() };
        assert_eq!(data, *want);
        assert_eq!(msgs.len(), 3, "one data record, progress, end: {msgs:?}");
        assert_eq!(&msgs[1..], &rec[1..], "progress and end as recorded");
    }

    #[test]
    fn the_recorded_output_settings() {
        // the custom output dialect of test_quick_query_readall_with_serialization_setting
        let (_, rec) = {
            let (_, _, m) = read(include_bytes!("testdata/custom_out.avro"));
            let Msg::Data(d) = &m[0] else { panic!() };
            (m.clone(), d.clone())
        };
        let (msgs, data) = query(
            "SELECT * from BlobStorage",
            &csv_cfg(",", "\"", "\n", "", false, "Input"),
            &csv_cfg(";", "'", ".", "\\", false, "Output"),
            CSV,
        );
        assert_eq!(data, rec);
        assert_eq!(msgs.last(), Some(&Msg::End(2256)));
        // with headers in the output the header line is a data record of its own (headers_out.avro)
        let (_, _, m) = read(include_bytes!("testdata/headers_out.avro"));
        let (msgs, _) = query(
            "SELECT * from BlobStorage",
            &csv_cfg(",", "\"", "\n", "", true, "Input"),
            &csv_cfg(",", "\"", "\n", "", true, "Output"),
            CSV,
        );
        assert_eq!(msgs[0], m[0]);
        assert_eq!(msgs[1], m[1]);
        assert_eq!(msgs.len(), 4);
        // without them the header row is dropped
        let (_, data) = query(
            "SELECT * from BlobStorage",
            &csv_cfg(",", "\"", "\n", "", true, "Input"),
            &csv_cfg(",", "\"", "\n", "", false, "Output"),
            CSV,
        );
        let Msg::Data(with) = &m[1] else { panic!() };
        assert_eq!(&data, with);
    }

    #[test]
    fn a_missing_column_is_a_non_fatal_error_at_the_record() {
        let (_, _, rec) = read(include_bytes!("testdata/nonfatal.avro"));
        let (msgs, data) = query(
            "SELECT RepoPath from BlobStorage",
            &csv_cfg(",", "\"", "\n", "", true, "Input"),
            &csv_cfg(";", "'", ".", "\\", false, "Output"),
            CSV,
        );
        let Msg::Data(want) = &rec[0] else { panic!() };
        assert_eq!(&data, want);
        assert_eq!(msgs[1], rec[1], "InvalidColumnOrdinal at offset 118, as recorded");
        assert_eq!(&msgs[2..], &rec[2..]);
    }

    fn kinds(m: &[Msg]) -> String {
        m.iter()
            .map(|m| match m {
                Msg::Data(_) => 'D',
                Msg::Error { fatal: true, .. } => 'F',
                Msg::Error { .. } => 'e',
                Msg::Progress(..) => 'P',
                Msg::End(_) => 'E',
            })
            .collect()
    }

    fn small() -> Limits {
        Limits { data_chunk_bytes: 100, ..Limits::default() }
    }

    #[test]
    fn results_come_in_bounded_records_with_progress_between() {
        let rows: String = (0..200).map(|i| format!("row{i:03},x\n")).collect();
        let (msgs, data) = query_with("SELECT * FROM BlobStorage", "", "", rows.as_bytes(), &small());
        assert_eq!(data, rows.as_bytes());
        let mut last = 0;
        let mut datas = 0;
        for m in &msgs {
            match m {
                Msg::Data(d) => {
                    datas += 1;
                    assert!(d.len() <= 100, "a data record of {} bytes", d.len());
                }
                Msg::Progress(scanned, total) => {
                    assert_eq!(*total, rows.len() as i64);
                    assert!(*scanned >= last && *scanned <= *total, "progress goes forward: {scanned} after {last}");
                    last = *scanned;
                }
                _ => {}
            }
        }
        assert_eq!(datas, rows.len().div_ceil(100), "{datas} data records");
        // after every full data record comes a progress message; the end is last, preceded by the closing progress
        let k = kinds(&msgs);
        assert!(k.starts_with("DPDPDP") && k.ends_with("PE"), "{k}");
        assert_eq!(msgs.last(), Some(&Msg::End(rows.len() as i64)));
        assert_eq!(msgs[msgs.len() - 2], Msg::Progress(rows.len() as i64, rows.len() as i64));
    }

    #[test]
    fn the_same_answer_from_a_blob_in_many_pieces() {
        let rows: String = (0..500).map(|i| format!("\"q,{i}\",{i}\r\n")).collect();
        let plan = request("SELECT _2, _1 FROM BlobStorage WHERE CAST(_2 AS INT) % 7 = 0", "", "");
        let whole = vec![Piece::Data(azemu_core::bytes::Bytes::from(rows.clone()))];
        let cut: Vec<Piece> = rows.as_bytes().chunks(13).map(|c| Piece::Data(azemu_core::bytes::Bytes::copy_from_slice(c))).collect();
        let data = |p: Vec<Piece>| {
            let (_, _, m) = read(&run(&plan, p, &Limits::default()));
            m.into_iter().filter_map(|m| if let Msg::Data(d) = m { Some(d) } else { None }).flatten().collect::<Vec<u8>>()
        };
        let (a, b) = (data(whole), data(cut));
        assert!(!a.is_empty());
        assert_eq!(a, b);
    }

    #[test]
    fn a_record_over_the_limit_is_a_fatal_error() {
        let big = format!("a,b\n{}\nc,d\n", "x".repeat(5000));
        let l = Limits { max_record_bytes: 1000, ..Limits::default() };
        let (msgs, data) = query_with("SELECT * FROM BlobStorage", "", "", big.as_bytes(), &l);
        assert_eq!(data, b"a,b\n");
        assert_eq!(kinds(&msgs), "DFPE");
        assert!(matches!(&msgs[1], Msg::Error { name, position: 4, .. } if name == "RecordTooLarge"));
        // the same for JSON
        let big = format!("{{\"a\":1}}\n{{\"a\":\"{}\"}}\n", "x".repeat(5000));
        let (msgs, data) = query_with("SELECT * FROM BlobStorage", &json_cfg("\n", "Input"), "", big.as_bytes(), &l);
        assert_eq!(data, b"{\"a\":1}\n");
        assert!(msgs.iter().any(|m| matches!(m, Msg::Error { fatal: true, name, .. } if name == "RecordTooLarge")));
    }

    #[test]
    fn the_answer_cannot_grow_without_bound() {
        let rows = "0123456789\n".repeat(100_000);
        let l = Limits { max_answer_bytes: 50_000, ..small() };
        let plan = request("SELECT * FROM BlobStorage", "", "");
        let file = run(&plan, vec![Piece::Data(azemu_core::bytes::Bytes::from(rows))], &l);
        let (_, _, msgs) = read(&file);
        assert!(
            matches!(msgs.iter().rev().nth(2), Some(Msg::Error { fatal: true, name, .. }) if name == "ResultTooLarge"),
            "{:?}",
            kinds(&msgs)
        );
        assert!(file.len() < 50_000 + 4096 + 2 * 100, "the answer is {} bytes", file.len());
    }

    #[test]
    fn a_query_out_of_time_ends_with_a_fatal_error() {
        let rows = "a,b\n".repeat(10_000);
        let l = Limits { time_budget: Duration::ZERO, ..Limits::default() };
        let (msgs, _) = query_with("SELECT * FROM BlobStorage", "", "", rows.as_bytes(), &l);
        assert!(msgs.iter().any(|m| matches!(m, Msg::Error { fatal: true, name, .. } if name == "QueryTimeout")), "{}", kinds(&msgs));
        assert_eq!(msgs.last(), Some(&Msg::End(rows.len() as i64)));
    }

    /// One record, many expensive select items: the limit has to be looked at inside the record.
    #[test]
    fn the_time_limit_is_checked_inside_one_record() {
        let blob = format!("{}\n", "a".repeat(4_000_000));
        // a pattern with `_` in it, the shape of LIKE that still costs time in proportion to the value
        let items: Vec<String> = (0..200).map(|i| format!("_1 LIKE '%{}b%' AS x{i}", "a_".repeat(100))).collect();
        let expr = format!("SELECT {} FROM BlobStorage", items.join(", "));
        let l = Limits { time_budget: Duration::from_millis(200), ..Limits::default() };
        let t = std::time::Instant::now();
        let (msgs, _) = query_with(&expr, "", "", blob.as_bytes(), &l);
        assert!(msgs.iter().any(|m| matches!(m, Msg::Error { fatal: true, name, .. } if name == "QueryTimeout")), "{}", kinds(&msgs));
        assert!(t.elapsed() < Duration::from_secs(2), "the query ran {:?} with a limit of 200 ms", t.elapsed());
    }

    #[test]
    fn too_many_row_errors_end_the_query() {
        let rows = "a\n".repeat(50);
        let l = Limits { max_errors: 5, ..Limits::default() };
        let (msgs, data) = query_with("SELECT _2 FROM BlobStorage", "", "", rows.as_bytes(), &l);
        assert!(data.is_empty());
        let nonfatal = msgs.iter().filter(|m| matches!(m, Msg::Error { fatal: false, .. })).count();
        assert_eq!(nonfatal, 5);
        assert!(msgs.iter().any(|m| matches!(m, Msg::Error { fatal: true, name, .. } if name == "TooManyErrors")));
        // without the limit every row is reported
        let (msgs, _) = query("SELECT _2 FROM BlobStorage", "", "", rows.as_bytes());
        assert_eq!(msgs.iter().filter(|m| matches!(m, Msg::Error { fatal: false, .. })).count(), 50);
    }

    #[test]
    fn an_empty_blob_and_a_query_without_results_still_have_the_closing_messages() {
        let (msgs, data) = query("SELECT * FROM BlobStorage", "", "", b"");
        assert!(data.is_empty());
        assert_eq!(msgs, vec![Msg::Data(vec![]), Msg::Progress(0, 0), Msg::End(0)]);
        let (msgs, _) = query("SELECT * FROM BlobStorage WHERE _1 = 'zzz'", "", "", b"a\n");
        assert_eq!(msgs, vec![Msg::Data(vec![]), Msg::Progress(2, 2), Msg::End(2)]);
    }

    #[test]
    fn json_documents_need_no_separator_between_them() {
        // test_quick_query_with_only_input_json_serialization_setting: the blob is three documents written one
        // after the other, with no line break, and the real service answers three records
        let blob = include_bytes!("testdata/only_input_json.json");
        assert!(!blob.contains(&b'\n'));
        let (_, _, rec) = read(include_bytes!("testdata/only_input_json.avro"));
        let Msg::Data(want) = &rec[0] else { panic!() };
        let (msgs, data) = query("SELECT name from BlobStorage", &json_cfg("\n", "Input"), &json_cfg("\n", "Output"), blob);
        assert_eq!(&data, want);
        assert_eq!(&msgs[1..], &rec[1..]);
    }

    #[test]
    fn json_input_errors_and_records() {
        let (_, _, rec) = read(include_bytes!("testdata/fatal_json.avro"));
        let (msgs, data) = query(
            "SELECT * from BlobStorage",
            &json_cfg("\n", "Input"),
            &csv_cfg(";", "'", ".", "\\", false, "Output"),
            include_bytes!("testdata/fatal_json.json"),
        );
        assert!(data.is_empty());
        assert_eq!(msgs, rec, "the fatal ParseError answer of the recording");
        // the recorded JSON-to-JSON answers
        let (_, _, rec) = read(include_bytes!("testdata/json_out.avro"));
        let Msg::Data(want) = &rec[0] else { panic!() };
        let (msgs, data) = query(
            "SELECT name from BlobStorage",
            &json_cfg("\n", "Input"),
            &json_cfg(";", "Output"),
            include_bytes!("testdata/json_out.json"),
        );
        assert_eq!(&data, want);
        assert_eq!(&msgs[1..], &rec[1..]);
    }
}
