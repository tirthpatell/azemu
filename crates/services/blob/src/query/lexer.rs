//! Turning the text of an expression into tokens.

use super::sql::{err, SqlError, MAX_NODES};

type R<T> = Result<T, SqlError>;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Tok {
    Ident(String),
    Quoted(String),
    Str(String),
    Int(i64),
    Float(f64),
    Sym(&'static str),
    Eof,
}

pub(super) struct Lexed {
    pub tok: Tok,
    pub pos: usize,
    /// Byte range in the text, for `Item::source`.
    pub start: usize,
    pub end: usize,
}

pub(super) fn lex(text: &str) -> R<Vec<Lexed>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (b, c) = chars[i];
        let pos = i;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let tok;
        if c.is_ascii_alphabetic() || c == '_' {
            let st = i;
            while i < chars.len() && (chars[i].1.is_ascii_alphanumeric() || chars[i].1 == '_' || chars[i].1 == '$') {
                i += 1;
            }
            tok = Tok::Ident(chars[st..i].iter().map(|x| x.1).collect());
        } else if c.is_ascii_digit() {
            let st = i;
            while i < chars.len() && chars[i].1.is_ascii_digit() {
                i += 1;
            }
            let mut float = false;
            if i + 1 < chars.len() && chars[i].1 == '.' && chars[i + 1].1.is_ascii_digit() {
                float = true;
                i += 1;
                while i < chars.len() && chars[i].1.is_ascii_digit() {
                    i += 1;
                }
            }
            if i < chars.len() && matches!(chars[i].1, 'e' | 'E') {
                let mut j = i + 1;
                if j < chars.len() && matches!(chars[j].1, '+' | '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].1.is_ascii_digit() {
                    float = true;
                    i = j;
                    while i < chars.len() && chars[i].1.is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let s: String = chars[st..i].iter().map(|x| x.1).collect();
            tok = if float {
                Tok::Float(s.parse().map_err(|_| SqlError { message: "bad number".into(), pos })?)
            } else {
                match s.parse::<i64>() {
                    Ok(n) => Tok::Int(n),
                    // too big for an INT: a FLOAT, as a number in a query is
                    Err(_) => Tok::Float(s.parse().map_err(|_| SqlError { message: "bad number".into(), pos })?),
                }
            };
        } else if c == '\'' || c == '"' {
            let q = c;
            i += 1;
            let mut s = String::new();
            loop {
                if i >= chars.len() {
                    return err(pos, if q == '\'' { "The string literal is not closed." } else { "The quoted name is not closed." });
                }
                let ch = chars[i].1;
                if ch == q {
                    if i + 1 < chars.len() && chars[i + 1].1 == q {
                        s.push(q);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                s.push(ch);
                i += 1;
            }
            tok = if q == '\'' { Tok::Str(s) } else { Tok::Quoted(s) };
        } else {
            let two: String = chars[i..(i + 2).min(chars.len())].iter().map(|x| x.1).collect();
            let sym = match two.as_str() {
                "<=" => Some("<="),
                ">=" => Some(">="),
                "<>" => Some("<>"),
                "!=" => Some("!="),
                "||" => Some("||"),
                _ => None,
            };
            if let Some(s) = sym {
                i += 2;
                tok = Tok::Sym(s);
            } else {
                let s = match c {
                    '(' => "(",
                    ')' => ")",
                    ',' => ",",
                    '.' => ".",
                    '*' => "*",
                    '+' => "+",
                    '-' => "-",
                    '/' => "/",
                    '%' => "%",
                    '=' => "=",
                    '<' => "<",
                    '>' => ">",
                    '[' => "[",
                    ']' => "]",
                    ';' => ";",
                    _ => return err(pos, format!("Unexpected character '{c}'.")),
                };
                i += 1;
                tok = Tok::Sym(s);
            }
        }
        let end = if i < chars.len() { chars[i].0 } else { text.len() };
        out.push(Lexed { tok, pos, start: b, end });
        if out.len() > MAX_NODES * 2 {
            return err(pos, "The expression has too many tokens.");
        }
    }
    out.push(Lexed { tok: Tok::Eof, pos: chars.len(), start: text.len(), end: text.len() });
    Ok(out)
}
