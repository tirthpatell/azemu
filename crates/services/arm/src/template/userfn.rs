//! User-defined functions: the `functions` section of a template (Microsoft Learn,
//! `azure-resource-manager/templates/syntax#functions`). A function is called as `namespace.name(arguments)`.
//! Its restrictions, from the same page: it cannot read variables or the parameters of the template (only its own),
//! cannot call another user-defined function, cannot use `reference`, and its parameters have no default values.
//! Bicep writes its `func` declarations in the namespace `__bicep`.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::schema::{coerce, Types};
use super::{eval_value, Env, EvalError};

pub struct UserFn {
    params: Vec<(String, Value)>,
    output: Value,
    out_schema: Value,
}

#[derive(Default)]
pub struct Functions {
    map: HashMap<String, UserFn>,
    definitions: Map<String, Value>,
}

impl Functions {
    /// Read the `functions` and `definitions` of a template; `Err` is a problem with the template.
    pub fn from_template(template: &Value) -> Result<Functions, String> {
        let mut f = Functions {
            map: HashMap::new(),
            definitions: template.get("definitions").and_then(Value::as_object).cloned().unwrap_or_default(),
        };
        let Some(list) = template.get("functions") else { return Ok(f) };
        let list = list.as_array().ok_or("'functions' must be an array of namespaces.")?;
        for ns in list {
            let namespace = ns
                .get("namespace")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .ok_or("every entry of 'functions' needs a 'namespace'.")?;
            let members =
                ns.get("members").and_then(Value::as_object).ok_or_else(|| format!("the namespace '{namespace}' has no 'members'."))?;
            for (name, def) in members {
                let full = format!("{namespace}.{name}");
                let mut params = Vec::new();
                for p in def.get("parameters").and_then(Value::as_array).into_iter().flatten() {
                    let pname = p
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("a parameter of the function '{full}' has no name."))?;
                    if p.get("defaultValue").is_some() {
                        return Err(format!("the parameter '{pname}' of the function '{full}' has a default value; the parameters of user-defined functions cannot have one."));
                    }
                    params.push((pname.to_string(), p.clone()));
                }
                let out = def.get("output").ok_or_else(|| format!("the function '{full}' has no 'output'."))?;
                let value = out.get("value").ok_or_else(|| format!("the output of the function '{full}' has no 'value'."))?;
                if f.map.insert(full.to_ascii_lowercase(), UserFn { params, output: value.clone(), out_schema: out.clone() }).is_some() {
                    return Err(format!("the function '{full}' is defined twice."));
                }
            }
        }
        Ok(f)
    }

    pub fn get(&self, lname: &str) -> Option<&UserFn> {
        self.map.get(lname)
    }
}

impl UserFn {
    /// Run the function with `args`, in an environment that holds only the function's own parameters.
    pub fn call(&self, name: &str, args: Vec<Value>, env: &Env<'_>) -> Result<Value, EvalError> {
        if args.len() != self.params.len() {
            return Err(EvalError::Msg(format!(
                "The template function '{name}' takes {} argument(s), but {} were given.",
                self.params.len(),
                args.len()
            )));
        }
        let defs = env.funcs.map(|f| &f.definitions);
        let empty = Map::new();
        let types = Types::new(defs.unwrap_or(&empty)).with_budget(&env.budget);
        let mut bound = Map::new();
        for ((pname, schema), v) in self.params.iter().zip(args) {
            let ty = types.type_of(schema).map_err(EvalError::Msg)?;
            let v = if v.is_null() || ty.is_empty() {
                v
            } else {
                coerce(&ty, v)
                    .map_err(|m| EvalError::Msg(format!("The argument '{pname}' of the template function '{name}' is not valid: {m}.")))?
            };
            types
                .check(schema, &v)
                .map_err(|m| EvalError::Msg(format!("The argument '{pname}' of the template function '{name}' is not valid: {m}")))?;
            bound.insert(pname.clone(), v);
        }
        let novars = Map::new();
        let child = env.function_scope(&bound, &novars);
        let out = eval_value(&self.output, &child)?;
        types
            .check(&self.out_schema, &out)
            .map_err(|m| EvalError::Msg(format!("The output of the template function '{name}' is not valid: {m}")))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_namespaces_and_refuses_defaults() {
        let t = json!({"functions": [{"namespace": "Contoso", "members": {"upper": {
            "parameters": [{"name": "s", "type": "string"}],
            "output": {"type": "string", "value": "[toUpper(parameters('s'))]"}}}}]});
        let f = Functions::from_template(&t).ok().unwrap();
        assert!(f.get("contoso.upper").is_some());
        let bad = json!({"functions": [{"namespace": "c", "members": {"f": {
            "parameters": [{"name": "s", "type": "string", "defaultValue": "x"}],
            "output": {"type": "string", "value": "x"}}}}]});
        assert!(Functions::from_template(&bad).err().unwrap().contains("default value"));
        assert!(Functions::from_template(&json!({"functions": [{"members": {}}]})).is_err());
    }
}
