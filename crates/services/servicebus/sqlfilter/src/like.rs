//! `LIKE` pattern matching (`%`, `_`, optional escape character).

use crate::error::EvalError;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Piece {
    Lit(char),
    One,
    Many,
}

fn compile(pattern: &str, escape: Option<char>) -> Result<Vec<Piece>, EvalError> {
    let mut out: Vec<Piece> = Vec::new();
    let mut it = pattern.chars();
    while let Some(c) = it.next() {
        if Some(c) == escape {
            match it.next() {
                Some(n) => out.push(Piece::Lit(n)),
                None => return Err(EvalError::new("LIKE pattern ends with an escape character")),
            }
        } else if c == '%' {
            // Collapse runs of % so matching stays linear-ish.
            if out.last() != Some(&Piece::Many) {
                out.push(Piece::Many);
            }
        } else if c == '_' {
            out.push(Piece::One);
        } else {
            out.push(Piece::Lit(c));
        }
    }
    Ok(out)
}

/// Case-sensitive match of the whole `text` against `pattern`.
pub(crate) fn like(text: &str, pattern: &str, escape: Option<char>) -> Result<bool, EvalError> {
    let pieces = compile(pattern, escape)?;
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pieces.get(p) {
            Some(Piece::Many) => {
                star = Some((p, t));
                p += 1;
            }
            Some(Piece::One) => {
                p += 1;
                t += 1;
            }
            Some(Piece::Lit(c)) if *c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return Ok(false),
            },
        }
    }
    while pieces.get(p) == Some(&Piece::Many) {
        p += 1;
    }
    Ok(p == pieces.len())
}
