//! Recursive-descent parser for the Cosmos DB NoSQL query language.

use crate::ast::*;
use crate::error::SqlError;
use crate::functions;
use crate::lexer::{tokenize, Tok, Token};

const MAX_NEST: usize = 996;
const MAX_EXPR_DEPTH: u16 = 4096;
/// Most JOINs a query may have, counted over the whole text (the subqueries too): Azure refuses an eleventh.
const MAX_JOINS: usize = 10;
/// Largest TOP, OFFSET or LIMIT count (a real account takes 2147483648 and refuses 99999999999; this is a guess between them).
pub(crate) const MAX_COUNT: i64 = u32::MAX as i64;

/// The reserved words of the language (Azure refuses them as property names after a dot, as aliases and as unquoted
/// object keys; checked against a real account, November 2026). `LEFT` and `RIGHT` are also function names.
const RESERVED: &[&str] = &[
    "AND",
    "ARRAY",
    "AS",
    "ASC",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "CONVERT",
    "CROSS",
    "DESC",
    "DISTINCT",
    "ELSE",
    "END",
    "ESCAPE",
    "EXISTS",
    "FALSE",
    "FOR",
    "FROM",
    "GROUP",
    "HAVING",
    "IN",
    "INNER",
    "INSERT",
    "INTO",
    "IS",
    "JOIN",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "RIGHT",
    "SELECT",
    "SET",
    "THEN",
    "TOP",
    "TRUE",
    "UDF",
    "UNDEFINED",
    "UPDATE",
    "VALUE",
    "WHEN",
    "WHERE",
    "WITH",
];

fn is_reserved(w: &str) -> bool {
    // `udf` is a keyword only in lower case (`UDF.f(1)` is a path, and a syntax error at the bracket)
    if w.eq_ignore_ascii_case("udf") {
        return w == "udf";
    }
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(w))
}

struct Parser<'s> {
    toks: Vec<Token>,
    pos: usize,
    nest: usize,
    last_end: usize,
    gen: usize,
    src: &'s [char],
    /// JOINs seen so far in the whole text.
    joins: usize,
}

/// Parses query text into an AST (syntax and function-name checks only).
pub(crate) fn parse_text(sql: &str) -> Result<Query, SqlError> {
    let src: Vec<char> = sql.chars().collect();
    let toks = tokenize(&src)?;
    let mut p = Parser { toks, pos: 0, nest: 0, last_end: 0, gen: 0, src: &src, joins: 0 };
    let q = p.parse_query()?;
    if !matches!(p.peek(), Tok::Eof) {
        return Err(p.err_here());
    }
    Ok(q)
}

impl<'s> Parser<'s> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }
    fn cur_start(&self) -> usize {
        self.toks[self.pos].start
    }
    fn advance(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        self.last_end = t.end;
        t
    }
    fn text(&self, start: usize, end: usize) -> String {
        self.src[start.min(self.src.len())..end.min(self.src.len())].iter().collect()
    }
    fn err_here(&self) -> SqlError {
        let t = &self.toks[self.pos];
        if matches!(t.tok, Tok::Eof) {
            return SqlError::at("SC1002", "Syntax error, unexpected end-of-file.".to_string(), t.start, t.end);
        }
        SqlError::syntax_near(&self.text(t.start, t.end), t.start, t.end)
    }
    fn is_word(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }
    fn is_word_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_at(n), Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }
    fn eat_word(&mut self, kw: &str) -> bool {
        if self.is_word(kw) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect_word(&mut self, kw: &str) -> Result<(), SqlError> {
        if self.eat_word(kw) {
            Ok(())
        } else {
            Err(self.err_here())
        }
    }
    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Sym(x) if *x == s)
    }
    fn is_sym_at(&self, n: usize, s: &str) -> bool {
        matches!(self.peek_at(n), Tok::Sym(x) if *x == s)
    }
    fn eat_sym(&mut self, s: &str) -> bool {
        if self.is_sym(s) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect_sym(&mut self, s: &str) -> Result<(), SqlError> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(self.err_here())
        }
    }

    fn mk(&self, kind: ExprKind, start: usize) -> Result<Expr, SqlError> {
        let depth = 1 + kind_depth(&kind);
        let span = Span { start, end: self.last_end };
        if depth > MAX_EXPR_DEPTH {
            return Err(too_complex(start, span.end));
        }
        Ok(Expr { kind, span, depth })
    }

    fn enter(&mut self) -> Result<(), SqlError> {
        self.nest += 1;
        if self.nest > MAX_NEST {
            let t = &self.toks[self.pos];
            return Err(too_complex(t.start, t.end));
        }
        Ok(())
    }

    // ---------------------------------------------------------------- query

    fn parse_query(&mut self) -> Result<Query, SqlError> {
        self.enter()?;
        let r = self.parse_query_inner();
        self.nest -= 1;
        r
    }

    fn parse_query_inner(&mut self) -> Result<Query, SqlError> {
        let qstart = self.cur_start();
        self.expect_word("SELECT")?;
        let distinct = self.eat_word("DISTINCT");
        let top_start = self.cur_start();
        let top = if self.eat_word("TOP") { Some(self.parse_count(Count::Top)?) } else { None };
        let top_span = top.as_ref().map(|_| Span { start: top_start, end: self.last_end });
        let items_start = self.cur_start();
        let spec = if self.eat_sym("*") {
            SelectSpec::Star
        } else if self.eat_word("VALUE") {
            let e = self.parse_expr()?;
            self.parse_alias()?;
            SelectSpec::Value(e)
        } else {
            let mut items = Vec::new();
            loop {
                let expr = self.parse_expr()?;
                let (alias, alias_span) = match self.parse_alias()? {
                    Some((a, sp)) => (Some(a), Some(sp)),
                    None => (None, None),
                };
                items.push(SelectItem { expr, alias, alias_span });
                if !self.eat_sym(",") {
                    break;
                }
            }
            check_duplicate_names(&items)?;
            SelectSpec::Items(items)
        };
        let items_span = Span { start: items_start, end: self.last_end };
        let select = Select { distinct, top, top_span, spec, items_span };

        let from = if self.is_word("FROM") { Some(self.parse_from()?) } else { None };
        let where_clause = if self.eat_word("WHERE") { Some(self.parse_expr()?) } else { None };
        let mut group_by = Vec::new();
        if self.is_word("GROUP") {
            self.advance();
            self.expect_word("BY")?;
            loop {
                group_by.push(self.parse_expr()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let mut order_by = Vec::new();
        let order_start = self.cur_start();
        if self.is_word("ORDER") {
            self.advance();
            self.expect_word("BY")?;
            loop {
                let expr = self.parse_expr()?;
                let descending = if self.eat_word("DESC") {
                    true
                } else {
                    self.eat_word("ASC");
                    false
                };
                order_by.push(OrderItem { expr, descending });
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let order_end = self.last_end;
        let order_by_present = !order_by.is_empty();
        let offset_start = self.cur_start();
        let offset_limit = if self.eat_word("OFFSET") {
            let offset = self.parse_count(Count::Offset)?;
            self.expect_word("LIMIT")?;
            let limit = self.parse_count(Count::Limit)?;
            Some(OffsetLimit { offset, limit, span: Span { start: offset_start, end: self.last_end } })
        } else {
            None
        };

        let select_aggregate = match &select.spec {
            SelectSpec::Star => false,
            SelectSpec::Value(e) => e.contains_aggregate(),
            SelectSpec::Items(items) => items.iter().any(|i| i.expr.contains_aggregate()),
        };
        let is_aggregate = !group_by.is_empty() || select_aggregate || order_by.iter().any(|o| o.expr.contains_aggregate());
        Ok(Query {
            select,
            from,
            where_clause,
            group_by,
            order_by,
            order_span: if order_by_present { Some(Span { start: order_start, end: order_end }) } else { None },
            offset_limit,
            is_aggregate,
            span: Span { start: qstart, end: self.last_end },
        })
    }

    /// `AS name` or a bare name, with the position of the name.
    fn parse_alias(&mut self) -> Result<Option<(String, Span)>, SqlError> {
        let had_as = self.eat_word("AS");
        if let Tok::Word(w) = self.peek().clone() {
            if !is_reserved(&w) {
                let t = self.advance();
                return Ok(Some((w, Span { start: t.start, end: t.end })));
            }
        }
        if had_as {
            return Err(self.err_here());
        }
        Ok(None)
    }

    /// The count after TOP, OFFSET or LIMIT: one number or a parameter (no sign, no expression).
    fn parse_count(&mut self, what: Count) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let (lit, end) = match self.peek().clone() {
            Tok::Int(i) => (Some(Literal::Int(i)), self.toks[self.pos].end),
            Tok::Float(f) if what == Count::Top => (Some(Literal::Float(f)), self.toks[self.pos].end),
            Tok::Param(_) => (None, self.toks[self.pos].end),
            _ => return Err(self.err_here()),
        };
        if let Some(l) = &lit {
            let value = match l {
                Literal::Int(i) => Some(*i),
                Literal::Float(f) if f.fract() == 0.0 && f.abs() < 4e18 => Some(*f as i64),
                _ => None,
            };
            if !matches!(value, Some(v) if (0..=MAX_COUNT).contains(&v)) {
                let (code, text) = what.limit_error();
                return Err(SqlError::at(code, text.to_string(), start, end));
            }
        }
        self.parse_primary()
    }

    fn parse_from(&mut self) -> Result<From, SqlError> {
        let start = self.cur_start();
        self.expect_word("FROM")?;
        let mut sources = vec![self.parse_source()?];
        while self.is_word("JOIN") {
            self.advance();
            self.joins += 1;
            if self.joins > MAX_JOINS {
                return Err(SqlError::at(
                    "SC3002",
                    format!("The SQL query exceeded the maximum number of joins. The allowed limit is {MAX_JOINS}."),
                    0,
                    self.src.len(),
                ));
            }
            sources.push(self.parse_source()?);
        }
        Ok(From { sources, span: Span { start, end: self.last_end } })
    }

    fn parse_source(&mut self) -> Result<FromSource, SqlError> {
        let start = self.cur_start();
        let mut alias = None;
        let mut alias_span = None;
        let mut iterate = false;
        if let Tok::Word(w) = self.peek().clone() {
            if !is_reserved(&w) && self.is_word_at(1, "IN") {
                let t = self.advance();
                self.advance();
                alias = Some(w);
                alias_span = Some(Span { start: t.start, end: t.end });
                iterate = true;
            }
        }
        let source = if self.is_sym("(") && self.is_word_at(1, "SELECT") {
            self.advance();
            let q = self.parse_query()?;
            self.expect_sym(")")?;
            SourceExpr::Subquery(Box::new(q))
        } else {
            SourceExpr::Expr(self.parse_path()?)
        };
        if !iterate {
            if let Some((a, sp)) = self.parse_alias()? {
                alias = Some(a);
                alias_span = Some(sp);
            }
        }
        let alias = match alias {
            Some(a) => a,
            None => {
                let inferred = match &source {
                    SourceExpr::Expr(e) => crate::eval::query::inferred_name(e),
                    SourceExpr::Subquery(_) => None,
                };
                inferred.unwrap_or_else(|| {
                    self.gen += 1;
                    format!("$s{}", self.gen)
                })
            }
        };
        Ok(FromSource { alias, alias_span, iterate, source, span: Span { start, end: self.last_end } })
    }

    /// A path in FROM or JOIN: a name followed by `.name` and `[index]` steps, nothing else.
    fn parse_path(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let name = match self.peek().clone() {
            Tok::Word(w) if !is_reserved(&w) => w,
            _ => return Err(self.err_here()),
        };
        self.advance();
        let mut e = self.mk(ExprKind::Ident(name), start)?;
        loop {
            if self.is_sym(".") {
                self.advance();
                e = self.property_step(e, start)?;
            } else if self.is_sym("[") {
                self.advance();
                e = self.index_step(e, start)?;
            } else {
                return Ok(e);
            }
        }
    }

    /// The name after a dot (a reserved word cannot be one).
    fn property_step(&mut self, base: Expr, start: usize) -> Result<Expr, SqlError> {
        match self.peek().clone() {
            Tok::Word(w) if !is_reserved(&w) => {
                self.advance();
                self.mk(ExprKind::Property(Box::new(base), w), start)
            }
            _ => Err(self.err_here()),
        }
    }

    /// The `[index]` step; the bracket is already read. Azure takes a number, a string or a parameter there.
    fn index_step(&mut self, base: Expr, start: usize) -> Result<Expr, SqlError> {
        let idx = self.parse_expr()?;
        self.expect_sym("]")?;
        if !matches!(idx.kind, ExprKind::Literal(Literal::Int(_) | Literal::Float(_) | Literal::Str(_)) | ExprKind::Param(_)) {
            return Err(SqlError::at(
                "SC2990",
                "The specified query includes 'member indexer' which is currently not supported.".to_string(),
                start,
                self.last_end,
            ));
        }
        self.mk(ExprKind::Index(Box::new(base), Box::new(idx)), start)
    }

    // ---------------------------------------------------------- expressions

    pub(crate) fn parse_expr(&mut self) -> Result<Expr, SqlError> {
        self.enter()?;
        let r = self.parse_ternary();
        self.nest -= 1;
        r
    }

    fn parse_ternary(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let cond = self.parse_coalesce()?;
        if self.eat_sym("?") {
            let a = self.parse_expr()?;
            self.expect_sym(":")?;
            let b = self.parse_expr()?;
            return self.mk(ExprKind::Ternary(Box::new(cond), Box::new(a), Box::new(b)), start);
        }
        Ok(cond)
    }

    fn parse_coalesce(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let first = self.parse_or()?;
        if !self.is_sym("??") {
            return Ok(first);
        }
        // Right associative. Built in a loop (not by recursion) so that a long chain cannot
        // overflow the stack; `mk` refuses a tree taller than the expression depth limit.
        let mut items: Vec<(Expr, usize)> = vec![(first, start)];
        while self.eat_sym("??") {
            let s = self.cur_start();
            items.push((self.parse_or()?, s));
        }
        let (mut acc, _) = items.pop().expect("a chain has operands");
        while let Some((left, s)) = items.pop() {
            acc = self.mk(ExprKind::Binary(BinaryOp::Coalesce, Box::new(left), Box::new(acc)), s)?;
        }
        Ok(acc)
    }

    fn parse_or(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let first = self.parse_and()?;
        if !self.is_word("OR") {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat_word("OR") {
            items.push(self.parse_and()?);
        }
        self.mk(ExprKind::Or(items), start)
    }

    fn parse_and(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let first = self.parse_not()?;
        if !self.is_word("AND") {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat_word("AND") {
            items.push(self.parse_not()?);
        }
        self.mk(ExprKind::And(items), start)
    }

    fn parse_not(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        if self.is_word("NOT") {
            self.advance();
            self.enter()?;
            let inner = self.parse_not();
            self.nest -= 1;
            return self.mk(ExprKind::Unary(UnaryOp::Not, Box::new(inner?)), start);
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let mut left = self.parse_bitor()?;
        loop {
            let negated = self.is_word("NOT") && (self.is_word_at(1, "IN") || self.is_word_at(1, "BETWEEN") || self.is_word_at(1, "LIKE"));
            if negated {
                self.advance();
            }
            if self.eat_word("IN") {
                self.expect_sym("(")?;
                let mut list = Vec::new();
                loop {
                    list.push(self.parse_expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
                left = self.mk(ExprKind::In { expr: Box::new(left), list, negated }, start)?;
            } else if self.eat_word("BETWEEN") {
                let low = self.parse_bitor()?;
                self.expect_word("AND")?;
                let high = self.parse_bitor()?;
                left = self.mk(ExprKind::Between { expr: Box::new(left), low: Box::new(low), high: Box::new(high), negated }, start)?;
            } else if self.eat_word("LIKE") {
                let pattern = self.parse_bitor()?;
                let escape = if self.eat_word("ESCAPE") {
                    let tok = self.toks[self.pos].clone();
                    let text = match &tok.tok {
                        Tok::Str(t) => t.clone(),
                        _ => return Err(self.err_here()),
                    };
                    if text.chars().count() != 1 {
                        return Err(SqlError::at(
                            "SC2207",
                            format!("The invalid escape character '{text}' was specified in a LIKE predicate."),
                            tok.start,
                            tok.end,
                        ));
                    }
                    Some(Box::new(self.parse_primary()?))
                } else {
                    None
                };
                left = self.mk(ExprKind::Like { expr: Box::new(left), pattern: Box::new(pattern), escape, negated }, start)?;
            } else {
                let op = match self.peek() {
                    Tok::Sym("=") => BinaryOp::Eq,
                    Tok::Sym("!=") | Tok::Sym("<>") => BinaryOp::Ne,
                    Tok::Sym("<") => BinaryOp::Lt,
                    Tok::Sym("<=") => BinaryOp::Le,
                    Tok::Sym(">") => BinaryOp::Gt,
                    Tok::Sym(">=") => BinaryOp::Ge,
                    _ => break,
                };
                self.advance();
                let right = self.parse_bitor()?;
                left = self.mk(ExprKind::Binary(op, Box::new(left), Box::new(right)), start)?;
            }
        }
        Ok(left)
    }

    /// Left-associative binary level.
    fn binary_level(&mut self, next: fn(&mut Self) -> Result<Expr, SqlError>, ops: &[(&str, BinaryOp)]) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let mut left = next(self)?;
        loop {
            let op = match self.peek() {
                Tok::Sym(s) => ops.iter().find(|(o, _)| o == s).map(|(_, op)| *op),
                _ => None,
            };
            match op {
                Some(op) => {
                    self.advance();
                    let right = next(self)?;
                    left = self.mk(ExprKind::Binary(op, Box::new(left), Box::new(right)), start)?;
                }
                None => return Ok(left),
            }
        }
    }

    fn parse_bitor(&mut self) -> Result<Expr, SqlError> {
        self.binary_level(Self::parse_bitxor, &[("|", BinaryOp::BitOr)])
    }
    fn parse_bitxor(&mut self) -> Result<Expr, SqlError> {
        self.binary_level(Self::parse_bitand, &[("^", BinaryOp::BitXor)])
    }
    fn parse_bitand(&mut self) -> Result<Expr, SqlError> {
        self.binary_level(Self::parse_shift, &[("&", BinaryOp::BitAnd)])
    }
    fn parse_shift(&mut self) -> Result<Expr, SqlError> {
        self.binary_level(Self::parse_additive, &[("<<", BinaryOp::Shl), (">>", BinaryOp::Shr), (">>>", BinaryOp::UShr)])
    }
    fn parse_additive(&mut self) -> Result<Expr, SqlError> {
        self.binary_level(Self::parse_multiplicative, &[("+", BinaryOp::Add), ("-", BinaryOp::Sub), ("||", BinaryOp::Concat)])
    }
    fn parse_multiplicative(&mut self) -> Result<Expr, SqlError> {
        self.binary_level(Self::parse_unary, &[("*", BinaryOp::Mul), ("/", BinaryOp::Div), ("%", BinaryOp::Mod)])
    }

    fn parse_unary(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let op = match self.peek() {
            Tok::Sym("-") => Some(UnaryOp::Neg),
            Tok::Sym("+") => Some(UnaryOp::Plus),
            Tok::Sym("~") => Some(UnaryOp::BitNot),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            self.enter()?;
            let inner = self.parse_unary();
            self.nest -= 1;
            let inner = inner?;
            // Fold `-<number>` so that -9223372036854775808 stays an exact integer.
            if op == UnaryOp::Neg {
                match &inner.kind {
                    ExprKind::Literal(Literal::Int(i)) => {
                        return self.mk(ExprKind::Literal(Literal::Int(-*i)), start);
                    }
                    // 9223372036854775808 does not fit i64 as written; -9223372036854775808 does.
                    ExprKind::Literal(Literal::Float(f)) if *f == i64::MAX as f64 + 1.0 => {
                        return self.mk(ExprKind::Literal(Literal::Int(i64::MIN)), start);
                    }
                    ExprKind::Literal(Literal::Float(f)) if !f.is_nan() => {
                        return self.mk(ExprKind::Literal(Literal::Float(-*f)), start);
                    }
                    _ => {}
                }
            }
            return self.mk(ExprKind::Unary(op, Box::new(inner)), start);
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        let mut e = self.parse_primary()?;
        loop {
            if self.is_sym(".") {
                self.advance();
                e = self.property_step(e, start)?;
            } else if self.is_sym("[") {
                self.advance();
                e = self.index_step(e, start)?;
            } else {
                return Ok(e);
            }
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, SqlError> {
        let start = self.cur_start();
        match self.peek().clone() {
            Tok::Int(i) => {
                self.advance();
                self.mk(ExprKind::Literal(Literal::Int(i)), start)
            }
            Tok::Float(f) => {
                self.advance();
                self.mk(ExprKind::Literal(Literal::Float(f)), start)
            }
            Tok::Str(s) => {
                self.advance();
                self.mk(ExprKind::Literal(Literal::Str(s)), start)
            }
            Tok::Param(p) => {
                self.advance();
                self.mk(ExprKind::Param(p), start)
            }
            Tok::FilterPlaceholder => {
                self.advance();
                self.mk(ExprKind::Literal(Literal::Bool(true)), start)
            }
            Tok::Sym("(") => {
                self.advance();
                if self.is_word("SELECT") {
                    let q = self.parse_query()?;
                    self.expect_sym(")")?;
                    return self.subquery_expr(q, start, SubKind::Scalar);
                }
                let e = self.parse_expr()?;
                self.expect_sym(")")?;
                Ok(Expr { span: Span { start, end: self.last_end }, ..e })
            }
            Tok::Sym("[") => {
                self.advance();
                let mut items = Vec::new();
                if !self.is_sym("]") {
                    loop {
                        items.push(self.parse_expr()?);
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                }
                self.expect_sym("]")?;
                self.mk(ExprKind::Array(items), start)
            }
            Tok::Sym("{") => {
                self.advance();
                let mut pairs: Vec<(String, Expr)> = Vec::new();
                if !self.is_sym("}") {
                    loop {
                        let key = match self.peek().clone() {
                            Tok::Word(w) if !is_reserved(&w) => w,
                            Tok::Str(s) => s,
                            _ => return Err(self.err_here()),
                        };
                        let key_tok = self.advance();
                        if pairs.iter().any(|(k, _)| *k == key) {
                            return Err(SqlError::at(
                                "SC2022",
                                format!("Object creation error, property name '{key}' specified more than once."),
                                key_tok.start,
                                key_tok.end,
                            ));
                        }
                        self.expect_sym(":")?;
                        pairs.push((key, self.parse_expr()?));
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                }
                self.expect_sym("}")?;
                self.mk(ExprKind::Object(pairs), start)
            }
            Tok::Word(w) => self.parse_word(w, start),
            _ => Err(self.err_here()),
        }
    }

    fn subquery_expr(&mut self, q: Query, start: usize, kind: SubKind) -> Result<Expr, SqlError> {
        let inner = max_query_depth(&q);
        let b = Box::new(q);
        let k = match kind {
            SubKind::Scalar => ExprKind::Subquery(b),
            SubKind::Exists => ExprKind::Exists(b),
            SubKind::Array => ExprKind::ArraySubquery(b),
        };
        let mut e = self.mk(k, start)?;
        e.depth = e.depth.max(inner.saturating_add(1));
        if e.depth > MAX_EXPR_DEPTH {
            return Err(too_complex(start, self.last_end));
        }
        Ok(e)
    }

    fn parse_word(&mut self, w: String, start: usize) -> Result<Expr, SqlError> {
        let up = w.to_ascii_uppercase();
        match up.as_str() {
            "TRUE" | "FALSE" | "NULL" | "UNDEFINED" => {
                self.advance();
                let lit = match up.as_str() {
                    "TRUE" => Literal::Bool(true),
                    "FALSE" => Literal::Bool(false),
                    "NULL" => Literal::Null,
                    _ => Literal::Undefined,
                };
                return self.mk(ExprKind::Literal(lit), start);
            }
            "EXISTS" | "ARRAY" if self.is_sym_at(1, "(") => {
                self.advance();
                self.advance();
                if !self.is_word("SELECT") {
                    return Err(self.err_here());
                }
                let q = self.parse_query()?;
                self.expect_sym(")")?;
                let kind = if up == "EXISTS" { SubKind::Exists } else { SubKind::Array };
                return self.subquery_expr(q, start, kind);
            }
            _ => {}
        }
        if (w == "NaN" || w == "Infinity") && !self.is_sym_at(1, "(") {
            self.advance();
            let v = if w == "NaN" { f64::NAN } else { f64::INFINITY };
            return self.mk(ExprKind::Literal(Literal::Float(v)), start);
        }
        if w == "udf" && !self.is_sym_at(1, ".") {
            // the keyword is followed by `.name(`; the error is at the token after it
            self.advance();
            return Err(self.err_here());
        }
        if w == "udf" && self.is_sym_at(1, ".") {
            let name_tok = self.toks[(self.pos + 2).min(self.toks.len() - 1)].clone();
            let name = match &name_tok.tok {
                Tok::Word(n) => n.clone(),
                _ => {
                    self.advance();
                    self.advance();
                    return Err(self.err_here());
                }
            };
            if !self.is_sym_at(3, "(") {
                self.advance();
                self.advance();
                self.advance();
                return Err(self.err_here());
            }
            self.advance();
            self.advance();
            self.advance();
            self.advance();
            let mut args = Vec::new();
            if !self.is_sym(")") {
                loop {
                    args.push(self.parse_expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
            }
            self.expect_sym(")")?;
            return self.mk(ExprKind::Udf { name, args }, start);
        }
        if is_reserved(&w) && !(self.is_sym_at(1, "(") && functions::lookup(&up).is_some()) {
            return Err(self.err_here());
        }
        if self.is_sym_at(1, "(") {
            return self.parse_call(w, up, start);
        }
        self.advance();
        self.mk(ExprKind::Ident(w), start)
    }

    fn parse_call(&mut self, name: String, up: String, start: usize) -> Result<Expr, SqlError> {
        // a call counts as a level of its own on top of its arguments: Azure takes about 500 nested calls, 996 parentheses
        self.enter()?;
        let r = self.parse_call_inner(name, up, start);
        self.nest -= 1;
        r
    }

    fn parse_call_inner(&mut self, name: String, up: String, start: usize) -> Result<Expr, SqlError> {
        let name_end = self.toks[self.pos].end;
        self.advance();
        self.advance();
        let mut args = Vec::new();
        if !self.is_sym(")") {
            loop {
                args.push(self.parse_expr()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        self.expect_sym(")")?;

        let agg = match up.as_str() {
            "COUNT" => Some(AggregateKind::Count),
            "SUM" => Some(AggregateKind::Sum),
            "AVG" => Some(AggregateKind::Average),
            "MIN" => Some(AggregateKind::Min),
            "MAX" => Some(AggregateKind::Max),
            _ => None,
        };
        if let Some(kind) = agg {
            if args.len() != 1 {
                return Err(SqlError::semantic("SC2050", format!("The {up} function requires 1 argument(s)."), start, self.last_end));
            }
            let arg = Box::new(args.remove(0));
            return self.mk(ExprKind::Aggregate { kind, arg }, start);
        }

        let spec = match functions::lookup(&up) {
            Some(s) => s,
            None => {
                return Err(SqlError::semantic("SC2005", format!("'{name}' is not a recognized built-in function name."), start, name_end))
            }
        };
        if spec.unsupported {
            return Err(SqlError::not_supported(&format!("Function {}", spec.name), start, self.last_end));
        }
        let n = args.len();
        if n < spec.min as usize || (spec.max != functions::VARIADIC && n > spec.max as usize) {
            let req = if spec.min == spec.max {
                format!("{}", spec.min)
            } else if spec.max == functions::VARIADIC {
                format!("at least {}", spec.min)
            } else {
                format!("{} to {}", spec.min, spec.max)
            };
            return Err(SqlError::semantic(
                "SC2050",
                format!("The {} function requires {req} argument(s).", spec.name),
                start,
                self.last_end,
            ));
        }
        self.mk(ExprKind::Call { name, func: FuncRef(spec), args }, start)
    }
}

/// Which count a number follows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Count {
    Top,
    Offset,
    Limit,
}

impl Count {
    fn limit_error(self) -> (&'static str, &'static str) {
        match self {
            Count::Top => ("SC2060", "The count value provided for a TOP clause must be an integer."),
            Count::Offset => ("SC2062", "The OFFSET count value exceeds the maximum allowed value."),
            Count::Limit => ("SC2061", "The LIMIT count value exceeds the maximum allowed value."),
        }
    }
}

/// What Azure answers for a query nested too deeply (`SC1030`); the kind stays a limit.
fn too_complex(start: usize, end: usize) -> SqlError {
    let mut e = SqlError::at(
        "SC1030",
        "The query is too complex to be processed. To resolve this issue, please consider breaking the query into multiple smaller queries, simplifying the complex expression, and/or using Computed Properties."
            .to_string(),
        start,
        end,
    );
    e.kind = crate::error::ErrorKind::LimitExceeded;
    e
}

/// Two select items with the same name are refused (`SELECT c.id, c.id`); the error sits on the name of the second.
fn check_duplicate_names(items: &[SelectItem]) -> Result<(), SqlError> {
    let mut seen: Vec<String> = Vec::new();
    for it in items {
        let (name, span) = match (&it.alias, it.alias_span) {
            (Some(a), Some(sp)) => (a.clone(), sp),
            _ => match &it.expr.kind {
                ExprKind::Property(_, n) => {
                    (n.clone(), Span { start: it.expr.span.end.saturating_sub(n.chars().count()), end: it.expr.span.end })
                }
                ExprKind::Index(_, i) => match &i.kind {
                    ExprKind::Literal(Literal::Str(s)) => (s.clone(), i.span),
                    _ => continue,
                },
                ExprKind::Ident(n) => (n.clone(), it.expr.span),
                _ => continue,
            },
        };
        if seen.contains(&name) {
            return Err(SqlError::at(
                "SC2022",
                format!("Object creation error, property name '{name}' specified more than once."),
                span.start,
                span.end,
            ));
        }
        seen.push(name);
    }
    Ok(())
}

enum SubKind {
    Scalar,
    Exists,
    Array,
}

fn kind_depth(kind: &ExprKind) -> u16 {
    kind.children().iter().map(|c| c.depth).max().unwrap_or(0)
}

/// Largest expression depth anywhere in a query, including nested queries.
pub(crate) fn max_query_depth(q: &Query) -> u16 {
    let mut m = q.exprs().iter().map(|e| e.depth).max().unwrap_or(0);
    if let Some(f) = &q.from {
        for s in &f.sources {
            if let SourceExpr::Subquery(sq) = &s.source {
                m = m.max(max_query_depth(sq));
            }
        }
    }
    m
}
