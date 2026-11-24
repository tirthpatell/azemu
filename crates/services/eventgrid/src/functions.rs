//! Azure Function destinations (`endpointType: AzureFunction`, `resourceId` `…/sites/{app}/functions/{name}`).
//!
//! On Azure, Event Grid calls the Event Grid trigger endpoint of the function app. The emulator cannot know where a Functions host
//! runs, so it is told: `AZEMU_EVENTGRID_FUNCTION_HOSTS=app=http://localhost:7071,other=http://host:7072` or
//! `PUT /_azemu/eventgrid/functions/{app}` with `{"baseUrl":"http://localhost:7071"}`. A delivery then goes to
//! `{base}/runtime/webhooks/EventGrid?functionName={name}` with the headers a webhook gets: the Functions Event Grid extension
//! answers a request that has `aeg-event-type: Notification` and reads the body as an array of events (Event Grid schema) or one
//! CloudEvent. Without an entry for the app the delivery fails with a reason that says so.

use std::collections::BTreeMap;

use crate::webhook_policy::WebhookPolicy;

/// Read `app=url` pairs, separated by commas.
pub fn parse_hosts(text: &str) -> Vec<(String, String)> {
    text.split(',')
        .filter_map(|p| {
            let (app, url) = p.split_once('=')?;
            let (app, url) = (app.trim(), url.trim());
            (!app.is_empty() && !url.is_empty()).then(|| (app.to_ascii_lowercase(), url.to_string()))
        })
        .collect()
}

/// The address a delivery to `function` of `app` goes to. `Err` is the reason the delivery fails.
pub fn target_url(hosts: &BTreeMap<String, String>, app: &str, function: &str) -> Result<String, String> {
    let base = hosts.get(&app.to_ascii_lowercase()).ok_or_else(|| {
        format!(
            "NotFound: no Functions host is set for the function app '{app}'. Set AZEMU_EVENTGRID_FUNCTION_HOSTS={app}=http://localhost:7071 \
             or PUT /_azemu/eventgrid/functions/{app} with {{\"baseUrl\":\"http://localhost:7071\"}}."
        )
    })?;
    let enc: String = function
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    Ok(format!("{}/runtime/webhooks/EventGrid?functionName={enc}", base.trim_end_matches('/')))
}

/// Check a base address the way a webhook address is checked. `Err` is the text of a 400.
pub fn check_base(base: &str, policy: &WebhookPolicy) -> Result<(), String> {
    crate::http_client::check_url(base, policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_parse_and_url_is_built() {
        let h: BTreeMap<_, _> = parse_hosts("MyApp=http://localhost:7071/, b = http://h:1 ,junk").into_iter().collect();
        assert_eq!(h.len(), 2);
        assert_eq!(target_url(&h, "myapp", "Fn1").unwrap(), "http://localhost:7071/runtime/webhooks/EventGrid?functionName=Fn1");
        let e = target_url(&h, "other", "f").unwrap_err();
        assert!(e.contains("AZEMU_EVENTGRID_FUNCTION_HOSTS") && e.contains("other"));
    }
}
