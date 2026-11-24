//! Binding the values of a deployment to the parameters of its template, and the outputs at the end of it.

use serde_json::{json, Map, Value};

use super::scope::Scope;
use super::{invalid_template, titlecase};
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::limits;
use crate::template::schema::{coerce, Types};
use crate::template::EvalError;

/// Check the parameters that were given against the template, take defaults for the others, and check every value
/// against its schema. Returns the values by name and the `parameters` object of the deployment record.
pub(super) fn bind_parameters(
    cx: &Cx<'_>,
    scope: &Scope,
    template: &Value,
    given: &Value,
) -> ArmResult<(Map<String, Value>, Map<String, Value>)> {
    let declared = template.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
    let given_map = given.as_object().cloned().unwrap_or_default();
    let definitions = template.get("definitions").and_then(Value::as_object).cloned().unwrap_or_default();
    let types = Types::new(&definitions).with_budget(&scope.budget);
    for k in given_map.keys() {
        if !declared.keys().any(|d| d.eq_ignore_ascii_case(k)) {
            return Err(ArmError::bad_request(
                "InvalidTemplate",
                format!("Deployment template validation failed: The template parameters '{k}' in the parameters file are not valid; they are not present in the original template and can therefore not be provided at deployment time."),
            ));
        }
    }
    let mut bound = Map::new();
    let mut shown = Map::new();
    for (name, decl) in &declared {
        let ty = match types.type_of(decl) {
            Ok(t) if t.is_empty() => "string".to_string(),
            Ok(t) => t,
            Err(m) => return Err(invalid_template(format!("The template parameter '{name}' is not valid: {m}"))),
        };
        let secure = ty.starts_with("secure");
        let provided = given_map.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v);
        let raw = match provided {
            Some(p) if p.get("reference").is_some() => {
                return Err(ArmError::not_implemented(format!("parameter '{name}' uses a Key Vault reference; the emulator cannot read secrets from a deployment parameters object")));
            }
            Some(p) => p.get("value").cloned().ok_or_else(|| invalid_template(format!("The value for the template parameter '{name}' is not provided.")))?,
            None => match decl.get("defaultValue") {
                Some(d) => scope.eval_default(cx, d).map_err(|e| invalid_template(e.text()))?,
                None if types.nullable(decl) => Value::Null,
                None => {
                    return Err(invalid_template(format!(
                        "The template parameter '{name}' is not found in the parameters file and has no default value. Please see https://aka.ms/arm-deploy/#parameter-file for usage details."
                    )))
                }
            },
        };
        let v = if raw.is_null() { Ok(raw) } else { coerce(&ty, raw) }
            .map_err(|m| invalid_template(format!("The provided value for the template parameter '{name}' is not valid: {m}")))?;
        types
            .check(decl, &v)
            .map_err(|m| invalid_template(format!("The provided value for the template parameter '{name}' is not valid. {m}")))?;
        let mut entry = json!({"type": titlecase(&ty).replace("Securestring", "SecureString").replace("Secureobject", "SecureObject")});
        if !secure && !v.is_null() {
            entry["value"] = v.clone();
        }
        shown.insert(name.clone(), entry);
        bound.insert(name.clone(), v);
    }
    Ok((bound, shown))
}

pub(super) enum Outputs {
    Done(Map<String, Value>),
    /// Evaluating an output failed after the deployment started: the deployment fails (`output` names the output when it
    /// was the output that could not be evaluated, `target` the resource a `list*` action failed for).
    Failed {
        error: ArmError,
        target: Option<String>,
        output: Option<String>,
    },
}

/// Evaluate the `outputs` of the template. `v2` is a `languageVersion` 2.0 template: its outputs are checked against
/// their types (which may be `$ref`s) and may be made by a `copy` of `count` times `input`.
pub(super) async fn evaluate_outputs(cx: &Cx<'_>, scope: &Scope, template: &Value, v2: bool, validate_only: bool) -> ArmResult<Outputs> {
    let mut out = Map::new();
    let Some(outputs) = template.get("outputs").and_then(Value::as_object) else { return Ok(Outputs::Done(out)) };
    let definitions = template.get("definitions").and_then(Value::as_object).cloned().unwrap_or_default();
    let types = Types::new(&definitions).with_budget(&scope.budget);
    for (n, o) in outputs {
        let ty = types
            .type_of(o)
            .ok()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| o.get("type").and_then(Value::as_str).unwrap_or("string").to_ascii_lowercase());
        let result = if let Some(c) = o.get("copy") {
            output_copy(cx, scope, c).await
        } else {
            scope.settle(cx, || scope.eval(cx, &[], o.get("value").unwrap_or(&Value::Null))).await
        };
        match result {
            Ok(v) => {
                if v2 {
                    types.check(o, &v).map_err(|m| invalid_template(format!("The template output '{n}' is not valid: {m}")))?;
                }
                // an output that is null has a type and no `value` (what a real deployment answers)
                out.insert(
                    n.clone(),
                    if v.is_null() { json!({"type": titlecase(&ty)}) } else { json!({"type": titlecase(&ty), "value": v}) },
                );
            }
            Err(EvalError::Invalid(m)) => return Err(invalid_template(format!("The template output '{n}' is not valid: {m}"))),
            Err(EvalError::Runtime(_) | EvalError::Remote(_)) if validate_only => continue,
            Err(EvalError::Runtime(m)) => {
                // the words of Azure: "The template output 'o' is not valid: ... usage details.." (a full stop is added)
                let error =
                    ArmError::bad_request("DeploymentOutputEvaluationFailed", format!("The template output '{n}' is not valid: {m}."));
                return Ok(Outputs::Failed { error, target: None, output: Some(n.clone()) });
            }
            Err(EvalError::Remote(r)) => {
                return Ok(Outputs::Failed {
                    error: ArmError::new(404, &r.code, r.message.clone()),
                    target: r.target.clone(),
                    output: None,
                })
            }
            Err(e) => {
                // while validating, an output that waits for a resource that is not deployed is not an error
                if validate_only {
                    continue;
                }
                return Err(invalid_template(format!("The template output '{n}' is not valid: {}", e.text())));
            }
        }
    }
    Ok(Outputs::Done(out))
}

/// An output made with `copy`: `{"copy": {"count": ..., "input": ...}}` is an array of `count` evaluations of `input`.
async fn output_copy(cx: &Cx<'_>, scope: &Scope, c: &Value) -> Result<Value, EvalError> {
    let count = scope.settle(cx, || scope.eval(cx, &[], c.get("count").unwrap_or(&json!(0)))).await?;
    let count = count.as_i64().or_else(|| count.as_str().and_then(|t| t.parse().ok())).unwrap_or(-1);
    if !(0..=limits::MAX_COPY_PROPERTIES).contains(&count) {
        return Err(EvalError::Invalid(format!(
            "The copy count {count} is not valid: it must be between 0 and {}.",
            limits::MAX_COPY_PROPERTIES
        )));
    }
    let name = c.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let input = c.get("input").cloned().unwrap_or(Value::Null);
    let mut items = Vec::new();
    let mut size = 0u64;
    for i in 0..count {
        let stack = [(name.clone(), i)];
        let item = scope.settle(cx, || scope.eval(cx, &stack, &input)).await?;
        size += limits::weight(&item).map_err(|m| EvalError::Invalid(format!("A copy loop made a value that is not allowed: {m}")))?;
        if size > limits::MAX_VALUE_BYTES {
            return Err(EvalError::Invalid(format!("The copy loop makes more than {} MB.", limits::MAX_VALUE_BYTES / (1024 * 1024))));
        }
        items.push(item);
    }
    Ok(Value::Array(items))
}
