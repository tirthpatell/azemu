//! Recursive-descent parser for expressions (filters and action values).
//!
//! Precedence, loosest to tightest: `OR`, `AND`, `NOT`, comparison / `IN` /
//! `LIKE` / `IS NULL`, `+ -`, `* / %`, unary `+ -`, primary.

use crate::ast::{BinOp, Expr, PropRef};
use crate::error::ParseError;
use crate::lexer::{lex, Tok, Token};
use crate::value::{Scope, Value};
use crate::{MAX_DEPTH, MAX_SQL_LEN};

const RESERVED: &[&str] = &["AND", "OR", "NOT", "IN", "LIKE", "ESCAPE", "IS", "NULL", "EXISTS", "TRUE", "FALSE"];

fn is_reserved(w: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(w))
}

pub(crate) struct Parser {
    toks: Vec<Token>,
    i: usize,
    depth: usize,
}

impl Parser {
    pub(crate) fn new(src: &str, what: &str) -> Result<Parser, ParseError> {
        if src.chars().count() > MAX_SQL_LEN {
            return Err(ParseError::new(format!("{what} is longer than the maximum of {MAX_SQL_LEN} characters"), MAX_SQL_LEN));
        }
        Ok(Parser { toks: lex(src)?, i: 0, depth: 0 })
    }

    pub(crate) fn peek(&self) -> &Tok {
        self.toks.get(self.i).map(|t| &t.tok).unwrap_or(&Tok::Eof)
    }

    pub(crate) fn peek_at(&self, n: usize) -> &Tok {
        self.toks.get(self.i + n).map(|t| &t.tok).unwrap_or(&Tok::Eof)
    }

    pub(crate) fn pos(&self) -> usize {
        self.toks.get(self.i).or_else(|| self.toks.last()).map(|t| t.pos).unwrap_or(0)
    }

    pub(crate) fn advance(&mut self) {
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
    }

    pub(crate) fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Word(x) if x.eq_ignore_ascii_case(w))
    }

    fn is_word_at(&self, n: usize, w: &str) -> bool {
        matches!(self.peek_at(n), Tok::Word(x) if x.eq_ignore_ascii_case(w))
    }

    pub(crate) fn err<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError::new(msg, self.pos()))
    }

    pub(crate) fn expect(&mut self, t: Tok, what: &str) -> Result<(), ParseError> {
        if *self.peek() == t {
            self.advance();
            Ok(())
        } else {
            self.err(format!("expected {what} but found {}", self.peek().describe()))
        }
    }

    pub(crate) fn expect_eof(&self) -> Result<(), ParseError> {
        if *self.peek() == Tok::Eof {
            Ok(())
        } else {
            self.err(format!("unexpected {}", self.peek().describe()))
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err(format!("expression is nested deeper than {MAX_DEPTH} levels"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    pub(crate) fn parse_or(&mut self) -> Result<Expr, ParseError> {
        self.enter()?;
        let mut left = self.parse_and()?;
        while self.is_word("OR") {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(right));
        }
        self.leave();
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_not()?;
        while self.is_word("AND") {
            self.advance();
            let right = self.parse_not()?;
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, ParseError> {
        if self.is_word("NOT") {
            self.advance();
            self.enter()?;
            let e = self.parse_not()?;
            self.leave();
            return Ok(Expr::Not(Box::new(e)));
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Expr, ParseError> {
        let left = self.parse_add()?;
        let op = match self.peek() {
            Tok::Eq => Some(BinOp::Eq),
            Tok::Ne => Some(BinOp::Ne),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Le => Some(BinOp::Le),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Ge => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let right = self.parse_add()?;
            return Ok(Expr::Binary(op, Box::new(left), Box::new(right)));
        }
        if self.is_word("IS") {
            self.advance();
            let negated = if self.is_word("NOT") {
                self.advance();
                true
            } else {
                false
            };
            if !self.is_word("NULL") {
                return self.err(format!("expected NULL after IS but found {}", self.peek().describe()));
            }
            self.advance();
            return Ok(Expr::IsNull { expr: Box::new(left), negated });
        }
        let negated = if self.is_word("NOT") && (self.is_word_at(1, "IN") || self.is_word_at(1, "LIKE")) {
            self.advance();
            true
        } else {
            false
        };
        if self.is_word("IN") {
            self.advance();
            self.expect(Tok::LParen, "'(' after IN")?;
            let mut list = vec![self.parse_add()?];
            while *self.peek() == Tok::Comma {
                self.advance();
                list.push(self.parse_add()?);
            }
            self.expect(Tok::RParen, "')' to close the IN list")?;
            return Ok(Expr::In { expr: Box::new(left), list, negated });
        }
        if self.is_word("LIKE") {
            self.advance();
            let pattern = self.parse_add()?;
            let escape = if self.is_word("ESCAPE") {
                self.advance();
                Some(Box::new(self.parse_add()?))
            } else {
                None
            };
            return Ok(Expr::Like { expr: Box::new(left), pattern: Box::new(pattern), escape, negated });
        }
        Ok(left)
    }

    /// Additive expression: the value grammar used on the right of `SET`.
    pub(crate) fn parse_add(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_mul()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_mul(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Rem,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        match self.peek() {
            Tok::Minus => {
                self.advance();
                // -9223372036854775808 is the one literal whose magnitude
                // does not fit in an i64 before negation.
                if let Tok::Int(n) = self.peek() {
                    if *n == 1u64 << 63 {
                        self.advance();
                        return Ok(Expr::Lit(Value::Long(i64::MIN)));
                    }
                }
                self.enter()?;
                let e = self.parse_unary()?;
                self.leave();
                Ok(Expr::Neg(Box::new(e)))
            }
            Tok::Plus => {
                self.advance();
                self.enter()?;
                let e = self.parse_unary()?;
                self.leave();
                Ok(Expr::Plus(Box::new(e)))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let pos = self.pos();
        match self.peek().clone() {
            Tok::Int(n) => match i64::try_from(n) {
                Ok(v) => {
                    self.advance();
                    Ok(Expr::Lit(Value::Long(v)))
                }
                Err(_) => Err(ParseError::new("integer constant out of range", pos)),
            },
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Lit(Value::Double(f)))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Lit(Value::Str(s)))
            }
            Tok::Param(p) => {
                self.advance();
                Ok(Expr::Param(p))
            }
            Tok::LParen => {
                self.advance();
                let e = self.parse_or()?;
                self.expect(Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::Bracket(_) | Tok::Quoted(_) => Ok(Expr::Prop(self.parse_prop_ref()?)),
            Tok::Word(w) => self.parse_word(&w),
            t => Err(ParseError::new(format!("unexpected {}", t.describe()), pos)),
        }
    }

    fn parse_word(&mut self, w: &str) -> Result<Expr, ParseError> {
        let upper = w.to_ascii_uppercase();
        match upper.as_str() {
            "TRUE" => {
                self.advance();
                return Ok(Expr::Lit(Value::Bool(true)));
            }
            "FALSE" => {
                self.advance();
                return Ok(Expr::Lit(Value::Bool(false)));
            }
            "NULL" => {
                self.advance();
                return Ok(Expr::Lit(Value::Null));
            }
            "EXISTS" => {
                self.advance();
                self.expect(Tok::LParen, "'(' after EXISTS")?;
                let p = self.parse_prop_ref()?;
                self.expect(Tok::RParen, "')' after EXISTS(property")?;
                return Ok(Expr::Exists(p));
            }
            _ => {}
        }
        if is_reserved(w) {
            return self.err(format!("unexpected keyword '{w}'"));
        }
        if *self.peek_at(1) == Tok::LParen {
            return self.parse_function(&upper);
        }
        Ok(Expr::Prop(self.parse_prop_ref()?))
    }

    fn parse_function(&mut self, upper: &str) -> Result<Expr, ParseError> {
        let name_pos = self.pos();
        match upper {
            "NEWID" => {
                self.advance();
                self.advance();
                self.expect(Tok::RParen, "')' (newid() takes no arguments)")?;
                Ok(Expr::NewId)
            }
            "PROPERTY" | "P" => {
                self.advance();
                self.advance();
                let arg = self.parse_or()?;
                self.expect(Tok::RParen, "')' after the function argument")?;
                Ok(Expr::PropertyFn(Box::new(arg)))
            }
            _ => Err(ParseError::new(format!("unknown function '{}'", upper.to_lowercase()), name_pos)),
        }
    }

    /// `[scope .] name` where name is a regular, `[bracketed]` or
    /// `"quoted"` identifier.
    pub(crate) fn parse_prop_ref(&mut self) -> Result<PropRef, ParseError> {
        let mut scope = Scope::User;
        if let Tok::Word(w) = self.peek() {
            if *self.peek_at(1) == Tok::Dot {
                if w.eq_ignore_ascii_case("sys") {
                    scope = Scope::Sys;
                } else if w.eq_ignore_ascii_case("user") {
                    scope = Scope::User;
                } else {
                    return self.err(format!("unknown scope '{w}'; use sys. or user. (wrap names containing '.' in [ ])"));
                }
                self.advance();
                self.advance();
            }
        }
        let name = match self.peek().clone() {
            Tok::Word(w) => {
                if is_reserved(&w) {
                    return self.err(format!("'{w}' is a reserved keyword; wrap it in [ ] to use it as a property name"));
                }
                w
            }
            Tok::Bracket(s) | Tok::Quoted(s) => s,
            t => return self.err(format!("expected a property name but found {}", t.describe())),
        };
        self.advance();
        Ok(PropRef { scope, name })
    }
}

/// Parses a complete filter predicate.
pub(crate) fn parse_filter(sql: &str) -> Result<Expr, ParseError> {
    let mut p = Parser::new(sql, "filter")?;
    if *p.peek() == Tok::Eof {
        return Err(ParseError::new("the filter is empty", 0));
    }
    let e = p.parse_or()?;
    p.expect_eof()?;
    match &e {
        Expr::Lit(Value::Bool(_)) | Expr::Param(_) | Expr::Prop(_) | Expr::PropertyFn(_) => {}
        Expr::Lit(_) | Expr::NewId | Expr::Neg(_) | Expr::Plus(_) => {
            return Err(ParseError::new("the filter must be a boolean expression", 0))
        }
        Expr::Binary(BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem, _, _) => {
            return Err(ParseError::new("the filter must be a boolean expression", 0))
        }
        _ => {}
    }
    Ok(e)
}
