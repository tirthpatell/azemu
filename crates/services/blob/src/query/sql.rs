//! The query acceleration SQL dialect: the parser (`lexer.rs` and `ast.rs` hold the rest).
//!
//! `SELECT * | expr [[AS] alias], ... FROM BlobStorage[[*]][.path...] [alias] [WHERE expr] [LIMIT n]`
//! (see `docs/services/blob-query.md` for what is and is not built).
//!
//! Bounds (the expression may be 256 KiB): at most [`MAX_DEPTH`] levels of nested expression
//! (parentheses and operator chains both count, so evaluation and dropping the tree cannot overflow the
//! stack), at most [`MAX_NODES`] nodes, and the parser never reads past the text.

pub use super::ast::*;
use super::datetime::{self, Part};
use super::lexer::{lex, Lexed, Tok};
use super::value::{CastType, Value};

pub const MAX_DEPTH: u32 = 100;
pub const MAX_NODES: usize = 100_000;

#[derive(Debug, Clone, PartialEq)]
pub struct SqlError {
    pub message: String,
    /// Character offset in the expression.
    pub pos: usize,
}

pub(super) type R<T> = Result<T, SqlError>;

pub(super) fn err<T>(pos: usize, message: impl Into<String>) -> R<T> {
    Err(SqlError { message: message.into(), pos })
}

// ---------------------------------------------------------------- parser
const RESERVED: &[&str] = &[
    "SELECT", "FROM", "WHERE", "LIMIT", "AS", "AND", "OR", "NOT", "IS", "NULL", "MISSING", "BETWEEN", "IN", "LIKE", "ESCAPE", "CAST",
    "TRUE", "FALSE", "LEADING", "TRAILING", "BOTH", "FOR",
];

struct Parser<'a> {
    text: &'a str,
    toks: Vec<Lexed>,
    p: usize,
    /// Recursion of the parser (parentheses and prefix operators).
    nest: u32,
    nodes: usize,
    saw_agg: bool,
    alias: Option<String>,
}

/// An expression and the depth of its tree.
type PE = (Expr, u32);

fn bx(e: PE) -> (Box<Expr>, u32) {
    (Box::new(e.0), e.1)
}

pub fn parse(text: &str) -> R<Select> {
    let toks = lex(text)?;
    let mut p = Parser { text, toks, p: 0, nest: 0, nodes: 0, saw_agg: false, alias: None };
    p.select()
}

impl<'a> Parser<'a> {
    fn tok(&self) -> &Tok {
        &self.toks[self.p].tok
    }

    fn pos(&self) -> usize {
        self.toks[self.p].pos
    }

    fn bump(&mut self) {
        if self.p + 1 < self.toks.len() {
            self.p += 1;
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.tok(), Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> R<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            self.unexpected(&format!("Expected {kw}"))
        }
    }

    fn is_sym(&self, s: &str) -> bool {
        matches!(self.tok(), Tok::Sym(x) if *x == s)
    }

    fn eat_sym(&mut self, s: &str) -> bool {
        if self.is_sym(s) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, s: &str) -> R<()> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            self.unexpected(&format!("Expected '{s}'"))
        }
    }

    fn unexpected<T>(&self, what: &str) -> R<T> {
        let found = match self.tok() {
            Tok::Eof => "the end of the expression".to_string(),
            Tok::Ident(s) => format!("'{s}'"),
            Tok::Quoted(s) => format!("\"{s}\""),
            Tok::Str(s) => format!("'{s}'"),
            Tok::Int(i) => i.to_string(),
            Tok::Float(f) => f.to_string(),
            Tok::Sym(s) => format!("'{s}'"),
        };
        err(self.pos(), format!("{what}, found {found}."))
    }

    fn node(&mut self) -> R<()> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return err(self.pos(), "The expression is too large.");
        }
        Ok(())
    }

    fn mk(&mut self, e: Expr, d: u32) -> R<PE> {
        self.node()?;
        if d + 1 > MAX_DEPTH {
            return err(self.pos(), format!("The expression is nested more than {MAX_DEPTH} levels deep."));
        }
        Ok((e, d + 1))
    }

    fn enter(&mut self) -> R<()> {
        self.nest += 1;
        if self.nest > MAX_DEPTH {
            return err(self.pos(), format!("The expression is nested more than {MAX_DEPTH} levels deep."));
        }
        Ok(())
    }

    fn ident_name(&mut self) -> R<(String, bool)> {
        match self.tok().clone() {
            Tok::Ident(s) => {
                if RESERVED.iter().any(|k| k.eq_ignore_ascii_case(&s)) {
                    return self.unexpected("Expected a name; put a name that is also a keyword in double quotes");
                }
                self.bump();
                Ok((s, false))
            }
            Tok::Quoted(s) => {
                self.bump();
                Ok((s, true))
            }
            _ => self.unexpected("Expected a name"),
        }
    }

    // SELECT ... FROM ... [WHERE ...] [LIMIT n]
    fn select(&mut self) -> R<Select> {
        self.expect_kw("SELECT")?;
        // The table alias is needed to read the select list, so read the FROM clause first.
        let list_start = self.p;
        let mut depth = 0i32;
        let mut from_at = None;
        for (i, t) in self.toks.iter().enumerate().skip(self.p) {
            match &t.tok {
                Tok::Sym("(") | Tok::Sym("[") => depth += 1,
                Tok::Sym(")") | Tok::Sym("]") => depth -= 1,
                Tok::Ident(s) if depth == 0 && s.eq_ignore_ascii_case("FROM") => {
                    from_at = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(from_at) = from_at else {
            return err(self.toks[self.toks.len() - 1].pos, "Expected FROM, found the end of the expression.");
        };
        self.p = from_at;
        self.expect_kw("FROM")?;
        let from = self.table()?;
        let after_table = self.p;
        self.p = list_start;
        let projection = self.projection(from_at)?;
        self.p = after_table;

        let mut filter = None;
        if self.eat_kw("WHERE") {
            let saw = self.saw_agg;
            self.saw_agg = false;
            let (e, _) = self.expr()?;
            if self.saw_agg {
                return err(self.pos(), "An aggregate function cannot be used in WHERE.");
            }
            self.saw_agg = saw;
            filter = Some(e);
        }
        let mut limit = None;
        if self.eat_kw("LIMIT") {
            match self.tok() {
                Tok::Int(n) if *n >= 0 => {
                    limit = Some(*n as u64);
                    self.bump();
                }
                _ => return self.unexpected("Expected a whole number after LIMIT"),
            }
        }
        self.eat_sym(";");
        if !matches!(self.tok(), Tok::Eof) {
            return self.unexpected("Unexpected text after the end of the statement");
        }
        let aggregate = match &projection {
            Projection::Star => false,
            Projection::List(items) => {
                let aggs = items.iter().filter(|i| matches!(i.expr, Expr::Agg(..))).count();
                if aggs > 0 && aggs != items.len() {
                    return err(0, "A SELECT list is either aggregates only or no aggregates at all.");
                }
                if self.saw_agg && aggs == 0 {
                    return err(0, "An aggregate function can only be a whole select item.");
                }
                aggs > 0
            }
        };
        Ok(Select { projection, from, filter, limit, aggregate })
    }

    /// `BlobStorage[[*]][.path][alias]`
    fn table(&mut self) -> R<Vec<FromStep>> {
        let mut steps = Vec::new();
        match self.tok().clone() {
            Tok::Ident(s) if s.eq_ignore_ascii_case("BlobStorage") => self.bump(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("sys") => return err(self.pos(), "sys.split is not built in this emulator."),
            _ => return self.unexpected("Expected BlobStorage"),
        }
        loop {
            if self.eat_sym("[") {
                match self.tok().clone() {
                    Tok::Sym("*") => {
                        self.bump();
                        // `BlobStorage[*]` is the whole record, the same as `BlobStorage`
                        if !steps.is_empty() {
                            steps.push(FromStep::All);
                        }
                    }
                    Tok::Int(n) if n >= 0 => {
                        self.bump();
                        steps.push(FromStep::Index(n as usize));
                    }
                    _ => return self.unexpected("Expected * or a position in []"),
                }
                self.expect_sym("]")?;
            } else if self.eat_sym(".") {
                let (n, _) = self.ident_name()?;
                steps.push(FromStep::Field(n));
            } else {
                break;
            }
            if steps.len() > 64 {
                return err(self.pos(), "The path in FROM is too long.");
            }
        }
        // `FROM BlobStorage s`, `FROM BlobStorage AS s`
        self.eat_kw("AS");
        if let Tok::Ident(s) = self.tok().clone() {
            if !RESERVED.iter().any(|k| k.eq_ignore_ascii_case(&s)) {
                self.alias = Some(s);
                self.bump();
            }
        } else if let Tok::Quoted(s) = self.tok().clone() {
            self.alias = Some(s);
            self.bump();
        }
        Ok(steps)
    }

    fn projection(&mut self, end: usize) -> R<Projection> {
        if self.is_sym("*") && self.p + 1 == end {
            self.bump();
            return Ok(Projection::Star);
        }
        let mut items = Vec::new();
        loop {
            let start = self.toks[self.p].start;
            let (e, _) = self.expr()?;
            let stop = self.toks[self.p - 1].end;
            let mut alias = None;
            if self.eat_kw("AS") {
                alias = Some(self.ident_name()?.0);
            } else if let Tok::Ident(s) = self.tok().clone() {
                if !RESERVED.iter().any(|k| k.eq_ignore_ascii_case(&s)) {
                    alias = Some(s);
                    self.bump();
                }
            } else if let Tok::Quoted(s) = self.tok().clone() {
                alias = Some(s);
                self.bump();
            }
            items.push(Item { expr: e, alias, source: self.text[start..stop].to_string() });
            if items.len() > 4096 {
                return err(self.pos(), "The SELECT list has too many columns.");
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        if self.p != end {
            return self.unexpected("Expected , or FROM");
        }
        Ok(Projection::List(items))
    }

    // expr := or
    fn expr(&mut self) -> R<PE> {
        self.enter()?;
        let r = self.or();
        self.nest -= 1;
        r
    }

    fn or(&mut self) -> R<PE> {
        let mut l = self.and()?;
        while self.eat_kw("OR") {
            let r = self.and()?;
            l = self.bin(BinOp::Or, l, r)?;
        }
        Ok(l)
    }

    fn and(&mut self) -> R<PE> {
        let mut l = self.not()?;
        while self.eat_kw("AND") {
            let r = self.not()?;
            l = self.bin(BinOp::And, l, r)?;
        }
        Ok(l)
    }

    fn bin(&mut self, op: BinOp, l: PE, r: PE) -> R<PE> {
        let d = l.1.max(r.1);
        self.mk(Expr::Bin(op, Box::new(l.0), Box::new(r.0)), d)
    }

    fn not(&mut self) -> R<PE> {
        if self.is_kw("NOT") {
            self.bump();
            self.enter()?;
            let e = self.not();
            self.nest -= 1;
            let (e, d) = bx(e?);
            return self.mk(Expr::Not(e), d);
        }
        self.comparison()
    }

    fn comparison(&mut self) -> R<PE> {
        let l = self.additive()?;
        let op = match self.tok() {
            Tok::Sym("=") => Some(BinOp::Eq),
            Tok::Sym("!=") | Tok::Sym("<>") => Some(BinOp::Ne),
            Tok::Sym("<") => Some(BinOp::Lt),
            Tok::Sym("<=") => Some(BinOp::Le),
            Tok::Sym(">") => Some(BinOp::Gt),
            Tok::Sym(">=") => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let r = self.additive()?;
            return self.bin(op, l, r);
        }
        if self.eat_kw("IS") {
            let neg = self.eat_kw("NOT");
            let (e, d) = bx(l);
            if self.eat_kw("NULL") {
                return self.mk(Expr::IsNull { e, neg }, d);
            }
            if self.eat_kw("MISSING") {
                return self.mk(Expr::IsMissing { e, neg }, d);
            }
            return self.unexpected("Expected NULL or MISSING after IS");
        }
        let neg = if self.is_kw("NOT")
            && matches!(self.toks.get(self.p + 1).map(|t| &t.tok), Some(Tok::Ident(s)) if ["BETWEEN", "IN", "LIKE"].iter().any(|k| k.eq_ignore_ascii_case(s)))
        {
            self.bump();
            true
        } else {
            false
        };
        if self.eat_kw("BETWEEN") {
            let lo = self.additive()?;
            self.expect_kw("AND")?;
            let hi = self.additive()?;
            let d = l.1.max(lo.1).max(hi.1);
            return self.mk(Expr::Between { e: Box::new(l.0), lo: Box::new(lo.0), hi: Box::new(hi.0), neg }, d);
        }
        if self.eat_kw("IN") {
            self.expect_sym("(")?;
            let mut list = Vec::new();
            let mut d = l.1;
            loop {
                let x = self.expr()?;
                d = d.max(x.1);
                list.push(x.0);
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            return self.mk(Expr::In { e: Box::new(l.0), list, neg }, d);
        }
        if self.eat_kw("LIKE") {
            let pat = self.additive()?;
            let mut d = l.1.max(pat.1);
            let mut esc = None;
            if self.eat_kw("ESCAPE") {
                let e = self.additive()?;
                d = d.max(e.1);
                esc = Some(Box::new(e.0));
            }
            return self.mk(Expr::Like { e: Box::new(l.0), pat: Box::new(pat.0), esc, neg }, d);
        }
        if neg {
            return self.unexpected("Expected BETWEEN, IN or LIKE");
        }
        Ok(l)
    }

    fn additive(&mut self) -> R<PE> {
        let mut l = self.multiplicative()?;
        loop {
            let op = match self.tok() {
                Tok::Sym("+") => BinOp::Add,
                Tok::Sym("-") => BinOp::Sub,
                Tok::Sym("||") => BinOp::Concat,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.multiplicative()?;
            l = self.bin(op, l, r)?;
        }
    }

    fn multiplicative(&mut self) -> R<PE> {
        let mut l = self.unary()?;
        loop {
            let op = match self.tok() {
                Tok::Sym("*") => BinOp::Mul,
                Tok::Sym("/") => BinOp::Div,
                Tok::Sym("%") => BinOp::Mod,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.unary()?;
            l = self.bin(op, l, r)?;
        }
    }

    fn unary(&mut self) -> R<PE> {
        if self.is_sym("-") || self.is_sym("+") {
            let neg = self.is_sym("-");
            self.bump();
            self.enter()?;
            let e = self.unary();
            self.nest -= 1;
            let (e, d) = bx(e?);
            return if neg { self.mk(Expr::Neg(e), d) } else { Ok((*e, d)) };
        }
        self.primary()
    }

    fn lit(&mut self, v: Value) -> R<PE> {
        self.bump();
        self.mk(Expr::Lit(v), 0)
    }

    fn primary(&mut self) -> R<PE> {
        match self.tok().clone() {
            Tok::Int(n) => self.lit(Value::Int(n)),
            Tok::Float(f) => self.lit(Value::Float(f)),
            Tok::Str(s) => self.lit(Value::Str(s)),
            Tok::Sym("(") => {
                self.bump();
                let e = self.expr()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            Tok::Quoted(_) => self.reference(),
            Tok::Ident(s) => {
                let up = s.to_ascii_uppercase();
                match up.as_str() {
                    "NULL" => return self.lit(Value::Null),
                    "TRUE" => return self.lit(Value::Bool(true)),
                    "FALSE" => return self.lit(Value::Bool(false)),
                    "CASE" => return err(self.pos(), "CASE expressions are not built in this emulator."),
                    "SYS" if matches!(self.toks.get(self.p + 1).map(|t| &t.tok), Some(Tok::Sym("."))) => {
                        return err(self.pos(), "sys.split is not built in this emulator.")
                    }
                    _ => {}
                }
                if matches!(self.toks.get(self.p + 1).map(|t| &t.tok), Some(Tok::Sym("("))) {
                    return self.call(&up);
                }
                self.reference()
            }
            _ => self.unexpected("Expected an expression"),
        }
    }

    /// A column or field: `name`, `"Name"`, `_3`, `alias.name`, `a.b[0].c`.
    fn reference(&mut self) -> R<PE> {
        let mut steps: Vec<Step> = Vec::new();
        let (first, quoted) = self.ident_name()?;
        steps.push(Step::Field { name: first, quoted });
        loop {
            if self.is_sym(".") {
                self.bump();
                let (n, q) = self.ident_name()?;
                steps.push(Step::Field { name: n, quoted: q });
            } else if self.is_sym("[") {
                self.bump();
                match self.tok().clone() {
                    Tok::Int(n) if n >= 0 => {
                        self.bump();
                        steps.push(Step::Index(n as usize));
                    }
                    Tok::Str(s) => {
                        self.bump();
                        steps.push(Step::Field { name: s, quoted: true });
                    }
                    _ => return self.unexpected("Expected a position or a name in []"),
                }
                self.expect_sym("]")?;
            } else {
                break;
            }
            if steps.len() > 64 {
                return err(self.pos(), "The field path is too long.");
            }
        }
        // `alias.name` and `BlobStorage.name`: the table name is not a field
        if steps.len() > 1 {
            if let Step::Field { name, quoted: false } = &steps[0] {
                let is_alias = self.alias.as_deref().map(|a| a.eq_ignore_ascii_case(name)).unwrap_or(false);
                if is_alias || name.eq_ignore_ascii_case("BlobStorage") {
                    steps.remove(0);
                }
            }
        }
        if let Some(Step::Field { name, quoted: false }) = steps.first() {
            if let Some(n) = name.strip_prefix('_').and_then(|d| {
                if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
                    None
                } else {
                    d.parse::<usize>().ok()
                }
            }) {
                if n == 0 {
                    return err(self.pos(), "Column ordinals start at _1.");
                }
                steps[0] = Step::Ordinal(n);
            }
        }
        self.mk(Expr::Ref(steps), 0)
    }

    fn part_arg(&mut self) -> R<Part> {
        let name = match self.tok().clone() {
            Tok::Ident(s) | Tok::Str(s) => s,
            _ => return self.unexpected("Expected a date part (year, month, day, hour, minute, second)"),
        };
        match datetime::part(&name) {
            Some(p) => {
                self.bump();
                Ok(p)
            }
            None => err(self.pos(), format!("'{name}' is not a date part (year, month, day, hour, minute, second).")),
        }
    }

    fn args(&mut self) -> R<Vec<PE>> {
        self.expect_sym("(")?;
        let mut v = Vec::new();
        if self.eat_sym(")") {
            return Ok(v);
        }
        loop {
            v.push(self.expr()?);
            if v.len() > 1024 {
                return err(self.pos(), "Too many arguments.");
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(v)
    }

    fn func(&mut self, id: FuncId, part: Option<Part>, args: Vec<PE>, min: usize, max: usize, name: &str) -> R<PE> {
        if args.len() < min || args.len() > max {
            return err(
                self.pos(),
                format!("{name} takes {} argument(s).", if min == max { min.to_string() } else { format!("{min} to {max}") }),
            );
        }
        let d = args.iter().map(|a| a.1).max().unwrap_or(0);
        self.mk(Expr::Func(id, part, args.into_iter().map(|a| a.0).collect()), d)
    }

    fn call(&mut self, name: &str) -> R<PE> {
        self.bump(); // the name; the next token is "("
        match name {
            "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" => {
                self.expect_sym("(")?;
                let f = match name {
                    "COUNT" => AggFn::Count,
                    "SUM" => AggFn::Sum,
                    "AVG" => AggFn::Avg,
                    "MIN" => AggFn::Min,
                    _ => AggFn::Max,
                };
                self.saw_agg = false;
                let arg = if f == AggFn::Count && self.is_sym("*") {
                    self.bump();
                    None
                } else {
                    Some(self.expr()?)
                };
                if self.saw_agg {
                    return err(self.pos(), "Aggregate functions cannot be nested.");
                }
                self.expect_sym(")")?;
                self.saw_agg = true;
                let d = arg.as_ref().map(|a| a.1).unwrap_or(0);
                self.mk(Expr::Agg(f, arg.map(|a| Box::new(a.0))), d)
            }
            "CAST" => {
                self.expect_sym("(")?;
                let e = self.expr()?;
                self.expect_kw("AS")?;
                let ty = self.cast_type()?;
                self.expect_sym(")")?;
                let (e, d) = bx(e);
                self.mk(Expr::Cast(e, ty), d)
            }
            "EXTRACT" => {
                self.expect_sym("(")?;
                let part = self.part_arg()?;
                self.expect_kw("FROM")?;
                let e = self.expr()?;
                self.expect_sym(")")?;
                let (e, d) = bx(e);
                self.mk(Expr::Extract(part, e), d)
            }
            "DATE_ADD" | "DATE_DIFF" => {
                self.expect_sym("(")?;
                let part = self.part_arg()?;
                let mut args = Vec::new();
                while self.eat_sym(",") {
                    args.push(self.expr()?);
                }
                self.expect_sym(")")?;
                let id = if name == "DATE_ADD" { FuncId::DateAdd } else { FuncId::DateDiff };
                self.func(id, Some(part), args, 2, 2, name)
            }
            "TRIM" => {
                self.expect_sym("(")?;
                let mut mode = TrimMode::Both;
                let mut explicit = false;
                for (kw, m) in [("BOTH", TrimMode::Both), ("LEADING", TrimMode::Leading), ("TRAILING", TrimMode::Trailing)] {
                    if self.eat_kw(kw) {
                        mode = m;
                        explicit = true;
                        break;
                    }
                }
                let mut chars = None;
                let first = if explicit && self.is_kw("FROM") { None } else { Some(self.expr()?) };
                let e;
                if self.eat_kw("FROM") {
                    chars = first;
                    e = self.expr()?;
                } else {
                    e = first.ok_or_else(|| SqlError { message: "TRIM needs a value.".into(), pos: self.pos() })?;
                }
                self.expect_sym(")")?;
                let d = e.1.max(chars.as_ref().map(|c| c.1).unwrap_or(0));
                self.mk(Expr::Trim { mode, chars: chars.map(|c| Box::new(c.0)), e: Box::new(e.0) }, d)
            }
            "SUBSTRING" => {
                self.expect_sym("(")?;
                let mut args = vec![self.expr()?];
                if self.eat_kw("FROM") {
                    args.push(self.expr()?);
                    if self.eat_kw("FOR") {
                        args.push(self.expr()?);
                    }
                } else {
                    while self.eat_sym(",") {
                        args.push(self.expr()?);
                    }
                }
                self.expect_sym(")")?;
                self.func(FuncId::Substring, None, args, 2, 3, name)
            }
            _ => {
                let (id, min, max) = match name {
                    "CHAR_LENGTH" | "CHARACTER_LENGTH" => (FuncId::CharLength, 1, 1),
                    "LOWER" => (FuncId::Lower, 1, 1),
                    "UPPER" => (FuncId::Upper, 1, 1),
                    "TO_STRING" => (FuncId::ToString, 2, 2),
                    "TO_TIMESTAMP" => (FuncId::ToTimestamp, 1, 1),
                    "UTCNOW" => (FuncId::UtcNow, 0, 0),
                    "COALESCE" => (FuncId::Coalesce, 1, 1024),
                    "NULLIF" => (FuncId::NullIf, 2, 2),
                    _ => {
                        self.p -= 1;
                        return err(self.pos(), format!("The function {name} is not supported."));
                    }
                };
                let args = self.args()?;
                self.func(id, None, args, min, max, name)
            }
        }
    }

    fn cast_type(&mut self) -> R<CastType> {
        let Tok::Ident(s) = self.tok().clone() else { return self.unexpected("Expected a type name") };
        let t = match s.to_ascii_uppercase().as_str() {
            "INT" | "INTEGER" | "BIGINT" | "SMALLINT" => CastType::Int,
            "FLOAT" | "DOUBLE" | "REAL" | "DECIMAL" | "NUMERIC" => CastType::Float,
            "STRING" | "VARCHAR" | "CHAR" | "TEXT" => CastType::Str,
            "BOOL" | "BOOLEAN" => CastType::Bool,
            "TIMESTAMP" => CastType::Timestamp,
            _ => return err(self.pos(), format!("'{s}' is not a type the language knows (INT, FLOAT, STRING, TIMESTAMP, BOOLEAN).")),
        };
        self.bump();
        // `DECIMAL(10, 2)`, `VARCHAR(20)`: the size is accepted and ignored
        if self.is_sym("(") {
            self.bump();
            while !self.is_sym(")") {
                if matches!(self.tok(), Tok::Eof) {
                    return self.unexpected("Expected )");
                }
                self.bump();
            }
            self.bump();
        }
        Ok(t)
    }
}
