//! The kit against a real emulator: the program built by `cargo build -p azemu` (`target/debug/azemu`), or the one `AZEMU_BIN`
//! names.

use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use azemu_testkit::{shared, unique_name, Emulator, Service};

/// Point the kit at the emulator that was built next to this crate, unless `AZEMU_BIN` already says which one.
/// `false` when there is none (azemu has not been built): the tests then say so and stop.
fn have_azemu() -> bool {
    if std::env::var_os("AZEMU_BIN").is_some() {
        return true;
    }
    let built = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/azemu");
    if built.exists() {
        std::env::set_var("AZEMU_BIN", built.canonicalize().unwrap());
        return true;
    }
    eprintln!("skipped: azemu is not built; run `cargo build -p azemu` first");
    false
}

macro_rules! emulator_or_skip {
    () => {
        if !have_azemu() {
            return;
        }
    };
}

const BEARER: (&str, &str) = ("Authorization", "Bearer azemu-testkit");

#[test]
fn one_port_serves_every_loopback_address_and_the_services_are_reached_by_their_host_names() {
    emulator_or_skip!();
    let emu = shared();
    assert_ne!(emu.port(), 0);
    assert_eq!(emu.gateway(), format!("http://127.0.0.1:{}", emu.port()));
    let health = emu.request("GET", "/_azemu/health", "");
    assert_eq!(health.status, 200, "{}", health.body);
    // with --port 0 the emulator uses the same port on 127.0.0.1 and on ::1, so a client that resolves `localhost` to either one gets through
    for addr in [format!("127.0.0.1:{}", emu.port()), format!("[::1]:{}", emu.port())] {
        match TcpStream::connect(&addr) {
            Ok(_) => {}
            Err(e) if addr.starts_with("[::1]") && e.kind() != std::io::ErrorKind::ConnectionRefused => eprintln!("no IPv6 here: {e}"),
            Err(e) => {
                let config = emu.request("GET", "/_azemu/config", "").body;
                assert!(addr.starts_with("[::1]") && !config.contains("::1"), "{addr}: {e}; the emulator listens on: {config}");
            }
        }
    }
    // every service answers to the host-name form (the answer is the service's own, not "no service matched this host")
    for service in [
        Service::Blob,
        Service::Queue,
        Service::Table,
        Service::DataLake,
        Service::KeyVault,
        Service::AppConfig,
        Service::Cosmos,
        Service::EventGrid,
    ] {
        let answer = emu.call(service, "someone", "GET", "/", &[], "");
        assert!(!answer.body.contains("NoServiceMatched"), "{service:?}: {} {}", answer.status, answer.body);
        assert_ne!(answer.status, 0, "{service:?}");
    }
    assert_eq!(emu.endpoint(Service::Blob, "acct"), format!("https://acct.blob.core.localhost:{}/", emu.port()));
    assert_eq!(emu.http_endpoint(Service::KeyVault, "kv"), format!("http://kv.vault.localhost:{}/", emu.port()));
    assert_eq!(emu.host_name(Service::EventHubs, "ns"), format!("ns.eventhub.localhost:{}", emu.port()));
}

#[test]
fn the_certificate_authority_the_kit_names_is_the_emulators_and_the_environment_is_set_for_the_sdks() {
    emulator_or_skip!();
    let emu = shared();
    let pem = emu.ca_pem();
    assert!(pem.contains("BEGIN CERTIFICATE"), "{pem}");
    let served = emu.request("GET", "/_azemu/ca.pem", "");
    assert_eq!(served.status, 200);
    assert_eq!(served.body.trim(), pem.trim(), "the file and what the emulator serves are the same certificate");
    let bundle = std::fs::read_to_string(emu.ca_bundle()).unwrap();
    assert!(bundle.contains(pem.trim()), "the bundle holds the emulator's authority");
    assert_eq!(std::env::var("SSL_CERT_FILE").as_deref(), Ok(emu.ca_bundle().to_str().unwrap()));
    assert_eq!(std::env::var("COSMOS_DISABLE_IMDS").as_deref(), Ok("1"));
    assert_eq!(std::env::var("AZURE_COSMOS_BINARY_ENCODING_ENABLED").as_deref(), Ok("false"));
    assert_eq!(std::env::var("IDENTITY_ENDPOINT").unwrap(), emu.identity_endpoint());
    // the emulator's token endpoint answers a managed-identity request
    let token = emu.request("GET", "/_azemu/identity/msi/token?resource=https://storage.azure.com&api-version=2019-08-01", "");
    assert!(token.status == 200 || token.status == 400, "{} {}", token.status, token.body);
    assert!(emu.log_file().is_some_and(|l| l.exists()));
}

#[test]
fn reset_of_one_account_leaves_the_others_alone() {
    emulator_or_skip!();
    let emu = shared();
    let (a, b) = (unique_name("keep"), unique_name("drop"));
    for acct in [&a, &b] {
        let made = emu.call(Service::Blob, acct, "PUT", "/box?restype=container", &[BEARER], "");
        assert_eq!(made.status, 201, "{}", made.body);
    }
    emu.reset_account(&b);
    assert_eq!(emu.call(Service::Blob, &a, "GET", "/box?restype=container", &[BEARER], "").status, 200);
    assert_eq!(emu.call(Service::Blob, &b, "GET", "/box?restype=container", &[BEARER], "").status, 404);
}

#[test]
fn event_hubs_and_service_bus_queues_can_be_made() {
    emulator_or_skip!();
    let emu = shared();
    let ns = unique_name("ns");
    emu.create_event_hub(&ns, "orders", 2);
    let hub = emu.request("GET", &format!("/_azemu/eventhubs/{ns}/hubs/orders"), "");
    assert_eq!(hub.status, 200, "{}", hub.body);
    assert!(hub.body.contains("orders"), "{}", hub.body);
    let sb = unique_name("sb");
    emu.create_service_bus_queue(&sb, "commands");
    let queue = emu.call(Service::ServiceBus, &sb, "GET", "/commands?api-version=2021-05", &[BEARER], "");
    assert_eq!(queue.status, 200, "{}", queue.body);
    assert!(queue.body.contains("commands"), "{}", queue.body);
}

#[test]
fn an_emulator_that_is_already_running_can_be_used_and_its_authority_is_fetched() {
    emulator_or_skip!();
    let running = Emulator::try_start(&[]).expect("start");
    let used = Emulator::connect(&running.gateway()).expect("connect");
    assert_eq!(used.port(), running.port());
    assert!(std::env::var_os("AZEMU_CA_FILE").is_none());
    assert_eq!(used.ca_pem().trim(), running.ca_pem().trim());
    assert!(used.ca_file().exists());
    // `host:port` is accepted too, and a wrong address is an error that says what to do
    assert!(Emulator::connect(&format!("127.0.0.1:{}", running.port())).is_ok());
    let err = Emulator::connect("http://127.0.0.1:1").unwrap_err();
    assert!(err.contains("no emulator answers") && err.contains("AZEMU_GATEWAY"), "{err}");
    // using it did not start or stop anything
    drop(used);
    assert_eq!(running.request("GET", "/_azemu/health", "").status, 200);
}

#[test]
fn an_emulator_the_kit_started_stops_when_it_is_dropped_and_takes_arguments() {
    emulator_or_skip!();
    let emu = Emulator::try_start(&["--lenient"]).expect("start");
    let port = emu.port();
    let config = emu.request("GET", "/_azemu/config", "");
    assert!(config.body.contains("AZEMU_LENIENT") && config.body.contains("\"value\":\"true\""), "{}", config.body);
    drop(emu);
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_ok() {
        assert!(Instant::now() < deadline, "the emulator still answers on port {port} after the value was dropped");
        std::thread::sleep(Duration::from_millis(50));
    }
}
