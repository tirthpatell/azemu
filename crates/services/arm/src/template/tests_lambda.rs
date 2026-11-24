//! The examples of "Lambda functions for ARM templates" (Microsoft Learn, `template-functions-lambda`): the inputs and
//! the outputs are the ones printed on that page, function by function.

use super::tests::run;
use serde_json::{json, Value};

fn dogs() -> Value {
    json!({"dogs": [
        {"name": "Evie", "age": 5, "interests": ["Ball", "Frisbee"]},
        {"name": "Casper", "age": 3, "interests": ["Other dogs"]},
        {"name": "Indy", "age": 2, "interests": ["Butter"]},
        {"name": "Kira", "age": 8, "interests": ["Rubs"]}
    ]})
}

fn ok(expr: &str, vars: Value) -> Value {
    run(expr, json!({}), vars).unwrap_or_else(|e| panic!("{expr}: {e}"))
}

#[test]
fn filter() {
    // page: filter
    let old = ok("[filter(variables('dogs'), lambda('dog', greaterOrEquals(lambdaVariables('dog').age, 5)))]", dogs());
    assert_eq!(
        old,
        json!([{"name": "Evie", "age": 5, "interests": ["Ball", "Frisbee"]}, {"name": "Kira", "age": 8, "interests": ["Rubs"]}])
    );
    let by_index = ok(
        "[filter(variables('dogs'), lambda('val', 'i', and(less(lambdaVariables('i'), 2), equals(substring(lambdaVariables('val').name, 0, 1), 'C'))))]",
        dogs(),
    );
    assert_eq!(by_index, json!([{"name": "Casper", "age": 3, "interests": ["Other dogs"]}]));
    assert_eq!(ok("[filter(range(0, 10), lambda('i', equals(0, mod(lambdaVariables('i'), 2))))]", json!({})), json!([0, 2, 4, 6, 8]));
    let looped = json!({"itemForLoop": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]});
    assert_eq!(ok("[filter(variables('itemForLoop'), lambda('i', greater(lambdaVariables('i'), 5)))]", looped), json!([6, 7, 8, 9]));
}

#[test]
fn group_by() {
    // page: groupBy
    let v = json!({"inputArray": ["foo", "bar", "baz"]});
    assert_eq!(
        ok("[groupBy(variables('inputArray'), lambda('x', substring(lambdaVariables('x'), 0, 1)))]", v),
        json!({"f": ["foo"], "b": ["bar", "baz"]})
    );
}

#[test]
fn map() {
    // page: map
    assert_eq!(
        ok("[map(variables('dogs'), lambda('dog', lambdaVariables('dog').name))]", dogs()),
        json!(["Evie", "Casper", "Indy", "Kira"])
    );
    assert_eq!(
        ok("[map(variables('dogs'), lambda('dog', format('Hello {0}!', lambdaVariables('dog').name)))]", dogs()),
        json!(["Hello Evie!", "Hello Casper!", "Hello Indy!", "Hello Kira!"])
    );
    let by_range = "[map(range(0, length(variables('dogs'))), lambda('i', createObject('i', lambdaVariables('i'), 'dog', variables('dogs')[lambdaVariables('i')].name, 'greeting', format('Ahoy, {0}!', variables('dogs')[lambdaVariables('i')].name))))]";
    assert_eq!(
        ok(by_range, dogs()),
        json!([
            {"i": 0, "dog": "Evie", "greeting": "Ahoy, Evie!"},
            {"i": 1, "dog": "Casper", "greeting": "Ahoy, Casper!"},
            {"i": 2, "dog": "Indy", "greeting": "Ahoy, Indy!"},
            {"i": 3, "dog": "Kira", "greeting": "Ahoy, Kira!"}
        ])
    );
    assert_eq!(
        ok(
            "[map(variables('dogs'), lambda('x', 'i', createObject('index', lambdaVariables('i'), 'val', lambdaVariables('x').name)))]",
            dogs()
        ),
        json!([{"index": 0, "val": "Evie"}, {"index": 1, "val": "Casper"}, {"index": 2, "val": "Indy"}, {"index": 3, "val": "Kira"}])
    );
}

#[test]
fn map_values() {
    // page: mapValues
    let v = json!({"inputObject": {"foo": "foo", "bar": "bar"}});
    assert_eq!(
        ok("[mapValues(variables('inputObject'), lambda('val', toUpper(lambdaVariables('val'))))]", v),
        json!({"foo": "FOO", "bar": "BAR"})
    );
}

#[test]
fn reduce() {
    // page: reduce; the ages are 5, 3, 2 and 8
    let v = json!({"ages": [5, 3, 2, 8]});
    assert_eq!(
        ok("[reduce(variables('ages'), 0, lambda('cur', 'next', add(lambdaVariables('cur'), lambdaVariables('next'))))]", v.clone()),
        json!(18)
    );
    assert_eq!(
        ok("[reduce(variables('ages'), 1, lambda('cur', 'next', add(lambdaVariables('cur'), lambdaVariables('next'))))]", v.clone()),
        json!(19)
    );
    assert_eq!(
        ok(
            "[reduce(variables('ages'), 0, lambda('cur', 'next', 'i', if(equals(mod(lambdaVariables('i'), 2), 0), add(lambdaVariables('cur'), lambdaVariables('next')), lambdaVariables('cur'))))]",
            v
        ),
        json!(7)
    );
    assert_eq!(
        ok(
            "[reduce(createArray(createObject('foo', 123), createObject('bar', 456), createObject('baz', 789)), createObject(), lambda('cur', 'next', union(lambdaVariables('cur'), lambdaVariables('next'))))]",
            json!({})
        ),
        json!({"foo": 123, "bar": 456, "baz": 789})
    );
}

#[test]
fn sort() {
    // page: sort (youngest first)
    let sorted = ok("[sort(variables('dogs'), lambda('a', 'b', less(lambdaVariables('a').age, lambdaVariables('b').age)))]", dogs());
    let names: Vec<&str> = sorted.as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Indy", "Casper", "Evie", "Kira"]);
    // equal elements keep their order, and a longer array sorts
    assert_eq!(
        ok("[sort(createArray(3, 1, 2, 1, 5, 4, 0), lambda('a', 'b', less(lambdaVariables('a'), lambdaVariables('b'))))]", json!({})),
        json!([0, 1, 1, 2, 3, 4, 5])
    );
    let stable = ok(
        "[sort(createArray(createObject('k', 1, 'n', 'a'), createObject('k', 0, 'n', 'b'), createObject('k', 1, 'n', 'c'), createObject('k', 0, 'n', 'd')), lambda('a', 'b', less(lambdaVariables('a').k, lambdaVariables('b').k)))]",
        json!({}),
    );
    let order: Vec<&str> = stable.as_array().unwrap().iter().map(|d| d["n"].as_str().unwrap()).collect();
    assert_eq!(order, ["b", "d", "a", "c"]);
    assert_eq!(ok("[sort(createArray(), lambda('a', 'b', less(lambdaVariables('a'), lambdaVariables('b'))))]", json!({})), json!([]));
}

#[test]
fn to_object() {
    // page: toObject, the two forms of the call
    let by_name = ok("[toObject(variables('dogs'), lambda('entry', lambdaVariables('entry').name))]", dogs());
    assert_eq!(by_name["Casper"], json!({"name": "Casper", "age": 3, "interests": ["Other dogs"]}));
    assert_eq!(by_name.as_object().unwrap().len(), 4);
    let with_value = ok(
        "[toObject(variables('dogs'), lambda('entry', lambdaVariables('entry').name), lambda('entry', lambdaVariables('entry')))]",
        dogs(),
    );
    assert_eq!(by_name, with_value);
    let props = ok(
        "[toObject(variables('dogs'), lambda('entry', lambdaVariables('entry').name), lambda('entry', lambdaVariables('entry').interests))]",
        dogs(),
    );
    assert_eq!(props["Evie"], json!(["Ball", "Frisbee"]));
    let e = run("[toObject(createArray(1), lambda('x', lambdaVariables('x')))]", json!({}), json!({})).unwrap_err();
    assert!(e.contains("key lambda"), "{e}");
}

#[test]
fn lambda_errors_say_what_is_wrong() {
    let e = |expr: &str| run(expr, json!({}), json!({})).unwrap_err();
    assert!(e("[lambda('x', 1)]").contains("directly as an argument"));
    assert!(e("[lambdaVariables('x')]").contains("lambda variable 'x'"));
    assert!(e("[map(createArray(1), 'x')]").contains("lambda expression"));
    assert!(e("[map(createArray(1), lambda('x', 'y', 'z', 1))]").contains("1 or 2 variable"));
    assert!(e("[map(5, lambda('x', 1))]").contains("array"));
    assert!(e("[map(createArray(1), lambda('x', 'x', 1))]").contains("declared twice"));
    assert!(e("[filter(createArray(1), lambda('x', 'text'))]").contains("boolean"));
}

#[test]
fn lambda_variables_nest_and_do_not_leak() {
    // an inner lambda sees its own variable and the outer one; both are gone after the call
    assert_eq!(
        ok(
            "[map(createArray(1, 2), lambda('a', map(createArray(10, 20), lambda('b', add(lambdaVariables('a'), lambdaVariables('b'))))))]",
            json!({})
        ),
        json!([[11, 21], [12, 22]])
    );
    let e =
        run("[concat(map(createArray(1), lambda('a', lambdaVariables('a'))), lambdaVariables('a'))]", json!({}), json!({})).unwrap_err();
    assert!(e.contains("not defined"), "{e}");
    // a variable of the template cannot see the variable of the lambda that uses it
    let vars = json!({"v": "[lambdaVariables('a')]"});
    let e = run("[map(createArray(1), lambda('a', variables('v')))]", json!({}), vars).unwrap_err();
    assert!(e.contains("not defined"), "{e}");
}

#[test]
fn a_lambda_over_a_large_array_is_limited_work() {
    // 10,000 elements, each evaluated a few times in the body: allowed
    let r = ok("[length(map(range(0, 10000), lambda('i', add(lambdaVariables('i'), 1))))]", json!({}));
    assert_eq!(r, json!(10_000));
    // the work of one request is bounded: a lambda inside a lambda over big arrays runs out
    let e = run(
        "[length(map(range(0, 10000), lambda('i', length(map(range(0, 10000), lambda('j', add(lambdaVariables('i'), lambdaVariables('j'))))))))]",
        json!({}),
        json!({}),
    )
    .unwrap_err();
    assert!(e.contains("too complex"), "{e}");
}

#[test]
fn deeply_nested_lambdas_fit_the_stack() {
    // A stack overflow kills the process: the deepest lambda nesting that the parser allows runs on a 1 MiB stack.
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            for depth in [5usize, 20, 30, 40, 200] {
                let mut e = String::from("lambdaVariables('a0')");
                for i in (0..depth).rev() {
                    e = format!("map(createArray(1), lambda('a{i}', {e}))");
                }
                let r = run(&format!("[{e}]"), json!({}), json!({}));
                if depth <= 30 {
                    assert!(r.is_ok() || r.unwrap_err().contains("not defined"), "depth {depth}");
                } else {
                    assert!(r.unwrap_err().contains("levels deep"), "depth {depth}");
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
