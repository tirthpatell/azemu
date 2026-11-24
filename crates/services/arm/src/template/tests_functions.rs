//! The examples of the pages of "ARM template functions" (Microsoft Learn, `azure-resource-manager/templates/
//! template-functions-*`): the input and the output of each test are the ones printed on the page of the function named
//! in the comment above it.

use super::dotnet::Dt;
use super::tests::run;
use super::*;
use serde_json::{json, Value};

fn ok(expr: &str) -> Value {
    run(expr, json!({}), json!({})).unwrap_or_else(|e| panic!("{expr}: {e}"))
}

fn ok_with(expr: &str, vars: Value) -> Value {
    run(expr, json!({}), vars).unwrap_or_else(|e| panic!("{expr}: {e}"))
}

fn fails(expr: &str) -> String {
    run(expr, json!({}), json!({})).expect_err(expr)
}

/// Evaluate a parameter's default value, the one place where `utcNow` and `newGuid` are allowed.
fn default_value(expr: &str, now: &str) -> Result<Value, String> {
    let p = Map::new();
    let rg = json!({"location": "eastus"});
    let look = |_: &str, _: bool| -> Result<Value, EvalError> { Ok(json!({})) };
    let env = Env::new(&p, &p, "s", "rg", &rg, &rg, "t", &rg, &look)
        .for_defaults()
        .with_now(dotnet::parse(now).unwrap_or(Dt { secs: 0, ticks: 0 }));
    eval_value(&Value::String(expr.to_string()), &env).map_err(|e| e.text())
}

// ---- date functions (template-functions-date)

#[test]
fn date_time_add_remarks() {
    // "Remarks": P1Y is 365 days and P1M is 30 days
    assert_eq!(ok("[dateTimeAdd('2023-01-01 00:00:00Z', 'P1Y')]"), json!("2024-01-01T00:00:00Z"));
    assert_eq!(ok("[dateTimeAdd('2024-01-01 00:00:00Z', 'P1Y')]"), json!("2024-12-31T00:00:00Z"));
    assert_eq!(ok("[dateTimeAdd('2023-02-01 00:00:00Z', 'P1M')]"), json!("2023-03-03T00:00:00Z"));
    assert_eq!(ok("[dateTimeAdd('2024-02-01 00:00:00Z', 'P1M')]"), json!("2024-03-02T00:00:00Z"));
}

#[test]
fn date_time_add_example() {
    // "Examples": base 2020-04-07 14:53:14Z, formats as in the page's output table
    assert_eq!(ok("[dateTimeAdd('2020-04-07 14:53:14Z', 'P3Y')]"), json!("2023-04-07T14:53:14Z"));
    assert_eq!(ok("[dateTimeAdd('2020-04-07 14:53:14Z', '-P9D')]"), json!("2020-03-29T14:53:14Z"));
    assert_eq!(ok("[dateTimeAdd('2020-04-07 14:53:14Z', 'PT1H')]"), json!("2020-04-07T15:53:14Z"));
    assert_eq!(ok("[dateTimeAdd('2020-04-07 14:53:14Z', 'PT1H', 'u')]"), json!("2020-04-07 15:53:14Z"));
    assert_eq!(ok("[dateTimeAdd('2020-04-07 14:53:14Z', 'PT1H', 'yyyyMMddTHHmmssZ')]"), json!("20200407T155314Z"));
    // the base may be what utcNow() returns
    assert_eq!(ok("[dateTimeAdd('20200407T145314Z', 'P1D')]"), json!("2020-04-08T14:53:14Z"));
}

#[test]
fn date_time_epoch_functions() {
    // dateTimeToEpoch / dateTimeFromEpoch: 2023-05-02T15:16:13Z is 1683040573
    assert_eq!(ok("[dateTimeToEpoch('2023-05-02T15:16:13Z')]"), json!(1_683_040_573));
    assert_eq!(ok("[dateTimeFromEpoch(1683040573)]"), json!("2023-05-02T15:16:13Z"));
    assert_eq!(ok("[dateTimeFromEpoch(dateTimeToEpoch(dateTimeAdd('2023-05-02T15:16:13Z', 'P1Y')))]"), json!("2024-05-01T15:16:13Z"));
    assert!(fails("[dateTimeToEpoch('yesterday')]").contains("not a valid ISO8601 DateTime string"));
    assert!(fails("[dateTimeFromEpoch('x')]").contains("integer"));
    assert!(fails("[dateTimeAdd('2020-01-01', 'P1')]").contains("ISO8601 Duration"));
    assert!(fails("[dateTimeAdd('2020-01-01', 'P1W')]").contains("The second parameter 'P1W' is not a valid ISO8601 Duration string"));
    assert!(fails("[dateTimeAdd('2020-01-01', 'P1D', 'a')]").contains("The third parameter 'a' is not a valid DateTime format string"));
    assert!(fails("[dateTimeAdd('2020-01-01', 'P1D', 'U')]").contains("third parameter 'U'"));
    assert!(fails("[dateTimeAdd('9999-12-31T00:00:00Z', 'P2D')]").contains("years 1 to 9999"));
    assert!(fails("[string(float(2))]").contains("The provided value is of type 'Float'"));
    assert!(fails("[format('{0:N2}', 3.14159)]").contains("float point numbers are not supported"));
}

#[test]
fn utc_now_formats() {
    // utcNow examples: the default is yyyyMMddTHHmmssZ, 'd' is MM/dd/yyyy, 'M d' is month and day
    let now = "2019-03-05T17:53:18Z";
    assert_eq!(default_value("[utcNow()]", now).unwrap(), json!("20190305T175318Z"));
    assert_eq!(default_value("[utcNow('d')]", now).unwrap(), json!("03/05/2019"));
    assert_eq!(default_value("[utcNow('M d')]", now).unwrap(), json!("3 5"));
    assert_eq!(default_value("[utcNow('u')]", now).unwrap(), json!("2019-03-05 17:53:18Z"));
    assert!(default_value("[utcNow('a')]", now).unwrap_err().contains("'a'"));
    // "can only be used in the default value for a parameter"
    assert!(fails("[utcNow()]").contains("parameter default value expresions"));
    assert!(fails("[newGuid()]").contains("parameter default value expresions"));
    assert!(fails("[concat('x', utcNow('d'))]").contains("not expected at this location"));
}

#[test]
fn new_guid_in_a_default() {
    let a = default_value("[newGuid()]", "2020-01-01T00:00:00Z").unwrap();
    let b = default_value("[newGuid()]", "2020-01-01T00:00:00Z").unwrap();
    assert_eq!(a.as_str().unwrap().len(), 36);
    assert_ne!(a, b);
}

// ---- array functions (template-functions-array)

#[test]
fn array_function() {
    assert_eq!(ok("[array(1)]"), json!([1]));
    assert_eq!(ok("[array('efgh')]"), json!(["efgh"]));
    assert_eq!(ok("[array(createObject('a', 'b', 'c', 'd'))]"), json!([{"a": "b", "c": "d"}]));
    assert_eq!(ok("[array(createArray(1, 2))]"), json!([1, 2]));
}

#[test]
fn distinct_function() {
    let v = json!({
        "numbers": [1, 2, 2, 3, 1], "names": ["apple", "banana", "apple", "cherry"],
        "resources": [{"name": "storage1", "type": "Microsoft.Storage/storageAccounts"}, {"name": "vm1", "type": "Microsoft.Compute/virtualMachines"}, {"name": "storage1", "type": "Microsoft.Storage/storageAccounts"}]
    });
    assert_eq!(ok_with("[distinct(variables('numbers'))]", v.clone()), json!([1, 2, 3]));
    assert_eq!(ok_with("[distinct(variables('names'))]", v.clone()), json!(["apple", "banana", "cherry"]));
    assert_eq!(
        ok_with("[distinct(variables('resources'))]", v),
        json!([{"name": "storage1", "type": "Microsoft.Storage/storageAccounts"}, {"name": "vm1", "type": "Microsoft.Compute/virtualMachines"}])
    );
}

#[test]
fn index_from_end_functions() {
    let v = json!({"items": ["apple", "banana", "orange", "grape"]});
    assert_eq!(ok_with("[indexFromEnd(variables('items'), 2)]", v.clone()), json!("orange"));
    assert_eq!(ok_with("[indexFromEnd(variables('items'), 1)]", v.clone()), json!("grape"));
    assert!(run("[indexFromEnd(variables('items'), 5)]", json!({}), v.clone()).unwrap_err().contains("out of range"));
    assert!(run("[indexFromEnd(variables('items'), 0)]", json!({}), v.clone()).is_err());
    assert_eq!(ok_with("[tryIndexFromEnd(variables('items'), 5)]", v.clone()), Value::Null);
    assert_eq!(ok_with("[tryIndexFromEnd(variables('items'), 4)]", v), json!("apple"));
}

#[test]
fn index_of_and_last_index_of_on_arrays() {
    // the page's variables and its output table
    let v = json!({
        "names": ["one", "two", "three"], "numbers": [4, 5, 6],
        "collection": [["one", "two", "three"], [4, 5, 6]], "duplicates": [1, 2, 3, 1]
    });
    let e = |x: &str| ok_with(x, v.clone());
    assert_eq!(e("[lastIndexOf(variables('names'), 'two')]"), json!(1));
    assert_eq!(e("[indexOf(variables('names'), 'one')]"), json!(0));
    assert_eq!(e("[lastIndexOf(variables('names'), 'Three')]"), json!(-1));
    assert_eq!(e("[lastIndexOf(variables('numbers'), 4)]"), json!(0));
    assert_eq!(e("[indexOf(variables('numbers'), 6)]"), json!(2));
    assert_eq!(e("[lastIndexOf(variables('numbers'), '5')]"), json!(-1));
    assert_eq!(e("[indexOf(variables('collection'), variables('numbers'))]"), json!(1));
    assert_eq!(e("[indexOf(variables('duplicates'), 1)]"), json!(0));
    assert_eq!(e("[lastIndexOf(variables('duplicates'), 1)]"), json!(3));
}

#[test]
fn index_of_and_last_index_of_on_strings() {
    // template-functions-string: case-insensitive
    assert_eq!(ok("[indexOf('test', 't')]"), json!(0));
    assert_eq!(ok("[lastIndexOf('test', 't')]"), json!(3));
    assert_eq!(ok("[indexOf('abcdef', 'CD')]"), json!(2));
    assert_eq!(ok("[lastIndexOf('abcdef', 'AB')]"), json!(0));
    assert_eq!(ok("[indexOf('abcdef', 'z')]"), json!(-1));
}

#[test]
fn intersection_function() {
    // template-functions-array#intersection (the same text is on the object page)
    assert_eq!(
        ok("[intersection(createObject('one', 'a', 'two', 'b', 'three', 'c'), createObject('one', 'a', 'two', 'z', 'three', 'c'))]"),
        json!({"one": "a", "three": "c"})
    );
    assert_eq!(ok("[intersection(createArray('one', 'two', 'three'), createArray('two', 'three'))]"), json!(["two", "three"]));
    assert_eq!(ok("[intersection(createArray(1, 2, 3), createArray(2, 3, 4), createArray(3, 9))]"), json!([3]));
    assert!(fails("[intersection(createArray(1), createObject())]").contains("every argument"));
}

#[test]
fn flatten_function() {
    assert_eq!(ok("[flatten(createArray(createArray(1, 2), createArray(3), createArray()))]"), json!([1, 2, 3]));
    assert!(fails("[flatten('x')]").contains("array"));
}

// ---- object functions (template-functions-object)

#[test]
fn items_function() {
    // the page sorts the entries by key: item001 comes before item002 although it is defined second
    let v = json!({"entities": {"item002": {"enabled": false, "displayName": "Example item 2", "number": 200}, "item001": {"enabled": true, "displayName": "Example item 1", "number": 300}}});
    assert_eq!(
        ok_with("[items(variables('entities'))]", v.clone()),
        json!([
            {"key": "item001", "value": {"displayName": "Example item 1", "enabled": true, "number": 300}},
            {"key": "item002", "value": {"displayName": "Example item 2", "enabled": false, "number": 200}}
        ])
    );
    assert_eq!(ok_with("[items(variables('entities'))[0].value.number]", v), json!(300));
}

#[test]
fn null_object_keys_shallow_merge_and_try_get() {
    // null(): empty(null()) is true
    assert_eq!(ok("[empty(null())]"), json!(true));
    assert_eq!(ok("[objectKeys(createObject('a', 1, 'b', 2))]"), json!(["a", "b"]));
    let v = json!({
        "firstArray": [{"one": "a"}, {"two": "b"}, {"two": "c"}],
        "secondArray": [{"one": "a", "nested": {"a": 1, "nested": {"c": 3}}}, {"two": "b", "nested": {"b": 2}}],
        "users": {"name": "John Doe", "age": 30}, "colors": ["red", "green"]
    });
    assert_eq!(ok_with("[shallowMerge(variables('firstArray'))]", v.clone()), json!({"one": "a", "two": "c"}));
    assert_eq!(ok_with("[shallowMerge(variables('secondArray'))]", v.clone()), json!({"one": "a", "nested": {"b": 2}, "two": "b"}));
    // tryGet
    assert_eq!(ok_with("[tryGet(variables('users'), 'region')]", v.clone()), Value::Null);
    assert_eq!(ok_with("[tryGet(variables('users'), 'name')]", v.clone()), json!("John Doe"));
    assert_eq!(ok_with("[tryGet(variables('colors'), 0)]", v.clone()), json!("red"));
    assert_eq!(ok_with("[tryGet(variables('colors'), 5)]", v), Value::Null);
}

// ---- string functions (template-functions-string)

#[test]
fn string_functions_of_the_page() {
    // join
    let v = json!({"arrayString": ["one", "two", "three"]});
    assert_eq!(ok_with("[join(variables('arrayString'), ',')]", v.clone()), json!("one,two,three"));
    assert_eq!(ok_with("[join(variables('arrayString'), ';')]", v), json!("one;two;three"));
    // like
    let names = json!({"resourceName": "prod-eastus-vm01"});
    for (pattern, want) in
        [("*prod*", true), ("prod*", true), ("*01", true), ("prod-eastus-vm01", true), ("*westus*", false), ("PROD*", true)]
    {
        assert_eq!(ok_with(&format!("[like(variables('resourceName'), '{pattern}')]"), names.clone()), json!(want), "{pattern}");
    }
    // dataUri and dataUriToString
    assert_eq!(ok("[dataUri('Hello')]"), json!("data:text/plain;charset=utf-8;base64,SGVsbG8="));
    assert_eq!(ok("[dataUriToString('data:;base64,SGVsbG8sIFdvcmxkIQ==')]"), json!("Hello, World!"));
    assert_eq!(ok("[dataUriToString(dataUri('Hello, World!'))]"), json!("Hello, World!"));
    // base64, base64ToString, base64ToJson
    assert_eq!(ok("[base64('one, two, three')]"), json!("b25lLCB0d28sIHRocmVl"));
    assert_eq!(ok("[base64ToString('b25lLCB0d28sIHRocmVl')]"), json!("one, two, three"));
    assert_eq!(ok("[base64ToJson('eyJvbmUiOiAiYSIsICJ0d28iOiAiYiJ9')]"), json!({"one": "a", "two": "b"}));
    // uri, uriComponent, uriComponentToString
    assert_eq!(
        ok("[uri('http://contoso.com/resources/', 'nested/azuredeploy.json')]"),
        json!("http://contoso.com/resources/nested/azuredeploy.json")
    );
    assert_eq!(
        ok("[uriComponent('http://contoso.com/resources/nested/azuredeploy.json')]"),
        json!("http%3A%2F%2Fcontoso.com%2Fresources%2Fnested%2Fazuredeploy.json")
    );
    assert_eq!(
        ok("[uriComponentToString('http%3A%2F%2Fcontoso.com%2Fresources%2Fnested%2Fazuredeploy.json')]"),
        json!("http://contoso.com/resources/nested/azuredeploy.json")
    );
}

#[test]
fn logical_and_numeric_constants() {
    assert_eq!(ok("[true()]"), json!(true));
    assert_eq!(ok("[false()]"), json!(false));
    assert_eq!(ok("[not(false())]"), json!(true));
    assert_eq!(ok("[float('3.5')]"), json!(3.5));
    assert_eq!(ok("[float(2)]"), json!(2.0));
    assert!(fails("[float('x')]").contains("not a number"));
}

// ---- resource and scope functions (template-functions-resource, -deployment)

#[test]
fn resource_id_functions() {
    // extensionResourceId: the id of a lock on a resource group
    assert_eq!(
        ok("[extensionResourceId(resourceGroup().id, 'Microsoft.Authorization/locks', 'lock1')]"),
        json!("/subscriptions/s/resourceGroups/rg/providers/Microsoft.Authorization/locks/lock1")
    );
    assert_eq!(
        ok("[tenantResourceId('Microsoft.Management/managementGroups', 'mg1')]"),
        json!("/providers/Microsoft.Management/managementGroups/mg1")
    );
    assert_eq!(
        ok("[managementGroupResourceId('mg1', 'Microsoft.Authorization/policyDefinitions', 'p')]"),
        json!("/providers/Microsoft.Management/managementGroups/mg1/providers/Microsoft.Authorization/policyDefinitions/p")
    );
    assert!(fails("[managementGroupResourceId('Microsoft.Authorization/policyDefinitions', 'p')]").contains("management group"));
    assert!(fails("[extensionResourceId('x', 'Microsoft.Authorization/locks', 'l')]").contains("full resource id"));
    // pickZones answers "no zones"
    assert_eq!(ok("[pickZones('Microsoft.Compute', 'virtualMachines', 'westus2')]"), json!([]));
    assert!(fails("[pickZones('Microsoft.Compute', 'virtualMachines', 'westus2', 4)]").contains("1 to 3"));
    assert!(fails("[managementGroup()]").contains("management group deployment"));
    assert_eq!(ok("[deployer().tenantId]"), json!("t"));
}

#[test]
fn environment_function() {
    // the public cloud's shape (template-functions-deployment#environment), with the emulator's addresses
    let e = ok("[environment()]");
    assert_eq!(e["name"], "AzureCloud");
    assert_eq!(e["authentication"]["identityProvider"], "AAD");
    assert_eq!(ok("[environment().suffixes.storage]"), json!("core.localhost"));
    assert_eq!(ok("[environment().suffixes.keyvaultDns]"), json!(".vault.localhost"));
    assert_eq!(ok("[environment().resourceManager]"), json!("https://management.localhost/"));
}

#[test]
fn user_defined_function_calls_need_a_table() {
    let e = fails("[contoso.upper('a')]");
    assert!(e.contains("contoso.upper") && e.contains("not defined"), "{e}");
}

#[test]
fn list_functions_need_a_deployment() {
    let e = fails("[listKeys('x')]");
    assert!(e.contains("at least 2"), "{e}");
    let e = fails("[listKeys('x', '2023-01-01')]");
    assert!(e.contains("listKeys"), "{e}");
}

#[test]
fn string_comparison_follows_azure() {
    // Outputs of a real deployment (see tests/azure/functions-template-06-arrays-objects.expected.json): equals, contains on
    // an array or a string, distinct, union and intersection compare strings with case; object keys, startsWith, endsWith,
    // indexOf on a string and less/greater without case.
    assert_eq!(ok("[equals('A', 'a')]"), json!(false));
    assert_eq!(ok("[equals(createArray('A'), createArray('a'))]"), json!(false));
    assert_eq!(ok("[contains('abcdef', 'CD')]"), json!(false));
    assert_eq!(ok("[contains(createArray('one'), 'ONE')]"), json!(false));
    assert_eq!(ok("[contains(createObject('Name', 1), 'NAME')]"), json!(true));
    assert_eq!(ok("[distinct(createArray('a', 'A', 'b'))]"), json!(["a", "A", "b"]));
    assert_eq!(ok("[intersection(createArray('One', 'two'), createArray('one', 'TWO'))]"), json!([]));
    assert_eq!(ok("[startsWith('Hello', 'hE')]"), json!(true));
    assert_eq!(ok("[less('a', 'B')]"), json!(true));
    assert_eq!(ok("[objectKeys(createObject('b', 1, 'a', 2))]"), json!(["a", "b"]));
}

#[test]
fn string_of_a_bool_is_capitalised() {
    // a real deployment: string(true()) is "True"
    assert_eq!(ok("[string(true())]"), json!("True"));
    assert_eq!(ok("[string(false())]"), json!("False"));
}
