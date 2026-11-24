//! Every service of the emulator, built and connected to each other. The `azemu` binary and the tests that need
//! the whole thing (for example the Rust tests of the ARM crate) use [`full_stack`], so there is one place that
//! says how the services fit together.
//!
//! The service crates do not depend on one another; this is where they meet: Blob Storage, Key Vault and App
//! Configuration raise events into Event Grid, Event Grid delivers to Storage queues, Service Bus queues and
//! topics and event hubs (see `docs/services/storage-events.md`), and ARM (`management.localhost`) holds a handle
//! on each service it creates resources in.

#[cfg(not(feature = "kafka"))]
mod kafka_off;

use std::sync::Arc;

use azemu_core::{RawProtocol, Service};
use azemu_eventgrid::{DestinationDelivery, DestinationKind, EventGridService, SourceEvent, SourceRef};

/// Hands Blob and Data Lake events to Event Grid.
struct StorageEvents {
    eventgrid: Arc<EventGridService>,
}

impl azemu_blob::StorageEventSink for StorageEvents {
    fn wants(&self, account: &str) -> bool {
        self.eventgrid.wants_storage_account(account)
    }

    fn raise(&self, e: azemu_blob::StorageEvent) {
        self.eventgrid.raise_source_event(
            &SourceRef::storage_account(&e.account),
            SourceEvent {
                subject: e.subject,
                event_type: e.event_type,
                data: e.data,
                data_version: e.data_version,
                id: Some(e.id),
                time: Some(e.time),
            },
        );
    }
}

/// Hands Key Vault events to Event Grid.
struct VaultEvents {
    eventgrid: Arc<EventGridService>,
}

impl azemu_keyvault::VaultEventSink for VaultEvents {
    fn wants(&self, vault: &str) -> bool {
        self.eventgrid.wants_key_vault(vault)
    }

    fn raise(&self, e: azemu_keyvault::VaultEvent) {
        self.eventgrid.raise_source_event(
            &SourceRef::key_vault(&e.vault),
            SourceEvent {
                subject: e.subject,
                event_type: e.event_type,
                data: e.data,
                data_version: e.data_version,
                id: Some(e.id),
                time: Some(e.time),
            },
        );
    }
}

/// Hands App Configuration events to Event Grid.
struct StoreEvents {
    eventgrid: Arc<EventGridService>,
}

impl azemu_appconfig::StoreEventSink for StoreEvents {
    fn wants(&self, store: &str) -> bool {
        self.eventgrid.wants_app_configuration_store(store)
    }

    fn raise(&self, e: azemu_appconfig::StoreEvent) {
        self.eventgrid.raise_source_event(
            &SourceRef::app_configuration_store(&e.store),
            SourceEvent {
                subject: e.subject,
                event_type: e.event_type,
                data: e.data,
                data_version: e.data_version,
                id: Some(e.id),
                time: Some(e.time),
            },
        );
    }
}

/// Service Bus limits a `MessageId`, a `SessionId` and a `PartitionKey` to 128 characters, and an empty `MessageId` or
/// `SessionId` is refused (tried against a real queue: such an event was not delivered).
const SB_ID_LIMIT: usize = 128;

/// The message Event Grid builds: the `aeg-*` properties, the content type, and the delivery properties the subscription maps.
fn sb_message(d: &DestinationDelivery) -> Result<azemu_servicebus::enqueue::EnqueueMessage, String> {
    let sys = |n: &str| d.system.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    for (name, v) in &d.system {
        let too_long = v.chars().count() > SB_ID_LIMIT && matches!(name.as_str(), "MessageId" | "SessionId" | "PartitionKey");
        let empty = v.is_empty() && matches!(name.as_str(), "MessageId" | "SessionId");
        if too_long || empty {
            return Err(format!(
                "InvalidDeliveryProperty: Service Bus does not take the value of {name} ({} characters; 1 to {SB_ID_LIMIT} are allowed).",
                v.chars().count()
            ));
        }
    }
    Ok(azemu_servicebus::enqueue::EnqueueMessage {
        body: d.body.to_vec(),
        content_type: Some(d.content_type.clone()),
        // Service Bus: "The default value of MessageId is the internal ID of the Event Grid event."
        message_id: Some(sys("MessageId").unwrap_or_else(|| d.event_id.clone())),
        application_properties: d.properties.clone(),
        session_id: sys("SessionId"),
        partition_key: sys("PartitionKey"),
        correlation_id: sys("CorrelationId"),
        label: sys("Label"),
        reply_to: sys("ReplyTo"),
        reply_to_session_id: sys("ReplyToSessionId"),
        to: sys("To"),
    })
}

/// The services to mount and the raw protocols (AMQP) to register.
pub struct Stack {
    pub services: Vec<Arc<dyn Service>>,
    pub raw: Vec<Arc<dyn RawProtocol>>,
}

/// Every service, wired: events from Blob, Key Vault and App Configuration into Event Grid, delivery to queues, Service
/// Bus and event hubs, and ARM on top of all of them. Fresh state each call.
pub fn full_stack() -> Stack {
    full_stack_with(azemu_core::auth::Auth::from_settings())
}

/// [`full_stack`] with the strict authorisation handle given (a test that needs the mode on without setting a variable for the whole process).
/// One handle, held by every service that checks roles and by Resource Manager; the identity service checks the tokens. With the mode off
/// the handle does nothing (`docs/strict-auth.md`).
pub fn full_stack_with(auth: azemu_core::auth::Auth) -> Stack {
    let identity = Arc::new(azemu_identity::IdentityService::new());
    auth.set_authority(identity.token_authority());
    let blob = Arc::new(azemu_blob::BlobService::new());
    let dfs = blob.datalake();
    // Service Bus and Event Hubs both speak AMQP. One dispatcher owns every AMQP
    // connection and picks the service from the host name in the `open` frame
    // (see docs/services/eventhubs.md), so the Service Bus raw protocol itself
    // is not registered.
    let (sb_service, _sb_amqp) = azemu_servicebus::pair();
    let (eh_service, eh_amqp) = azemu_eventhubs::pair();
    let sb_factory = Arc::new(azemu_servicebus::amqp_conn::SbFactory { shared: sb_service.shared.clone() });
    let amqp = eh_amqp.mux(sb_factory);
    // The Kafka endpoint of Event Hubs: a raw protocol next to the AMQP one (the first bytes tell them apart) and a
    // service named `kafka` for `--pin kafka=9093`.
    #[cfg(feature = "kafka")]
    let kafka = eh_amqp.kafka();
    // without the `kafka` feature a stand-in that answers "this build does not include it" (see `kafka_off.rs`)
    #[cfg(not(feature = "kafka"))]
    let kafka = Arc::new(kafka_off::KafkaNotBuilt::new());

    let queue = Arc::new(azemu_queue::QueueService::new());
    let eventgrid = Arc::new(EventGridService::new());

    let keyvault = Arc::new(azemu_keyvault::KeyVaultService::new());
    let appconfig = Arc::new(azemu_appconfig::AppConfigService::new());
    let table = Arc::new(azemu_table::TableService::new());
    let cosmos = Arc::new(azemu_cosmos::CosmosService::new());

    // strict authorisation: the services that check roles hold the handle (Blob and the Data Lake service share one)
    blob.set_auth(auth.clone());
    queue.set_auth(auth.clone());
    table.set_auth(auth.clone());
    keyvault.set_auth(auth.clone());
    appconfig.set_auth(auth.clone());
    cosmos.set_auth(auth.clone());
    eventgrid.set_auth(auth.clone());
    sb_service.set_auth(auth.clone());
    eh_service.set_auth(auth.clone());

    // Blob, Key Vault and App Configuration -> Event Grid
    blob.set_event_sink(Arc::new(StorageEvents { eventgrid: eventgrid.clone() }));
    keyvault.set_event_sink(Arc::new(VaultEvents { eventgrid: eventgrid.clone() }));
    appconfig.set_event_sink(Arc::new(StoreEvents { eventgrid: eventgrid.clone() }));

    // Event Grid -> Storage queue, Service Bus queue / topic, event hub
    {
        let queue = queue.clone();
        eventgrid.register_destination(
            DestinationKind::StorageQueue,
            Arc::new(move |d| {
                queue.enqueue_base64(&d.target.namespace, &d.target.entity, &d.body, d.ttl_seconds).map_err(|e| e.to_string())
            }),
        );
    }
    {
        let sb = sb_service.shared.clone();
        eventgrid.register_destination(
            DestinationKind::ServiceBusQueue,
            Arc::new(move |d| {
                sb.enqueue_message(&d.target.namespace, azemu_servicebus::enqueue::EnqueueTarget::Queue(&d.target.entity), sb_message(d)?)
                    .map_err(|e| e.description)
            }),
        );
    }
    {
        let sb = sb_service.shared.clone();
        eventgrid.register_destination(
            DestinationKind::ServiceBusTopic,
            Arc::new(move |d| {
                sb.enqueue_message(&d.target.namespace, azemu_servicebus::enqueue::EnqueueTarget::Topic(&d.target.entity), sb_message(d)?)
                    .map_err(|e| e.description)
            }),
        );
    }
    {
        let eh = eh_service.shared.clone();
        eventgrid.register_destination(
            DestinationKind::EventHub,
            Arc::new(move |d| {
                eh.enqueue_event_keyed(
                    &d.target.namespace,
                    &d.target.entity,
                    azemu_eventhubs::enqueue::EnqueueEvent {
                        body: d.body.to_vec(),
                        content_type: Some(d.content_type.clone()),
                        application_properties: d.properties.clone(),
                    },
                    d.system.iter().find(|(k, _)| k == "PartitionKey").map(|(_, v)| v.as_str()),
                )
                .map_err(|e| e.description)
            }),
        );
    }

    // Event Grid -> a blob container, for dead letters (the write raises no blob event)
    {
        let blob = blob.clone();
        eventgrid.register_dead_letter_writer(Arc::new(move |b| {
            blob.put_block_blob_silently(&b.account, &b.container, &b.name, &b.content_type, b.body.clone())
        }));
    }

    let file: Arc<dyn Service> = Arc::new(azemu_file::FileService::new());

    // The control plane (`management.localhost`): what it creates exists in the services above.
    let arm = Arc::new(azemu_arm::ArmService::new(azemu_arm::Backends {
        blob: Some(blob.clone()),
        queue: Some(queue.clone()),
        table: Some(table.clone()),
        file: Some(file.clone()),
        cosmos: Some(cosmos.clone()),
        eventgrid: Some(eventgrid.clone()),
        keyvault: Some(keyvault.clone()),
        appconfig: Some(appconfig.clone()),
        servicebus: Some(sb_service.shared.clone()),
        eventhubs: Some(eh_service.shared.clone()),
        auth: Some(auth.clone()),
    }));

    // the keys Resource Manager hands out for authorization rules are the keys the data planes check
    sb_service.set_sas_source(arm.sas_source(azemu_arm::SasService::ServiceBus));
    eh_service.set_sas_source(arm.sas_source(azemu_arm::SasService::EventHubs));

    let services: Vec<Arc<dyn Service>> = vec![
        blob,
        Arc::new(dfs),
        queue,
        table,
        keyvault,
        appconfig,
        cosmos,
        eventgrid,
        identity,
        sb_service,
        eh_service,
        kafka.clone(),
        arm,
        Arc::new(azemu_monitor::MonitorService::new()),
        file,
        Arc::new(azemu_core::auth::AuthService::new(auth)),
    ];
    Stack { services, raw: vec![amqp, kafka] }
}
