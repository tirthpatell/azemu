package azemu;

import com.azure.messaging.eventhubs.*;
import com.azure.messaging.eventhubs.models.*;
import org.junit.jupiter.api.*;

import java.time.Duration;
import java.time.Instant;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

/** Event Hubs over AMQP (qpid-proton-j): producer, consumer, positions, owner level, properties. */
@EmuTest
class EventHubsTest {
    static Emu emu;
    static String ns;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        ns = Emu.name("ehns");
    }

    static List<String> bodies(List<PartitionEvent> l) {
        return l.stream().map(e -> e.getData().getBodyAsString()).collect(Collectors.toList());
    }

    EventHubProducerClient producer(String hub) { return Eh.builder(emu, ns, hub).buildProducerClient(); }
    EventHubConsumerClient consumer(String hub) { return Eh.builder(emu, ns, hub).consumerGroup("$Default").buildConsumerClient(); }

    // ---- properties -------------------------------------------------------------------------------------------
    @Test void eventHubProperties_listFourPartitionsAndTheName() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub)) {
            EventHubProperties props = p.getEventHubProperties();
            assertEquals(hub, props.getName());
            assertNotNull(props.getCreatedAt());
            assertEquals(List.of("0", "1", "2", "3"), props.getPartitionIds().stream().sorted().collect(Collectors.toList()));
            assertEquals(4, p.getPartitionIds().stream().count());
        }
    }

    @Test void partitionProperties_ofAnEmptyPartition_areEmpty() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub)) {
            PartitionProperties pp = p.getPartitionProperties("2");
            assertEquals("2", pp.getId());
            assertEquals(hub, pp.getEventHubName());
            assertTrue(pp.isEmpty());
            assertEquals(-1, pp.getLastEnqueuedSequenceNumber());
        }
    }

    @Test void partitionProperties_afterSending_showTheLastSequenceNumberAndAreNotEmpty() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub)) {
            p.send(List.of(new EventData("a"), new EventData("b"), new EventData("c")), new SendOptions().setPartitionId("1"));
            PartitionProperties pp = p.getPartitionProperties("1");
            assertFalse(pp.isEmpty());
            assertEquals(pp.getBeginningSequenceNumber() + 2, pp.getLastEnqueuedSequenceNumber());
            assertNotNull(pp.getLastEnqueuedTime());
            assertNotNull(pp.getLastEnqueuedOffset());
        }
    }

    @Test void partitionProperties_ofAMissingPartition_fail() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub)) {
            assertThrows(RuntimeException.class, () -> p.getPartitionProperties("99"));
        }
    }

    // ---- send -------------------------------------------------------------------------------------------------
    @Test void sendBatchToAPartition_thenReadItFromEarliest_withEverySystemProperty() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            EventDataBatch batch = p.createBatch(new CreateBatchOptions().setPartitionId("0"));
            assertTrue(batch.getMaxSizeInBytes() > 0);
            for (int i = 0; i < 10; i++) assertTrue(batch.tryAdd(new EventData("e" + i)));
            assertEquals(10, batch.getCount());
            p.send(batch);

            List<PartitionEvent> got = Eh.read(c, "0", 10, EventPosition.earliest(), 10);
            assertEquals(IntStream10(), bodies(got));
            long prev = -1;
            for (PartitionEvent e : got) {
                EventData d = e.getData();
                assertNotNull(d.getEnqueuedTime());
                assertNotNull(d.getOffsetString());
                assertTrue(d.getSequenceNumber() > prev, "sequence numbers grow");
                prev = d.getSequenceNumber();
                assertEquals("0", e.getPartitionContext().getPartitionId());
                assertEquals(hub, e.getPartitionContext().getEventHubName());
                assertEquals("$Default", e.getPartitionContext().getConsumerGroup());
            }
        }
    }

    static List<String> IntStream10() {
        List<String> l = new ArrayList<>();
        for (int i = 0; i < 10; i++) l.add("e" + i);
        return l;
    }

    @Test void eventProperties_contentTypeMessageIdCorrelationIdAndApplicationProperties_roundTrip() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            EventData e = new EventData("payload").setContentType("application/json").setMessageId("m-1").setCorrelationId("c-1");
            e.getProperties().put("s", "str");
            e.getProperties().put("i", 7);
            e.getProperties().put("l", 99999999999L);
            e.getProperties().put("d", 1.5d);
            e.getProperties().put("b", false);
            p.send(List.of(e), new SendOptions().setPartitionId("3"));
            EventData got = Eh.read(c, "3", 1, EventPosition.earliest(), 10).get(0).getData();
            assertEquals("payload", got.getBodyAsString());
            assertEquals("application/json", got.getContentType());
            assertEquals("m-1", got.getMessageId());
            assertEquals("c-1", got.getCorrelationId());
            assertEquals("str", got.getProperties().get("s"));
            assertEquals(7, got.getProperties().get("i"));
            assertEquals(99999999999L, got.getProperties().get("l"));
            assertEquals(1.5d, got.getProperties().get("d"));
            assertEquals(false, got.getProperties().get("b"));
        }
    }

    @Test void binaryBody_roundTripsByteForByte() {
        String hub = Eh.newHub(emu, ns);
        byte[] data = Util.bytes(100_000);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            p.send(List.of(new EventData(data)), new SendOptions().setPartitionId("0"));
            assertArrayEquals(data, Eh.read(c, "0", 1, EventPosition.earliest(), 10).get(0).getData().getBody());
        }
    }

    @Test void partitionKey_sendsEveryEventWithTheSameKeyToTheSamePartition() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            for (int i = 0; i < 6; i++) p.send(List.of(new EventData("k" + i)), new SendOptions().setPartitionKey("customer-42"));
            int owners = 0;
            for (String id : List.of("0", "1", "2", "3")) {
                List<PartitionEvent> got = Eh.read(c, id, 10, EventPosition.earliest(), 2);
                if (!got.isEmpty()) {
                    owners++;
                    assertEquals(6, got.size(), "all six events of one key are in one partition");
                    assertEquals("customer-42", got.get(0).getData().getPartitionKey());
                }
            }
            assertEquals(1, owners);
        }
    }

    @Test void batchWithPartitionKey_keepsTheKeyOnEveryEvent() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            EventDataBatch b = p.createBatch(new CreateBatchOptions().setPartitionKey("key-a"));
            b.tryAdd(new EventData("1"));
            b.tryAdd(new EventData("2"));
            p.send(b);
            int total = 0;
            for (String id : List.of("0", "1", "2", "3")) {
                for (PartitionEvent e : Eh.read(c, id, 10, EventPosition.earliest(), 2)) {
                    total++;
                    assertEquals("key-a", e.getData().getPartitionKey());
                }
            }
            assertEquals(2, total);
        }
    }

    @Test void sendWithoutKeyOrPartition_spreadsAcrossPartitions() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            for (int i = 0; i < 40; i++) p.send(List.of(new EventData("r" + i)));
            int total = 0;
            for (String id : List.of("0", "1", "2", "3")) total += Eh.read(c, id, 100, EventPosition.earliest(), 2).size();
            assertEquals(40, total);
        }
    }

    @Test void batchThatIsFull_tryAddReturnsFalse_andAnEventTooBigForAnyBatchThrows() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub)) {
            EventDataBatch b = p.createBatch(new CreateBatchOptions().setMaximumSizeInBytes(1000));
            assertEquals(1000, b.getMaxSizeInBytes());
            assertTrue(b.tryAdd(new EventData(new byte[500])));
            assertFalse(b.tryAdd(new EventData(new byte[700])), "does not fit next to the first one");
            assertEquals(1, b.getCount());
            assertThrows(com.azure.core.amqp.exception.AmqpException.class, () -> p.createBatch(new CreateBatchOptions().setMaximumSizeInBytes(1000))
                .tryAdd(new EventData(new byte[2000])), "an event too big for an empty batch throws");
            assertThrows(RuntimeException.class, () -> p.send(List.of(new EventData(new byte[2_000_000]))));
        }
    }

    @Test void eventWithAnEmptyBody_roundTrips() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            p.send(List.of(new EventData(new byte[0]), new EventData("")), new SendOptions().setPartitionId("0"));
            List<PartitionEvent> got = Eh.read(c, "0", 2, EventPosition.earliest(), 10);
            assertEquals(2, got.size());
            assertEquals(0, got.get(0).getData().getBody().length);
            assertEquals("", got.get(1).getData().getBodyAsString());
        }
    }

    @Test void sendToAPartitionThatDoesNotExist_failsWithinTheRetryBudget() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub)) {
            long t0 = System.currentTimeMillis();
            assertThrows(RuntimeException.class, () -> p.send(List.of(new EventData("x")), new SendOptions().setPartitionId("99")));
            assertTrue(System.currentTimeMillis() - t0 < 60_000, "gave up in " + (System.currentTimeMillis() - t0) + " ms");
        }
    }

    // ---- start positions --------------------------------------------------------------------------------------
    @Test void startFromSequenceNumber_inclusiveAndExclusive() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            List<EventData> events = new ArrayList<>();
            for (int i = 0; i < 6; i++) events.add(new EventData("s" + i));
            p.send(events, new SendOptions().setPartitionId("0"));
            List<PartitionEvent> all = Eh.read(c, "0", 6, EventPosition.earliest(), 10);
            long third = all.get(2).getData().getSequenceNumber();
            assertEquals(List.of("s2", "s3", "s4", "s5"), bodies(Eh.read(c, "0", 10, EventPosition.fromSequenceNumber(third, true), 3)));
            assertEquals(List.of("s3", "s4", "s5"), bodies(Eh.read(c, "0", 10, EventPosition.fromSequenceNumber(third, false), 3)));
        }
    }

    @Test void startFromOffset_andFromEnqueuedTime() throws Exception {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            p.send(List.of(new EventData("old1"), new EventData("old2")), new SendOptions().setPartitionId("0"));
            Thread.sleep(1500);
            Instant cut = Instant.now();
            Thread.sleep(1500);
            p.send(List.of(new EventData("new1"), new EventData("new2")), new SendOptions().setPartitionId("0"));
            List<PartitionEvent> all = Eh.read(c, "0", 4, EventPosition.earliest(), 10);
            assertEquals(4, all.size());
            assertEquals(List.of("new1", "new2"), bodies(Eh.read(c, "0", 10, EventPosition.fromEnqueuedTime(cut), 3)));
            String offsetOfSecond = all.get(1).getData().getOffsetString();
            assertEquals(List.of("new1", "new2"), bodies(Eh.read(c, "0", 10, EventPosition.fromOffsetString(offsetOfSecond), 3)),
                "an offset start is exclusive by default");
        }
    }

    @Test void startFromLatest_seesOnlyEventsSentAfterTheReceiverAttached() throws Exception {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub)) {
            p.send(List.of(new EventData("before")), new SendOptions().setPartitionId("0"));
            EventHubConsumerAsyncClient c = Eh.builder(emu, ns, hub).consumerGroup("$Default").buildAsyncConsumerClient();
            List<String> got = Collections.synchronizedList(new ArrayList<>());
            var sub = c.receiveFromPartition("0", EventPosition.latest()).subscribe(e -> got.add(e.getData().getBodyAsString()));
            try {
                Thread.sleep(2000);
                p.send(List.of(new EventData("after")), new SendOptions().setPartitionId("0"));
                Instant end = Instant.now().plusSeconds(15);
                while (got.isEmpty() && Instant.now().isBefore(end)) Thread.sleep(100);
                assertEquals(List.of("after"), got);
            } finally {
                sub.dispose();
                c.close();
            }
        }
    }

    @Test void readingDoesNotRemoveEvents_aSecondReaderSeesThemAgain() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c1 = consumer(hub); EventHubConsumerClient c2 = consumer(hub)) {
            p.send(List.of(new EventData("x"), new EventData("y")), new SendOptions().setPartitionId("1"));
            assertEquals(List.of("x", "y"), bodies(Eh.read(c1, "1", 2, EventPosition.earliest(), 10)));
            assertEquals(List.of("x", "y"), bodies(Eh.read(c2, "1", 2, EventPosition.earliest(), 10)));
        }
    }

    @Test void consumerGroups_haveTheirOwnView() {
        String hub = Eh.newHub(emu, ns, "analytics");
        try (EventHubProducerClient p = producer(hub);
             EventHubConsumerClient c = Eh.builder(emu, ns, hub).consumerGroup("analytics").buildConsumerClient()) {
            p.send(List.of(new EventData("g")), new SendOptions().setPartitionId("2"));
            assertEquals("analytics", c.getConsumerGroup());
            assertEquals(List.of("g"), bodies(Eh.read(c, "2", 1, EventPosition.earliest(), 10)));
        }
    }

    @Test void receiveWithTrackedLastEnqueuedProperties_reportsThePartitionEnd() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub); EventHubConsumerClient c = consumer(hub)) {
            p.send(List.of(new EventData("1"), new EventData("2"), new EventData("3")), new SendOptions().setPartitionId("0"));
            List<PartitionEvent> got = new ArrayList<>();
            c.receiveFromPartition("0", 1, EventPosition.earliest(), Duration.ofSeconds(10),
                new ReceiveOptions().setTrackLastEnqueuedEventProperties(true)).forEach(got::add);
            LastEnqueuedEventProperties last = got.get(0).getLastEnqueuedEventProperties();
            assertNotNull(last);
            assertEquals(got.get(0).getData().getSequenceNumber() + 2, last.getSequenceNumber());
            assertNotNull(last.getEnqueuedTime());
            assertNotNull(last.getRetrievalTime());
        }
    }

    // ---- owner level ------------------------------------------------------------------------------------------
    @Test void receiverWithAHigherOwnerLevel_takesOverFromALowerOne() throws Exception {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub)) {
            p.send(List.of(new EventData("seed")), new SendOptions().setPartitionId("0"));
            EventHubConsumerAsyncClient low = Eh.builder(emu, ns, hub).consumerGroup("$Default").buildAsyncConsumerClient();
            EventHubConsumerAsyncClient high = Eh.builder(emu, ns, hub).consumerGroup("$Default").buildAsyncConsumerClient();
            List<Throwable> lowErrors = Collections.synchronizedList(new ArrayList<>());
            List<String> lowGot = Collections.synchronizedList(new ArrayList<>());
            List<String> highGot = Collections.synchronizedList(new ArrayList<>());
            var s1 = low.receiveFromPartition("0", EventPosition.earliest(), new ReceiveOptions().setOwnerLevel(1L))
                .subscribe(e -> lowGot.add(e.getData().getBodyAsString()), lowErrors::add);
            try {
                Util.eventually(10_000, () -> lowGot.isEmpty() ? null : true);
                assertEquals(List.of("seed"), lowGot);
                var s2 = high.receiveFromPartition("0", EventPosition.earliest(), new ReceiveOptions().setOwnerLevel(2L))
                    .subscribe(e -> highGot.add(e.getData().getBodyAsString()), t -> { });
                try {
                    assertNotNull(Util.eventually(15_000, () -> lowErrors.isEmpty() ? null : true), "the lower owner level was disconnected");
                    assertNotNull(Util.eventually(10_000, () -> highGot.isEmpty() ? null : true));
                    String msg = String.valueOf(lowErrors.get(0));
                    assertTrue(msg.toLowerCase().contains("epoch") || msg.toLowerCase().contains("stolen") || msg.toLowerCase().contains("higher"), msg);
                } finally { s2.dispose(); }
            } finally {
                s1.dispose();
                low.close();
                high.close();
            }
        }
    }

    @Test void receiverWithALowerOwnerLevel_cannotAttachNextToAHigherOne() throws Exception {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = producer(hub)) {
            p.send(List.of(new EventData("seed")), new SendOptions().setPartitionId("0"));
            EventHubConsumerAsyncClient high = Eh.builder(emu, ns, hub).consumerGroup("$Default").buildAsyncConsumerClient();
            EventHubConsumerAsyncClient low = Eh.builder(emu, ns, hub).consumerGroup("$Default").buildAsyncConsumerClient();
            List<String> highGot = Collections.synchronizedList(new ArrayList<>());
            List<Throwable> lowErrors = Collections.synchronizedList(new ArrayList<>());
            var s1 = high.receiveFromPartition("0", EventPosition.earliest(), new ReceiveOptions().setOwnerLevel(10L))
                .subscribe(e -> highGot.add(e.getData().getBodyAsString()), t -> { });
            try {
                Util.eventually(10_000, () -> highGot.isEmpty() ? null : true);
                var s2 = low.receiveFromPartition("0", EventPosition.earliest(), new ReceiveOptions().setOwnerLevel(5L))
                    .subscribe(e -> { }, lowErrors::add);
                try {
                    assertNotNull(Util.eventually(20_000, () -> lowErrors.isEmpty() ? null : true), "the lower owner level is refused");
                } finally { s2.dispose(); }
            } finally {
                s1.dispose();
                high.close();
                low.close();
            }
        }
    }

    // ---- WebSocket --------------------------------------------------------------------------------------------
    @Test void webSocketTransport_sendsReceivesAndReadsProperties() {
        String hub = Eh.newHub(emu, ns);
        try (EventHubProducerClient p = Eh.wsBuilder(emu, ns, hub).buildProducerClient();
             EventHubConsumerClient c = Eh.wsBuilder(emu, ns, hub).consumerGroup("$Default").buildConsumerClient()) {
            assertEquals(4, p.getEventHubProperties().getPartitionIds().stream().count());
            p.send(List.of(new EventData("ws event")), new SendOptions().setPartitionId("1"));
            assertEquals(List.of("ws event"), bodies(Eh.read(c, "1", 1, EventPosition.earliest(), 10)));
        }
    }

    @Test void webSocketTransport_largeEventSpansManyFrames() {
        String hub = Eh.newHub(emu, ns);
        byte[] data = Util.bytes(800_000);
        try (EventHubProducerClient p = Eh.wsBuilder(emu, ns, hub).buildProducerClient();
             EventHubConsumerClient c = Eh.builder(emu, ns, hub).consumerGroup("$Default").buildConsumerClient()) {
            p.send(List.of(new EventData(data)), new SendOptions().setPartitionId("0"));
            assertArrayEquals(data, Eh.read(c, "0", 1, EventPosition.earliest(), 15).get(0).getData().getBody());
        }
    }
}
