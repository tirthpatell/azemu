use super::*;

pub(super) fn run(expr: &str, params: Value, vars: Value) -> Result<Value, String> {
    let p = params.as_object().cloned().unwrap_or_default();
    let vv = vars.as_object().cloned().unwrap_or_default();
    let rg = json!({"id": "/subscriptions/s/resourceGroups/rg", "name": "rg", "location": "eastus", "tags": {}});
    let sub = json!({"subscriptionId": "s", "id": "/subscriptions/s"});
    let dep = json!({"name": "d"});
    let look = |n: &str, _full: bool| -> Result<Value, EvalError> {
        if n == "later" {
            Err(EvalError::Defer("not yet".into()))
        } else {
            Ok(json!({"endpoint": format!("https://{n}/")}))
        }
    };
    let env = Env::new(&p, &vv, "s", "rg", &rg, &sub, "t", &dep, &look);
    eval_value(&Value::String(expr.to_string()), &env).map_err(|e| e.text())
}

#[test]
fn strings_and_parameters() {
    let p = json!({"name": "abc", "n": 3});
    assert_eq!(run("[concat(parameters('name'), '-', 'x')]", p.clone(), json!({})).unwrap(), json!("abc-x"));
    assert_eq!(run("[toLower('ABC')]", p.clone(), json!({})).unwrap(), json!("abc"));
    assert_eq!(run("[format('{0}-{1}-{0}', 'a', 7)]", p.clone(), json!({})).unwrap(), json!("a-7-a"));
    assert_eq!(run("[substring('abcdef', 1, 3)]", p.clone(), json!({})).unwrap(), json!("bcd"));
    assert_eq!(run("[add(parameters('n'), 4)]", p.clone(), json!({})).unwrap(), json!(7));
    assert_eq!(run("[concat('it''s')]", p, json!({})).unwrap(), json!("it's"));
}

#[test]
fn escapes_and_plain_strings() {
    assert_eq!(run("[[literal]", json!({}), json!({})).unwrap(), json!("[literal]"));
    assert_eq!(run("plain", json!({}), json!({})).unwrap(), json!("plain"));
}

#[test]
fn variables_refer_to_each_other() {
    let vars = json!({"a": "[concat(parameters('p'), '1')]", "b": "[concat(variables('a'), '2')]"});
    assert_eq!(run("[variables('b')]", json!({"p": "x"}), vars).unwrap(), json!("x12"));
    let looping = json!({"a": "[variables('a')]"});
    assert!(run("[variables('a')]", json!({}), looping).is_err());
}

#[test]
fn resource_functions() {
    assert_eq!(run("[resourceGroup().location]", json!({}), json!({})).unwrap(), json!("eastus"));
    assert_eq!(run("[subscription().subscriptionId]", json!({}), json!({})).unwrap(), json!("s"));
    assert_eq!(
        run("[resourceId('Microsoft.Storage/storageAccounts', 'acct')]", json!({}), json!({})).unwrap(),
        json!("/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct")
    );
    assert_eq!(
        run(
            "[resourceId('other-rg', 'Microsoft.Storage/storageAccounts/blobServices/containers', 'a', 'default', 'c')]",
            json!({}),
            json!({})
        )
        .unwrap(),
        json!("/subscriptions/s/resourceGroups/other-rg/providers/Microsoft.Storage/storageAccounts/a/blobServices/default/containers/c")
    );
    assert_eq!(run("[reference('acct').endpoint]", json!({}), json!({})).unwrap(), json!("https://acct/"));
}

#[test]
fn unique_string_is_stable_and_13_long() {
    let a = run("[uniqueString(resourceGroup().id)]", json!({}), json!({})).unwrap();
    let b = run("[uniqueString(resourceGroup().id)]", json!({}), json!({})).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.as_str().unwrap().len(), 13);
}

#[test]
fn logic_and_indexing() {
    assert_eq!(run("[if(equals('A','a'), 'yes', 'no')]", json!({}), json!({})).unwrap(), json!("no")); // case-sensitive, as Azure answers
    assert_eq!(run("[if(equals('a','a'), 'yes', 'no')]", json!({}), json!({})).unwrap(), json!("yes"));
    assert_eq!(run("[createArray(1,2,3)[1]]", json!({}), json!({})).unwrap(), json!(2));
    assert_eq!(run("[createObject('k', 'v').k]", json!({}), json!({})).unwrap(), json!("v"));
    assert_eq!(run("[concat(createArray(1), createArray(2))]", json!({}), json!({})).unwrap(), json!([1, 2]));
}

#[test]
fn errors_name_the_problem() {
    let e = run("[parameters('missing')]", json!({}), json!({})).unwrap_err();
    assert!(e.contains("missing"));
    let e = run("[listKeys('x', '2020-01-01')]", json!({}), json!({})).unwrap_err();
    assert!(e.contains("listKeys"));
    let e = run("[bogus()]", json!({}), json!({})).unwrap_err();
    assert!(e.contains("bogus"));
    assert!(run("[concat('a'", json!({}), json!({})).unwrap().is_string()); // not an expression: no closing bracket
}

#[test]
fn deferred_reference() {
    assert!(matches!(
        eval_expr("reference('later')", &{
            let p = Map::new();
            let leak: &'static Map<String, Value> = Box::leak(Box::new(p));
            let rg: &'static Value = Box::leak(Box::new(json!({})));
            let look: &'static dyn Fn(&str, bool) -> Result<Value, EvalError> =
                Box::leak(Box::new(|_: &str, _: bool| -> Result<Value, EvalError> { Err(EvalError::Defer("x".into())) }));
            Env::new(leak, leak, "s", "r", rg, rg, "t", rg, look)
        }),
        Err(EvalError::Defer(_))
    ));
}

// ---- limits ----

/// A stack overflow kills the whole process. A tokio worker thread has 2 MiB (and the request handler uses some of it
/// before it gets here); these run in 1 MiB, so the limits keep a margin of at least two.
fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new().stack_size(1024 * 1024).spawn(f).unwrap().join().unwrap()
}

fn invalid(e: Result<Value, String>) -> String {
    e.expect_err("expected an error")
}

#[test]
fn nested_calls_are_limited_not_a_stack_overflow() {
    on_small_stack(|| {
        for depth in [10usize, 64, 65, 200, 2000] {
            let expr = format!("[{}'a'{}]", "concat(".repeat(depth), ")".repeat(depth));
            let r = run(&expr, json!({}), json!({}));
            if depth <= 63 {
                assert_eq!(r.unwrap(), json!("a"), "depth {depth}");
            } else if depth >= 65 {
                assert!(invalid(r).contains("levels deep"), "depth {depth}");
            }
        }
        // a long chain of `.a.a.a` or `[0][0]` nests the same way
        let chain = format!("[createObject('a', 1){}]", ".a".repeat(5000));
        assert!(invalid(run(&chain, json!({}), json!({}))).contains("levels deep"));
        let idx = format!("[createArray(1){}]", "[0]".repeat(5000));
        assert!(invalid(run(&idx, json!({}), json!({}))).contains("levels deep"));
        // nested indexes: a[a[a[...]]]
        let idx = format!("[{}0{}]", "createArray(0)[".repeat(1500), "]".repeat(1500));
        assert!(invalid(run(&idx, json!({}), json!({}))).contains("levels deep"));
    });
}

#[test]
fn the_deepest_allowed_nesting_fits_the_stack() {
    on_small_stack(|| {
        // 63 nested `if` and `concat`, and the evaluation depth cap of variables that refer to each other
        let expr = format!("[{}'a'{}]", "if(true, ".repeat(63), ", 'b')".repeat(63));
        assert_eq!(run(&expr, json!({}), json!({})).unwrap(), json!("a"));
        let mut vars = serde_json::Map::new();
        vars.insert("v0".into(), json!("x"));
        for i in 1..=200 {
            vars.insert(format!("v{i}"), json!(format!("[concat(variables('v{}'), 'x')]", i - 1)));
        }
        let r = run("[variables('v200')]", json!({}), Value::Object(vars.clone()));
        assert!(invalid(r).contains("levels deep"));
        let ok = run("[variables('v30')]", json!({}), Value::Object(vars)).unwrap();
        assert_eq!(ok.as_str().unwrap().len(), 31);
    });
}

#[test]
fn variables_holding_objects_and_deep_json_fit_the_stack() {
    on_small_stack(|| {
        // v_i = {"a": "[variables('v_(i-1)')]"}: three levels of evaluation per hop
        let mut vars = serde_json::Map::new();
        vars.insert("v0".into(), json!("x"));
        for i in 1..=100 {
            vars.insert(format!("v{i}"), json!({"a": [format!("[createArray(variables('v{}'))]", i - 1)]}));
        }
        let e = invalid(run("[variables('v100')]", json!({}), Value::Object(vars.clone())));
        assert!(e.contains("levels deep"), "{e}");
        assert!(run("[variables('v12')]", json!({}), Value::Object(vars)).is_ok());
        // a JSON value 120 levels deep (what serde_json accepts) holding an expression at the bottom
        let mut v = json!("[concat('a', 'b')]");
        for _ in 0..120 {
            v = json!({"k": v});
        }
        let p = Map::new();
        let vv = Map::new();
        let rg = json!({});
        let look = |_: &str, _: bool| -> Result<Value, EvalError> { Ok(json!({})) };
        let env = Env::new(&p, &vv, "s", "rg", &rg, &rg, "t", &rg, &look);
        assert!(eval_value(&v, &env).is_ok());
        // values the evaluation builds are at most 100 deep
        let nest = "createArray(".repeat(50) + "1" + &")".repeat(50);
        let vars = json!({"a": format!("[{nest}]"), "b": "[createArray(createArray(variables('a')))]"});
        assert!(run("[createArray(variables('b'), variables('b'))]", json!({}), vars).is_ok());
    });
}

#[test]
fn an_expression_has_at_most_24576_characters() {
    let long = format!("[concat('{}')]", "a".repeat(24_600));
    assert!(invalid(run(&long, json!({}), json!({}))).contains("24576"));
    let fine = format!("[concat('{}')]", "a".repeat(24_000));
    assert_eq!(run(&fine, json!({}), json!({})).unwrap().as_str().unwrap().len(), 24_000);
}

#[test]
fn range_is_bounded() {
    assert_eq!(run("[length(range(0, 10000))]", json!({}), json!({})).unwrap(), json!(10_000));
    assert!(invalid(run("[range(0, 10001)]", json!({}), json!({}))).contains("10000"));
    assert!(invalid(run("[range(0, 2000000000)]", json!({}), json!({}))).contains("range"));
    assert!(invalid(run("[range(0, -1)]", json!({}), json!({}))).contains("range"));
    assert!(invalid(run("[range(2147483647, 3)]", json!({}), json!({}))).contains("2147483647"));
    assert!(invalid(run("[range(9223372036854775807, 3)]", json!({}), json!({}))).contains("range"));
    assert_eq!(run("[range(2147483645, 2)]", json!({}), json!({})).unwrap(), json!([2147483645, 2147483646]));
}

#[test]
fn arithmetic_is_checked() {
    let max = i64::MAX;
    let min = i64::MIN;
    for expr in [
        format!("[add({max}, 1)]"),
        format!("[sub({}, 2)]", min + 1),
        format!("[mul({max}, 2)]"),
        format!("[div({}, -1)]", min),
        format!("[mod({}, -1)]", min),
        "[div(1, 0)]".to_string(),
        "[mod(1, 0)]".to_string(),
    ] {
        // `-9223372036854775808` does not parse as a literal in the expression language: use sub() to reach it
        let expr = expr.replace(&min.to_string(), "sub(-9223372036854775807, 1)");
        let e = invalid(run(&expr, json!({}), json!({})));
        assert!(e.contains("64-bit") || e.contains("division by zero"), "{expr}: {e}");
    }
    assert_eq!(run("[add(2, 3)]", json!({}), json!({})).unwrap(), json!(5));
    assert_eq!(run("[div(7, 2)]", json!({}), json!({})).unwrap(), json!(3));
    assert_eq!(run("[mod(7, 4)]", json!({}), json!({})).unwrap(), json!(3));
    assert!(run("[copyIndex(9223372036854775807)]", json!({}), json!({})).unwrap_err().contains("outside a copy loop"));
}

#[test]
fn format_widths_and_results_are_bounded() {
    assert_eq!(run("[format('{0:D4}', 7)]", json!({}), json!({})).unwrap(), json!("0007"));
    assert!(invalid(run("[format('{0:D4000000000}', 1)]", json!({}), json!({}))).contains("format"));
    assert!(invalid(run("[format('{0:N99999999999999999999}', 1)]", json!({}), json!({}))).contains("format"));
    assert!(invalid(run("[format('{18446744073709551615}', 1)]", json!({}), json!({}))).contains("no argument"));
    // many references to a big argument
    let r = run(&format!("[format('{}', padLeft('x', 1000000, 'x'))]", "{0}".repeat(8000)), json!({}), json!({}));
    assert!(invalid(r).contains("larger"));
}

#[test]
fn strings_and_arrays_made_by_other_functions_are_bounded() {
    let big = "[padLeft('x', 4000000, 'x')]";
    assert_eq!(run(big, json!({}), json!({})).unwrap().as_str().unwrap().len(), 4_000_000);
    assert!(invalid(run("[padLeft('x', 5000000, 'x')]", json!({}), json!({}))).contains("larger"));
    // replace with a long replacement for each of many matches
    let r = run("[replace(padLeft('a', 100000, 'a'), 'a', padLeft('b', 100000, 'b'))]", json!({}), json!({}));
    assert!(invalid(r).contains("larger"));
    assert!(invalid(run("[replace('abc', '', 'x')]", json!({}), json!({}))).contains("empty"));
    // substring with huge numbers does not overflow
    assert!(invalid(run("[substring('abc', 9223372036854775807, 9223372036854775807)]", json!({}), json!({}))).contains("out of range"));
    assert_eq!(run("[take('abc', 9223372036854775807)]", json!({}), json!({})).unwrap(), json!("abc"));
    assert_eq!(run("[skip(createArray(1, 2), 9223372036854775807)]", json!({}), json!({})).unwrap(), json!([]));
}

#[test]
fn variables_are_evaluated_once() {
    on_small_stack(|| {
        // v_i = concat(v_(i-1), v_(i-1)): 2^i evaluations without a cache, a string of 2^i bytes
        let mut vars = serde_json::Map::new();
        vars.insert("v0".into(), json!("x"));
        for i in 1..=30 {
            vars.insert(format!("v{i}"), json!(format!("[concat(variables('v{}'), variables('v{}'))]", i - 1, i - 1)));
        }
        let started = std::time::Instant::now();
        let ok = run("[length(variables('v10'))]", json!({}), Value::Object(vars.clone())).unwrap();
        assert_eq!(ok, json!(1024));
        // 30 levels would be a string of 1 GiB: refused by the size limit after a few steps, quickly
        let e = invalid(run("[length(variables('v30'))]", json!({}), Value::Object(vars)));
        assert!(e.contains("larger") || e.contains("complex"), "{e}");
        assert!(started.elapsed().as_secs() < 10, "took {:?}", started.elapsed());
        // a variable that refers to itself, and two that refer to each other
        let looping = json!({"a": "[variables('b')]", "b": "[variables('a')]"});
        assert!(invalid(run("[variables('a')]", json!({}), looping)).contains("refers to itself"));
    });
}

#[test]
fn a_variable_is_evaluated_one_time_per_use_count() {
    // The evaluation cost of n references to the same variable is n, not the size of its own definition each time.
    let vars = json!({"big": "[concat('a', 'b', 'c')]"});
    let counter = std::cell::Cell::new(0u32);
    let look = |_: &str, _: bool| -> Result<Value, EvalError> {
        counter.set(counter.get() + 1);
        Ok(json!({}))
    };
    let p = Map::new();
    let vv = vars.as_object().cloned().unwrap();
    let rg = json!({});
    let env = Env::new(&p, &vv, "s", "rg", &rg, &rg, "t", &rg, &look);
    for _ in 0..5 {
        assert_eq!(eval_expr("variables('big')", &env).ok(), Some(json!("abc")));
    }
    assert_eq!(env.memo.get("big|[]"), Some(json!("abc")));
}

#[test]
fn the_work_of_one_request_is_budgeted() {
    // 10,000 numbers, referenced again and again through a variable: each use costs its size
    let vars = json!({"r": "[range(0, 10000)]"});
    let mut expr = String::from("[length(concat(");
    let parts: Vec<String> = (0..1000).map(|_| "variables('r')".to_string()).collect();
    expr.push_str(&parts.join(","));
    expr.push_str("))]");
    let started = std::time::Instant::now();
    let e = invalid(run(&expr, json!({}), vars));
    assert!(e.contains("too complex") || e.contains("larger"), "{e}");
    assert!(started.elapsed().as_secs() < 10);
}

#[test]
fn union_is_not_quadratic() {
    let a = "[length(union(range(0, 10000), range(5000, 10000)))]";
    assert_eq!(run(a, json!({}), json!({})).unwrap(), json!(15_000));
    assert_eq!(run("[union(createArray('A', 'b'), createArray('a', 'c'))]", json!({}), json!({})).unwrap(), json!(["A", "b", "a", "c"]));
    assert_eq!(run("[union(createObject('a', 1), createObject('b', 2))]", json!({}), json!({})).unwrap(), json!({"a": 1, "b": 2}));
}

#[test]
fn copy_in_properties_is_bounded() {
    let env_run = |count: i64| {
        let p = Map::new();
        let v = Map::new();
        let rg = json!({});
        let look = |_: &str, _: bool| -> Result<Value, EvalError> { Ok(json!({})) };
        let env = Env::new(&p, &v, "s", "rg", &rg, &rg, "t", &rg, &look);
        eval_value(&json!({"copy": [{"name": "items", "count": count, "input": {"i": "[copyIndex()]"}}]}), &env).map_err(|e| e.text())
    };
    assert_eq!(env_run(3).unwrap()["items"].as_array().unwrap().len(), 3);
    assert!(env_run(2_000_000_000).unwrap_err().contains("copy count"));
    assert!(env_run(-1).unwrap_err().contains("copy count"));
}
