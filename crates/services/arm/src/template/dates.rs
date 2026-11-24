//! `utcNow`, `newGuid`, `dateTimeAdd`, `dateTimeToEpoch`, `dateTimeFromEpoch`. Source: Microsoft Learn,
//! `azure-resource-manager/templates/template-functions-date` and `template-functions-string#newguid`.
//!
//! `utcNow` and `newGuid` "can only be used in the default value for a parameter": they return a different value
//! each time, so anywhere else the template is refused (`Env::defaults` is set only while a default is evaluated).

use serde_json::{json, Value};

use super::dotnet::{self, Dt};
use super::{err, s, Env, EvalError};

type R = Result<Value, EvalError>;

/// The text of Azure's answer is not known; this wording follows the one ARM uses for a function in a place it is not
/// allowed ("The template function ... is not expected at this location"). See "Behaviour that could not be confirmed".
fn only_in_defaults(name: &str) -> R {
    // Azure's words, spelling included (checked against a real deployment): in an output the deployment fails while it
    // evaluates the outputs; in a variable the template is refused when it is validated (`deployments.rs`).
    super::runtime(format!(
        "The template function '{name}' is not expected at this location. The function can only be used with parameter default value expresions. Please see https://aka.ms/arm-functions for usage details."
    ))
}

/// Azure's text for a parameter that a date function cannot use.
fn bad_parameter(function: &str, position: &str, value: &str, kind: &str) -> EvalError {
    EvalError::Runtime(format!(
        "The template function '{function}' has an invalid parameter. The {position} parameter '{value}' is not a valid {kind}. Please see https://aka.ms/arm-functions for usage details."
    ))
}

fn now(env: &Env<'_>) -> Dt {
    env.now.unwrap_or_else(|| {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Dt { secs: d.as_secs() as i64, ticks: d.subsec_nanos() / 100 }
    })
}

fn date_arg(fname: &str, v: &Value, wall: bool) -> Result<Dt, EvalError> {
    let t = s(v);
    (if wall { dotnet::parse_wall(&t) } else { dotnet::parse(&t) })
        .ok_or_else(|| bad_parameter(fname, "first", &t, "ISO8601 DateTime string"))
}

pub fn call(name: &str, lname: &str, v: &[Value], env: &Env<'_>) -> Option<R> {
    Some(match lname {
        "utcnow" => {
            if !env.defaults {
                return Some(only_in_defaults(name));
            }
            let f = v.first().map(s).unwrap_or_else(|| "yyyyMMddTHHmmssZ".into());
            dotnet::format(&now(env), &f).map(Value::String).map_err(|_| bad_parameter(name, "first", &f, "DateTime format string"))
        }
        "newguid" => {
            if !env.defaults {
                return Some(only_in_defaults(name));
            }
            Ok(Value::String(uuid::Uuid::new_v4().to_string()))
        }
        "datetimeadd" => {
            if v.len() < 2 {
                return Some(err(format!("The function '{name}' needs at least 2 argument(s).")));
            }
            let run = || -> R {
                let base = date_arg(name, &v[0], true)?;
                let duration = s(&v[1]);
                let (secs, ticks) =
                    dotnet::parse_duration(&duration).map_err(|_| bad_parameter(name, "second", &duration, "ISO8601 Duration string"))?;
                let r = base
                    .add(secs, ticks)
                    .map_err(|m| EvalError::Runtime(format!("The template function '{name}' cannot add '{duration}': {m}")))?;
                match v.get(2) {
                    Some(f) => dotnet::format(&r, &s(f))
                        .map(Value::String)
                        .map_err(|_| bad_parameter(name, "third", &s(f), "DateTime format string")),
                    None => Ok(Value::String(dotnet::iso(&r))),
                }
            };
            run()
        }
        "datetimetoepoch" => {
            if v.is_empty() {
                return Some(err(format!("The function '{name}' needs at least 1 argument(s).")));
            }
            date_arg(name, &v[0], false).map(|d| json!(d.secs))
        }
        "datetimefromepoch" => {
            if v.is_empty() {
                return Some(err(format!("The function '{name}' needs at least 1 argument(s).")));
            }
            let n = match &v[0] {
                Value::Number(n) => n.as_i64(),
                Value::String(t) => t.trim().parse().ok(),
                _ => None,
            };
            match n {
                Some(n) => Dt::from_epoch(n)
                    .map(|d| Value::String(dotnet::iso(&d)))
                    .map_err(|m| EvalError::Runtime(format!("The template function '{name}' cannot use {n}: {m}"))),
                None => bad_param_int(name, &v[0]),
            }
        }
        _ => return None,
    })
}

fn bad_param_int(name: &str, v: &Value) -> R {
    Err(bad_parameter(name, "first", &s(v), "integer"))
}
