package azemu;

import com.azure.messaging.eventhubs.*;
import com.azure.messaging.eventhubs.checkpointstore.blob.BlobCheckpointStore;
import com.azure.messaging.eventhubs.models.*;
import com.azure.storage.blob.BlobContainerAsyncClient;
import com.azure.storage.blob.BlobContainerClient;
import com.azure.storage.blob.BlobContainerClientBuilder;
import com.azure.storage.blob.BlobServiceClient;
import com.azure.storage.blob.BlobServiceClientBuilder;
import com.azure.storage.common.StorageSharedKeyCredential;
import org.junit.jupiter.api.*;

import java.time.Duration;
import java.util.*;
import java.util.concurrent.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

/** EventProcessorClient with the Blob checkpoint store (the store lives in azemu's own Blob service). */
@EmuTest
class EventHubsProcessorTest {
    static Emu emu;
    static String ns;
    static String acct;
    static BlobServiceClient blobs;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        ns = Emu.name("ehns");
        acct = Emu.name("ehstore");
        blobs = new BlobServiceClientBuilder().endpoint(emu.blobUrl(acct))
            .credential(new StorageSharedKeyCredential(acct, Emu.DEV_KEY)).buildClient();
    }

    BlobContainerAsyncClient store() {
        String name = Emu.dashed("cp");
        blobs.createBlobContainer(name);
        return new BlobContainerClientBuilder().endpoint(emu.blobUrl(acct)).containerName(name)
            .credential(new StorageSharedKeyCredential(acct, Emu.DEV_KEY)).buildAsyncClient();
    }

    EventProcessorClientBuilder processor(String hub, BlobContainerAsyncClient container) {
        return new EventProcessorClientBuilder().connectionString(emu.ehConnStr(ns, hub))
            .consumerGroup("$Default").checkpointStore(new BlobCheckpointStore(container))
            .retryOptions(Eh.FAST_RETRY)
            .initialPartitionEventPosition(id -> EventPosition.earliest())
            .loadBalancingUpdateInterval(Duration.ofSeconds(1))
            .partitionOwnershipExpirationInterval(Duration.ofSeconds(8))
            .processError(e -> {
                // the SDK reports "Scheduler unavailable" for every partition when a processor is stopped
                if (!String.valueOf(e.getThrowable()).contains("Scheduler unavailable"))
                    System.err.println("processor error: " + e.getThrowable());
            });
    }

    static void sendToAll(EventHubProducerClient p, String prefix, int perPartition) {
        for (String id : List.of("0", "1", "2", "3")) {
            List<EventData> l = new ArrayList<>();
            for (int i = 0; i < perPartition; i++) l.add(new EventData(prefix + id + "-" + i));
            p.send(l, new SendOptions().setPartitionId(id));
        }
    }

    @Test void singleProcessor_readsEveryPartitionAndWritesCheckpointBlobs() throws Exception {
        String hub = Eh.newHub(emu, ns);
        BlobContainerAsyncClient c = store();
        Set<String> seen = ConcurrentHashMap.newKeySet();
        CountDownLatch all = new CountDownLatch(12);
        try (EventHubProducerClient p = Eh.builder(emu, ns, hub).buildProducerClient()) {
            sendToAll(p, "a", 3);
        }
        EventProcessorClient proc = processor(hub, c)
            .processEvent(ctx -> { if (seen.add(ctx.getEventData().getBodyAsString())) all.countDown(); ctx.updateCheckpoint(); })
            .buildEventProcessorClient();
        proc.start();
        try {
            assertTrue(all.await(40, TimeUnit.SECONDS), "saw " + seen.size() + " of 12");
        } finally { proc.stop(); }
        BlobContainerClient sync = blobs.getBlobContainerClient(c.getBlobContainerName());
        List<String> names = sync.listBlobs().stream().map(b -> b.getName()).collect(Collectors.toList());
        long checkpoints = names.stream().filter(n -> n.contains("/checkpoint/")).count();
        long ownership = names.stream().filter(n -> n.contains("/ownership/")).count();
        assertEquals(4, checkpoints, names.toString());
        assertEquals(4, ownership, names.toString());
    }

    @Test void restartedProcessor_resumesAfterTheLastCheckpoint_andSeesOnlyNewEvents() throws Exception {
        String hub = Eh.newHub(emu, ns);
        BlobContainerAsyncClient c = store();
        try (EventHubProducerClient p = Eh.builder(emu, ns, hub).buildProducerClient()) {
            sendToAll(p, "first-", 2);
            Set<String> seen1 = ConcurrentHashMap.newKeySet();
            CountDownLatch l1 = new CountDownLatch(8);
            EventProcessorClient p1 = processor(hub, c)
                .processEvent(ctx -> { if (seen1.add(ctx.getEventData().getBodyAsString())) l1.countDown(); ctx.updateCheckpoint(); })
                .buildEventProcessorClient();
            p1.start();
            try { assertTrue(l1.await(40, TimeUnit.SECONDS), "first run saw " + seen1.size()); } finally { p1.stop(); }

            sendToAll(p, "second-", 1);
            Set<String> seen2 = ConcurrentHashMap.newKeySet();
            CountDownLatch l2 = new CountDownLatch(4);
            EventProcessorClient p2 = processor(hub, c)
                .processEvent(ctx -> { if (seen2.add(ctx.getEventData().getBodyAsString())) l2.countDown(); ctx.updateCheckpoint(); })
                .buildEventProcessorClient();
            p2.start();
            try { assertTrue(l2.await(40, TimeUnit.SECONDS), "second run saw " + seen2); } finally { p2.stop(); }
            Thread.sleep(1500);
            assertEquals(Set.of("second-0-0", "second-1-0", "second-2-0", "second-3-0"), seen2, "no event is delivered twice across restarts");
        }
    }

    @Test void twoProcessors_shareThePartitionsOfOneConsumerGroup() throws Exception {
        String hub = Eh.newHub(emu, ns);
        BlobContainerAsyncClient c = store();
        Map<String, Set<String>> partitionsByProcessor = new ConcurrentHashMap<>();
        Set<String> seen = ConcurrentHashMap.newKeySet();
        EventProcessorClient a = processor(hub, c)
            .processEvent(ctx -> { partitionsByProcessor.computeIfAbsent("a", k -> ConcurrentHashMap.newKeySet()).add(ctx.getPartitionContext().getPartitionId());
                seen.add(ctx.getEventData().getBodyAsString()); ctx.updateCheckpoint(); })
            .buildEventProcessorClient();
        EventProcessorClient b = processor(hub, c)
            .processEvent(ctx -> { partitionsByProcessor.computeIfAbsent("b", k -> ConcurrentHashMap.newKeySet()).add(ctx.getPartitionContext().getPartitionId());
                seen.add(ctx.getEventData().getBodyAsString()); ctx.updateCheckpoint(); })
            .buildEventProcessorClient();
        BlobContainerClient sync = blobs.getBlobContainerClient(c.getBlobContainerName());
        Set<String> owners = new HashSet<>();
        try (EventHubProducerClient p = Eh.builder(emu, ns, hub).buildProducerClient()) {
            a.start();
            b.start();
            try {
                // send in rounds so that each processor, once it owns something, gets events
                int round = 0;
                long end = System.currentTimeMillis() + 60_000;
                while (System.currentTimeMillis() < end) {
                    sendToAll(p, "r" + (round++) + "-", 1);
                    Thread.sleep(1500);
                    Set<String> pa = partitionsByProcessor.getOrDefault("a", Set.of());
                    Set<String> pb = partitionsByProcessor.getOrDefault("b", Set.of());
                    if (!pa.isEmpty() && !pb.isEmpty() && pa.size() + pb.size() >= 4) break;
                }
                // read the owners while both run: a stopping processor gives its ownership back (empty owner id)
                for (com.azure.storage.blob.models.BlobItem blob : sync.listBlobs()) {
                    if (blob.getName().contains("/ownership/")) {
                        Map<String, String> md = sync.getBlobClient(blob.getName()).getProperties().getMetadata();
                        owners.add(String.valueOf(md.get("ownerid")));
                    }
                }
            } finally { a.stop(); b.stop(); }
        }
        Set<String> pa = partitionsByProcessor.getOrDefault("a", Set.of());
        Set<String> pb = partitionsByProcessor.getOrDefault("b", Set.of());
        assertFalse(pa.isEmpty(), "processor a got partitions: " + partitionsByProcessor);
        assertFalse(pb.isEmpty(), "processor b got partitions: " + partitionsByProcessor);
        Set<String> union = new TreeSet<>(pa);
        union.addAll(pb);
        assertEquals(Set.of("0", "1", "2", "3"), union);
        assertEquals(2, owners.size(), "two owner ids in the ownership blobs: " + owners);
    }

    @Test void processorBatch_deliversEventsInBatchesWithCheckpoint() throws Exception {
        String hub = Eh.newHub(emu, ns);
        BlobContainerAsyncClient c = store();
        try (EventHubProducerClient p = Eh.builder(emu, ns, hub).buildProducerClient()) {
            List<EventData> l = new ArrayList<>();
            for (int i = 0; i < 10; i++) l.add(new EventData("b" + i));
            p.send(l, new SendOptions().setPartitionId("2"));
        }
        List<String> got = Collections.synchronizedList(new ArrayList<>());
        CountDownLatch done = new CountDownLatch(10);
        EventProcessorClient proc = processor(hub, c)
            .processEventBatch(ctx -> { ctx.getEvents().forEach(e -> { got.add(e.getBodyAsString()); done.countDown(); }); ctx.updateCheckpoint(); }, 5, Duration.ofSeconds(2))
            .buildEventProcessorClient();
        proc.start();
        try { assertTrue(done.await(40, TimeUnit.SECONDS), "got " + got); } finally { proc.stop(); }
        List<String> expected = new ArrayList<>();
        for (int i = 0; i < 10; i++) expected.add("b" + i);
        assertEquals(expected, got);
    }
}
