package azemu;

import org.apache.kafka.clients.admin.*;
import org.apache.kafka.clients.consumer.*;
import org.apache.kafka.clients.producer.*;
import org.apache.kafka.common.*;
import org.apache.kafka.common.config.ConfigResource;
import org.apache.kafka.common.errors.*;
import org.apache.kafka.common.header.Header;
import org.apache.kafka.common.header.internals.RecordHeader;
import org.apache.kafka.common.serialization.*;
import org.junit.jupiter.api.*;

import java.net.HttpURLConnection;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicBoolean;

import static org.junit.jupiter.api.Assertions.*;

/**
 * The Kafka endpoint of Event Hubs with the reference client (Apache kafka-clients 3.9.1): producer (idempotent by default),
 * consumer groups, the admin client, TLS with the suite's trust store, SASL PLAIN with a connection string, SASL OAUTHBEARER.
 * The expectations are Kafka's and Microsoft Learn's ("Use Azure Event Hubs from Apache Kafka applications"), not the emulator's.
 */
@EmuTest
class KafkaTest {
    static Emu emu;
    static final String CONN_NS = "javans";

    @BeforeAll static void setUp() { emu = Emu.current(); }

    // ---- helpers -------------------------------------------------------------------------------------------

    static String plainServers() { return "127.0.0.1:" + emu.port; }

    static Properties plain() {
        Properties p = new Properties();
        p.put("bootstrap.servers", plainServers());
        p.put("request.timeout.ms", "20000");
        p.put("default.api.timeout.ms", "30000");
        return p;
    }

    static String connString(String ns) {
        return "Endpoint=sb://" + ns + ".eventhub.localhost:" + emu.port + "/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any";
    }

    static Properties tls(String ns) {
        Properties p = new Properties();
        p.put("bootstrap.servers", ns + ".eventhub.localhost:" + emu.port);
        p.put("security.protocol", "SSL");
        // the suite's trust store: the JDK's cacerts plus the emulator's CA (set by Emu before any client exists)
        p.put("ssl.truststore.location", System.getProperty("javax.net.ssl.trustStore"));
        p.put("ssl.truststore.password", "changeit");
        p.put("ssl.truststore.type", "PKCS12");
        p.put("request.timeout.ms", "20000");
        p.put("default.api.timeout.ms", "30000");
        return p;
    }

    static Properties saslSsl(String ns) {
        Properties p = tls(ns);
        p.put("security.protocol", "SASL_SSL");
        p.put("sasl.mechanism", "PLAIN");
        p.put("sasl.jaas.config", "org.apache.kafka.common.security.plain.PlainLoginModule required username=\"$ConnectionString\" password=\""
            + connString(ns) + "\";");
        return p;
    }

    static Properties saslPlain(String ns) {
        Properties p = plain();
        p.put("security.protocol", "SASL_PLAINTEXT");
        p.put("sasl.mechanism", "PLAIN");
        p.put("sasl.jaas.config", "org.apache.kafka.common.security.plain.PlainLoginModule required username=\"$ConnectionString\" password=\""
            + connString(ns) + "\";");
        return p;
    }

    static KafkaProducer<String, String> producer(Properties base, Map<String, Object> extra) {
        Properties p = new Properties();
        p.putAll(base);
        p.put("key.serializer", StringSerializer.class.getName());
        p.put("value.serializer", StringSerializer.class.getName());
        p.putAll(extra);
        return new KafkaProducer<>(p);
    }

    static KafkaProducer<String, String> producer(Properties base) { return producer(base, Map.of()); }

    static KafkaConsumer<String, String> consumer(Properties base, String group, Map<String, Object> extra) {
        Properties p = new Properties();
        p.putAll(base);
        p.put("group.id", group);
        p.put("key.deserializer", StringDeserializer.class.getName());
        p.put("value.deserializer", StringDeserializer.class.getName());
        p.put("auto.offset.reset", "earliest");
        p.put("enable.auto.commit", "false");
        p.put("session.timeout.ms", "6000");
        p.put("heartbeat.interval.ms", "1000");
        p.put("max.poll.interval.ms", "20000");
        p.putAll(extra);
        return new KafkaConsumer<>(p);
    }

    static KafkaConsumer<String, String> consumer(Properties base, String group) { return consumer(base, group, Map.of()); }

    static String topic() { return Emu.dashed("jt"); }

    /** Create an event hub through the emulator's admin API (namespace `default` for plain connections). */
    static void hub(String ns, String name, int partitions) throws Exception {
        HttpURLConnection c = (HttpURLConnection) new URI(emu.gateway() + "/_azemu/eventhubs/" + ns + "/hubs/" + name).toURL().openConnection();
        c.setRequestMethod("PUT");
        c.setDoOutput(true);
        c.setRequestProperty("Content-Type", "application/json");
        c.getOutputStream().write(("{\"partitionCount\":" + partitions + "}").getBytes(StandardCharsets.UTF_8));
        int code = c.getResponseCode();
        c.disconnect();
        assertTrue(code == 200 || code == 201, "create hub: " + code);
    }

    static String hubInfo(String ns, String name) throws Exception {
        HttpURLConnection c = (HttpURLConnection) new URI(emu.gateway() + "/_azemu/eventhubs/" + ns + "/hubs/" + name).toURL().openConnection();
        int code = c.getResponseCode();
        if (code == 404) return null;
        String body = new String(c.getInputStream().readAllBytes(), StandardCharsets.UTF_8);
        c.disconnect();
        return body;
    }

    static List<ConsumerRecord<String, String>> poll(KafkaConsumer<String, String> c, int n, int seconds) {
        List<ConsumerRecord<String, String>> out = new ArrayList<>();
        long end = System.currentTimeMillis() + seconds * 1000L;
        while (out.size() < n && System.currentTimeMillis() < end) {
            for (ConsumerRecord<String, String> r : c.poll(Duration.ofMillis(500))) out.add(r);
        }
        return out;
    }

    static void sendAll(KafkaProducer<String, String> p, String topic, int partition, String... values) throws Exception {
        List<Future<RecordMetadata>> fs = new ArrayList<>();
        for (String v : values) fs.add(p.send(new ProducerRecord<>(topic, partition, null, v)));
        p.flush();
        for (Future<RecordMetadata> f : fs) f.get(20, TimeUnit.SECONDS);
    }

    // ---- producer and consumer -------------------------------------------------------------------------------

    @Test void producerAndConsumer_roundTrip_withKeyHeadersAndOffsets() throws Exception {
        String t = topic();
        hub("default", t, 2);
        try (KafkaProducer<String, String> p = producer(plain())) {
            ProducerRecord<String, String> r = new ProducerRecord<>(t, 1, "user-1", "payload");
            r.headers().add(new RecordHeader("trace", "abc".getBytes(StandardCharsets.UTF_8)));
            r.headers().add(new RecordHeader("nothing", null));
            RecordMetadata md = p.send(r).get(20, TimeUnit.SECONDS);
            assertEquals(1, md.partition());
            assertEquals(0, md.offset());
            RecordMetadata md2 = p.send(new ProducerRecord<>(t, 1, "user-2", "second")).get(20, TimeUnit.SECONDS);
            assertEquals(1, md2.offset());
        }
        try (KafkaConsumer<String, String> c = consumer(plain(), Emu.dashed("g"))) {
            c.subscribe(List.of(t));
            List<ConsumerRecord<String, String>> got = poll(c, 2, 30);
            assertEquals(2, got.size());
            ConsumerRecord<String, String> first = got.get(0);
            assertEquals("user-1", first.key());
            assertEquals("payload", first.value());
            assertEquals(1, first.partition());
            assertEquals(0, first.offset());
            assertEquals("abc", new String(first.headers().lastHeader("trace").value(), StandardCharsets.UTF_8));
            assertNull(first.headers().lastHeader("nothing").value());
            assertTrue(first.timestamp() > 0);
        }
    }

    @Test void idempotentProducer_isTheDefault_andKeepsOrder() throws Exception {
        String t = topic();
        hub("default", t, 1);
        try (KafkaProducer<String, String> p = producer(plain(), Map.of("linger.ms", "5", "max.in.flight.requests.per.connection", "5"))) {
            List<Future<RecordMetadata>> fs = new ArrayList<>();
            for (int i = 0; i < 300; i++) fs.add(p.send(new ProducerRecord<>(t, 0, null, "v" + i)));
            p.flush();
            for (int i = 0; i < 300; i++) assertEquals(i, fs.get(i).get(20, TimeUnit.SECONDS).offset());
        }
        try (KafkaConsumer<String, String> c = consumer(plain(), Emu.dashed("g"))) {
            c.subscribe(List.of(t));
            List<ConsumerRecord<String, String>> got = poll(c, 300, 40);
            assertEquals(300, got.size());
            for (int i = 0; i < 300; i++) assertEquals("v" + i, got.get(i).value());
        }
    }

    @Test void gzip_isAccepted_andOtherCodecsAreRefusedAsEventHubsDoes() throws Exception {
        String t = topic();
        hub("default", t, 1);
        try (KafkaProducer<String, String> p = producer(plain(), Map.of("compression.type", "gzip", "linger.ms", "20"))) {
            sendAll(p, t, 0, "z".repeat(5000), "y".repeat(5000));
        }
        try (KafkaConsumer<String, String> c = consumer(plain(), Emu.dashed("g"))) {
            c.subscribe(List.of(t));
            assertEquals(2, poll(c, 2, 30).size());
        }
        for (String codec : new String[] {"snappy", "lz4", "zstd"}) {
            try (KafkaProducer<String, String> p = producer(plain(), Map.of("compression.type", codec, "request.timeout.ms", "5000", "delivery.timeout.ms", "10000", "retries", "0"))) {
                ExecutionException e = assertThrows(ExecutionException.class, () -> p.send(new ProducerRecord<>(t, 0, null, "x".repeat(1000))).get(20, TimeUnit.SECONDS), codec);
                assertInstanceOf(UnsupportedForMessageFormatException.class, e.getCause(), codec + ": " + e.getCause());
            }
        }
    }

    // By default (as on Event Hubs) a Kafka client that names a topic nobody created creates nothing, although the Java consumer and
    // producer ask for auto-creation; the tests of auto-creation itself are in KafkaAutoCreateTest, which starts the emulator with
    // AZEMU_EVENTHUBS_AUTOCREATE=1.
    @Test void producer_toAMissingTopic_failsByDefault_andCreatesNoEventHub() throws Exception {
        String t = topic();
        try (KafkaProducer<String, String> p = producer(plain(), Map.of("max.block.ms", "5000", "request.timeout.ms", "5000", "delivery.timeout.ms", "10000", "retries", "0"))) {
            assertThrows(Exception.class, () -> p.send(new ProducerRecord<>(t, "k", "v")).get(20, TimeUnit.SECONDS));
        }
        assertNull(hubInfo("default", t));
    }

    @Test void consumer_subscribingToAMissingTopic_createsNothingByDefault() throws Exception {
        String t = topic();
        try (KafkaConsumer<String, String> c = consumer(plain(), Emu.dashed("g"))) {
            c.subscribe(List.of(t));
            long end = System.currentTimeMillis() + 4_000;
            while (System.currentTimeMillis() < end) c.poll(Duration.ofMillis(300));
        }
        assertNull(hubInfo("default", t));
    }

    @Test void messageOverTheLimit_isRefused() throws Exception {
        String t = topic();
        hub("default", t, 1);
        try (KafkaProducer<String, String> p = producer(plain(), Map.of("max.request.size", "5000000", "request.timeout.ms", "5000", "delivery.timeout.ms", "10000", "retries", "0"))) {
            ExecutionException e = assertThrows(ExecutionException.class, () -> p.send(new ProducerRecord<>(t, 0, null, "x".repeat(2_000_000))).get(20, TimeUnit.SECONDS));
            assertInstanceOf(RecordTooLargeException.class, e.getCause(), String.valueOf(e.getCause()));
        }
    }

    @Test void transactions_areNotAvailable() throws Exception {
        try (KafkaProducer<String, String> p = producer(plain(), Map.of("transactional.id", Emu.dashed("tx"), "max.block.ms", "4000"))) {
            assertThrows(KafkaException.class, p::initTransactions);
        }
    }

    // ---- consumer groups ----------------------------------------------------------------------------------------

    @Test void commitAndResume_afterARestartOfTheConsumer() throws Exception {
        String t = topic();
        hub("default", t, 1);
        try (KafkaProducer<String, String> p = producer(plain())) {
            sendAll(p, t, 0, "a", "b", "c", "d", "e");
        }
        String group = Emu.dashed("g");
        try (KafkaConsumer<String, String> c = consumer(plain(), group)) {
            c.subscribe(List.of(t));
            List<ConsumerRecord<String, String>> got = poll(c, 3, 30);
            assertTrue(got.size() >= 3, "got " + got.size()); // one poll may return all five
            c.commitSync(Map.of(new TopicPartition(t, 0), new OffsetAndMetadata(got.get(2).offset() + 1, "note")));
        }
        try (KafkaConsumer<String, String> c = consumer(plain(), group)) {
            Map<TopicPartition, OffsetAndMetadata> committed = c.committed(Set.of(new TopicPartition(t, 0)));
            assertEquals(3, committed.get(new TopicPartition(t, 0)).offset());
            assertEquals("note", committed.get(new TopicPartition(t, 0)).metadata());
            c.subscribe(List.of(t));
            List<ConsumerRecord<String, String>> got = poll(c, 2, 30);
            assertEquals(List.of("d", "e"), got.stream().map(ConsumerRecord::value).toList());
        }
    }

    @Test void autoOffsetReset_latest_skipsTheBacklog_andSeekGoesWhereTold() throws Exception {
        String t = topic();
        hub("default", t, 1);
        try (KafkaProducer<String, String> p = producer(plain())) {
            sendAll(p, t, 0, "old1", "old2", "old3");
        }
        try (KafkaConsumer<String, String> c = consumer(plain(), Emu.dashed("g"), Map.of("auto.offset.reset", "latest"))) {
            c.subscribe(List.of(t));
            long end = System.currentTimeMillis() + 20_000;
            while (c.assignment().isEmpty() && System.currentTimeMillis() < end) c.poll(Duration.ofMillis(200));
            assertTrue(poll(c, 1, 2).isEmpty());
            assertEquals(3, c.position(new TopicPartition(t, 0)));
            c.seek(new TopicPartition(t, 0), 1);
            assertEquals(List.of("old2", "old3"), poll(c, 2, 20).stream().map(ConsumerRecord::value).toList());
            c.seekToBeginning(List.of(new TopicPartition(t, 0)));
            assertEquals("old1", poll(c, 1, 20).get(0).value());
        }
        try (KafkaConsumer<String, String> c = consumer(plain(), Emu.dashed("g"))) {
            TopicPartition tp = new TopicPartition(t, 0);
            assertEquals(0, c.beginningOffsets(List.of(tp)).get(tp));
            assertEquals(3, c.endOffsets(List.of(tp)).get(tp));
            Map<TopicPartition, OffsetAndTimestamp> byTime = c.offsetsForTimes(Map.of(tp, System.currentTimeMillis() - 60_000));
            assertEquals(0, byTime.get(tp).offset());
            assertNull(c.offsetsForTimes(Map.of(tp, System.currentTimeMillis() + 3_600_000)).get(tp));
        }
    }

    @Test void twoConsumersInOneGroup_splitThePartitions_andALeaverTriggersARebalance() throws Exception {
        String t = topic();
        hub("default", t, 4);
        String group = Emu.dashed("g");
        ExecutorService pool = Executors.newFixedThreadPool(2);
        AtomicBoolean stop1 = new AtomicBoolean(), stop2 = new AtomicBoolean();
        ConcurrentMap<String, Set<TopicPartition>> owned = new ConcurrentHashMap<>();
        Runnable r1 = loop("c1", group, t, owned, stop1);
        Runnable r2 = loop("c2", group, t, owned, stop2);
        Future<?> f1 = pool.submit(r1);
        try {
            waitFor(() -> owned.getOrDefault("c1", Set.of()).size() == 4, 30, "the first consumer to own all four partitions");
            Future<?> f2 = pool.submit(r2);
            waitFor(() -> owned.getOrDefault("c1", Set.of()).size() == 2 && owned.getOrDefault("c2", Set.of()).size() == 2, 40, "a split");
            Set<TopicPartition> all = new HashSet<>(owned.get("c1"));
            all.addAll(owned.get("c2"));
            assertEquals(4, all.size());
            stop2.set(true); // leaves the group on close
            f2.get(20, TimeUnit.SECONDS);
            waitFor(() -> owned.getOrDefault("c1", Set.of()).size() == 4, 40, "the leaver's partitions to move");
        } finally {
            stop1.set(true);
            stop2.set(true);
            f1.get(20, TimeUnit.SECONDS);
            pool.shutdownNow();
        }
    }

    private Runnable loop(String name, String group, String t, ConcurrentMap<String, Set<TopicPartition>> owned, AtomicBoolean stop) {
        return () -> {
            try (KafkaConsumer<String, String> c = consumer(plain(), group)) {
                c.subscribe(List.of(t), new ConsumerRebalanceListener() {
                    @Override public void onPartitionsRevoked(Collection<TopicPartition> ps) { owned.put(name, Set.of()); }
                    @Override public void onPartitionsAssigned(Collection<TopicPartition> ps) { owned.put(name, new HashSet<>(ps)); }
                });
                while (!stop.get()) c.poll(Duration.ofMillis(200));
            }
            owned.put(name, Set.of());
        };
    }

    static void waitFor(java.util.function.BooleanSupplier cond, int seconds, String what) throws Exception {
        long end = System.currentTimeMillis() + seconds * 1000L;
        while (System.currentTimeMillis() < end) {
            if (cond.getAsBoolean()) return;
            Thread.sleep(100);
        }
        fail("timed out waiting for " + what);
    }

    // ---- admin ---------------------------------------------------------------------------------------------------

    @Test void adminClient_topicsConfigsAndGroups() throws Exception {
        try (Admin a = Admin.create(plain())) {
            String t = topic();
            a.createTopics(List.of(new NewTopic(t, 3, (short) 1).configs(Map.of("retention.ms", "7200000")))).all().get(20, TimeUnit.SECONDS);
            assertTrue(hubInfo("default", t).contains("\"partitionCount\":3"));
            assertTrue(a.listTopics().names().get(20, TimeUnit.SECONDS).contains(t));
            TopicDescription d = a.describeTopics(List.of(t)).allTopicNames().get(20, TimeUnit.SECONDS).get(t);
            assertEquals(3, d.partitions().size());
            assertEquals(1, d.partitions().get(0).replicas().size());
            ConfigResource res = new ConfigResource(ConfigResource.Type.TOPIC, t);
            Config cfg = a.describeConfigs(List.of(res)).all().get(20, TimeUnit.SECONDS).get(res);
            assertEquals("7200000", cfg.get("retention.ms").value());
            assertEquals("delete", cfg.get("cleanup.policy").value());
            Collection<Node> nodes = a.describeCluster().nodes().get(20, TimeUnit.SECONDS);
            assertEquals(1, nodes.size());
            Node n = nodes.iterator().next();
            assertEquals("127.0.0.1", n.host());
            assertEquals(emu.port, n.port());

            // creating it again fails; so does compaction
            ExecutionException e = assertThrows(ExecutionException.class, () -> a.createTopics(List.of(new NewTopic(t, 3, (short) 1))).all().get(20, TimeUnit.SECONDS));
            assertInstanceOf(TopicExistsException.class, e.getCause());
            e = assertThrows(ExecutionException.class, () -> a.createTopics(List.of(new NewTopic(topic(), 1, (short) 1).configs(Map.of("cleanup.policy", "compact")))).all().get(20, TimeUnit.SECONDS));
            assertInstanceOf(InvalidConfigurationException.class, e.getCause());
            // the partition count of an event hub does not change
            e = assertThrows(ExecutionException.class, () -> a.createPartitions(Map.of(t, NewPartitions.increaseTo(6))).all().get(20, TimeUnit.SECONDS));
            assertNotNull(e.getCause());

            // groups
            String g = Emu.dashed("g");
            try (KafkaProducer<String, String> p = producer(plain())) { sendAll(p, t, 0, "m1", "m2"); }
            try (KafkaConsumer<String, String> c = consumer(plain(), g, Map.of("client.id", "java-describe"))) {
                c.subscribe(List.of(t));
                List<ConsumerRecord<String, String>> got = poll(c, 2, 30);
                assertEquals(2, got.size());
                c.commitSync();
                assertTrue(a.listConsumerGroups().all().get(20, TimeUnit.SECONDS).stream().anyMatch(x -> x.groupId().equals(g)));
                ConsumerGroupDescription gd = a.describeConsumerGroups(List.of(g)).all().get(20, TimeUnit.SECONDS).get(g);
                assertEquals(ConsumerGroupState.STABLE, gd.state());
                assertEquals(1, gd.members().size());
                assertEquals("java-describe", gd.members().iterator().next().clientId());
                assertEquals(Set.of(new TopicPartition(t, 0)), gd.members().iterator().next().assignment().topicPartitions().stream().filter(tp -> tp.partition() == 0).collect(java.util.stream.Collectors.toSet()));
                Map<TopicPartition, OffsetAndMetadata> off = a.listConsumerGroupOffsets(g).partitionsToOffsetAndMetadata().get(20, TimeUnit.SECONDS);
                assertEquals(2, off.get(new TopicPartition(t, 0)).offset());
                Map<TopicPartition, ListOffsetsResult.ListOffsetsResultInfo> lo = a.listOffsets(Map.of(new TopicPartition(t, 0), OffsetSpec.latest())).all().get(20, TimeUnit.SECONDS);
                assertEquals(2, lo.get(new TopicPartition(t, 0)).offset());
            }

            a.deleteTopics(List.of(t)).all().get(20, TimeUnit.SECONDS);
            assertNull(hubInfo("default", t));
            e = assertThrows(ExecutionException.class, () -> a.deleteTopics(List.of(t)).all().get(20, TimeUnit.SECONDS));
            assertInstanceOf(UnknownTopicOrPartitionException.class, e.getCause());
        }
    }

    // ---- TLS and SASL ----------------------------------------------------------------------------------------------

    @Test void tls_producerAndConsumer_usingTheSuitesTrustStore() throws Exception {
        String t = topic();
        hub(CONN_NS, t, 2);
        try (KafkaProducer<String, String> p = producer(tls(CONN_NS))) {
            RecordMetadata md = p.send(new ProducerRecord<>(t, 1, "k", "over tls")).get(30, TimeUnit.SECONDS);
            assertEquals(1, md.partition());
        }
        try (KafkaConsumer<String, String> c = consumer(tls(CONN_NS), Emu.dashed("g"))) {
            c.subscribe(List.of(t));
            assertEquals("over tls", poll(c, 1, 30).get(0).value());
        }
        // the namespace is the TLS server name, and the broker address is the name the client used
        try (Admin a = Admin.create(tls(CONN_NS))) {
            Node n = a.describeCluster().nodes().get(20, TimeUnit.SECONDS).iterator().next();
            assertEquals(CONN_NS + ".eventhub.localhost", n.host());
            assertEquals(emu.port, n.port());
            assertTrue(a.listTopics().names().get(20, TimeUnit.SECONDS).contains(t));
        }
        try (Admin a = Admin.create(tls("othernamespace"))) {
            assertFalse(a.listTopics().names().get(20, TimeUnit.SECONDS).contains(t));
        }
    }

    @Test void saslPlain_withAConnectionString_overTls_andWithoutTls() throws Exception {
        String t = topic();
        hub(CONN_NS, t, 1);
        try (KafkaProducer<String, String> p = producer(saslSsl(CONN_NS))) {
            p.send(new ProducerRecord<>(t, 0, null, "sasl ssl")).get(30, TimeUnit.SECONDS);
        }
        try (KafkaConsumer<String, String> c = consumer(saslSsl(CONN_NS), Emu.dashed("g"))) {
            c.subscribe(List.of(t));
            assertEquals("sasl ssl", poll(c, 1, 30).get(0).value());
        }
        // without TLS the namespace comes from the Endpoint inside the password
        try (Admin a = Admin.create(saslPlain(CONN_NS))) {
            assertTrue(a.listTopics().names().get(20, TimeUnit.SECONDS).contains(t));
            Node n = a.describeCluster().nodes().get(20, TimeUnit.SECONDS).iterator().next();
            assertEquals(CONN_NS + ".eventhub.localhost", n.host());
        }
    }

    @Test void saslOauthbearer_isAcceptedWithAnyToken() throws Exception {
        String t = topic();
        hub(CONN_NS, t, 1);
        Properties p = tls(CONN_NS);
        p.put("security.protocol", "SASL_SSL");
        p.put("sasl.mechanism", "OAUTHBEARER");
        // Kafka's built-in unsecured token (a JWS with alg none): the broker accepts any bearer token
        p.put("sasl.jaas.config", "org.apache.kafka.common.security.oauthbearer.OAuthBearerLoginModule required unsecuredLoginStringClaim_sub=\"alice\";");
        try (KafkaProducer<String, String> prod = producer(p)) {
            prod.send(new ProducerRecord<>(t, 0, null, "bearer")).get(30, TimeUnit.SECONDS);
        }
        try (KafkaConsumer<String, String> c = consumer(p, Emu.dashed("g"))) {
            c.subscribe(List.of(t));
            assertEquals("bearer", poll(c, 1, 30).get(0).value());
        }
    }

    @Test void aClientThatSkipsSasl_isAccepted() throws Exception {
        String t = topic();
        hub("default", t, 1);
        try (KafkaProducer<String, String> p = producer(plain())) {
            sendAll(p, t, 0, "no sasl");
        }
    }
}
