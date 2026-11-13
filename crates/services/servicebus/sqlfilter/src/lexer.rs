//! Hand-written lexer. Positions are character indexes.

use crate::error::ParseError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Word(String),
    /// `[delimited identifier]`
    Bracket(String),
    /// `"quoted identifier"`
    Quoted(String),
    /// `'string constant'`
    Str(String),
    /// Unsigned integer magnitude (the parser applies range and sign).
    Int(u64),
    Float(f64),
    Param(String),
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    LParen,
    RParen,
    Comma,
    Dot,
    Semi,
    Eof,
}

impl Tok {
    pub(crate) fn describe(&self) -> String {
        match self {
            Tok::Word(w) => format!("'{w}'"),
            Tok::Bracket(w) => format!("'[{w}]'"),
            Tok::Quoted(w) => format!("\"{w}\""),
            Tok::Str(s) => format!("string '{s}'"),
            Tok::Int(n) => format!("'{n}'"),
            Tok::Float(n) => format!("'{n}'"),
            Tok::Param(p) => format!("'@{p}'"),
            Tok::Eq => "'='".into(),
            Tok::Ne => "'<>'".into(),
            Tok::Lt => "'<'".into(),
            Tok::Le => "'<='".into(),
            Tok::Gt => "'>'".into(),
            Tok::Ge => "'>='".into(),
            Tok::Plus => "'+'".into(),
            Tok::Minus => "'-'".into(),
            Tok::Star => "'*'".into(),
            Tok::Slash => "'/'".into(),
            Tok::Percent => "'%'".into(),
            Tok::LParen => "'('".into(),
            Tok::RParen => "')'".into(),
            Tok::Comma => "','".into(),
            Tok::Dot => "'.'".into(),
            Tok::Semi => "';'".into(),
            Tok::Eof => "end of input".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub tok: Tok,
    pub pos: usize,
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Reads a delimited run starting after the opening delimiter. `close`
/// doubled is an escaped `close`. Returns the text and the index after the
/// closing delimiter.
fn delimited(chars: &[char], start: usize, close: char, what: &str, open_pos: usize) -> Result<(String, usize), ParseError> {
    let mut out = String::new();
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if c == close {
            if chars.get(i + 1) == Some(&close) {
                out.push(close);
                i += 2;
                continue;
            }
            return Ok((out, i + 1));
        }
        out.push(c);
        i += 1;
    }
    Err(ParseError::new(format!("unterminated {what}"), open_pos))
}

pub(crate) fn lex(src: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let pos = i;
        let next = chars.get(i + 1).copied();
        let simple = |t: Tok, n: usize| (t, n);
        let (tok, len) = match c {
            _ if c.is_whitespace() => {
                i += 1;
                continue;
            }
            '(' => simple(Tok::LParen, 1),
            ')' => simple(Tok::RParen, 1),
            ',' => simple(Tok::Comma, 1),
            ';' => simple(Tok::Semi, 1),
            '+' => simple(Tok::Plus, 1),
            '-' => simple(Tok::Minus, 1),
            '*' => simple(Tok::Star, 1),
            '/' => simple(Tok::Slash, 1),
            '%' => simple(Tok::Percent, 1),
            '=' if next == Some('=') => simple(Tok::Eq, 2),
            '=' => simple(Tok::Eq, 1),
            '!' if next == Some('=') => simple(Tok::Ne, 2),
            '<' if next == Some('>') => simple(Tok::Ne, 2),
            '<' if next == Some('=') => simple(Tok::Le, 2),
            '<' => simple(Tok::Lt, 1),
            '>' if next == Some('=') => simple(Tok::Ge, 2),
            '>' => simple(Tok::Gt, 1),
            '.' if !next.is_some_and(|n| n.is_ascii_digit()) => simple(Tok::Dot, 1),
            '\'' => {
                let (s, end) = delimited(&chars, i + 1, '\'', "string constant", pos)?;
                (Tok::Str(s), end - i)
            }
            '[' => {
                // Inside brackets only `]` is special, doubled to escape.
                let (s, end) = delimited(&chars, i + 1, ']', "bracketed identifier", pos)?;
                (Tok::Bracket(s), end - i)
            }
            '"' => {
                let (s, end) = delimited(&chars, i + 1, '"', "quoted identifier", pos)?;
                (Tok::Quoted(s), end - i)
            }
            '@' => {
                let mut j = i + 1;
                while j < chars.len() && is_word_char(chars[j]) {
                    j += 1;
                }
                if j == i + 1 {
                    return Err(ParseError::new("expected a parameter name after '@'", pos));
                }
                (Tok::Param(chars[i + 1..j].iter().collect()), j - i)
            }
            _ if c.is_ascii_digit() || c == '.' => {
                let (t, end) = lex_number(&chars, i)?;
                (t, end - i)
            }
            _ if is_word_start(c) => {
                let mut j = i + 1;
                while j < chars.len() && is_word_char(chars[j]) {
                    j += 1;
                }
                (Tok::Word(chars[i..j].iter().collect()), j - i)
            }
            _ => return Err(ParseError::new(format!("unexpected character '{c}'"), pos)),
        };
        toks.push(Token { tok, pos });
        i += len;
    }
    toks.push(Token { tok: Tok::Eof, pos: chars.len() });
    Ok(toks)
}

fn lex_number(chars: &[char], start: usize) -> Result<(Tok, usize), ParseError> {
    let mut i = start;
    let mut is_float = false;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
        is_float = true;
        i += 1;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
    }
    if matches!(chars.get(i), Some('e') | Some('E')) {
        let mut j = i + 1;
        if matches!(chars.get(j), Some('+') | Some('-')) {
            j += 1;
        }
        if chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            is_float = true;
            i = j;
        }
    }
    if chars.get(i).is_some_and(|c| is_word_char(*c)) {
        return Err(ParseError::new("invalid number", start));
    }
    let text: String = chars[start..i].iter().collect();
    if is_float {
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok((Tok::Float(f), i)),
            _ => Err(ParseError::new("decimal constant out of range", start)),
        }
    } else {
        match text.parse::<u64>() {
            Ok(n) => Ok((Tok::Int(n), i)),
            Err(_) => Err(ParseError::new("integer constant out of range", start)),
        }
    }
}
