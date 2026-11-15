//! Tokenizer. Works on characters; positions are character offsets.

use crate::error::SqlError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Word(String),
    Int(i64),
    Float(f64),
    Str(String),
    Param(String),
    Sym(&'static str),
    /// `{documentdb-formattableorderbyquery-filter}`, accepted as `true`.
    FilterPlaceholder,
    Eof,
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

const PLACEHOLDER: &str = "{documentdb-formattableorderbyquery-filter}";

const SYMS: &[&str] = &[
    ">>>", "<<", ">>", "<=", ">=", "<>", "!=", "||", "??", "=", "<", ">", "+", "-", "*", "/", "%", "&", "|", "^", "~", "?", ":", ",", ".",
    "(", ")", "[", "]", "{", "}",
];

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

pub(crate) fn tokenize(src: &[char]) -> Result<Vec<Token>, SqlError> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let n = src.len();
    while i < n {
        let c = src[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // `-- text` runs to the end of the line (Azure accepts it anywhere a space is allowed)
        if c == '-' && i + 1 < n && src[i + 1] == '-' {
            while i < n && src[i] != '\n' && src[i] != '\r' {
                i += 1;
            }
            continue;
        }
        let start = i;
        // a dot right after a name or a closing bracket is a member access; after a space it starts a number (`VALUE .5`)
        let after_operand = out.last().is_some_and(|t: &Token| t.end == i && matches!(t.tok, Tok::Word(_) | Tok::Sym(")") | Tok::Sym("]")));
        if c == '\'' || c == '"' {
            let (s, next) = lex_string(src, i)?;
            out.push(Token { tok: Tok::Str(s), start, end: next });
            i = next;
        } else if c.is_ascii_digit() || (c == '.' && !after_operand && i + 1 < n && src[i + 1].is_ascii_digit()) {
            let (tok, next) = lex_number(src, i)?;
            out.push(Token { tok, start, end: next });
            i = next;
        } else if c == '@' {
            let mut j = i + 1;
            // a parameter name starts like an identifier: `@1x` and a lone `@` are invalid tokens
            if j >= n || !is_word_start(src[j]) {
                return Err(SqlError::at("SC1010", "Syntax error, invalid token '@'.".to_string(), i, i + 1));
            }
            while j < n && is_word_char(src[j]) {
                j += 1;
            }
            let name: String = src[i + 1..j].iter().collect();
            out.push(Token { tok: Tok::Param(name), start, end: j });
            i = j;
        } else if is_word_start(c) {
            let mut j = i + 1;
            while j < n && is_word_char(src[j]) {
                j += 1;
            }
            out.push(Token { tok: Tok::Word(src[i..j].iter().collect()), start, end: j });
            i = j;
        } else if c == '{' && starts_with(src, i, PLACEHOLDER) {
            let end = i + PLACEHOLDER.chars().count();
            out.push(Token { tok: Tok::FilterPlaceholder, start, end });
            i = end;
        } else {
            let mut matched = None;
            for s in SYMS {
                if starts_with(src, i, s) {
                    matched = Some(*s);
                    break;
                }
            }
            match matched {
                Some(s) => {
                    let end = i + s.chars().count();
                    out.push(Token { tok: Tok::Sym(s), start, end });
                    i = end;
                }
                None => return Err(SqlError::at("SC1010", format!("Syntax error, invalid token '{c}'."), i, i + 1)),
            }
        }
    }
    out.push(Token { tok: Tok::Eof, start: n, end: n });
    Ok(out)
}

fn starts_with(src: &[char], at: usize, pat: &str) -> bool {
    for (k, pc) in (at..).zip(pat.chars()) {
        if k >= src.len() || src[k] != pc {
            return false;
        }
    }
    true
}

fn lex_number(src: &[char], start: usize) -> Result<(Tok, usize), SqlError> {
    let n = src.len();
    let mut i = start;
    if src[i] == '0' && i + 1 < n && (src[i + 1] == 'x' || src[i + 1] == 'X') {
        let mut j = i + 2;
        while j < n && src[j].is_ascii_hexdigit() {
            j += 1;
        }
        if j == i + 2 {
            return Err(SqlError::syntax_near(&src[start..j].iter().collect::<String>(), start, j));
        }
        let digits: String = src[i + 2..j].iter().collect();
        let tok = match i64::from_str_radix(&digits, 16) {
            Ok(v) => Tok::Int(v),
            Err(_) => match u128::from_str_radix(&digits, 16) {
                Ok(v) => Tok::Float(v as f64),
                Err(_) => {
                    let text: String = src[start..j].iter().collect();
                    return Err(SqlError::at("SC1011", format!("Syntax error, invalid numeric value token '{text}'."), start, j));
                }
            },
        };
        return Ok((tok, j));
    }
    while i < n && src[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i < n && src[i] == '.' {
        if i + 1 < n && src[i + 1].is_ascii_digit() {
            is_float = true;
            i += 1;
            while i < n && src[i].is_ascii_digit() {
                i += 1;
            }
        } else if i > start
            && (i + 1 >= n
                || src[i + 1].is_whitespace()
                || matches!(src[i + 1], ')' | ',' | ']' | '}')
                || (matches!(src[i + 1], 'e' | 'E') && src.get(i + 2).is_some_and(|c| c.is_ascii_digit() || matches!(c, '+' | '-'))))
        {
            // `5.` is the number 5
            i += 1;
        }
    }
    if i < n && (src[i] == 'e' || src[i] == 'E') {
        let mut j = i + 1;
        if j < n && (src[j] == '+' || src[j] == '-') {
            j += 1;
        }
        if j < n && src[j].is_ascii_digit() {
            while j < n && src[j].is_ascii_digit() {
                j += 1;
            }
            is_float = true;
            i = j;
        } else {
            // an exponent mark without digits
            let text: String = src[start..j].iter().collect();
            return Err(SqlError::at("SC1011", format!("Syntax error, invalid numeric value token '{text}'."), start, j));
        }
    }
    let text: String = src[start..i].iter().collect();
    let text = text.trim_end_matches('.');
    if !is_float {
        if let Ok(v) = text.parse::<i64>() {
            return Ok((Tok::Int(v), i));
        }
    }
    match text.parse::<f64>() {
        Ok(v) if v.is_finite() => Ok((Tok::Float(v), i)),
        _ => Err(SqlError::at("SC1011", format!("Syntax error, invalid numeric value token '{text}'."), start, i)),
    }
}

fn lex_string(src: &[char], start: usize) -> Result<(String, usize), SqlError> {
    let quote = src[start];
    let n = src.len();
    let mut i = start + 1;
    let mut out = String::new();
    while i < n {
        let c = src[i];
        if c == quote {
            return Ok((out, i + 1));
        }
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        if i >= n {
            break;
        }
        let e = src[i];
        match e {
            '\'' | '"' | '\\' | '/' => out.push(e),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let (cp, next) = read_hex4(src, i + 1).ok_or_else(|| bad_escape(src, i - 1, i + 1))?;
                i = next - 1;
                if (0xD800..0xDC00).contains(&cp) {
                    // High surrogate: needs a following \uXXXX low surrogate.
                    if i + 2 < n && src[i + 1] == '\\' && src[i + 2] == 'u' {
                        if let Some((lo, next2)) = read_hex4(src, i + 3) {
                            if (0xDC00..0xE000).contains(&lo) {
                                let full = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                out.push(char::from_u32(full).unwrap_or('\u{FFFD}'));
                                i = next2 - 1;
                                i += 1;
                                continue;
                            }
                        }
                    }
                    out.push('\u{FFFD}');
                } else {
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                }
            }
            // any other escaped character stands for itself (`'a\\qb'` is `aqb`)
            other => out.push(other),
        }
        i += 1;
    }
    Err(SqlError::at(
        "SC1012",
        format!("Syntax error, invalid string literal token '{}'.", src[start..n].iter().collect::<String>()),
        start,
        n,
    ))
}

fn bad_escape(src: &[char], s: usize, e: usize) -> SqlError {
    let e = e.min(src.len());
    SqlError::syntax_near(&src[s..e].iter().collect::<String>(), s, e)
}

fn read_hex4(src: &[char], at: usize) -> Option<(u32, usize)> {
    if at + 4 > src.len() {
        return None;
    }
    let mut v = 0u32;
    for c in &src[at..at + 4] {
        v = v * 16 + c.to_digit(16)?;
    }
    Some((v, at + 4))
}
