//! The templates in `azure/` have no resources: they only compute outputs with the template functions, from fixed inputs.
//! Each one was deployed to a real resource group (November 2026) and is deployed to the emulator here;
//! `azure/{name}.expected.json` holds what Azure answered (output name to value) and this test checks that the emulator
//! answers the same. `{"__raw__": X}` stands for an output that Azure answered with the object X and no `value` (a null).
//!
//! `azure/refused.json` holds expressions that Azure refused, with the text of its error: the emulator has to refuse each one
//! with the same code and leading sentence.
//!
//! `cargo test -p azemu-arm --test azure_functions -- --ignored --nocapture` prints what the emulator answers for each
//! template, as `.expected.json` files would hold it.

mod common;

use common::{standalone, SUB};
use serde_json::{json, Map, Value};

fn templates() -> Vec<(String, Value, Option<Value>)> {
    let dir = format!("{}/tests/azure", env!("CARGO_MANIFEST_DIR"));
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("functions-template") && n.ends_with(".json") && !n.ends_with(".expected.json"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let t: Value = serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{n}")).unwrap()).unwrap();
            let stem = n.trim_end_matches(".json").to_string();
            let expected = std::fs::read_to_string(format!("{dir}/{stem}.expected.json")).ok().map(|s| serde_json::from_str(&s).unwrap());
            (stem, t, expected)
        })
        .collect()
}

fn url(name: &str) -> String {
    format!("/subscriptions/{SUB}/resourcegroups/rg-azure/providers/Microsoft.Resources/deployments/{name}")
}

/// The `outputs` of the deployment: name to the output object (`type` and `value`).
async fn emulator_outputs(a: &common::Arm, name: &str, template: &Value) -> Result<Map<String, Value>, String> {
    let r = a.put(&url(name), json!({"properties": {"mode": "Incremental", "template": template}})).await;
    if r.status != 200 && r.status != 201 {
        return Err(format!("{} {}", r.status, r.text()));
    }
    let j = r.json();
    if j["properties"]["provisioningState"] != "Succeeded" {
        return Err(j.to_string());
    }
    Ok(j["properties"]["outputs"].as_object().unwrap().clone())
}

#[tokio::test]
async fn the_emulator_answers_what_azure_answered() {
    let a = standalone().await;
    a.group("rg-azure").await;
    let mut compared = 0;
    for (name, template, expected) in templates() {
        assert!(template["outputs"].as_object().unwrap().len() <= 64, "{name} has more than 64 outputs");
        assert!(
            template["resources"]
                .as_array()
                .map(|r| r.is_empty())
                .unwrap_or_else(|| template["resources"].as_object().is_some_and(|o| o.is_empty())),
            "{name} has resources"
        );
        let got = emulator_outputs(&a, &name, &template).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(got.len(), template["outputs"].as_object().unwrap().len(), "{name}: every output is answered");
        let expected = expected.unwrap_or_else(|| panic!("{name} has no .expected.json"));
        let expected = expected.as_object().unwrap_or_else(|| panic!("{name}: the expected file is not an object"));
        assert_eq!(expected.len(), got.len(), "{name}: the expected file and the template have different outputs");
        let mut wrong = Vec::new();
        for (k, v) in expected {
            let (want, have) = match v.get("__raw__") {
                Some(raw) => (raw.clone(), got.get(k).cloned()),
                None => (v.clone(), got.get(k).and_then(|o| o.get("value")).cloned()),
            };
            if have.as_ref() != Some(&want) {
                wrong.push(format!("{k}: Azure {want}, emulator {}", have.map(|x| x.to_string()).unwrap_or_else(|| "(none)".into())));
            }
        }
        assert!(wrong.is_empty(), "{name} differs from Azure in {} output(s):\n{}", wrong.len(), wrong.join("\n"));
        compared += 1;
    }
    assert_eq!(compared, 9);
}

/// What Azure refused: `{name, template, expression, type, azure_error}`. Azure's error text starts with "The template output
/// 'x' is not valid: ..." (the deployment failed while it evaluated the outputs) or with "Deployment template validation
/// failed" (it refused the template before starting); the emulator must do the same, with the same sentence in it.
#[tokio::test]
async fn the_emulator_refuses_what_azure_refused() {
    let a = standalone().await;
    a.group("rg-azure").await;
    let dir = format!("{}/tests/azure/refused.json", env!("CARGO_MANIFEST_DIR"));
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(dir).unwrap()).unwrap();
    assert_eq!(cases.len(), 6);
    for c in cases {
        let (name, expr, ty, azure) = (
            c["name"].as_str().unwrap(),
            c["expression"].as_str().unwrap(),
            c["type"].as_str().unwrap(),
            c["azure_error"].as_str().unwrap(),
        );
        let tpl = json!({"contentVersion": "1.0.0.0", "resources": [], "outputs": {name: {"type": ty, "value": expr}}});
        let r = a.put(&url("refused"), json!({"properties": {"mode": "Incremental", "template": tpl}})).await;
        let body = r.text();
        // the sentence that tells what is wrong: after "is not valid: " for an output that failed, after the position for a refused template
        let key = azure.split("is not valid: ").last().unwrap().split(" Please see").next().unwrap().trim_end_matches('.').to_string();
        assert!(body.contains(&key), "{name}: Azure says {key:?}, emulator answered {} {body}", r.status);
        if azure.starts_with("Deployment template validation failed") {
            assert_eq!(r.status, 400, "{name}: {body}");
            assert!(body.contains("InvalidTemplate"), "{name}: {body}");
        } else {
            assert!(r.status == 200 || r.status == 201, "{name}: {} {body}", r.status);
            let j = r.json();
            assert_eq!(j["properties"]["provisioningState"], "Failed", "{name}: {body}");
            assert!(body.contains("DeploymentOutputEvaluationFailed"), "{name}: {body}");
            assert!(body.contains(&format!("The template output '{name}' is not valid: ")), "{name}: {body}");
        }
    }
}

#[tokio::test]
#[ignore = "prints the emulator's outputs; run by hand to make or check an .expected.json"]
async fn print_emulator_outputs() {
    let a = standalone().await;
    a.group("rg-azure").await;
    for (name, template, _) in templates() {
        match emulator_outputs(&a, &name, &template).await {
            Ok(o) => {
                let shown: Map<String, Value> =
                    o.into_iter().map(|(k, v)| (k, v.get("value").cloned().unwrap_or_else(|| json!({"__raw__": v})))).collect();
                println!("=== {name}\n{}", serde_json::to_string_pretty(&Value::Object(shown)).unwrap())
            }
            Err(e) => println!("=== {name}\nERROR {e}"),
        }
    }
}
