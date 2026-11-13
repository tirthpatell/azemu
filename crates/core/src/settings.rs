//! Settings from the environment (`AZEMU_*`), read one way by every crate.
//!
//! Before this module each crate parsed its own variables, each a little differently
//! (`AZEMU_SERVICEBUS_AUTOCREATE=false` left auto-creation on because only `0` meant off). Rules now:
//!
//! * a switch is on for `1`, `true`, `yes`, `on` and off for `0`, `false`, `no`, `off` (any case). An empty
//!   value means "not set". Anything else is reported once on stderr and the default is used;
//! * sizes take a number of bytes or a number with `K`, `M`, `G` (powers of 1024; `KB`, `KiB` and so on
//!   mean the same); durations take seconds or a number with `ms`, `s`, `m`, `h`, `d`;
//! * every setting that is read is remembered with the value in force, so `GET /_azemu/config` can show
//!   them and start-up can warn about an `AZEMU_*` variable that nothing reads (a typing mistake);
//! * [`set_override`] lets a command-line option stand in for a variable. It must be called before the
//!   services are built, because most services read their settings when they are constructed.
//!
//! [`lenient`] (`AZEMU_LENIENT`, `--lenient`) is the one switch for conveniences Azure does not have:
//! with it, things a client names but nobody created come into being on first use (queues, topics, event
//! hubs, consumer groups, Event Grid topics) and new storage accounts keep blob versions. Without it the
//! emulator behaves as Azure does. A service asks with [`lenient_flag`], which lets one variable turn a
//! single convenience on or off whatever `AZEMU_LENIENT` says.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A setting that some code has read, as shown by `GET /_azemu/config`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InForce {
    pub name: String,
    /// The value in force, as text.
    pub value: String,
    /// Where it came from: `default`, `environment`, `option` (a command-line option) or `lenient`.
    pub source: &'static str,
}

/// What a setting is for, as listed in `docs/SETTINGS.md`. See [`describe`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Doc {
    pub name: String,
    /// The value used when nothing sets it, in words (`off`, `64M`, `none`).
    pub default: String,
    pub meaning: String,
}

#[derive(Default)]
struct Store {
    overrides: BTreeMap<String, String>,
    read: BTreeMap<String, InForce>,
    complained: std::collections::BTreeSet<String>,
    docs: BTreeMap<String, Doc>,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::default()))
}

fn lock() -> std::sync::MutexGuard<'static, Store> {
    store().lock().unwrap_or_else(|e| e.into_inner())
}

/// Make `name` read as `value` whatever the environment says (for command-line options).
pub fn set_override(name: &str, value: &str) {
    lock().overrides.insert(name.to_string(), value.to_string());
}

/// The raw text of a setting and where it came from; `None` when it is not set or empty.
fn raw(name: &str) -> Option<(String, &'static str)> {
    if let Some(v) = lock().overrides.get(name) {
        return Some((v.clone(), "option"));
    }
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some((v.trim().to_string(), "environment")),
        _ => None,
    }
}

fn remember(name: &str, value: String, source: &'static str) {
    lock().read.insert(name.to_string(), InForce { name: name.to_string(), value, source });
}

fn complain(name: &str, text: &str, expected: &str) {
    if lock().complained.insert(name.to_string()) {
        eprintln!("azemu: {name}={text:?} is not {expected}; using the default");
    }
}

fn parse_switch(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// A switch. See the module text for the spellings.
pub fn flag(name: &str, default: bool) -> bool {
    let (value, source) = match raw(name) {
        Some((text, source)) => match parse_switch(&text) {
            Some(v) => (v, source),
            None => {
                complain(name, &text, "a switch (1, true, yes, on, 0, false, no, off)");
                (default, "default")
            }
        },
        None => (default, "default"),
    };
    remember(name, value.to_string(), source);
    value
}

/// `AZEMU_LENIENT`: conveniences Azure does not have. Off unless asked for.
pub fn lenient() -> bool {
    flag("AZEMU_LENIENT", false)
}

/// A convenience that is off as on Azure, on with [`lenient`], and that `name` can set either way.
pub fn lenient_flag(name: &str) -> bool {
    if let Some((text, source)) = raw(name) {
        if let Some(v) = parse_switch(&text) {
            remember(name, v.to_string(), source);
            return v;
        }
        complain(name, &text, "a switch (1, true, yes, on, 0, false, no, off)");
    }
    let v = lenient();
    remember(name, v.to_string(), if v { "lenient" } else { "default" });
    v
}

/// A whole number.
pub fn number(name: &str, default: u64) -> u64 {
    let (value, source) = match raw(name) {
        Some((text, source)) => match text.replace('_', "").parse::<u64>() {
            Ok(v) => (v, source),
            Err(_) => {
                complain(name, &text, "a whole number");
                (default, "default")
            }
        },
        None => (default, "default"),
    };
    remember(name, value.to_string(), source);
    value
}

fn parse_bytes(text: &str) -> Option<u64> {
    let t = text.trim().replace('_', "");
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (digits, unit) = t.split_at(split);
    let n: u64 = digits.parse().ok()?;
    let shift = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        _ => return None,
    };
    n.checked_mul(1u64 << shift)
}

/// A size in bytes: `1048576`, `64K`, `256M`, `1G`.
pub fn bytes(name: &str, default: u64) -> u64 {
    let (value, source) = match raw(name) {
        Some((text, source)) => match parse_bytes(&text) {
            Some(v) => (v, source),
            None => {
                complain(name, &text, "a size (bytes, or a number with K, M or G)");
                (default, "default")
            }
        },
        None => (default, "default"),
    };
    remember(name, value.to_string(), source);
    value
}

fn parse_duration(text: &str) -> Option<Duration> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (digits, unit) = t.split_at(split);
    let n: u64 = digits.parse().ok()?;
    Some(match unit.trim().to_ascii_lowercase().as_str() {
        "ms" => Duration::from_millis(n),
        "" | "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.checked_mul(60)?),
        "h" => Duration::from_secs(n.checked_mul(3600)?),
        "d" => Duration::from_secs(n.checked_mul(86400)?),
        _ => return None,
    })
}

/// A length of time: `30` (seconds), `500ms`, `30s`, `5m`, `2h`, `7d`.
pub fn duration(name: &str, default: Duration) -> Duration {
    let (value, source) = match raw(name) {
        Some((text, source)) => match parse_duration(&text) {
            Some(v) => (v, source),
            None => {
                complain(name, &text, "a length of time (seconds, or a number with ms, s, m, h or d)");
                (default, "default")
            }
        },
        None => (default, "default"),
    };
    remember(name, format!("{}ms", value.as_millis()), source);
    value
}

/// Free text (a path, a host name). `None` when not set.
pub fn text(name: &str) -> Option<String> {
    let found = raw(name);
    match &found {
        Some((v, source)) => remember(name, v.clone(), source),
        None => remember(name, String::new(), "default"),
    }
    found.map(|(v, _)| v)
}

/// A comma-separated list; empty items are dropped.
pub fn list(name: &str) -> Vec<String> {
    text(name).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

/// Say what a setting is for. Every crate calls this next to the code that reads the setting (in the same
/// constructor), so that the table in `docs/SETTINGS.md`, `azemu __settings-table` and the check for mistyped
/// variables know the setting. Calling it again with the same name replaces the entry.
///
/// * `default`: what is used when nothing sets the variable, in words (`off`, `64M`, `none`);
/// * `meaning`: one sentence in plain words; name the command-line option too when there is one.
///
/// A test in `crates/azemu` fails when a setting is read and not described, or when `docs/SETTINGS.md` differs from
/// what the code describes.
pub fn describe(name: &str, default: &str, meaning: &str) {
    lock().docs.insert(name.to_string(), Doc { name: name.to_string(), default: default.to_string(), meaning: meaning.to_string() });
}

/// Every described setting, by name.
pub fn described() -> Vec<Doc> {
    lock().docs.values().cloned().collect()
}

/// Settings that were read but never described.
pub fn undescribed() -> Vec<String> {
    let st = lock();
    st.read.keys().filter(|k| !st.docs.contains_key(*k)).cloned().collect()
}

/// The table in `docs/SETTINGS.md`: one row for every described setting.
pub fn table_markdown() -> String {
    let mut out = String::from("| Variable | Default | Meaning |\n|---|---|---|\n");
    for d in described() {
        let cell = |t: &str| t.replace('|', "\\|").replace('\n', " ");
        out.push_str(&format!("| `{}` | {} | {} |\n", d.name, cell(&d.default), cell(&d.meaning)));
    }
    out
}

/// Every setting that has been read so far, by name.
pub fn in_force() -> Vec<InForce> {
    lock().read.values().cloned().collect()
}

/// Names that belong to the programs that test or drive the emulator, not to the emulator. A harness passes its own
/// environment on to the emulator it starts, so these show up in the emulator's environment although nothing in it reads
/// them; the check for mistyped variables lets them be. Each has one line of meaning, which `docs/SETTINGS.md` shows.
pub const TEST_TOOLING: &[(&str, &str)] = &[
    ("AZEMU_BIN", "The `azemu` program a test harness starts (the SDK suites, `tests/run_all.sh`, `azemu-testkit`)."),
    ("AZEMU_FULL_BIN", "The path of the all-services program, for the Python suites (older name of `AZEMU_UNIFIED_BIN`)."),
    ("AZEMU_UNIFIED_BIN", "The path of the all-services program, for the Python suites."),
    ("AZEMU_GATEWAY", "The address of an emulator that is already running, such as `http://127.0.0.1:4577`; a harness that finds it uses that emulator instead of starting one."),
    ("AZEMU_CA_FILE", "The path of that running emulator's `ca.pem`."),
    ("AZEMU_CERT_DIR", "The certificate directory of the emulator that the JavaScript suite started (set by its runner for the test files)."),
    ("AZEMU_EXTRA_TRUST_PEM", "A PEM file that the Java suite adds to its trust store, for a proxy in front of the emulator."),
    ("AZEMU_STORAGE_CONN_STR", "A storage connection string that the Python suites use as it is, instead of building one for the emulator they start."),
    ("AZEMU_COSMOS_ENDPOINT", "The Cosmos endpoint that the Python Cosmos suite uses instead of starting an emulator."),
    ("AZEMU_COSMOS_VERIFY", "`0` turns off the certificate check of that endpoint in the Python Cosmos suite."),
    ("AZEMU_EH_STACK", "The ports (Event Hubs, then Blob) of an emulator the Python Event Hubs checkpoint test uses instead of starting one."),
    ("AZEMU_EVENTHUB_HOST", "The host name the container test's client uses to reach Event Hubs."),
    ("AZEMU_HOST", "The host name of the emulator that the example checks and the container test's client connect to."),
    ("AZEMU_PORT", "The gateway port of the emulator that the JavaScript suite's runner started (set for the test files)."),
    ("AZEMU_IMAGE", "The container image the examples and the container test use (default `azemu:latest`)."),
    ("AZEMU_CONTAINER_KEEP", "Keep the container test's containers and files after it ends."),
    ("AZEMU_CONTAINER_SCRATCH", "Where the container test keeps its files."),
    ("AZEMU_FUNC_SCRATCH", "Where the Azure Functions test keeps its files."),
    ("AZEMU_KEEP_LOG", "Where the Python and Go suites copy the emulator's log when they end."),
    ("AZEMU_LOG_PATH", "The file the JavaScript suite writes the emulator's log to (set by its runner for the test files)."),
    ("AZEMU_DUMP", "When set, the Go suite prints every request and response to stderr."),
    ("AZEMU_FINDINGS", "`1` runs the .NET tests that show a known difference from Azure (they fail while it stands)."),
    ("AZEMU_RUN_FINDINGS", "`1` runs the Go tests that show a known difference from Azure (they fail while it stands)."),
    ("AZEMU_PERSIST_BENCH_DIR", "A directory on a real disk for the ignored persistence timing tests of the service crates (they write about 100 MB)."),
    ("AZEMU_COST_DIR", "A directory for the persistence cost test of Blob (default `~/.cache/azemu-cost`)."),
    ("AZEMU_UPDATE_SETTINGS_DOC", "Set to 1 when running `cargo test -p azemu --test settings_doc` to rewrite the table in `docs/SETTINGS.md`."),
];

/// Prefixes that are test tooling too: `AZEMU_TEST_` (for example `AZEMU_TEST_KEEP_LOGS` and `AZEMU_TEST_LOG_LEVEL`, which
/// the Rust, JavaScript and Go suites read) and `AZEMU_TESTKIT_` (`azemu-testkit`).
pub const TEST_TOOLING_PREFIXES: &[&str] = &["AZEMU_TEST_", "AZEMU_TESTKIT_"];

/// Is this one of the names of [`TEST_TOOLING`] or does it start with one of [`TEST_TOOLING_PREFIXES`]?
pub fn is_test_tooling(name: &str) -> bool {
    TEST_TOOLING.iter().any(|(n, _)| *n == name) || TEST_TOOLING_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The section of `docs/SETTINGS.md` that lists [`TEST_TOOLING`].
pub fn test_tooling_markdown() -> String {
    let mut out = String::from("| Variable | Used by |\n|---|---|\n");
    for (name, meaning) in TEST_TOOLING {
        out.push_str(&format!("| `{name}` | {meaning} |\n"));
    }
    for p in TEST_TOOLING_PREFIXES {
        out.push_str(&format!(
            "| `{p}*` | Any name that starts so: options of the test harnesses (see the README of the suite that reads it). |\n"
        ));
    }
    out
}

/// `AZEMU_*` variables in the environment that no code has read: most likely a typing mistake. Call it
/// once the services are built and the command line is parsed. A service that reads a setting lazily (on
/// first use) shows up here although the name is right; start-up uses [`unknown_variables`] instead.
pub fn unread_variables() -> Vec<String> {
    let read = lock();
    let mut names: Vec<String> =
        // AZEMU_GIT_COMMIT is the build's own (cargo passes it on to the tests it runs), not a setting
        std::env::vars().map(|(k, _)| k).filter(|k| k.starts_with("AZEMU_") && k != "AZEMU_GIT_COMMIT" && !is_test_tooling(k) && !read.read.contains_key(k)).collect();
    names.sort();
    names
}

/// Settings that crates read today and have not described yet (see [`describe`]). A variable named here is never
/// reported as a typing mistake, even when the crate reads it lazily (on first use). A crate that describes its
/// settings can drop its names from this list; a test in `crates/azemu` keeps the list honest.
pub const NOT_YET_DESCRIBED: &[&str] = &[];

/// [`unread_variables`] without the ones that are known names: described with [`describe`], or listed in
/// [`NOT_YET_DESCRIBED`]. Those are real settings even when the code that reads them has not run yet. What is left is a
/// mistyped name.
pub fn unknown_variables() -> Vec<String> {
    let docs: std::collections::BTreeSet<String> = lock().docs.keys().cloned().collect();
    unread_variables().into_iter().filter(|n| !docs.contains(n) && !NOT_YET_DESCRIBED.contains(&n.as_str())).collect()
}

/// Options that stand in for variables, read from the process arguments before the services are built (they read
/// their settings in their constructors, which run before `run_cli` parses the command line). Call it first thing in
/// `main`; every `main.rs` of the workspace does. An embedder that does not call it still gets the variables.
///
/// * `--lenient` sets `AZEMU_LENIENT=1`;
/// * `--allow-webhook-host <name>` / `--allow-webhook-host=<name>` (repeatable) adds to `AZEMU_ALLOW_WEBHOOK_HOST`
///   (the names of the variable are kept, so both can be used together).
///
/// `clap` still parses and shows the same options (`--help`); this function only reads them early and never fails.
pub fn apply_early_options() {
    apply_early_options_from(std::env::args().skip(1));
}

/// What [`apply_early_options`] finds in a list of arguments: whether `--lenient` is there, and the names given to
/// `--allow-webhook-host` (a comma separated value counts as several).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EarlyOptions {
    pub lenient: bool,
    pub allow_webhook_hosts: Vec<String>,
}

/// Read the early options out of arguments (the program name left out). Anything else is skipped, `--` ends the search.
pub fn parse_early_options<I: IntoIterator<Item = String>>(args: I) -> EarlyOptions {
    let mut out = EarlyOptions::default();
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        if a == "--" {
            break;
        }
        let value = if a == "--lenient" {
            out.lenient = true;
            None
        } else if a == "--allow-webhook-host" {
            args.next()
        } else {
            a.strip_prefix("--allow-webhook-host=").map(str::to_string)
        };
        if let Some(v) = value {
            out.allow_webhook_hosts.extend(v.split(',').map(|h| h.trim().to_string()).filter(|h| !h.is_empty()));
        }
    }
    out
}

/// [`apply_early_options`] for a given list of arguments (the program name left out).
pub fn apply_early_options_from<I: IntoIterator<Item = String>>(args: I) {
    let found = parse_early_options(args);
    if found.lenient {
        set_override("AZEMU_LENIENT", "1");
    }
    if !found.allow_webhook_hosts.is_empty() {
        // the names from the environment stay; the options add to them
        let mut all = list("AZEMU_ALLOW_WEBHOOK_HOST");
        all.extend(found.allow_webhook_hosts);
        // that read went through the environment alone: forget it, the override below is the value in force
        lock().read.remove("AZEMU_ALLOW_WEBHOOK_HOST");
        set_override("AZEMU_ALLOW_WEBHOOK_HOST", &all.join(","));
    }
}

/// The settings that core itself reads, described. Called when the table is printed and when the emulator starts.
pub fn describe_core() {
    describe("AZEMU_LENIENT", "off", "Turn on every convenience Azure does not have: things a client names but nobody created (queues, topics, event hubs, consumer groups, Event Grid topics) come into being on first use, and new storage accounts keep blob versions. Option: `--lenient`. A convenience's own variable overrides this for that convenience.");
    describe(
        "AZEMU_ALLOW_HOST",
        "none",
        "Also serve requests whose Host is this name or a name under it (comma separated). Option: `--allow-host`.",
    );
    describe("AZEMU_ALLOW_ORIGIN", "none", "Let web pages from this origin, host or `*.suffix` use the emulator although they are on another site; `*` turns the cross-site check off (comma separated). Option: `--allow-origin`.");
    describe("AZEMU_TLS_NAME", "none", "Extra names the local CA may sign for, besides localhost and loopback addresses (comma separated). A different set makes a new CA. Option: `--tls-name`.");
    describe("AZEMU_DATA_DIR", "none (state is kept in memory only)", "Keep state between runs in this directory. Option: `--data-dir`.");
    describe("AZEMU_SAVE_INTERVAL", "15 (seconds)", "Seconds between saves of changed state when a data directory is set; 0 saves only on `POST /_azemu/save` and at shutdown. Option: `--save-interval`.");
    describe(
        "AZEMU_LOOSE_AUTH",
        "off",
        "Skip the signature checks of storage requests (Shared Key and SAS) for the one account whose key is known, `devstoreaccount1`.",
    );
    describe("AZEMU_STANDARD_PORTS", "off", "Open one fixed port per service next to the gateway port: 10000 Blob, 10001 Queue, 10002 Table, 10003 Data Lake, 10010 Key Vault, 10011 App Configuration, 10012 Event Grid, 10013 ARM, 10014 Identity, 8081 Cosmos, 5672 and 5671 AMQP (plain and TLS on both), 9093 Kafka, and 443 when the process may bind it (a warning when it may not). Option: `--standard-ports`.");
    describe(
        "AZEMU_ALLOW_WEBHOOK_HOST",
        "none",
        "Event Grid webhooks may also go to this host name (comma separated; `*` allows any). Option: `--allow-webhook-host`.",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test uses its own variable names: the store is shared by the whole test process.

    #[test]
    fn switches_have_one_spelling_everywhere() {
        for (text, want) in
            [("1", true), ("true", true), ("YES", true), ("On", true), ("0", false), ("false", false), ("no", false), ("OFF", false)]
        {
            set_override("AZEMU_TEST_SWITCH", text);
            assert_eq!(flag("AZEMU_TEST_SWITCH", !want), want, "{text}");
        }
        set_override("AZEMU_TEST_SWITCH", "maybe");
        assert!(flag("AZEMU_TEST_SWITCH", true));
        assert!(!flag("AZEMU_TEST_SWITCH", false));
        assert!(flag("AZEMU_TEST_SWITCH_NOT_SET", true));
    }

    #[test]
    fn a_lenient_flag_follows_lenient_unless_set_itself() {
        assert!(!lenient_flag("AZEMU_TEST_CONVENIENCE_A"));
        set_override("AZEMU_TEST_CONVENIENCE_A", "on");
        assert!(lenient_flag("AZEMU_TEST_CONVENIENCE_A"));
        let seen = in_force().into_iter().find(|s| s.name == "AZEMU_TEST_CONVENIENCE_A").unwrap();
        assert_eq!((seen.value.as_str(), seen.source), ("true", "option"));
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_bytes("1048576"), Some(1 << 20));
        assert_eq!(parse_bytes("64K"), Some(64 << 10));
        assert_eq!(parse_bytes("256 MiB"), Some(256 << 20));
        assert_eq!(parse_bytes("1g"), Some(1 << 30));
        assert_eq!(parse_bytes("12 parsecs"), None);
        assert_eq!(parse_bytes("99999999999999G"), None);
        assert_eq!(parse_duration("30"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(7 * 86400)));
        assert_eq!(parse_duration("soon"), None);
        set_override("AZEMU_TEST_SIZE", "2M");
        assert_eq!(bytes("AZEMU_TEST_SIZE", 5), 2 << 20);
        assert_eq!(bytes("AZEMU_TEST_SIZE_NOT_SET", 5), 5);
        assert_eq!(number("AZEMU_TEST_NUMBER_NOT_SET", 7), 7);
        assert_eq!(duration("AZEMU_TEST_TIME_NOT_SET", Duration::from_secs(9)), Duration::from_secs(9));
    }

    #[test]
    fn early_options_are_found_in_the_arguments() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_early_options(args(&["--port", "5", "--standard-ports"])), EarlyOptions::default());
        let found = parse_early_options(args(&[
            "--lenient",
            "--allow-webhook-host",
            "a.example",
            "--allow-webhook-host=b.example, c.example",
            "--port",
            "1",
        ]));
        assert!(found.lenient);
        assert_eq!(found.allow_webhook_hosts, vec!["a.example", "b.example", "c.example"]);
        // after `--` nothing counts; a missing value is not an error
        assert_eq!(parse_early_options(args(&["--", "--lenient"])), EarlyOptions::default());
        assert_eq!(parse_early_options(args(&["--allow-webhook-host"])), EarlyOptions::default());
    }

    #[test]
    fn lists_and_text() {
        set_override("AZEMU_TEST_LIST", "a, b,,c ");
        assert_eq!(list("AZEMU_TEST_LIST"), vec!["a", "b", "c"]);
        assert!(list("AZEMU_TEST_LIST_NOT_SET").is_empty());
        assert_eq!(text("AZEMU_TEST_TEXT_NOT_SET"), None);
        assert!(in_force().iter().any(|s| s.name == "AZEMU_TEST_LIST" && s.source == "option"));
    }
}
