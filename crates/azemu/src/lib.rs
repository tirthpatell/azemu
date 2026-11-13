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

/// The `aeg-*` properties and content type Event Grid sets on a delivered message.
fn sb_message(d: &DestinationDelivery) -> azemu_servicebus::enqueue::EnqueueMessage {
    azemu_servicebus::enqueue::EnqueueMessage {
        body: d.body.to_vec(),
        content_type: Some(d.content_type.clone()),
        // Service Bus: "The default value of MessageId is the internal ID of the Event Grid event."
        message_id: Some(d.event_id.clone()),
        application_properties: d.properties.clone(),
    }
}

/// The services to mount and the raw protocols (AMQP) to register.
pub struct Stack {
    pub services: Vec<Arc<dyn Service>>,
    pub raw: Vec<Arc<dyn RawProtocol>>,
}

/// Every service, wired: events from Blob, Key Vault and App Configuration into Event Grid, delivery to queues, Service
/// Bus and event hubs, and ARM on top of all of them. Fresh state each call.
pub fn full_stack() -> Stack {
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
                sb.enqueue_message(&d.target.namespace, azemu_servicebus::enqueue::EnqueueTarget::Queue(&d.target.entity), sb_message(d))
                    .map_err(|e| e.description)
            }),
        );
    }
    {
        let sb = sb_service.shared.clone();
        eventgrid.register_destination(
            DestinationKind::ServiceBusTopic,
            Arc::new(move |d| {
                sb.enqueue_message(&d.target.namespace, azemu_servicebus::enqueue::EnqueueTarget::Topic(&d.target.entity), sb_message(d))
                    .map_err(|e| e.description)
            }),
        );
    }
    {
        let eh = eh_service.shared.clone();
        eventgrid.register_destination(
            DestinationKind::EventHub,
            Arc::new(move |d| {
                eh.enqueue_event(
                    &d.target.namespace,
                    &d.target.entity,
                    azemu_eventhubs::enqueue::EnqueueEvent {
                        body: d.body.to_vec(),
                        content_type: Some(d.content_type.clone()),
                        application_properties: d.properties.clone(),
                    },
                )
                .map_err(|e| e.description)
            }),
        );
    }

    // The control plane (`management.localhost`): what it creates exists in the services above.
    let arm = Arc::new(azemu_arm::ArmService::new(azemu_arm::Backends {
        blob: Some(blob.clone()),
        queue: Some(queue.clone()),
        table: Some(table.clone()),
        cosmos: Some(cosmos.clone()),
        eventgrid: Some(eventgrid.clone()),
        keyvault: Some(keyvault.clone()),
        appconfig: Some(appconfig.clone()),
        servicebus: Some(sb_service.shared.clone()),
        eventhubs: Some(eh_service.shared.clone()),
    }));

    let services: Vec<Arc<dyn Service>> = vec![
        blob,
        Arc::new(dfs),
        queue,
        table,
        keyvault,
        appconfig,
        cosmos,
        eventgrid,
        Arc::new(azemu_identity::IdentityService::new()),
        sb_service,
        eh_service,
        kafka.clone(),
        arm,
    ];
    Stack { services, raw: vec![amqp, kafka] }
}
