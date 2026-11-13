//! Parser of ARM template expressions (the text between `[` and `]`). Nesting is limited: see `limits.rs`.

use serde_json::{json, Value};

use crate::limits::{MAX_PARSE_DEPTH, TOO_DEEP};

// ---- parser ----------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Ast {
    Lit(Value),
    Call(String, Vec<Ast>),
    Prop(Box<Ast>, String),
    Index(Box<Ast>, Box<Ast>),
}

pub struct Parser<'a> {
    pub s: &'a [u8],
    pub i: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    pub fn new(body: &'a str) -> Parser<'a> {
        Parser { s: body.as_bytes(), i: 0, depth: 0 }
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    pub fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    /// One expression. Calls and `[...]` indexes nest through here, so this is where the depth is counted.
    pub fn expr(&mut self) -> Result<Ast, String> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(format!("{TOO_DEEP} {MAX_PARSE_DEPTH} levels deep"));
        }
        let r = self.expr_inner();
        self.depth -= 1;
        r
    }

    fn expr_inner(&mut self) -> Result<Ast, String> {
        let mut node = self.primary()?;
        // `.name` and `[index]` after a value build a chain that evaluation walks recursively, so each link counts too.
        let mut links = 0;
        loop {
            match self.peek() {
                Some(b'.') => {
                    self.link()?;
                    links += 1;
                    self.i += 1;
                    let name = self.ident()?;
                    node = Ast::Prop(Box::new(node), name);
                }
                Some(b'[') => {
                    self.link()?;
                    links += 1;
                    self.i += 1;
                    let idx = self.expr()?;
                    if self.peek() != Some(b']') {
                        return Err("expected ']'".into());
                    }
                    self.i += 1;
                    node = Ast::Index(Box::new(node), Box::new(idx));
                }
                _ => {
                    self.depth -= links;
                    return Ok(node);
                }
            }
        }
    }

    fn link(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(format!("{TOO_DEEP} {MAX_PARSE_DEPTH} levels deep"));
        }
        Ok(())
    }

    fn ident(&mut self) -> Result<String, String> {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_' || self.s[self.i] == b'$') {
            self.i += 1;
        }
        if start == self.i {
            return Err(format!("unexpected character at {}", self.i));
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.i]).to_string())
    }

    fn primary(&mut self) -> Result<Ast, String> {
        match self.peek() {
            Some(b'\'') => {
                self.i += 1;
                let mut out = Vec::new();
                loop {
                    match self.s.get(self.i) {
                        None => return Err("unterminated string".into()),
                        Some(b'\'') => {
                            if self.s.get(self.i + 1) == Some(&b'\'') {
                                out.push(b'\'');
                                self.i += 2;
                            } else {
                                self.i += 1;
                                break;
                            }
                        }
                        Some(c) => {
                            out.push(*c);
                            self.i += 1;
                        }
                    }
                }
                Ok(Ast::Lit(Value::String(String::from_utf8_lossy(&out).to_string())))
            }
            Some(c) if c.is_ascii_digit() || c == b'-' => {
                let start = self.i;
                self.i += 1;
                while self.i < self.s.len() && (self.s[self.i].is_ascii_digit() || self.s[self.i] == b'.') {
                    self.i += 1;
                }
                let t = String::from_utf8_lossy(&self.s[start..self.i]).to_string();
                if let Ok(n) = t.parse::<i64>() {
                    Ok(Ast::Lit(json!(n)))
                } else if let Ok(f) = t.parse::<f64>() {
                    Ok(Ast::Lit(json!(f)))
                } else {
                    Err(format!("'{t}' is not a number"))
                }
            }
            Some(_) => {
                let name = self.ident()?;
                if self.peek() != Some(b'(') {
                    return match name.as_str() {
                        "true" => Ok(Ast::Lit(json!(true))),
                        "false" => Ok(Ast::Lit(json!(false))),
                        "null" => Ok(Ast::Lit(Value::Null)),
                        _ => Err(format!("'{name}' is not a function call")),
                    };
                }
                self.i += 1;
                let mut args = Vec::new();
                if self.peek() == Some(b')') {
                    self.i += 1;
                    return Ok(Ast::Call(name, args));
                }
                loop {
                    args.push(self.expr()?);
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b')') => {
                            self.i += 1;
                            break;
                        }
                        _ => return Err(format!("expected ',' or ')' in the call to {name}")),
                    }
                }
                Ok(Ast::Call(name, args))
            }
            None => Err("the expression is empty".into()),
        }
    }
}
