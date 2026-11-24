//! Lambda functions: `map`, `filter`, `reduce`, `sort`, `toObject`, `mapValues`, `groupBy`, with `lambda(...)` and
//! `lambdaVariables(...)`. Source: Microsoft Learn, `azure-resource-manager/templates/template-functions-lambda`
//! ("Lambda functions for ARM templates"): a lambda can only be passed directly to these seven functions, so
//! `lambda(...)` is not a function that evaluates to a value; it is read here from the syntax tree and its body is
//! evaluated once per element with the lambda variables set. Every evaluation is charged to the request's budget
//! like any other expression, so a lambda over a large array can run out of work (`limits.rs`).

use serde_json::{Map, Value};

use super::{err, eval, invalid, s, truthy, Env, EvalError};
use crate::template_parse::Ast;

type R = Result<Value, EvalError>;

/// `lambda('x', 'i', body)`: the names and the body.
fn parse_lambda<'a>(fname: &str, arg: &'a Ast) -> Result<(Vec<String>, &'a Ast), EvalError> {
    let Ast::Call(n, parts) = arg else {
        return err(format!("The function '{fname}' needs a lambda expression, such as lambda('x', ...), here."));
    };
    if !n.eq_ignore_ascii_case("lambda") || parts.len() < 2 {
        return err(format!("The function '{fname}' needs a lambda expression, such as lambda('x', ...), here."));
    }
    let mut names: Vec<String> = Vec::new();
    for p in &parts[..parts.len() - 1] {
        match p {
            Ast::Lit(Value::String(t)) if !t.is_empty() => {
                if names.iter().any(|x| x.eq_ignore_ascii_case(t)) {
                    return err(format!("The lambda variable '{t}' is declared twice."));
                }
                names.push(t.clone());
            }
            _ => return err("The variables of a lambda expression must be text such as lambda('x', ...)."),
        }
    }
    Ok((names, &parts[parts.len() - 1]))
}

/// Evaluate the body with `vals` bound to the lambda's variables (extra values are dropped, missing ones are an error).
#[inline(never)]
fn apply(env: &Env<'_>, names: &[String], vals: Vec<Value>, body: &Ast) -> R {
    env.charge(64)?;
    let before = env.lvars.borrow().len();
    {
        let mut l = env.lvars.borrow_mut();
        for (n, v) in names.iter().zip(vals) {
            l.push((n.clone(), v));
        }
    }
    let r = eval(body, env);
    env.lvars.borrow_mut().truncate(before);
    r
}

fn expect_vars(fname: &str, names: &[String], allowed: &[usize]) -> Result<(), EvalError> {
    if allowed.contains(&names.len()) {
        Ok(())
    } else {
        err(format!(
            "The lambda expression of '{fname}' takes {} variable(s), but it declares {}.",
            allowed.iter().map(usize::to_string).collect::<Vec<_>>().join(" or "),
            names.len()
        ))
    }
}

fn array_arg(fname: &str, v: Value) -> Result<Vec<Value>, EvalError> {
    match v {
        Value::Array(a) => Ok(a),
        _ => err(format!("The first argument of '{fname}' must be an array.")),
    }
}

fn args_count(fname: &str, args: &[Ast], min: usize, max: usize) -> Result<(), EvalError> {
    if args.len() < min || args.len() > max {
        err(format!("The function '{fname}' takes {min} to {max} argument(s), but {} were given.", args.len()))
    } else {
        Ok(())
    }
}

#[inline(never)]
pub fn call(lname: &str, name: &str, args: &[Ast], env: &Env<'_>) -> R {
    match lname {
        "map" | "filter" | "groupby" => {
            args_count(name, args, 2, 2)?;
            let arr = array_arg(name, eval(&args[0], env)?)?;
            let (vars, body) = parse_lambda(name, &args[1])?;
            expect_vars(name, &vars, &[1, 2])?;
            let with_index = vars.len() == 2;
            let call_one = |i: usize, x: &Value| -> R {
                let vals = if with_index { vec![x.clone(), Value::from(i as u64)] } else { vec![x.clone()] };
                apply(env, &vars, vals, body)
            };
            match lname {
                "map" => {
                    let mut out = Vec::with_capacity(arr.len());
                    for (i, x) in arr.iter().enumerate() {
                        out.push(call_one(i, x)?);
                    }
                    Ok(Value::Array(out))
                }
                "filter" => {
                    let mut out = Vec::new();
                    for (i, x) in arr.iter().enumerate() {
                        if truthy(&call_one(i, x)?)? {
                            out.push(x.clone());
                        }
                    }
                    Ok(Value::Array(out))
                }
                _ => {
                    let mut out = Map::new();
                    for (i, x) in arr.iter().enumerate() {
                        let key = s(&call_one(i, x)?);
                        match out.get_mut(&key) {
                            Some(Value::Array(g)) => g.push(x.clone()),
                            _ => {
                                out.insert(key, Value::Array(vec![x.clone()]));
                            }
                        }
                    }
                    Ok(Value::Object(out))
                }
            }
        }
        "reduce" => {
            args_count(name, args, 3, 3)?;
            let arr = array_arg(name, eval(&args[0], env)?)?;
            let mut acc = eval(&args[1], env)?;
            let (vars, body) = parse_lambda(name, &args[2])?;
            expect_vars(name, &vars, &[2, 3])?;
            for (i, x) in arr.into_iter().enumerate() {
                let mut vals = vec![acc, x];
                if vars.len() == 3 {
                    vals.push(Value::from(i as u64));
                }
                acc = apply(env, &vars, vals, body)?;
                env.charge_value(&acc)?;
            }
            Ok(acc)
        }
        "sort" => {
            args_count(name, args, 2, 2)?;
            let arr = array_arg(name, eval(&args[0], env)?)?;
            let (vars, body) = parse_lambda(name, &args[1])?;
            expect_vars(name, &vars, &[2])?;
            // A bottom-up merge sort: stable, no recursion, and the comparison may fail. The lambda answers "true
            // if the second element is to be ordered after the first"; the right element goes first only when the
            // lambda says the left one belongs after it.
            let mut cur = arr;
            let n = cur.len();
            let mut width = 1;
            while width < n {
                let mut next: Vec<Value> = Vec::with_capacity(n);
                let mut start = 0;
                while start < n {
                    let mid = (start + width).min(n);
                    let end = (start + 2 * width).min(n);
                    let (mut i, mut j) = (start, mid);
                    while i < mid && j < end {
                        let right_first = truthy(&apply(env, &vars, vec![cur[j].clone(), cur[i].clone()], body)?)?;
                        if right_first {
                            next.push(cur[j].clone());
                            j += 1;
                        } else {
                            next.push(cur[i].clone());
                            i += 1;
                        }
                    }
                    next.extend_from_slice(&cur[i..mid]);
                    next.extend_from_slice(&cur[j..end]);
                    start = end;
                }
                cur = next;
                width *= 2;
            }
            Ok(Value::Array(cur))
        }
        "toobject" => {
            args_count(name, args, 2, 3)?;
            let arr = array_arg(name, eval(&args[0], env)?)?;
            let (kvars, kbody) = parse_lambda(name, &args[1])?;
            expect_vars(name, &kvars, &[1])?;
            let value_lambda = match args.get(2) {
                Some(a) => {
                    let (vv, vb) = parse_lambda(name, a)?;
                    expect_vars(name, &vv, &[1])?;
                    Some((vv, vb))
                }
                None => None,
            };
            let mut out = Map::new();
            for x in arr {
                let key = match apply(env, &kvars, vec![x.clone()], kbody)? {
                    Value::String(k) => k,
                    other => return err(format!("toObject: the key lambda must return a string, but it returned {other}.")),
                };
                let val = match &value_lambda {
                    Some((vv, vb)) => apply(env, vv, vec![x], vb)?,
                    None => x,
                };
                out.insert(key, val);
            }
            Ok(Value::Object(out))
        }
        "mapvalues" => {
            args_count(name, args, 2, 2)?;
            let Value::Object(obj) = eval(&args[0], env)? else {
                return err("The first argument of 'mapValues' must be an object.");
            };
            let (vars, body) = parse_lambda(name, &args[1])?;
            expect_vars(name, &vars, &[1])?;
            let mut out = Map::new();
            for (k, v) in obj {
                out.insert(k, apply(env, &vars, vec![v], body)?);
            }
            Ok(Value::Object(out))
        }
        _ => invalid(format!("The function '{name}' is not a lambda function.")),
    }
}

/// `lambdaVariables('x')`.
pub fn variable(env: &Env<'_>, name: &str) -> R {
    env.lvars.borrow().iter().rev().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()).ok_or_else(|| {
        EvalError::Msg(format!("The lambda variable '{name}' is not defined here: lambdaVariables() can only be used inside lambda(...)."))
    })
}

/// Lambda names, so that the evaluator knows which calls take syntax trees instead of values.
pub fn is_lambda_function(lname: &str) -> bool {
    matches!(lname, "map" | "filter" | "reduce" | "sort" | "toobject" | "mapvalues" | "groupby")
}
