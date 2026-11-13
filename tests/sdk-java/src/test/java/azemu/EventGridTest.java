package azemu;

import com.azure.core.credential.AzureKeyCredential;
import com.azure.core.exception.HttpResponseException;
import com.azure.core.models.CloudEvent;
import com.azure.core.models.CloudEventDataFormat;
import com.azure.core.util.BinaryData;
import com.azure.messaging.eventgrid.*;
import com.azure.messaging.eventgrid.namespaces.*;
import com.azure.messaging.eventgrid.namespaces.models.*;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.sun.net.httpserver.HttpServer;
import org.junit.jupiter.api.*;

import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.*;
import java.time.Duration;
import java.time.OffsetDateTime;
import java.util.*;
import java.util.concurrent.*;

import static org.junit.jupiter.api.Assertions.*;

/** Event Grid: classic publish (Event Grid schema, CloudEvents, custom schema) with a webhook subscriber, and namespace topics (pull). */
@EmuTest
class EventGridTest {
    static Emu emu;
    static HttpServer hook;
    static final BlockingQueue<String> bodies = new LinkedBlockingQueue<>();
    static final BlockingQueue<Map<String, String>> hookHeaders = new LinkedBlockingQueue<>();
    static final ObjectMapper JSON = new ObjectMapper();
    static final AzureKeyCredential KEY = new AzureKeyCredential("any-key");

    @BeforeAll static void setUp() throws Exception {
        emu = Emu.current();
        hook = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        hook.createContext("/hook", ex -> {
            bodies.add(new String(ex.getRequestBody().readAllBytes(), java.nio.charset.StandardCharsets.UTF_8));
            Map<String, String> h = new HashMap<>();
            ex.getRequestHeaders().forEach((k, v) -> h.put(k.toLowerCase(), String.join(",", v)));
            hookHeaders.add(h);
            ex.sendResponseHeaders(200, -1);
            ex.close();
        });
        hook.start();
    }

    @AfterAll static void tearDown() { if (hook != null) hook.stop(0); }

    static String admin(String method, String path, String body) throws Exception {
        HttpRequest.Builder b = HttpRequest.newBuilder(URI.create(emu.gateway() + "/_azemu/eventgrid" + path)).header("content-type", "application/json");
        b.method(method, body == null ? HttpRequest.BodyPublishers.noBody() : HttpRequest.BodyPublishers.ofString(body));
        HttpResponse<String> r = HttpClient.newHttpClient().send(b.build(), HttpResponse.BodyHandlers.ofString());
        assertTrue(r.statusCode() < 300, r.statusCode() + " " + r.body());
        return r.body();
    }

    String topicWithWebhook(String schema) throws Exception {
        String topic = Emu.name("egtopic");
        admin("PUT", "/topics/" + topic, "{\"kind\":\"custom\"}");
        admin("PUT", "/topics/" + topic + "/subscriptions/hook", "{\"webhookUrl\":\"http://127.0.0.1:" + hook.getAddress().getPort() + "/hook\",\"skipValidation\":true"
            + (schema == null ? "" : ",\"deliverySchema\":\"" + schema + "\"") + "}");
        bodies.clear();
        hookHeaders.clear();
        return topic;
    }

    String topicUrl(String topic) { return emu.https("eventgrid", topic) + "/api/events"; }

    @Test void eventGridSchema_publishedWithTheSdk_isDeliveredToAWebhook() throws Exception {
        String topic = topicWithWebhook(null);
        EventGridPublisherClient<EventGridEvent> c = new EventGridPublisherClientBuilder().endpoint(topicUrl(topic)).credential(KEY).buildEventGridEventPublisherClient();
        EventGridEvent e = new EventGridEvent("/orders/1", "Order.Created", BinaryData.fromObject(Map.of("qty", 3, "sku", "abc")), "1.0");
        c.sendEvent(e);
        String body = bodies.poll(15, TimeUnit.SECONDS);
        assertNotNull(body, "the webhook was called");
        JsonNode arr = JSON.readTree(body);
        assertTrue(arr.isArray());
        JsonNode got = arr.get(0);
        assertEquals("/orders/1", got.get("subject").asText());
        assertEquals("Order.Created", got.get("eventType").asText());
        assertEquals("1.0", got.get("dataVersion").asText());
        assertEquals(3, got.get("data").get("qty").asInt());
        assertEquals(e.getId(), got.get("id").asText());
        assertEquals("Notification", hookHeaders.peek().get("aeg-event-type"));
    }

    @Test void batchOfEventGridEvents_isDeliveredAsSeparateEvents() throws Exception {
        String topic = topicWithWebhook(null);
        EventGridPublisherClient<EventGridEvent> c = new EventGridPublisherClientBuilder().endpoint(topicUrl(topic)).credential(KEY).buildEventGridEventPublisherClient();
        List<EventGridEvent> l = new ArrayList<>();
        for (int i = 0; i < 5; i++) l.add(new EventGridEvent("/s/" + i, "T", BinaryData.fromObject(Map.of("i", i)), "1"));
        c.sendEvents(l);
        Set<String> subjects = new TreeSet<>();
        long end = System.currentTimeMillis() + 20_000;
        while (subjects.size() < 5 && System.currentTimeMillis() < end) {
            String b = bodies.poll(1, TimeUnit.SECONDS);
            if (b != null) for (JsonNode n : JSON.readTree(b)) subjects.add(n.get("subject").asText());
        }
        assertEquals(new TreeSet<>(List.of("/s/0", "/s/1", "/s/2", "/s/3", "/s/4")), subjects);
    }

    @Test void cloudEvent_publishedWithTheSdk_isDeliveredAsACloudEvent() throws Exception {
        String topic = topicWithWebhook(null);
        EventGridPublisherClient<CloudEvent> c = new EventGridPublisherClientBuilder().endpoint(topicUrl(topic)).credential(KEY).buildCloudEventPublisherClient();
        CloudEvent ce = new CloudEvent("/sources/a", "Thing.Happened", BinaryData.fromObject(Map.of("k", "v")), CloudEventDataFormat.JSON, "application/json")
            .setSubject("sub").setTime(OffsetDateTime.now());
        c.sendEvent(ce);
        String body = bodies.poll(15, TimeUnit.SECONDS);
        assertNotNull(body);
        JsonNode got = JSON.readTree(body);
        if (got.isArray()) got = got.get(0);
        assertEquals("1.0", got.get("specversion").asText());
        assertEquals("/sources/a", got.get("source").asText());
        assertEquals("Thing.Happened", got.get("type").asText());
        assertEquals("v", got.get("data").get("k").asText());
        assertEquals(ce.getId(), got.get("id").asText());
    }

    @Test void customSchema_publishedWithTheSdk_isDeliveredUnchanged() throws Exception {
        String topic = Emu.name("egcustom");
        admin("PUT", "/topics/" + topic, "{\"kind\":\"custom\",\"inputSchema\":\"CustomEventSchema\"}");
        admin("PUT", "/topics/" + topic + "/subscriptions/hook", "{\"webhookUrl\":\"http://127.0.0.1:" + hook.getAddress().getPort() + "/hook\",\"skipValidation\":true}");
        bodies.clear();
        EventGridPublisherClient<BinaryData> c = new EventGridPublisherClientBuilder().endpoint(topicUrl(topic)).credential(KEY).buildCustomEventPublisherClient();
        c.sendEvent(BinaryData.fromObject(Map.of("customField", "x", "n", 7)));
        String body = bodies.poll(15, TimeUnit.SECONDS);
        assertNotNull(body);
        JsonNode got = JSON.readTree(body);
        if (got.isArray()) got = got.get(0);
        assertEquals("x", got.get("customField").asText());
        assertEquals(7, got.get("n").asInt());
    }

    @Test void subscriptionFilter_onEventType_dropsOtherEvents() throws Exception {
        String topic = Emu.name("egfilter");
        admin("PUT", "/topics/" + topic, "{\"kind\":\"custom\"}");
        admin("PUT", "/topics/" + topic + "/subscriptions/hook", "{\"webhookUrl\":\"http://127.0.0.1:" + hook.getAddress().getPort() + "/hook\",\"skipValidation\":true,"
            + "\"filter\":{\"includedEventTypes\":[\"Keep\"]}}");
        bodies.clear();
        EventGridPublisherClient<EventGridEvent> c = new EventGridPublisherClientBuilder().endpoint(topicUrl(topic)).credential(KEY).buildEventGridEventPublisherClient();
        c.sendEvents(List.of(new EventGridEvent("/a", "Drop", BinaryData.fromObject(Map.of()), "1"), new EventGridEvent("/b", "Keep", BinaryData.fromObject(Map.of()), "1")));
        String body = bodies.poll(15, TimeUnit.SECONDS);
        assertNotNull(body);
        assertEquals("Keep", JSON.readTree(body).get(0).get("eventType").asText());
        assertNull(bodies.poll(2, TimeUnit.SECONDS), "the dropped event is not delivered");
    }

    @Test void publishWithoutAnyCredential_is401() {
        String topic = Emu.name("egnoauth");
        EventGridPublisherClient<EventGridEvent> c = new EventGridPublisherClientBuilder().endpoint(topicUrl(topic))
            .addPolicy((ctx, next) -> { ctx.getHttpRequest().getHeaders().remove("aeg-sas-key"); return next.process(); })
            .credential(KEY).buildEventGridEventPublisherClient();
        HttpResponseException e = assertThrows(HttpResponseException.class,
            () -> c.sendEvent(new EventGridEvent("/a", "T", BinaryData.fromObject(Map.of()), "1")));
        assertEquals(401, e.getResponse().getStatusCode());
    }

    // ---- namespace topics (pull) -------------------------------------------------------------------------------
    String namespaceTopic() throws Exception {
        String ns = Emu.name("egns"), topic = "jobs";
        admin("PUT", "/topics/" + topic + "?namespace=" + ns, "{\"kind\":\"namespace\"}");
        admin("PUT", "/topics/" + topic + "/subscriptions/workers?namespace=" + ns, "{\"deliveryMode\":\"pull\",\"lockDurationSeconds\":60}");
        return ns;
    }

    @Test void namespaceTopic_sendReceiveAcknowledge() throws Exception {
        String ns = namespaceTopic();
        String endpoint = emu.https("eventgrid", ns);
        EventGridSenderClient sender = new EventGridSenderClientBuilder().endpoint(endpoint).credential(KEY).topicName("jobs").buildClient();
        EventGridReceiverClient receiver = new EventGridReceiverClientBuilder().endpoint(endpoint).credential(KEY).topicName("jobs").subscriptionName("workers").buildClient();
        sender.send(List.of(
            new CloudEvent("/jobs", "Job.Created", BinaryData.fromObject(Map.of("n", 1)), CloudEventDataFormat.JSON, "application/json"),
            new CloudEvent("/jobs", "Job.Created", BinaryData.fromObject(Map.of("n", 2)), CloudEventDataFormat.JSON, "application/json")));
        ReceiveResult r = receiver.receive(10, Duration.ofSeconds(5));
        assertEquals(2, r.getDetails().size());
        List<String> tokens = new ArrayList<>();
        for (ReceiveDetails d : r.getDetails()) {
            assertEquals(1, d.getBrokerProperties().getDeliveryCount());
            assertEquals("Job.Created", d.getEvent().getType());
            tokens.add(d.getBrokerProperties().getLockToken());
        }
        AcknowledgeResult ack = receiver.acknowledge(tokens);
        assertEquals(2, ack.getSucceededLockTokens().size());
        assertTrue(ack.getFailedLockTokens().isEmpty());
        assertEquals(0, receiver.receive(10, Duration.ofSeconds(1)).getDetails().size());
    }

    @Test void namespaceTopic_releaseMakesTheEventAvailableAgainWithAHigherDeliveryCount() throws Exception {
        String ns = namespaceTopic();
        String endpoint = emu.https("eventgrid", ns);
        EventGridSenderClient sender = new EventGridSenderClientBuilder().endpoint(endpoint).credential(KEY).topicName("jobs").buildClient();
        EventGridReceiverClient receiver = new EventGridReceiverClientBuilder().endpoint(endpoint).credential(KEY).topicName("jobs").subscriptionName("workers").buildClient();
        sender.send(new CloudEvent("/jobs", "Job.Created", BinaryData.fromObject(Map.of("n", 1)), CloudEventDataFormat.JSON, "application/json"));
        ReceiveDetails first = receiver.receive(1, Duration.ofSeconds(5)).getDetails().get(0);
        ReleaseResult released = receiver.release(List.of(first.getBrokerProperties().getLockToken()));
        assertEquals(1, released.getSucceededLockTokens().size());
        ReceiveDetails second = receiver.receive(1, Duration.ofSeconds(5)).getDetails().get(0);
        assertEquals(2, second.getBrokerProperties().getDeliveryCount());
    }

    @Test void namespaceTopic_rejectRemovesTheEvent_andAnUnknownLockTokenFails() throws Exception {
        String ns = namespaceTopic();
        String endpoint = emu.https("eventgrid", ns);
        EventGridSenderClient sender = new EventGridSenderClientBuilder().endpoint(endpoint).credential(KEY).topicName("jobs").buildClient();
        EventGridReceiverClient receiver = new EventGridReceiverClientBuilder().endpoint(endpoint).credential(KEY).topicName("jobs").subscriptionName("workers").buildClient();
        sender.send(new CloudEvent("/jobs", "Job.Created", BinaryData.fromObject(Map.of("n", 1)), CloudEventDataFormat.JSON, "application/json"));
        ReceiveDetails d = receiver.receive(1, Duration.ofSeconds(5)).getDetails().get(0);
        assertEquals(1, receiver.reject(List.of(d.getBrokerProperties().getLockToken())).getSucceededLockTokens().size());
        assertEquals(0, receiver.receive(1, Duration.ofSeconds(1)).getDetails().size());
        AcknowledgeResult bad = receiver.acknowledge(List.of("not-a-real-token"));
        assertEquals(1, bad.getFailedLockTokens().size());
    }

    @Test void namespaceTopic_receiveOnAnEmptyQueueReturnsNothingAfterTheWait() throws Exception {
        String ns = namespaceTopic();
        EventGridReceiverClient receiver = new EventGridReceiverClientBuilder().endpoint(emu.https("eventgrid", ns)).credential(KEY).topicName("jobs").subscriptionName("workers").buildClient();
        long t0 = System.currentTimeMillis();
        assertEquals(0, receiver.receive(5, Duration.ofSeconds(2)).getDetails().size());
        assertTrue(System.currentTimeMillis() - t0 < 30_000);
    }
}
