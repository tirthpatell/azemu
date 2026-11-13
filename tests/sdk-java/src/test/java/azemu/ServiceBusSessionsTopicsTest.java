package azemu;

import com.azure.core.exception.ResourceExistsException;
import com.azure.core.exception.ResourceNotFoundException;
import com.azure.messaging.servicebus.*;
import com.azure.messaging.servicebus.administration.ServiceBusAdministrationClient;
import com.azure.messaging.servicebus.administration.models.*;
import com.azure.messaging.servicebus.models.*;
import org.junit.jupiter.api.*;

import java.time.Duration;
import java.time.OffsetDateTime;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

/** Sessions, topics, subscriptions and rules (AMQP rule manager and HTTPS administration client). */
@EmuTest
class ServiceBusSessionsTopicsTest {
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

    static List<String> bodies(Collection<ServiceBusReceivedMessage> l) {
        return l.stream().map(m -> m.getBody().toString()).collect(Collectors.toList());
    }

    String sessionQueue() {
        return admin.createQueue(Emu.dashed("sq"), new CreateQueueOptions().setSessionRequired(true)).getName();
    }

    // ---- sessions ---------------------------------------------------------------------------------------------
    @Test void acceptSession_receivesOnlyThatSessionsMessagesInOrder() {
        String q = sessionQueue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusSessionReceiverClient sr = sb.sessionReceiver().queueName(q).disableAutoComplete().buildClient()) {
            s.sendMessage(new ServiceBusMessage("a1").setSessionId("A"));
            s.sendMessage(new ServiceBusMessage("b1").setSessionId("B"));
            s.sendMessage(new ServiceBusMessage("a2").setSessionId("A"));
            s.sendMessage(new ServiceBusMessage("b2").setSessionId("B"));
            try (ServiceBusReceiverClient r = sr.acceptSession("B")) {
                assertEquals("B", r.getSessionId());
                List<ServiceBusReceivedMessage> got = Sb.receive(r, 2, 10);
                assertEquals(List.of("b1", "b2"), bodies(got));
                got.forEach(m -> assertEquals("B", m.getSessionId()));
                got.forEach(r::complete);
            }
            try (ServiceBusReceiverClient r = sr.acceptSession("A")) {
                List<ServiceBusReceivedMessage> got = Sb.receive(r, 2, 10);
                assertEquals(List.of("a1", "a2"), bodies(got));
                got.forEach(r::complete);
            }
        }
    }

    @Test void sessionState_roundTripsAndSurvivesReacceptingTheSession() {
        String q = sessionQueue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusSessionReceiverClient sr = sb.sessionReceiver().queueName(q).disableAutoComplete().buildClient()) {
            s.sendMessage(new ServiceBusMessage("x").setSessionId("state-session"));
            try (ServiceBusReceiverClient r = sr.acceptSession("state-session")) {
                assertNull(r.getSessionState(), "a new session has no state");
                r.setSessionState("progress=42".getBytes());
                assertArrayEquals("progress=42".getBytes(), r.getSessionState());
            }
            try (ServiceBusReceiverClient r = sr.acceptSession("state-session")) {
                assertArrayEquals("progress=42".getBytes(), r.getSessionState());
                r.setSessionState(null);
                assertNull(r.getSessionState());
            }
        }
    }

    @Test void acceptNextSession_picksASessionThatHasMessages() {
        String q = sessionQueue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusSessionReceiverClient sr = sb.sessionReceiver().queueName(q).disableAutoComplete().buildClient()) {
            s.sendMessage(new ServiceBusMessage("n1").setSessionId("next-1"));
            try (ServiceBusReceiverClient r = sr.acceptNextSession()) {
                assertEquals("next-1", r.getSessionId());
                ServiceBusReceivedMessage m = Sb.receiveOne(r);
                assertEquals("n1", m.getBody().toString());
                r.complete(m);
            }
        }
    }

    @Test void acceptNextSession_whileAnotherReceiverHoldsTheOnlySession_getsTheOtherSession() {
        String q = sessionQueue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusSessionReceiverClient sr = sb.sessionReceiver().queueName(q).disableAutoComplete().buildClient()) {
            s.sendMessage(new ServiceBusMessage("one").setSessionId("held"));
            s.sendMessage(new ServiceBusMessage("two").setSessionId("free"));
            try (ServiceBusReceiverClient held = sr.acceptSession("held");
                 ServiceBusReceiverClient next = sr.acceptNextSession()) {
                assertEquals("free", next.getSessionId());
            }
        }
    }

    @Test void acceptSessionThatAnotherReceiverHolds_failsWithSessionCannotBeLockedCondition() {
        String q = sessionQueue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusSessionReceiverClient sr1 = sb.sessionReceiver().queueName(q).buildClient();
             ServiceBusSessionReceiverClient sr2 = Sb.client(emu, ns).sessionReceiver().queueName(q).buildClient()) {
            s.sendMessage(new ServiceBusMessage("one").setSessionId("busy"));
            try (ServiceBusReceiverClient first = sr1.acceptSession("busy")) {
                com.azure.core.amqp.exception.AmqpException e = assertThrows(com.azure.core.amqp.exception.AmqpException.class,
                    () -> sr2.acceptSession("busy"));
                assertEquals(com.azure.core.amqp.exception.AmqpErrorCondition.SESSION_CANNOT_BE_LOCKED, e.getErrorCondition());
            }
        }
    }

    @Test void renewSessionLock_returnsALaterExpiry() throws Exception {
        String q = sessionQueue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusSessionReceiverClient sr = sb.sessionReceiver().queueName(q).buildClient()) {
            s.sendMessage(new ServiceBusMessage("one").setSessionId("renew"));
            try (ServiceBusReceiverClient r = sr.acceptSession("renew")) {
                OffsetDateTime first = r.renewSessionLock();
                Thread.sleep(1500);
                OffsetDateTime second = r.renewSessionLock();
                assertTrue(second.isAfter(first), first + " -> " + second);
            }
        }
    }

    @Test void listSessions_showsSessionsThatHoldMessages() {
        String q = sessionQueue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusSessionReceiverClient sr = sb.sessionReceiver().queueName(q).buildClient()) {
            s.sendMessage(new ServiceBusMessage("one").setSessionId("ls-1"));
            s.sendMessage(new ServiceBusMessage("two").setSessionId("ls-2"));
            Set<String> ids = new TreeSet<>();
            sr.listSessions().forEach(ids::add);
            assertEquals(new TreeSet<>(List.of("ls-1", "ls-2")), ids);
        }
    }

    @Test void deadLetterInASession_andPeekOfSessionMessages() {
        String q = sessionQueue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusSessionReceiverClient sr = sb.sessionReceiver().queueName(q).disableAutoComplete().buildClient()) {
            s.sendMessage(new ServiceBusMessage("bad").setSessionId("dl"));
            try (ServiceBusReceiverClient r = sr.acceptSession("dl")) {
                ServiceBusReceivedMessage peeked = r.peekMessage();
                assertEquals("bad", peeked.getBody().toString());
                ServiceBusReceivedMessage m = Sb.receiveOne(r);
                r.deadLetter(m, new DeadLetterOptions().setDeadLetterReason("r"));
            }
            try (ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
                // the dead letter queue of a session entity is read without a session
                ServiceBusReceivedMessage d = Sb.receiveOne(dlq);
                assertEquals("dl", d.getSessionId());
                assertEquals("r", d.getDeadLetterReason());
                dlq.complete(d);
            }
        }
    }

    @Test void acceptNextSession_whenNoSessionHasMessages_endsWithATimeoutCondition() {
        String q = sessionQueue();
        ServiceBusClientBuilder b = new ServiceBusClientBuilder().connectionString(emu.sbConnStr(ns))
            .retryOptions(new com.azure.core.amqp.AmqpRetryOptions().setMaxRetries(0).setTryTimeout(Duration.ofSeconds(4)));
        try (ServiceBusSessionReceiverClient sr = b.sessionReceiver().queueName(q).buildClient()) {
            long t0 = System.currentTimeMillis();
            // the sync client wraps the timeout in an IllegalStateException
            RuntimeException e = assertThrows(RuntimeException.class, sr::acceptNextSession);
            Throwable c = e;
            while (c != null && !(c instanceof com.azure.core.amqp.exception.AmqpException)) c = c.getCause();
            assertNotNull(c, "an AmqpException is in the cause chain: " + e);
            assertEquals(com.azure.core.amqp.exception.AmqpErrorCondition.TIMEOUT_ERROR,
                ((com.azure.core.amqp.exception.AmqpException) c).getErrorCondition());
            assertTrue(System.currentTimeMillis() - t0 < 30_000);
        }
    }

    // ---- topics and subscriptions ------------------------------------------------------------------------------
    @Test void topic_copiesEachMessageToEverySubscription() {
        String t = admin.createTopic(Emu.dashed("t")).getName();
        admin.createSubscription(t, "s1");
        admin.createSubscription(t, "s2");
        try (ServiceBusSenderClient s = sb.sender().topicName(t).buildClient();
             ServiceBusReceiverClient r1 = Sb.peekLockSub(sb, t, "s1");
             ServiceBusReceiverClient r2 = Sb.peekLockSub(sb, t, "s2")) {
            s.sendMessage(new ServiceBusMessage("fan"));
            ServiceBusReceivedMessage a = Sb.receiveOne(r1);
            ServiceBusReceivedMessage b = Sb.receiveOne(r2);
            assertEquals("fan", a.getBody().toString());
            assertEquals("fan", b.getBody().toString());
            r1.complete(a);
            r2.complete(b);
        }
    }

    @Test void subscriptionCreatedWithASqlFilterAsDefaultRule_receivesOnlyMatchingMessages() {
        String t = admin.createTopic(Emu.dashed("t")).getName();
        admin.createSubscription(t, "red", "isred", new CreateSubscriptionOptions(),
            new CreateRuleOptions(new SqlRuleFilter("color = 'red'")));
        admin.createSubscription(t, "all");
        try (ServiceBusSenderClient s = sb.sender().topicName(t).buildClient();
             ServiceBusReceiverClient red = Sb.peekLockSub(sb, t, "red");
             ServiceBusReceiverClient all = Sb.peekLockSub(sb, t, "all")) {
            ServiceBusMessage m1 = new ServiceBusMessage("m-red");
            m1.getApplicationProperties().put("color", "red");
            ServiceBusMessage m2 = new ServiceBusMessage("m-blue");
            m2.getApplicationProperties().put("color", "blue");
            s.sendMessages(List.of(m1, m2));
            assertEquals(List.of("m-red"), bodies(Sb.receive(red, 5, 4)));
            assertEquals(List.of("m-red", "m-blue"), bodies(Sb.receive(all, 5, 4)));
            List<RuleProperties> rules = new ArrayList<>();
            admin.listRules(t, "red").forEach(rules::add);
            assertEquals(1, rules.size());
            assertEquals("isred", rules.get(0).getName());
            assertEquals("color = 'red'", ((SqlRuleFilter) rules.get(0).getFilter()).getSqlExpression());
        }
    }

    @Test void ruleManager_createsListsAndDeletesRulesOverAmqp() {
        String t = admin.createTopic(Emu.dashed("t")).getName();
        admin.createSubscription(t, "sub");
        try (ServiceBusRuleManagerClient rm = sb.ruleManager().topicName(t).subscriptionName("sub").buildClient()) {
            List<RuleProperties> initial = rm.listRules().stream().collect(Collectors.toList());
            assertEquals(1, initial.size());
            assertEquals("$Default", initial.get(0).getName());
            assertTrue(initial.get(0).getFilter() instanceof TrueRuleFilter);

            rm.deleteRule("$Default");
            rm.createRule("sqlrule", new CreateRuleOptions(new SqlRuleFilter("n > 5"))
                .setAction(new SqlRuleAction("SET marked = 'yes'")));
            rm.createRule("corr", new CreateRuleOptions(new CorrelationRuleFilter().setCorrelationId("c-1").setLabel("subj")));

            Map<String, RuleProperties> byName = rm.listRules().stream().collect(Collectors.toMap(RuleProperties::getName, r -> r));
            assertEquals(Set.of("sqlrule", "corr"), byName.keySet());
            assertEquals("n > 5", ((SqlRuleFilter) byName.get("sqlrule").getFilter()).getSqlExpression());
            assertEquals("SET marked = 'yes'", ((SqlRuleAction) byName.get("sqlrule").getAction()).getSqlExpression());
            CorrelationRuleFilter cf = (CorrelationRuleFilter) byName.get("corr").getFilter();
            assertEquals("c-1", cf.getCorrelationId());
            assertEquals("subj", cf.getLabel());

            rm.deleteRule("corr");
            assertEquals(List.of("sqlrule"), rm.listRules().stream().map(RuleProperties::getName).collect(Collectors.toList()));
        }
    }

    @Test void ruleManager_createRuleWithAnExistingName_failsWithEntityAlreadyExists() {
        String t = admin.createTopic(Emu.dashed("t")).getName();
        admin.createSubscription(t, "sub");
        try (ServiceBusRuleManagerClient rm = sb.ruleManager().topicName(t).subscriptionName("sub").buildClient()) {
            rm.createRule("dup", new CreateRuleOptions(new TrueRuleFilter()));
            ServiceBusException e = assertThrows(ServiceBusException.class,
                () -> rm.createRule("dup", new CreateRuleOptions(new TrueRuleFilter())));
            assertEquals(ServiceBusFailureReason.MESSAGING_ENTITY_ALREADY_EXISTS, e.getReason());
        }
    }

    @Test void rulesAddedOverAmqp_decideWhichMessagesTheSubscriptionGets_andActionsRewriteProperties() {
        String t = admin.createTopic(Emu.dashed("t")).getName();
        admin.createSubscription(t, "sub");
        try (ServiceBusRuleManagerClient rm = sb.ruleManager().topicName(t).subscriptionName("sub").buildClient();
             ServiceBusSenderClient s = sb.sender().topicName(t).buildClient();
             ServiceBusReceiverClient r = Sb.peekLockSub(sb, t, "sub")) {
            rm.deleteRule("$Default");
            rm.createRule("big", new CreateRuleOptions(new SqlRuleFilter("n >= 10"))
                .setAction(new SqlRuleAction("SET size = 'big'")));
            ServiceBusMessage small = new ServiceBusMessage("small");
            small.getApplicationProperties().put("n", 3);
            ServiceBusMessage big = new ServiceBusMessage("big");
            big.getApplicationProperties().put("n", 30);
            s.sendMessages(List.of(small, big));
            List<ServiceBusReceivedMessage> got = Sb.receive(r, 5, 4);
            assertEquals(List.of("big"), bodies(got));
            assertEquals("big", got.get(0).getApplicationProperties().get("size"));
            r.complete(got.get(0));
        }
    }

    @Test void correlationFilter_matchesOnSystemAndCustomProperties() {
        String t = admin.createTopic(Emu.dashed("t")).getName();
        admin.createSubscription(t, "sub", "corr", new CreateSubscriptionOptions(),
            new CreateRuleOptions(new CorrelationRuleFilter().setCorrelationId("order-7").setTo("shop")));
        try (ServiceBusSenderClient s = sb.sender().topicName(t).buildClient();
             ServiceBusReceiverClient r = Sb.peekLockSub(sb, t, "sub")) {
            s.sendMessages(List.of(
                new ServiceBusMessage("match").setCorrelationId("order-7").setTo("shop"),
                new ServiceBusMessage("other-id").setCorrelationId("order-8").setTo("shop"),
                new ServiceBusMessage("other-to").setCorrelationId("order-7").setTo("elsewhere")));
            assertEquals(List.of("match"), bodies(Sb.receive(r, 5, 4)));
        }
    }

    @Test void subscriptionDeadLetterQueue_holdsDeadLetteredMessages() {
        String t = admin.createTopic(Emu.dashed("t")).getName();
        admin.createSubscription(t, "sub", new CreateSubscriptionOptions().setMaxDeliveryCount(2));
        try (ServiceBusSenderClient s = sb.sender().topicName(t).buildClient();
             ServiceBusReceiverClient r = Sb.peekLockSub(sb, t, "sub");
             ServiceBusReceiverClient dlq = sb.receiver().topicName(t).subscriptionName("sub")
                 .subQueue(SubQueue.DEAD_LETTER_QUEUE).disableAutoComplete().buildClient()) {
            s.sendMessage(new ServiceBusMessage("poison"));
            r.abandon(Sb.receiveOne(r));
            r.abandon(Sb.receiveOne(r));
            ServiceBusReceivedMessage d = Sb.receiveOne(dlq);
            assertEquals("poison", d.getBody().toString());
            assertEquals("MaxDeliveryCountExceeded", d.getDeadLetterReason());
            dlq.complete(d);
        }
    }

    @Test void sessionEnabledSubscription_deliversPerSession() {
        String t = admin.createTopic(Emu.dashed("t")).getName();
        admin.createSubscription(t, "sub", new CreateSubscriptionOptions().setSessionRequired(true));
        try (ServiceBusSenderClient s = sb.sender().topicName(t).buildClient();
             ServiceBusSessionReceiverClient sr = sb.sessionReceiver().topicName(t).subscriptionName("sub").buildClient()) {
            s.sendMessage(new ServiceBusMessage("x1").setSessionId("S"));
            s.sendMessage(new ServiceBusMessage("y1").setSessionId("T"));
            try (ServiceBusReceiverClient r = sr.acceptSession("T")) {
                assertEquals(List.of("y1"), bodies(Sb.receive(r, 3, 3)));
            }
        }
    }

    // ---- administration client --------------------------------------------------------------------------------
    @Test void adminQueue_createGetUpdateDelete_roundTripsEveryProperty() {
        String name = Emu.dashed("aq");
        QueueProperties created = admin.createQueue(name, new CreateQueueOptions()
            .setLockDuration(Duration.ofSeconds(45)).setMaxDeliveryCount(7).setDefaultMessageTimeToLive(Duration.ofHours(2))
            .setDeadLetteringOnMessageExpiration(true).setMaxSizeInMegabytes(2048).setUserMetadata("meta")
            .setDuplicateDetectionRequired(true).setDuplicateDetectionHistoryTimeWindow(Duration.ofMinutes(3)));
        assertEquals(name, created.getName());
        QueueProperties got = admin.getQueue(name);
        assertEquals(Duration.ofSeconds(45), got.getLockDuration());
        assertEquals(7, got.getMaxDeliveryCount());
        assertEquals(Duration.ofHours(2), got.getDefaultMessageTimeToLive());
        assertTrue(got.isDeadLetteringOnMessageExpiration());
        assertEquals(2048, got.getMaxSizeInMegabytes());
        assertEquals("meta", got.getUserMetadata());
        assertTrue(got.isDuplicateDetectionRequired());
        assertEquals(Duration.ofMinutes(3), got.getDuplicateDetectionHistoryTimeWindow());
        assertEquals(EntityStatus.ACTIVE, got.getStatus());

        got.setMaxDeliveryCount(9).setLockDuration(Duration.ofSeconds(60));
        QueueProperties updated = admin.updateQueue(got);
        assertEquals(9, updated.getMaxDeliveryCount());
        assertEquals(Duration.ofSeconds(60), admin.getQueue(name).getLockDuration());

        assertTrue(admin.getQueueExists(name));
        admin.deleteQueue(name);
        assertFalse(admin.getQueueExists(name));
    }

    @Test void adminQueue_createTwice_isResourceExists() {
        String name = Emu.dashed("aq");
        admin.createQueue(name);
        assertThrows(ResourceExistsException.class, () -> admin.createQueue(name));
    }

    @Test void adminQueue_getMissing_isResourceNotFound() {
        assertThrows(ResourceNotFoundException.class, () -> admin.getQueue(Emu.dashed("missing")));
        assertThrows(ResourceNotFoundException.class, () -> admin.deleteQueue(Emu.dashed("missing")));
    }

    @Test void adminQueue_lockDurationOverFiveMinutes_isRefused() {
        com.azure.core.exception.HttpResponseException e = assertThrows(com.azure.core.exception.HttpResponseException.class,
            () -> admin.createQueue(Emu.dashed("aq"), new CreateQueueOptions().setLockDuration(Duration.ofMinutes(10))));
        assertEquals(400, e.getResponse().getStatusCode());
    }

    @Test void adminListQueues_pagesThroughAllOfThem() {
        String prefix = Emu.dashed("lq");
        for (int i = 0; i < 7; i++) admin.createQueue(prefix + "-" + i);
        Set<String> names = new TreeSet<>();
        admin.listQueues().forEach(q -> { if (q.getName().startsWith(prefix)) names.add(q.getName()); });
        assertEquals(7, names.size());
    }

    @Test void adminQueueRuntimeProperties_countActiveScheduledAndDeadLetteredMessages() {
        String q = admin.createQueue(Emu.dashed("rt")).getName();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessages(List.of(new ServiceBusMessage("1"), new ServiceBusMessage("2"), new ServiceBusMessage("3")));
            s.scheduleMessage(new ServiceBusMessage("later"), OffsetDateTime.now().plusMinutes(10));
            r.deadLetter(Sb.receiveOne(r));
            QueueRuntimeProperties rt = admin.getQueueRuntimeProperties(q);
            assertEquals(2, rt.getActiveMessageCount());
            assertEquals(1, rt.getDeadLetterMessageCount());
            assertEquals(1, rt.getScheduledMessageCount());
            assertEquals(4, rt.getTotalMessageCount());
            assertTrue(rt.getSizeInBytes() > 0);
        }
    }

    @Test void adminTopicSubscriptionRule_lifecycle() {
        String t = Emu.dashed("at");
        TopicProperties tp = admin.createTopic(t, new CreateTopicOptions().setDefaultMessageTimeToLive(Duration.ofDays(1)));
        assertEquals(t, tp.getName());
        assertEquals(Duration.ofDays(1), admin.getTopic(t).getDefaultMessageTimeToLive());

        SubscriptionProperties sp = admin.createSubscription(t, "sub1", new CreateSubscriptionOptions().setMaxDeliveryCount(4));
        assertEquals("sub1", sp.getSubscriptionName());
        assertEquals(t, sp.getTopicName());
        assertEquals(4, admin.getSubscription(t, "sub1").getMaxDeliveryCount());
        admin.createSubscription(t, "sub2");
        List<String> subs = new ArrayList<>();
        admin.listSubscriptions(t).forEach(x -> subs.add(x.getSubscriptionName()));
        assertEquals(List.of("sub1", "sub2"), subs.stream().sorted().collect(Collectors.toList()));

        RuleProperties rule = admin.createRule(t, "r1", "sub1", new CreateRuleOptions(new SqlRuleFilter("a = 1")))  /* the SDK takes (topic, rule, subscription) */;
        assertEquals("r1", rule.getName());
        rule.setFilter(new SqlRuleFilter("a = 2"));
        admin.updateRule(t, "sub1", rule);
        assertEquals("a = 2", ((SqlRuleFilter) admin.getRule(t, "sub1", "r1").getFilter()).getSqlExpression());
        admin.deleteRule(t, "sub1", "r1");
        assertThrows(ResourceNotFoundException.class, () -> admin.getRule(t, "sub1", "r1"));

        assertEquals(0, admin.getTopicRuntimeProperties(t).getScheduledMessageCount());
        assertEquals(2, admin.getTopicRuntimeProperties(t).getSubscriptionCount());
        assertEquals(0, admin.getSubscriptionRuntimeProperties(t, "sub1").getActiveMessageCount());
        admin.deleteSubscription(t, "sub2");
        assertFalse(admin.getSubscriptionExists(t, "sub2"));
        admin.deleteTopic(t);
        assertFalse(admin.getTopicExists(t));
    }

    @Test void adminNamespaceProperties_areReadable() {
        NamespaceProperties np = admin.getNamespaceProperties();
        assertNotNull(np.getName());
        assertNotNull(np.getCreatedTime());
    }
}
