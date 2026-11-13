//! OData `$filter` parser and evaluator for Table Storage.
//!
//! Grammar (lowest to highest precedence): `or`, `and`, `not`, comparison,
//! parenthesised expression. A comparison is `operand op operand` with
//! `eq ne gt ge lt le`; an operand is a property name or a literal. Literals:
//! `'text'` (`''` is a quote), `12`, `12L`, `1.5`, `1e3`, `true`, `false`,
//! `null`, `datetime'..'`, `guid'..'`, `binary'..'`, `X'..'`, `NaN`, `INF`.
//!
//! Evaluation rules: strings compare by UTF-16 code units; numbers compare
//! across Int32/Int64/Double; a property the entity lacks, or has with a
//! different type than the literal, makes the comparison false (for every
//! operator, `ne` included) instead of failing.

use std::borrow::Cow;
use std::cmp::Ordering;

use crate::values::{parse_datetime, parse_guid, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Prop(String),
    Lit(Value),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp(Operand, Op, Operand),
    /// A lone operand, true only for the boolean `true`.
    Bare(Operand),
}

/// A syntax error. `pos` is the offset (in characters) where the parser gave up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterError {
    pub pos: usize,
}

impl FilterError {
    pub fn message(&self, filter: &str) -> String {
        format!("Syntax error at position {} in '{}'.", self.pos, filter)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    /// Typed literal prefix and body: `datetime'..'`.
    Typed(String, String),
    Num(String),
    LParen,
    RParen,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    end: usize,
}

fn lex(s: &str) -> Result<Vec<Token>, FilterError> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let is_start = |c: char| c.is_alphabetic() || c == '_';
    let is_part = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            i += 1;
            out.push(Token { tok: Tok::LParen, end: i });
        } else if c == ')' {
            i += 1;
            out.push(Token { tok: Tok::RParen, end: i });
        } else if c == '\'' {
            let (text, next) = read_quoted(&chars, i)?;
            i = next;
            out.push(Token { tok: Tok::Str(text), end: i });
        } else if is_start(c) {
            let start = i;
            while i < chars.len() && is_part(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if i < chars.len() && chars[i] == '\'' {
                let (text, next) = read_quoted(&chars, i)?;
                i = next;
                out.push(Token { tok: Tok::Typed(word, text), end: i });
            } else {
                out.push(Token { tok: Tok::Ident(word), end: i });
            }
        } else if c.is_ascii_digit() || (c == '-' && i + 1 < chars.len() && (chars[i + 1].is_ascii_digit() || chars[i + 1] == 'I')) {
            let start = i;
            i += 1;
            if c == '-' && chars[i] == 'I' {
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    i += 1;
                }
            } else {
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric()
                        || chars[i] == '.'
                        || ((chars[i] == '+' || chars[i] == '-') && matches!(chars[i - 1], 'e' | 'E')))
                {
                    i += 1;
                }
            }
            out.push(Token { tok: Tok::Num(chars[start..i].iter().collect()), end: i });
        } else {
            return Err(FilterError { pos: i + 1 });
        }
    }
    Ok(out)
}

/// Read a `'...'` literal starting at the opening quote; `''` is a quote.
fn read_quoted(chars: &[char], start: usize) -> Result<(String, usize), FilterError> {
    let mut i = start + 1;
    let mut text = String::new();
    loop {
        match chars.get(i) {
            None => return Err(FilterError { pos: chars.len() }),
            Some('\'') => {
                if chars.get(i + 1) == Some(&'\'') {
                    text.push('\'');
                    i += 2;
                } else {
                    return Ok((text, i + 1));
                }
            }
            Some(c) => {
                text.push(*c);
                i += 1;
            }
        }
    }
}

/// Nesting and size limits keep a hostile filter from exhausting the stack.
const MAX_DEPTH: usize = 100;
const MAX_COMPARISONS: usize = 100;

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    len: usize,
    depth: usize,
    comparisons: usize,
}

/// Parse a filter expression.
pub fn parse(filter: &str) -> Result<Expr, FilterError> {
    let toks = lex(filter)?;
    let mut p = Parser { toks, pos: 0, len: filter.chars().count(), depth: 0, comparisons: 0 };
    if p.toks.is_empty() {
        return Err(FilterError { pos: p.len });
    }
    let e = p.or()?;
    if p.pos != p.toks.len() {
        return Err(p.fail());
    }
    Ok(e)
}

impl Parser {
    /// Error at the end of the token being looked at (or at the end of input).
    fn fail(&self) -> FilterError {
        FilterError { pos: self.toks.get(self.pos).map(|t| t.end).unwrap_or(self.len) }
    }

    fn peek_ident(&self, word: &str) -> bool {
        matches!(self.toks.get(self.pos), Some(Token { tok: Tok::Ident(w), .. }) if w == word)
    }

    fn or(&mut self) -> Result<Expr, FilterError> {
        let mut left = self.and()?;
        while self.peek_ident("or") {
            self.pos += 1;
            let right = self.and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, FilterError> {
        let mut left = self.not()?;
        while self.peek_ident("and") {
            self.pos += 1;
            let right = self.not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, FilterError> {
        if self.peek_ident("not") {
            // `not` followed by an operator is a property called "not"
            let is_prop = matches!(self.toks.get(self.pos + 1), Some(Token { tok: Tok::Ident(w), .. }) if is_cmp_word(w));
            if !is_prop {
                self.pos += 1;
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err(self.fail());
                }
                let inner = self.not()?;
                self.depth -= 1;
                return Ok(Expr::Not(Box::new(inner)));
            }
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Expr, FilterError> {
        if matches!(self.toks.get(self.pos), Some(Token { tok: Tok::LParen, .. })) {
            self.pos += 1;
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                return Err(self.fail());
            }
            let e = self.or()?;
            self.depth -= 1;
            if !matches!(self.toks.get(self.pos), Some(Token { tok: Tok::RParen, .. })) {
                return Err(self.fail());
            }
            self.pos += 1;
            return Ok(e);
        }
        self.comparisons += 1;
        if self.comparisons > MAX_COMPARISONS {
            return Err(self.fail());
        }
        let left = self.operand()?;
        let op = match self.toks.get(self.pos) {
            Some(Token { tok: Tok::Ident(w), .. }) if is_cmp_word(w) => match w.as_str() {
                "eq" => Op::Eq,
                "ne" => Op::Ne,
                "gt" => Op::Gt,
                "ge" => Op::Ge,
                "lt" => Op::Lt,
                _ => Op::Le,
            },
            Some(Token { tok: Tok::Ident(w), .. }) if w != "and" && w != "or" => return Err(self.fail()),
            Some(Token { tok: Tok::Str(_) | Tok::Num(_) | Tok::Typed(..) | Tok::LParen, .. }) => return Err(self.fail()),
            _ => return Ok(Expr::Bare(left)),
        };
        self.pos += 1;
        let right = self.operand()?;
        Ok(Expr::Cmp(left, op, right))
    }

    fn operand(&mut self) -> Result<Operand, FilterError> {
        let Some(t) = self.toks.get(self.pos).cloned() else {
            return Err(self.fail());
        };
        let bad = FilterError { pos: t.end };
        let o = match t.tok {
            Tok::Str(s) => Operand::Lit(Value::Str(s)),
            Tok::Typed(prefix, body) => match prefix.as_str() {
                "datetime" => Operand::Lit(Value::DateTime(parse_datetime(&body).ok_or(bad)?)),
                "guid" => Operand::Lit(Value::Guid(parse_guid(&body).ok_or(bad)?)),
                "binary" | "X" => Operand::Lit(Value::Binary(parse_hex(&body).ok_or(bad)?)),
                _ => return Err(bad),
            },
            Tok::Num(n) => Operand::Lit(parse_number(&n).ok_or(bad)?),
            Tok::Ident(w) => match w.as_str() {
                "true" => Operand::Lit(Value::Bool(true)),
                "false" => Operand::Lit(Value::Bool(false)),
                "null" => Operand::Null,
                "NaN" => Operand::Lit(Value::Double(f64::NAN)),
                "INF" => Operand::Lit(Value::Double(f64::INFINITY)),
                "and" | "or" | "eq" | "ne" | "gt" | "ge" | "lt" | "le" => return Err(bad),
                _ => Operand::Prop(w),
            },
            Tok::LParen | Tok::RParen => return Err(bad),
        };
        self.pos += 1;
        Ok(o)
    }
}

fn is_cmp_word(w: &str) -> bool {
    matches!(w, "eq" | "ne" | "gt" | "ge" | "lt" | "le")
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn parse_number(n: &str) -> Option<Value> {
    if n == "-INF" {
        return Some(Value::Double(f64::NEG_INFINITY));
    }
    let (body, suffix) = match n.chars().last()? {
        c @ ('L' | 'l' | 'D' | 'd' | 'F' | 'f' | 'M' | 'm') => (&n[..n.len() - 1], Some(c.to_ascii_lowercase())),
        _ => (n, None),
    };
    match suffix {
        Some('l') => body.parse::<i64>().ok().map(Value::Int64),
        Some(_) => body.parse::<f64>().ok().map(Value::Double),
        None => {
            if body.contains(['.', 'e', 'E']) {
                body.parse::<f64>().ok().map(Value::Double)
            } else if let Ok(i) = body.parse::<i32>() {
                Some(Value::Int32(i))
            } else {
                body.parse::<i64>().ok().map(Value::Int64)
            }
        }
    }
}

/// Where property values come from during evaluation.
pub trait Lookup {
    fn get(&self, name: &str) -> Option<Cow<'_, Value>>;
}

/// Compare two values; `None` when the types do not compare.
pub fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Str(x), Str(y)) => Some(x.encode_utf16().cmp(y.encode_utf16())),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (DateTime(x), DateTime(y)) => Some(x.cmp(y)),
        (Guid(x), Guid(y)) => Some(x.cmp(y)),
        (Binary(x), Binary(y)) => Some(x.cmp(y)),
        (Int32(_) | Int64(_) | Double(_), Int32(_) | Int64(_) | Double(_)) => match (as_int(a), as_int(b)) {
            (Some(x), Some(y)) => Some(x.cmp(&y)),
            _ => as_f64(a).partial_cmp(&as_f64(b)),
        },
        _ => None,
    }
}

fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(i) => Some(*i as i64),
        Value::Int64(i) => Some(*i),
        _ => None,
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int32(i) => *i as f64,
        Value::Int64(i) => *i as f64,
        Value::Double(f) => *f,
        _ => f64::NAN,
    }
}

fn resolve<'a>(o: &'a Operand, ent: &'a dyn Lookup) -> Option<Option<Cow<'a, Value>>> {
    match o {
        Operand::Lit(v) => Some(Some(Cow::Borrowed(v))),
        Operand::Prop(p) => Some(ent.get(p)),
        Operand::Null => None,
    }
}

/// Evaluate a parsed filter against one entity.
pub fn eval(e: &Expr, ent: &dyn Lookup) -> bool {
    match e {
        Expr::Or(a, b) => eval(a, ent) || eval(b, ent),
        Expr::And(a, b) => eval(a, ent) && eval(b, ent),
        Expr::Not(a) => !eval(a, ent),
        Expr::Bare(o) => matches!(resolve(o, ent), Some(Some(v)) if *v == Value::Bool(true)),
        Expr::Cmp(l, op, r) => {
            let (lv, rv) = (resolve(l, ent), resolve(r, ent));
            match (lv, rv) {
                // comparison with null: only eq / ne mean something
                (None, None) => matches!(op, Op::Eq),
                (None, Some(x)) | (Some(x), None) => match op {
                    Op::Eq => x.is_none(),
                    Op::Ne => x.is_some(),
                    _ => false,
                },
                (Some(Some(a)), Some(Some(b))) => match compare(&a, &b) {
                    None => false,
                    Some(ord) => match op {
                        Op::Eq => ord == Ordering::Equal,
                        Op::Ne => ord != Ordering::Equal,
                        Op::Gt => ord == Ordering::Greater,
                        Op::Ge => ord != Ordering::Less,
                        Op::Lt => ord == Ordering::Less,
                        Op::Le => ord != Ordering::Greater,
                    },
                },
                _ => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Row(HashMap<&'static str, Value>);
    impl Lookup for Row {
        fn get(&self, name: &str) -> Option<Cow<'_, Value>> {
            self.0.get(name).map(Cow::Borrowed)
        }
    }

    fn row() -> Row {
        let mut m = HashMap::new();
        m.insert("name", Value::Str("Alice".into()));
        m.insert("age", Value::Int32(30));
        m.insert("big", Value::Int64(10_000_000_000));
        m.insert("score", Value::Double(1.5));
        m.insert("on", Value::Bool(true));
        m.insert("born", Value::DateTime(parse_datetime("2000-01-01T00:00:00Z").unwrap()));
        m.insert("uid", Value::Guid("c9da6455-213d-42c9-9a79-3e9149a57833".into()));
        m.insert("raw", Value::Binary(vec![0x61]));
        m.insert("empty", Value::Str(String::new()));
        Row(m)
    }

    fn t(f: &str) -> bool {
        eval(&parse(f).unwrap_or_else(|e| panic!("{f}: {e:?}")), &row())
    }

    #[test]
    fn comparison_operators() {
        assert!(t("age eq 30"));
        assert!(!t("age ne 30"));
        assert!(t("age ge 30") && t("age le 30") && !t("age gt 30") && !t("age lt 30"));
        assert!(t("age gt -5"));
        assert!(t("name eq 'Alice'") && !t("name eq 'alice'"));
        assert!(t("name lt 'a'") && t("name gt 'Alic'"));
        assert!(t("empty eq ''"));
    }

    #[test]
    fn numeric_types_compare_with_each_other() {
        assert!(t("big eq 10000000000L") && t("big gt 5L") && t("big gt 5"));
        assert!(t("score eq 1.5") && t("score gt 1") && t("age lt 30.5"));
        assert!(t("score lt 1e3") && t("score eq 1.50"));
        assert!(!t("score eq 'x'"));
    }

    #[test]
    fn typed_literals() {
        assert!(t("born eq datetime'2000-01-01T00:00:00Z'"));
        assert!(t("born eq datetime'2000-01-01T00:00:00.0000000Z'"));
        assert!(t("born lt datetime'2001-01-01T00:00:00Z'"));
        assert!(t("uid eq guid'C9DA6455-213D-42C9-9A79-3E9149A57833'"));
        assert!(t("raw eq X'61'") && t("raw eq binary'61'") && !t("raw eq X'62'"));
        assert!(t("on eq true") && !t("on eq false") && t("on ne false"));
    }

    #[test]
    fn boolean_logic_and_precedence() {
        assert!(t("age eq 1 or age eq 30"));
        assert!(t("age eq 1 or age eq 30 and name eq 'Alice'"));
        assert!(!t("(age eq 1 or age eq 30) and name eq 'Bob'"));
        assert!(t("not (age gt 30)"));
        assert!(t("not age gt 30"));
        assert!(t("not (name eq 'x' and (age lt 30 or age gt 30))"));
        assert!(t("on"));
        assert!(t("true"));
    }

    #[test]
    fn missing_or_mistyped_properties_do_not_match() {
        assert!(!t("ghost eq 'x'") && !t("ghost ne 'x'") && !t("ghost gt 1"));
        assert!(!t("age eq '30'") && !t("age ne '30'"));
        assert!(t("ghost eq 'x' or age eq 30"));
        assert!(!t("name eq 5"));
    }

    #[test]
    fn null_literal() {
        assert!(t("ghost eq null") && !t("name eq null") && t("name ne null"));
    }

    #[test]
    fn string_escaping_and_unicode() {
        let e = parse("name eq 'O''Brien'").unwrap();
        assert_eq!(e, Expr::Cmp(Operand::Prop("name".into()), Op::Eq, Operand::Lit(Value::Str("O'Brien".into()))));
        let e = parse("name eq 'héllo 你好'").unwrap();
        assert!(matches!(e, Expr::Cmp(_, _, Operand::Lit(Value::Str(s))) if s == "héllo 你好"));
        // an escaped quote cannot break out of the literal
        let e = parse("name eq 'x'' or name ne ''x'").unwrap();
        assert!(matches!(e, Expr::Cmp(..)));
    }

    #[test]
    fn utf16_ordering() {
        // U+FFFD (one unit 0xFFFD) sorts after U+1F642 (surrogate pair 0xD83D..)
        assert_eq!(compare(&Value::Str("\u{fffd}".into()), &Value::Str("\u{1f642}".into())), Some(Ordering::Greater));
        assert_eq!(compare(&Value::Str("Z".into()), &Value::Str("a".into())), Some(Ordering::Less));
    }

    #[test]
    fn syntax_errors() {
        for bad in [
            "aaa bbb ccc",
            "name eq",
            "(name eq 'x'",
            "name eq 'x')",
            "name equals 'x'",
            "eq 'x'",
            "name eq 'x' and",
            "and name eq 'x'",
            "name eq 'unterminated",
            "name eq 'O'Brien'",
            "",
            "name eq datetime'nope'",
            "name eq guid'x'",
            "name eq X'abc'",
            "name eq 12x",
            "name eq @param",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn error_position_matches_the_service_for_the_recorded_case() {
        let e = parse("aaa bbb ccc").unwrap_err();
        assert_eq!(e.message("aaa bbb ccc"), "Syntax error at position 7 in 'aaa bbb ccc'.");
    }

    #[test]
    fn number_forms() {
        for (f, want) in [
            ("age eq 30", true),
            ("age eq 30.0", true),
            ("age eq 3.0E1", true),
            ("age eq 30L", true),
            ("age eq 30D", true),
            ("age eq 30f", true),
            ("score eq 1.5f", true),
            ("age gt -1e2", true),
            ("score gt -INF", true),
            ("score lt INF", true),
            ("score eq NaN", false),
            ("score ne NaN", false),
            ("big gt 2147483648", true),
            ("big lt 9223372036854775807L", true),
        ] {
            assert_eq!(t(f), want, "{f}");
        }
    }

    #[test]
    fn whitespace_and_property_names() {
        assert!(t("  age   eq   30  "));
        assert!(t("(age eq 30)"));
        assert!(t("((((age eq 30))))"));
        assert!(t("not(age eq 31)"));
        assert!(t("age eq 30 and(name eq 'Alice')"));
        // keyword-like property names are still properties
        let e = parse("not eq 1").unwrap();
        assert_eq!(e, Expr::Cmp(Operand::Prop("not".into()), Op::Eq, Operand::Lit(Value::Int32(1))));
        let e = parse("_a_b1 eq 1").unwrap();
        assert!(matches!(e, Expr::Cmp(Operand::Prop(p), _, _) if p == "_a_b1"));
        // unicode names and values
        assert!(matches!(parse("啊齄 eq '你好'").unwrap(), Expr::Cmp(Operand::Prop(p), _, _) if p == "啊齄"));
    }

    #[test]
    fn literal_on_the_left_and_property_to_property() {
        assert!(t("30 eq age"));
        assert!(t("'Alice' eq name"));
        assert!(t("age eq age"));
        assert!(t("age ge age and name le name"));
        assert!(!t("age lt age"));
        assert!(t("1 lt 2"));
    }

    #[test]
    fn datetime_literal_forms() {
        assert!(t("born eq datetime'2000-01-01'"));
        assert!(t("born eq datetime'2000-01-01T00:00:00+00:00'"));
        assert!(t("born eq datetime'1999-12-31T19:00:00-05:00'"));
        assert!(t("born gt datetime'1999-12-31T23:59:59.9999999Z'"));
        assert!(!t("born gt datetime'2000-01-01T00:00:00.0000000Z'"));
    }

    #[test]
    fn empty_and_odd_binary_literals() {
        let e = parse("raw eq X''").unwrap();
        assert!(matches!(e, Expr::Cmp(_, _, Operand::Lit(Value::Binary(b))) if b.is_empty()));
        assert!(!t("raw eq X''"));
        assert!(t("raw eq X'61'") && t("raw gt X'60'") && t("raw lt X'6100'"));
        assert!(parse("raw eq X'zz'").is_err());
    }

    #[test]
    fn chained_operators_are_left_associative() {
        let e = parse("a eq 1 or b eq 2 or c eq 3").unwrap();
        let Expr::Or(l, _) = e else { panic!() };
        assert!(matches!(*l, Expr::Or(..)));
        let e = parse("a eq 1 and b eq 2 and c eq 3").unwrap();
        let Expr::And(l, _) = e else { panic!() };
        assert!(matches!(*l, Expr::And(..)));
    }

    #[test]
    fn nesting_and_size_are_limited() {
        let f = format!("{}age eq 30{}", "(".repeat(100), ")".repeat(100));
        assert!(parse(&f).is_ok());
        let f = format!("{}age eq 30{}", "(".repeat(5000), ")".repeat(5000));
        assert!(parse(&f).is_err());
        let f = format!("{}age eq 30", "not ".repeat(5000));
        assert!(parse(&f).is_err());
        let ok = vec!["age eq 30"; 100].join(" or ");
        assert!(t(&ok));
        let too_many = vec!["age eq 30"; 101].join(" or ");
        assert!(parse(&too_many).is_err());
    }

    #[test]
    fn more_syntax_errors_and_their_positions() {
        assert_eq!(parse("name eq").unwrap_err().pos, 7);
        assert_eq!(parse("name eq 'x' and").unwrap_err().pos, 15);
        assert_eq!(parse("(name eq 'x'").unwrap_err().pos, 12);
        assert_eq!(parse("name eq 'x')").unwrap_err().pos, 12);
        assert!(parse("name eq eq").is_err());
        assert!(parse("name eq and").is_err());
        assert!(parse("name 'x'").is_err());
        assert!(parse("name eq 'x' 'y'").is_err());
        assert!(parse("name eq 1 2").is_err());
        assert!(parse("name # 1").is_err());
        assert!(parse("name eq (1)").is_err());
    }
}
