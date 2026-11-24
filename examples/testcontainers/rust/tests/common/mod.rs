//! Starts azemu in a container and answers "which host port is that container port on".
#![allow(dead_code)]
use std::path::Path;

use azure_core::http::Url;
use azure_storage_blob::BlobServiceClient;
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use testcontainers::core::wait::HttpWaitStrategy;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// Pin the version: below 1.0 a minor version may change a flag or a setting.
const IMAGE: &str = "ghcr.io/tirthpatell/azemu";
const TAG: &str = "0.3.0";

/// The gateway, and the standard ports of the services these tests use.
pub const GATEWAY: u16 = 4577;
pub const BLOB: u16 = 10000;
pub const KEY_VAULT: u16 = 10010;
pub const SERVICE_BUS: u16 = 5672;

/// The id of the most recent container, for a program that shares one (tests/shared.rs).
pub static CONTAINER_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub struct Emulator {
    // The container is stopped and removed when this value is dropped.
    container: ContainerAsync<GenericImage>,
    pub host: String,
}

impl Emulator {
    pub async fn start() -> Self {
        Self::launch(false).await
    }

    /// For clients that ignore the port of the connection string and always dial the standard one (`azservicebus` dials 5672):
    /// publish that port on the same number on the host. Only one such container can run at a time on a machine.
    pub async fn start_on_standard_service_bus_port() -> Self {
        Self::launch(true).await
    }

    async fn launch(fixed_service_bus_port: bool) -> Self {
        let seed = Path::new(env!("CARGO_MANIFEST_DIR")).join("seed.json");
        let mut request = GenericImage::new(IMAGE, TAG)
            // Ready when the gateway answers 200 on /_azemu/health.
            .with_wait_for(WaitFor::http(
                HttpWaitStrategy::new("/_azemu/health").with_port(GATEWAY.tcp()).with_expected_status_code(200u16),
            ))
            // A seed file: copied into the container (a bind mount would need the file on the Docker host).
            .with_copy_to("/seed.json", seed.as_path())
            .with_env_var("AZEMU_SEED", "/seed.json")
            // The emulator has no sign-in: publish only on the loopback address (the default is every address).
            // Remove this for a Docker host on another machine.
            .with_host_config_modifier(|host_config| {
                for bindings in host_config.port_bindings.iter_mut().flat_map(|map| map.values_mut()) {
                    for binding in bindings.iter_mut().flatten() {
                        binding.host_ip = Some("127.0.0.1".to_string());
                    }
                }
            });
        // A Docker host that is not "localhost" needs a name in the certificate: see the page.
        if let Ok(name) = std::env::var("AZEMU_TLS_NAME") {
            request = request.with_env_var("AZEMU_TLS_NAME", name);
        }
        // Host port 0: Docker picks a free one. A port that is listed here is published; the others are not.
        for port in [GATEWAY, BLOB, KEY_VAULT, SERVICE_BUS] {
            let host_port = if fixed_service_bus_port && port == SERVICE_BUS { SERVICE_BUS } else { 0 };
            request = request.with_mapped_port(host_port, port.tcp());
        }
        let container = request.start().await.expect("start the azemu container");
        CONTAINER_ID.set(container.id().to_string()).ok();
        let host = container.get_host().await.expect("container host").to_string();
        Emulator { container, host }
    }

    /// The host port that a container port is published on (a different one for every container).
    pub async fn port(&self, container_port: u16) -> u16 {
        self.container.get_host_port_ipv4(container_port.tcp()).await.expect("mapped port")
    }

    /// `ca.pem` of this container: a new authority is made at every start.
    pub async fn ca_pem(&self) -> Vec<u8> {
        let url = format!("http://{}:{}/_azemu/ca.pem", self.host, self.port(GATEWAY).await);
        reqwest::get(url).await.unwrap().bytes().await.unwrap().to_vec()
    }

    /// Empties every service; the seed file is applied again.
    pub async fn reset(&self) {
        let url = format!("http://{}:{}/_azemu/reset", self.host, self.port(GATEWAY).await);
        let response = reqwest::Client::new().post(url).send().await.unwrap();
        assert!(response.status().is_success(), "reset: {}", response.status());
    }

    /// Empties one account in every service that has accounts, and leaves the others alone.
    pub async fn reset_account(&self, account: &str) {
        let url = format!("http://{}:{}/_azemu/reset?account={account}", self.host, self.port(GATEWAY).await);
        let response = reqwest::Client::new().post(url).send().await.unwrap();
        assert!(response.status().is_success(), "reset: {}", response.status());
    }

    /// A Blob client for one account, over plain http. The Rust crates have no connection string and no Shared Key, so the
    /// client has no credential and the URL carries an account SAS, signed with the well-known key of the development
    /// account (docs/rust.md). Any other account name is a new, empty account; the emulator does not check its SAS.
    pub async fn blob_service(&self, account: &str) -> BlobServiceClient {
        let port = self.port(BLOB).await;
        let url = Url::parse(&format!("http://{}:{port}/{account}?{}", self.host, dev_sas())).unwrap();
        BlobServiceClient::new(url, None, None).unwrap()
    }
}

const DEV_KEY: &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

/// An account SAS for Blob: read, write, delete, list, add, create; service, container and object.
fn dev_sas() -> String {
    let (sp, ss, srt, se, spr, sv) = ("rwdlac", "b", "sco", "2099-12-31T00:00:00Z", "https,http", "2020-12-06");
    let to_sign = format!("devstoreaccount1\n{sp}\n{ss}\n{srt}\n\n{se}\n\n{spr}\n{sv}\n\n");
    let mut mac = Hmac::<Sha256>::new_from_slice(&STANDARD.decode(DEV_KEY).unwrap()).unwrap();
    mac.update(to_sign.as_bytes());
    let sig = STANDARD.encode(mac.finalize().into_bytes());
    let enc = |s: &str| s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D").replace(':', "%3A").replace(',', "%2C");
    format!("sv={sv}&ss={ss}&srt={srt}&sp={sp}&se={}&spr={}&sig={}", enc(se), enc(spr), enc(&sig))
}
