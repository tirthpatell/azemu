package azemu;

import com.azure.messaging.servicebus.*;
import com.azure.messaging.servicebus.administration.ServiceBusAdministrationClient;
import com.azure.messaging.servicebus.administration.models.CreateQueueOptions;
import com.azure.messaging.servicebus.models.*;
import org.junit.jupiter.api.*;

import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.*;

/** The processor clients (push model) and the WebSocket transport. */
@EmuTest
class ServiceBusProcessorTest {
    static Emu emu;
    static String ns;
    static ServiceBusClientBuilder sb;
    static ServiceBusAdministrationClient admin;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        ns = Emu.name("sbns");
        sb = Sb.client(emu, ns);
        admin = Sb.admin(emu, ns);
    }

    // ---- processor --------------------------------------------------------------------------------------------
    @Test void processor_receivesAllMessagesAndCompletesThem() throws Exception {
        String q = admin.createQueue(Emu.dashed("pq")).getName();
        int n = 20;
        CountDownLatch done = new CountDownLatch(n);
        Set<String> seen = ConcurrentHashMap.newKeySet();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            for (int i = 0; i < n; i++) s.sendMessage(new ServiceBusMessage("p" + i));
        }
        ServiceBusProcessorClient p = sb.processor().queueName(q).maxConcurrentCalls(4)
            .processMessage(c -> { seen.add(c.getMessage().getBody().toString()); done.countDown(); })
            .processError(e -> System.err.println("processor error: " + e.getException()))
            .buildProcessorClient();
        p.start();
        try {
            assertTrue(done.await(30, TimeUnit.SECONDS), "got " + seen.size() + " of " + n);
            assertEquals(n, seen.size());
        } finally { p.close(); }
        // auto-complete removed them
        try (ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            assertTrue(Sb.receive(r, 1, 2).isEmpty());
        }
    }

    @Test void processor_handlerThatThrows_abandonsAndTheMessageComesBackWithHigherDeliveryCount() throws Exception {
        String q = admin.createQueue(Emu.dashed("pq")).getName();
        List<Long> counts = Collections.synchronizedList(new ArrayList<>());
        CountDownLatch done = new CountDownLatch(2);
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            s.sendMessage(new ServiceBusMessage("flaky"));
        }
        ServiceBusProcessorClient p = sb.processor().queueName(q)
            .processMessage(c -> {
                counts.add(c.getMessage().getDeliveryCount());
                done.countDown();
                if (c.getMessage().getDeliveryCount() == 0) throw new IllegalStateException("first time fails");
            })
            .processError(e -> { })
            .buildProcessorClient();
        p.start();
        try {
            assertTrue(done.await(30, TimeUnit.SECONDS), "counts " + counts);
        } finally { p.close(); }
        assertEquals(List.of(0L, 1L), counts);
    }

    @Test void processor_handlerThatThrowsUntilMaxDeliveryCount_deadLettersTheMessage() throws Exception {
        String q = admin.createQueue(Emu.dashed("pq"), new CreateQueueOptions().setMaxDeliveryCount(2)).getName();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            s.sendMessage(new ServiceBusMessage("always fails").setMessageId("poison"));
        }
        AtomicInteger calls = new AtomicInteger();
        ServiceBusProcessorClient p = sb.processor().queueName(q)
            .processMessage(c -> { calls.incrementAndGet(); throw new IllegalStateException("no"); })
            .processError(e -> { })
            .buildProcessorClient();
        p.start();
        try {
            Object dead = Util.eventually(30_000, () -> admin.getQueueRuntimeProperties(q).getDeadLetterMessageCount() == 1 ? true : null);
            assertNotNull(dead, "dead-lettered after MaxDeliveryCount; handler calls " + calls.get());
        } finally { p.close(); }
        // the dead-letter receiver attaches after the dead-lettering (see J2 for one that was already waiting)
        try (ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
            ServiceBusReceivedMessage d = Sb.receiveOne(dlq);
            assertEquals("poison", d.getMessageId());
            assertEquals("MaxDeliveryCountExceeded", d.getDeadLetterReason());
            assertEquals(2, calls.get());
        }
    }

    @Test void processor_explicitSettlementInTheHandler_deferAndDeadLetter() throws Exception {
        String q = admin.createQueue(Emu.dashed("pq")).getName();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            s.sendMessage(new ServiceBusMessage("to-dead"));
        }
        CountDownLatch done = new CountDownLatch(1);
        ServiceBusProcessorClient p = sb.processor().queueName(q).disableAutoComplete()
            .processMessage(c -> { c.deadLetter(new DeadLetterOptions().setDeadLetterReason("explicit")); done.countDown(); })
            .processError(e -> { })
            .buildProcessorClient();
        p.start();
        try {
            assertTrue(done.await(20, TimeUnit.SECONDS));
        } finally { p.close(); }
        try (ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
            assertEquals("explicit", Sb.receiveOne(dlq).getDeadLetterReason());
        }
    }

    @Test void processor_onAMissingQueueWithAutoCreateOn_justWaits_andStopStartWorks() throws Exception {
        String q = admin.createQueue(Emu.dashed("pq")).getName();
        CountDownLatch first = new CountDownLatch(1);
        CountDownLatch second = new CountDownLatch(1);
        ServiceBusProcessorClient p = sb.processor().queueName(q)
            .processMessage(c -> { if (c.getMessage().getBody().toString().equals("one")) first.countDown(); else second.countDown(); })
            .processError(e -> { })
            .buildProcessorClient();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            p.start();
            s.sendMessage(new ServiceBusMessage("one"));
            assertTrue(first.await(20, TimeUnit.SECONDS));
            p.stop();
            assertFalse(p.isRunning());
            s.sendMessage(new ServiceBusMessage("two"));
            p.start();
            assertTrue(second.await(20, TimeUnit.SECONDS));
        } finally { p.close(); }
    }

    @Test void sessionProcessor_receivesEverySessionsMessages() throws Exception {
        String q = admin.createQueue(Emu.dashed("psq"), new CreateQueueOptions().setSessionRequired(true)).getName();
        Map<String, List<String>> bySession = new ConcurrentHashMap<>();
        CountDownLatch done = new CountDownLatch(6);
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            for (int i = 0; i < 3; i++) {
                s.sendMessage(new ServiceBusMessage("a" + i).setSessionId("A"));
                s.sendMessage(new ServiceBusMessage("b" + i).setSessionId("B"));
            }
        }
        ServiceBusProcessorClient p = sb.sessionProcessor().queueName(q).maxConcurrentSessions(2)
            .sessionIdleTimeout(java.time.Duration.ofSeconds(3))
            .processMessage(c -> {
                bySession.computeIfAbsent(c.getMessage().getSessionId(), k -> Collections.synchronizedList(new ArrayList<>()))
                    .add(c.getMessage().getBody().toString());
                done.countDown();
            })
            .processError(e -> { })
            .buildProcessorClient();
        p.start();
        try {
            assertTrue(done.await(40, TimeUnit.SECONDS), bySession.toString());
        } finally { p.close(); }
        assertEquals(List.of("a0", "a1", "a2"), bySession.get("A"));
        assertEquals(List.of("b0", "b1", "b2"), bySession.get("B"));
    }

    // ---- WebSocket --------------------------------------------------------------------------------------------
    @Test void webSocketTransport_sendsAndReceivesQueueMessages() {
        String q = admin.createQueue(Emu.dashed("wq")).getName();
        ServiceBusClientBuilder ws = Sb.wsClient(emu, ns);
        try (ServiceBusSenderClient s = ws.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(ws, q)) {
            s.sendMessage(new ServiceBusMessage("over websocket").setMessageId("ws-1"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertEquals("over websocket", m.getBody().toString());
            assertEquals("ws-1", m.getMessageId());
            r.complete(m);
        }
    }

    @Test void webSocketTls_wss_sendsAndReceives() {
        String q = admin.createQueue(Emu.dashed("wq")).getName();
        ServiceBusClientBuilder wss = Sb.wssClient(emu, ns);
        try (ServiceBusSenderClient s = wss.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(wss, q)) {
            s.sendMessage(new ServiceBusMessage("over wss"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertEquals("over wss", m.getBody().toString());
            r.complete(m);
        }
    }

    @Test void webSocketTransport_largeMessageSpansManyWebSocketFrames() {
        String q = admin.createQueue(Emu.dashed("wq")).getName();
        ServiceBusClientBuilder ws = Sb.wsClient(emu, ns);
        byte[] data = Util.bytes(240_000);
        try (ServiceBusSenderClient s = ws.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(ws, q)) {
            s.sendMessage(new ServiceBusMessage(data));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertArrayEquals(data, m.getBody().toBytes());
            r.complete(m);
        }
    }

    @Test void webSocketTransport_andPlainAmqpClientsShareTheSameQueue() {
        String q = admin.createQueue(Emu.dashed("wq")).getName();
        ServiceBusClientBuilder ws = Sb.wsClient(emu, ns);
        try (ServiceBusSenderClient s = ws.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("ws to tcp"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertEquals("ws to tcp", m.getBody().toString());
            r.complete(m);
        }
    }

    @Test void webSocketTransport_sessionsOnATopicSubscription() {
        String t = admin.createTopic(Emu.dashed("wt")).getName();
        admin.createSubscription(t, "s", new com.azure.messaging.servicebus.administration.models.CreateSubscriptionOptions().setSessionRequired(true));
        ServiceBusClientBuilder ws = Sb.wsClient(emu, ns);
        try (ServiceBusSenderClient s = ws.sender().topicName(t).buildClient();
             ServiceBusSessionReceiverClient sr = ws.sessionReceiver().topicName(t).subscriptionName("s").buildClient()) {
            s.sendMessage(new ServiceBusMessage("ws session").setSessionId("w"));
            try (ServiceBusReceiverClient r = sr.acceptSession("w")) {
                assertEquals("ws session", Sb.receiveOne(r).getBody().toString());
            }
        }
    }

    @Test void webSocketTransport_ruleManagerListsTheDefaultRule() {
        String t = admin.createTopic(Emu.dashed("wt")).getName();
        admin.createSubscription(t, "s");
        try (ServiceBusRuleManagerClient rm = Sb.wsClient(emu, ns).ruleManager().topicName(t).subscriptionName("s").buildClient()) {
            assertEquals(1, rm.listRules().stream().count());
        }
    }
}
