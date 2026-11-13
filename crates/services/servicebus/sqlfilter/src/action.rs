//! Rule actions: `SET` / `REMOVE` statements.

use crate::ast::{Expr, Stmt};
use crate::error::{EvalError, ParseError};
use crate::eval::{eval, Ctx};
use crate::lexer::Tok;
use crate::parser::Parser;
use crate::time::parse_timestamp;
use crate::value::{Params, Properties, PropertiesMut, Scope, Value};
use crate::MAX_ACTION_STATEMENTS;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Verb {
    Set,
    Remove,
}

pub(crate) fn parse_action(sql: &str) -> Result<Vec<Stmt>, ParseError> {
    let mut p = Parser::new(sql, "action")?;
    let mut stmts: Vec<Stmt> = Vec::new();
    let mut verb: Option<Verb> = None;
    let mut after_comma = false;
    loop {
        if *p.peek() == Tok::Eof {
            if stmts.is_empty() {
                return p.err("the action is empty; expected SET or REMOVE");
            }
            if after_comma {
                return p.err("expected a statement after ','");
            }
            break;
        }
        let v = if p.is_word("SET") {
            p.advance();
            Verb::Set
        } else if p.is_word("REMOVE") {
            p.advance();
            Verb::Remove
        } else if let (Some(v), true) = (verb, after_comma) {
            v
        } else {
            return p.err(format!("expected SET or REMOVE but found {}", p.peek().describe()));
        };
        let target_pos = p.pos();
        let target = p.parse_prop_ref()?;
        match v {
            Verb::Set => {
                p.expect(Tok::Eq, "'=' after the property name")?;
                let expr = p.parse_add()?;
                stmts.push(Stmt::Set { target, expr });
            }
            Verb::Remove => {
                if target.scope == Scope::Sys {
                    return Err(ParseError::new("only user properties can be removed, not system properties", target_pos));
                }
                stmts.push(Stmt::Remove(target));
            }
        }
        if stmts.len() > MAX_ACTION_STATEMENTS {
            return p.err(format!("an action can contain at most {MAX_ACTION_STATEMENTS} statements"));
        }
        after_comma = false;
        verb = Some(v);
        match p.peek() {
            Tok::Semi => {
                p.advance();
                verb = None;
            }
            Tok::Comma => {
                p.advance();
                after_comma = true;
            }
            _ => {}
        }
    }
    Ok(stmts)
}

/// Pending changes layered over the real properties, so that later
/// statements see earlier ones and a failing action changes nothing.
struct Overlay<'a> {
    base: &'a dyn PropertiesMut,
    changes: Vec<(Scope, String, Option<Value>)>,
}

impl Properties for Overlay<'_> {
    fn get(&self, scope: Scope, name: &str) -> Option<Value> {
        for (s, n, v) in self.changes.iter().rev() {
            if *s == scope && n.to_lowercase() == name.to_lowercase() {
                return v.clone();
            }
        }
        self.base.get(scope, name)
    }
}

/// `SET` converts to the type of an existing property where possible: a
/// string becomes a timestamp when the property is a timestamp, an integer
/// becomes a double when the property is a double.
fn convert(existing: Option<Value>, new: Value) -> Result<Value, EvalError> {
    match (existing, new) {
        (Some(Value::Timestamp(_)), Value::Str(s)) => {
            parse_timestamp(&s).map(Value::Timestamp).ok_or_else(|| EvalError::new(format!("cannot convert '{s}' to a timestamp")))
        }
        (Some(Value::Double(_)), Value::Long(n)) => Ok(Value::Double(n as f64)),
        (_, v) => Ok(v),
    }
}

pub(crate) fn run(stmts: &[Stmt], props: &mut dyn PropertiesMut, params: &Params) -> Result<(), EvalError> {
    let changes = {
        let mut ov = Overlay { base: &*props, changes: Vec::new() };
        for st in stmts {
            match st {
                Stmt::Set { target, expr } => {
                    let new = {
                        let cx = Ctx { props: &ov, params };
                        eval_value(expr, &cx)?
                    };
                    let existing = ov.get(target.scope, &target.name);
                    let value = convert(existing, new)?;
                    ov.changes.push((target.scope, target.name.clone(), Some(value)));
                }
                Stmt::Remove(target) => {
                    ov.changes.push((target.scope, target.name.clone(), None));
                }
            }
        }
        ov.changes
    };
    for (scope, name, v) in changes {
        match v {
            Some(v) => props.set(scope, &name, v),
            None => props.remove(scope, &name),
        }
    }
    Ok(())
}

/// An unknown result is stored as `Null`.
fn eval_value(expr: &Expr, cx: &Ctx) -> Result<Value, EvalError> {
    Ok(eval(expr, cx)?.unwrap_or(Value::Null))
}
