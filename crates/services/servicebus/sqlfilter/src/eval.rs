//! Expression evaluation with three-valued logic.

use crate::ast::{BinOp, Expr, PropRef};
use crate::error::EvalError;
use crate::like::like;
use crate::ops::{arith, compare, negate, new_guid, Val};
use crate::value::{Params, Properties, Scope, Value};

pub(crate) struct Ctx<'a> {
    pub props: &'a dyn Properties,
    pub params: &'a Params,
}

/// System property names. A missing property with one of these names is
/// simply unset (unknown); a missing property with any other `sys.` name
/// does not exist at all, which Service Bus treats as an error.
const KNOWN_SYS: &[&str] = &[
    "MessageId",
    "CorrelationId",
    "Label",
    "Subject",
    "To",
    "ReplyTo",
    "ReplyToSessionId",
    "SessionId",
    "ContentType",
    "PartitionKey",
    "ViaPartitionKey",
    "TimeToLive",
    "ScheduledEnqueueTimeUtc",
    "EnqueuedTimeUtc",
    "DeliveryCount",
    "SequenceNumber",
    "EnqueuedSequenceNumber",
    "Size",
    "LockToken",
    "LockedUntilUtc",
    "ExpiresAtUtc",
    "DeadLetterSource",
    "DeadLetterReason",
    "DeadLetterErrorDescription",
    "State",
];

fn tri(v: &Val) -> Option<bool> {
    match v {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

fn from_tri(t: Option<bool>) -> Val {
    t.map(Value::Bool)
}

fn lookup(cx: &Ctx, p: &PropRef) -> Result<Val, EvalError> {
    match cx.props.get(p.scope, &p.name) {
        Some(Value::Null) => Ok(None),
        Some(v) => Ok(Some(v)),
        None if p.scope == Scope::Sys && !KNOWN_SYS.iter().any(|k| k.eq_ignore_ascii_case(&p.name)) => {
            Err(EvalError::new(format!("the system property '{}' does not exist", p.name)))
        }
        None => Ok(None),
    }
}

pub(crate) fn eval(e: &Expr, cx: &Ctx) -> Result<Val, EvalError> {
    match e {
        Expr::Lit(Value::Null) => Ok(None),
        Expr::Lit(v) => Ok(Some(v.clone())),
        Expr::Param(name) => Ok(match cx.params.get(name) {
            Some(Value::Null) | None => None,
            Some(v) => Some(v.clone()),
        }),
        Expr::Prop(p) => lookup(cx, p),
        Expr::PropertyFn(arg) => match eval(arg, cx)? {
            Some(Value::Str(name)) => lookup(cx, &PropRef { scope: Scope::User, name }),
            _ => Ok(None),
        },
        Expr::NewId => Ok(Some(Value::Str(new_guid()))),
        Expr::Neg(inner) => Ok(eval(inner, cx)?.as_ref().and_then(negate)),
        Expr::Plus(inner) => Ok(match eval(inner, cx)? {
            v @ Some(Value::Long(_)) | v @ Some(Value::Double(_)) => v,
            _ => None,
        }),
        Expr::Not(inner) => Ok(from_tri(tri(&eval(inner, cx)?).map(|b| !b))),
        Expr::Binary(BinOp::And, l, r) => {
            let left = tri(&eval(l, cx)?);
            if left == Some(false) {
                return Ok(Some(Value::Bool(false)));
            }
            let right = tri(&eval(r, cx)?);
            Ok(from_tri(match (left, right) {
                (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            }))
        }
        Expr::Binary(BinOp::Or, l, r) => {
            let left = tri(&eval(l, cx)?);
            if left == Some(true) {
                return Ok(Some(Value::Bool(true)));
            }
            let right = tri(&eval(r, cx)?);
            Ok(from_tri(match (left, right) {
                (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }))
        }
        Expr::Binary(op, l, r) => {
            let (Some(a), Some(b)) = (eval(l, cx)?, eval(r, cx)?) else {
                return Ok(None);
            };
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => arith(*op, &a, &b),
                _ => Ok(from_tri(compare(*op, &a, &b))),
            }
        }
        Expr::In { expr, list, negated } => {
            let Some(left) = eval(expr, cx)? else {
                return Ok(None);
            };
            let mut found = false;
            for item in list {
                if let Some(v) = eval(item, cx)? {
                    if compare(BinOp::Eq, &left, &v) == Some(true) {
                        found = true;
                        break;
                    }
                }
            }
            Ok(Some(Value::Bool(found != *negated)))
        }
        Expr::Like { expr, pattern, escape, negated } => {
            let text = eval(expr, cx)?;
            let pat = eval(pattern, cx)?;
            let esc = match escape {
                Some(x) => Some(eval(x, cx)?),
                None => None,
            };
            let (Some(Value::Str(text)), Some(Value::Str(pat))) = (text, pat) else {
                return Ok(None);
            };
            let esc_char = match esc {
                None => None,
                Some(Some(Value::Str(s))) => {
                    let mut it = s.chars();
                    match (it.next(), it.next()) {
                        (Some(c), None) => Some(c),
                        _ => return Err(EvalError::new("the ESCAPE value must be a string of length 1")),
                    }
                }
                Some(_) => return Ok(None),
            };
            Ok(Some(Value::Bool(like(&text, &pat, esc_char)? != *negated)))
        }
        Expr::IsNull { expr, negated } => {
            let is_null = match &**expr {
                // IS NULL is true for a missing property of any scope.
                Expr::Prop(p) => matches!(cx.props.get(p.scope, &p.name), None | Some(Value::Null)),
                other => eval(other, cx)?.is_none(),
            };
            Ok(Some(Value::Bool(is_null != *negated)))
        }
        Expr::Exists(p) => Ok(Some(Value::Bool(cx.props.get(p.scope, &p.name).is_some()))),
    }
}
