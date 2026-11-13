package azemu;

import com.azure.core.amqp.models.AmqpMessageBody;
import com.azure.core.amqp.models.AmqpMessageBodyType;
import com.azure.core.util.BinaryData;
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

/** Service Bus queues over AMQP (qpid-proton-j) and the administration client over HTTPS. One emulator for the class. */
@EmuTest
class ServiceBusTest {
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

    String queue() { return admin.createQueue(Emu.dashed("q")).getName(); }

    String queue(CreateQueueOptions o) { return admin.createQueue(Emu.dashed("q"), o).getName(); }

    // ---- send and receive -------------------------------------------------------------------------------------
    @Test void sendOneMessage_receiveItInPeekLock_carriesEveryPropertyTheSenderSet() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            ServiceBusMessage m = new ServiceBusMessage("hello java")
                .setMessageId("mid-1").setSubject("subj").setContentType("text/plain").setCorrelationId("corr-1")
                .setTo("somewhere").setReplyTo("back").setTimeToLive(Duration.ofMinutes(5));
            m.getApplicationProperties().put("s", "text");
            m.getApplicationProperties().put("i", 42);
            m.getApplicationProperties().put("l", 1234567890123L);
            m.getApplicationProperties().put("d", 2.5d);
            m.getApplicationProperties().put("b", true);
            s.sendMessage(m);

            ServiceBusReceivedMessage got = Sb.receiveOne(r);
            assertEquals("hello java", got.getBody().toString());
            assertEquals("mid-1", got.getMessageId());
            assertEquals("subj", got.getSubject());
            assertEquals("text/plain", got.getContentType());
            assertEquals("corr-1", got.getCorrelationId());
            assertEquals("somewhere", got.getTo());
            assertEquals("back", got.getReplyTo());
            assertEquals(Duration.ofMinutes(5), got.getTimeToLive());
            assertEquals("text", got.getApplicationProperties().get("s"));
            assertEquals(42, got.getApplicationProperties().get("i"));
            assertEquals(1234567890123L, got.getApplicationProperties().get("l"));
            assertEquals(2.5d, got.getApplicationProperties().get("d"));
            assertEquals(true, got.getApplicationProperties().get("b"));
            assertEquals(0L, got.getDeliveryCount(), "the first delivery has delivery count 0 (AMQP header)");
            assertTrue(got.getSequenceNumber() > 0);
            assertNotNull(got.getEnqueuedTime());
            assertNotNull(got.getLockToken());
            assertNotNull(got.getLockedUntil());
            assertNotNull(got.getExpiresAt());
            assertEquals(ServiceBusMessageState.ACTIVE, got.getState());
            r.complete(got);
            assertTrue(Sb.receive(r, 1, 2).isEmpty());
        }
    }

    @Test void messageSentWithoutMessageId_canStillBeReceivedAndCompleted() {
        // Azure does not invent a message id; the Java SDK also leaves it unset. Either way the message must be completable.
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("no id"));
            ServiceBusReceivedMessage got = Sb.receiveOne(r);
            r.complete(got);
            assertTrue(Sb.receive(r, 1, 2).isEmpty());
        }
    }

    @Test void binaryBody_andAmqpValueBody_roundTrip() {
        String q = queue();
        byte[] data = Util.bytes(5000);
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage(data));
            s.sendMessage(new ServiceBusMessage(AmqpMessageBody.fromValue("a value body")));
            List<ServiceBusReceivedMessage> got = Sb.receive(r, 2, 10);
            assertEquals(2, got.size());
            assertEquals(AmqpMessageBodyType.DATA, got.get(0).getRawAmqpMessage().getBody().getBodyType());
            assertArrayEquals(data, got.get(0).getBody().toBytes());
            assertEquals(AmqpMessageBodyType.VALUE, got.get(1).getRawAmqpMessage().getBody().getBodyType());
            assertEquals("a value body", got.get(1).getRawAmqpMessage().getBody().getValue());
            got.forEach(r::complete);
        }
    }

    @Test void sendBatch_ofManyMessages_arrivesInOrder() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            ServiceBusMessageBatch batch = s.createMessageBatch();
            assertTrue(batch.getMaxSizeInBytes() > 0);
            int n = 0;
            while (n < 50 && batch.tryAddMessage(new ServiceBusMessage("m" + n))) n++;
            assertEquals(50, n, "all 50 small messages fit one batch");
            assertEquals(50, batch.getCount());
            s.sendMessages(batch);
            List<ServiceBusReceivedMessage> got = Sb.receive(r, 50, 15);
            assertEquals(50, got.size());
            for (int i = 0; i < 50; i++) assertEquals("m" + i, got.get(i).getBody().toString());
            got.forEach(r::complete);
        }
    }

    @Test void batchMaxSize_matchesTheQueueMaxMessageSize_andAMessageThatDoesNotFitIsNotAdded() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            ServiceBusMessageBatch batch = s.createMessageBatch();
            assertEquals(262144, batch.getMaxSizeInBytes(), "Standard tier: 256 KB");
            assertTrue(batch.tryAddMessage(new ServiceBusMessage(new byte[200_000])));
            assertFalse(batch.tryAddMessage(new ServiceBusMessage(new byte[200_000])), "second one does not fit");
            assertEquals(1, batch.getCount());
            ServiceBusException e = assertThrows(ServiceBusException.class,
                () -> s.createMessageBatch().tryAddMessage(new ServiceBusMessage(new byte[300_000])));
            assertEquals(ServiceBusFailureReason.MESSAGE_SIZE_EXCEEDED, e.getReason(), "a message too big for an empty batch throws");
        }
    }

    @Test void messageLargerThanTheLimit_isRefusedWithMessageSizeExceeded() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            ServiceBusException e = assertThrows(ServiceBusException.class,
                () -> s.sendMessage(new ServiceBusMessage(new byte[300_000])));
            assertEquals(ServiceBusFailureReason.MESSAGE_SIZE_EXCEEDED, e.getReason());
        }
    }

    @Test void largeMessageJustUnderTheLimit_roundTripsAcrossManyFrames() {
        String q = queue();
        byte[] data = Util.bytes(250_000);
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage(data));
            ServiceBusReceivedMessage got = Sb.receiveOne(r);
            assertArrayEquals(data, got.getBody().toBytes());
            r.complete(got);
        }
    }

    // ---- settlement -------------------------------------------------------------------------------------------
    @Test void abandon_returnsTheMessageWithDeliveryCountOne() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("abandon me"));
            ServiceBusReceivedMessage first = Sb.receiveOne(r);
            assertEquals(0, first.getDeliveryCount());
            r.abandon(first);
            ServiceBusReceivedMessage second = Sb.receiveOne(r);
            assertEquals(first.getMessageId(), second.getMessageId());
            assertEquals(1, second.getDeliveryCount(), "abandon increments the delivery count");
            r.complete(second);
        }
    }

    @Test void abandonWithPropertiesToModify_writesThemToTheApplicationProperties() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("props"));
            ServiceBusReceivedMessage first = Sb.receiveOne(r);
            r.abandon(first, new AbandonOptions().setPropertiesToModify(Map.of("added", "yes", "n", 7)));
            ServiceBusReceivedMessage second = Sb.receiveOne(r);
            assertEquals("yes", second.getApplicationProperties().get("added"));
            assertEquals(7, second.getApplicationProperties().get("n"));
            r.complete(second);
        }
    }

    @Test void defer_thenReceiveDeferredBySequenceNumber() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("later"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            long seq = m.getSequenceNumber();
            r.defer(m);
            assertTrue(Sb.receive(r, 1, 2).isEmpty(), "a deferred message is not delivered again");
            ServiceBusReceivedMessage d = r.receiveDeferredMessage(seq);
            assertNotNull(d);
            assertEquals("later", d.getBody().toString());
            assertEquals(ServiceBusMessageState.DEFERRED, d.getState());
            r.complete(d);
        }
    }

    @Test void deadLetter_withReasonDescriptionAndProperties_arrivesInTheDeadLetterQueue() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q);
             ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
            s.sendMessage(new ServiceBusMessage("poison").setMessageId("dl-1"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            r.deadLetter(m, new DeadLetterOptions().setDeadLetterReason("bad").setDeadLetterErrorDescription("it was bad"));
            assertTrue(Sb.receive(r, 1, 2).isEmpty());
            ServiceBusReceivedMessage d = Sb.receiveOne(dlq);
            assertEquals("dl-1", d.getMessageId());
            assertEquals("bad", d.getDeadLetterReason());
            assertEquals("it was bad", d.getDeadLetterErrorDescription());
            assertNull(d.getDeadLetterSource(), "only set when a dead-lettered message was auto-forwarded");
            dlq.complete(d);
        }
    }

    @Test void deadLetterWithPropertiesToModify_arrivesAsApplicationPropertiesOfTheDeadLetteredMessage() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q);
             ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
            s.sendMessage(new ServiceBusMessage("poison"));
            r.deadLetter(Sb.receiveOne(r), new DeadLetterOptions().setDeadLetterReason("bad").setPropertiesToModify(Map.of("extra", "value")));
            ServiceBusReceivedMessage d = Sb.receiveOne(dlq);
            assertEquals("bad", d.getDeadLetterReason());
            assertEquals("value", d.getApplicationProperties().get("extra"));
            dlq.complete(d);
        }
    }

    @Test void deadLetterQueueReceiverThatIsAlreadyWaiting_getsAMessageDeadLetteredMeanwhile() throws Exception {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q);
             ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
            s.sendMessage(new ServiceBusMessage("later dead"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            Thread t = new Thread(() -> {
                try { Thread.sleep(2000); } catch (InterruptedException e) { return; }
                r.deadLetter(m, new DeadLetterOptions().setDeadLetterReason("later"));
            });
            t.start();
            List<ServiceBusReceivedMessage> got = Sb.receive(dlq, 1, 8);   // waits while the message is dead-lettered
            t.join();
            assertEquals(1, got.size(), "the waiting receiver gets the message dead-lettered after it started to wait");
            assertEquals("later", got.get(0).getDeadLetterReason());
        }
    }

    @Test void deadLetterQueueReceiverThatIsAlreadyWaiting_getsAMessageThatReachedMaxDeliveryCount() throws Exception {
        String q = queue(new CreateQueueOptions().setMaxDeliveryCount(2));
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q);
             ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
            s.sendMessage(new ServiceBusMessage("poison"));
            Thread t = new Thread(() -> {
                try {
                    Thread.sleep(1500);
                    r.abandon(Sb.receiveOne(r));
                    r.abandon(Sb.receiveOne(r));
                } catch (InterruptedException e) { /* test over */ }
            });
            t.start();
            List<ServiceBusReceivedMessage> got = Sb.receive(dlq, 1, 15);   // already waiting when the broker dead-letters the message
            t.join();
            assertEquals(1, got.size(), "the waiting receiver gets the message the broker dead-lettered");
            assertEquals("MaxDeliveryCountExceeded", got.get(0).getDeadLetterReason());
        }
    }

    @Test void abandonUntilMaxDeliveryCount_movesTheMessageToTheDeadLetterQueue() {
        String q = queue(new CreateQueueOptions().setMaxDeliveryCount(3));
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q);
             ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
            s.sendMessage(new ServiceBusMessage("loop").setMessageId("loop-1"));
            for (int i = 0; i < 3; i++) {
                ServiceBusReceivedMessage m = Sb.receiveOne(r);
                assertEquals(i, m.getDeliveryCount());
                r.abandon(m);
            }
            assertTrue(Sb.receive(r, 1, 2).isEmpty(), "after MaxDeliveryCount the message leaves the queue");
            ServiceBusReceivedMessage d = Sb.receiveOne(dlq);
            assertEquals("loop-1", d.getMessageId());
            assertEquals("MaxDeliveryCountExceeded", d.getDeadLetterReason());
            dlq.complete(d);
        }
    }

    @Test void receiveAndDelete_removesTheMessageAtOnce() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.receiveAndDelete(sb, q);
             ServiceBusReceiverClient peek = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("gone"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertEquals("gone", m.getBody().toString());
            assertNull(peek.peekMessage());
        }
    }

    @Test void completeAfterLockExpired_failsWithMessageLockLost() throws Exception {
        String q = queue(new CreateQueueOptions().setLockDuration(Duration.ofSeconds(5)));
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLockNoRenew(sb, q)) {
            s.sendMessage(new ServiceBusMessage("slow"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            Thread.sleep(6500);
            ServiceBusException e = assertThrows(ServiceBusException.class, () -> r.complete(m));
            assertEquals(ServiceBusFailureReason.MESSAGE_LOCK_LOST, e.getReason());
            ServiceBusReceivedMessage again = Sb.receiveOne(r);
            assertEquals(1, again.getDeliveryCount());
            r.complete(again);
        }
    }

    @Test void renewMessageLock_extendsLockedUntil() throws Exception {
        String q = queue(new CreateQueueOptions().setLockDuration(Duration.ofSeconds(10)));
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("renew"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            OffsetDateTime before = m.getLockedUntil();
            Thread.sleep(3000);
            OffsetDateTime after = r.renewMessageLock(m);
            assertTrue(after.isAfter(before.plusSeconds(1)), "renew moves the lock later: " + before + " -> " + after);
            r.complete(m);
        }
    }

    // ---- peek, schedule ---------------------------------------------------------------------------------------
    @Test void peekMessages_doesNotLock_andPeekFromSequenceNumberSkipsEarlierOnes() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessages(List.of(new ServiceBusMessage("a"), new ServiceBusMessage("b"), new ServiceBusMessage("c")));
            List<ServiceBusReceivedMessage> peeked = new ArrayList<>();
            r.peekMessages(10, 1).forEach(peeked::add);
            assertEquals(List.of("a", "b", "c"), peeked.stream().map(x -> x.getBody().toString()).collect(Collectors.toList()));
            long second = peeked.get(1).getSequenceNumber();
            List<ServiceBusReceivedMessage> from = new ArrayList<>();
            r.peekMessages(10, second).forEach(from::add);
            assertEquals(List.of("b", "c"), from.stream().map(x -> x.getBody().toString()).collect(Collectors.toList()));
            assertEquals(3, Sb.receive(r, 3, 10).size(), "peeked messages are still receivable");
        }
    }

    @Test void scheduleMessage_isInvisibleUntilDue_andCancelRemovesIt() throws Exception {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            long keep = s.scheduleMessage(new ServiceBusMessage("due"), OffsetDateTime.now().plusSeconds(4));
            long cancel = s.scheduleMessage(new ServiceBusMessage("cancelled"), OffsetDateTime.now().plusSeconds(4));
            assertTrue(keep > 0 && cancel > 0 && keep != cancel);
            s.cancelScheduledMessage(cancel);
            assertTrue(Sb.receive(r, 1, 1).isEmpty(), "not yet due");
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertEquals("due", m.getBody().toString());
            assertEquals(ServiceBusMessageState.ACTIVE, m.getState());
            r.complete(m);
            assertTrue(Sb.receive(r, 1, 6).isEmpty(), "the cancelled message never arrives");
        }
    }

    @Test void messageWithScheduledEnqueueTime_isDeliveredLater() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("later").setScheduledEnqueueTime(OffsetDateTime.now().plusSeconds(3)));
            assertTrue(Sb.receive(r, 1, 1).isEmpty());
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertEquals("later", m.getBody().toString());
            r.complete(m);
        }
    }

    @Test void messageTimeToLive_expiresAndDeadLettersWhenTheQueueAsksFor_it() throws Exception {
        String q = queue(new CreateQueueOptions().setDeadLetteringOnMessageExpiration(true));
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q);
             ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
            s.sendMessage(new ServiceBusMessage("short").setTimeToLive(Duration.ofSeconds(2)));
            Thread.sleep(3500);
            assertTrue(Sb.receive(r, 1, 2).isEmpty());
            ServiceBusReceivedMessage d = Sb.receiveOne(dlq);
            assertEquals("TTLExpiredException", d.getDeadLetterReason());
            dlq.complete(d);
        }
    }

    @Test void duplicateDetection_dropsTheSecondMessageWithTheSameId() {
        String q = queue(new CreateQueueOptions().setDuplicateDetectionRequired(true)
            .setDuplicateDetectionHistoryTimeWindow(Duration.ofMinutes(5)));
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("one").setMessageId("same"));
            s.sendMessage(new ServiceBusMessage("two").setMessageId("same"));
            s.sendMessage(new ServiceBusMessage("three").setMessageId("other"));
            List<ServiceBusReceivedMessage> got = Sb.receive(r, 5, 5);
            assertEquals(List.of("one", "three"), got.stream().map(x -> x.getBody().toString()).collect(Collectors.toList()));
            got.forEach(r::complete);
        }
    }

    @Test void queueForwardTo_deliversToTheTargetQueue() {
        String target = queue();
        String source = queue(new CreateQueueOptions().setForwardTo(target));
        try (ServiceBusSenderClient s = sb.sender().queueName(source).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, target)) {
            s.sendMessage(new ServiceBusMessage("forwarded"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertEquals("forwarded", m.getBody().toString());
            r.complete(m);
        }
    }

    // ---- errors -----------------------------------------------------------------------------------------------
    @Test void settlingTheSameMessageTwice_isRefusedByTheSdk() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("twice"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            r.complete(m);
            IllegalArgumentException e = assertThrows(IllegalArgumentException.class, () -> r.complete(m));
            assertTrue(e.getMessage().toLowerCase().contains("settled"), e.getMessage());
        }
    }

    @Test void receiveDeferredMessageWithUnknownSequenceNumber_fails() {
        String q = queue();
        try (ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            // Azure refuses it; the reason the SDK derives depends on the status and condition of the answer
            assertThrows(ServiceBusException.class, () -> r.receiveDeferredMessage(987654321L));
        }
    }
}
