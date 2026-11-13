package azemu;

import com.azure.messaging.servicebus.*;
import com.azure.messaging.servicebus.administration.ServiceBusAdministrationClient;
import com.azure.messaging.servicebus.models.*;
import org.junit.jupiter.api.*;

import java.util.*;

import static org.junit.jupiter.api.Assertions.*;

/**
 * Service Bus transactions. The Java SDK is the only SDK that has them (it attaches a coordinator link,
 * sends a declare, then discharge(fail = false) for commit and discharge(fail = true) for rollback).
 */
@EmuTest
class ServiceBusTransactionTest {
    static Emu emu;
    static String ns;
    static ServiceBusClientBuilder sb;
    static ServiceBusAdministrationClient admin;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        ns = Emu.name("sbns");
        sb = Sb.client(emu, ns).enableCrossEntityTransactions();
        admin = Sb.admin(emu, ns);
    }

    String queue() { return admin.createQueue(Emu.dashed("tq")).getName(); }

    @Test void createTransaction_returnsAContextWithAnId() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            ServiceBusTransactionContext tx = s.createTransaction();
            assertNotNull(tx);
            assertNotNull(tx.getTransactionId());
            assertTrue(tx.getTransactionId().remaining() > 0);
            s.rollbackTransaction(tx);
        }
    }

    @Test void sendInATransaction_isInvisibleUntilCommit_thenArrives() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            ServiceBusTransactionContext tx = s.createTransaction();
            s.sendMessage(new ServiceBusMessage("in tx"), tx);
            assertTrue(Sb.receive(r, 1, 2).isEmpty(), "not visible before commit");
            s.commitTransaction(tx);
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertEquals("in tx", m.getBody().toString());
            r.complete(m);
        }
    }

    @Test void sendInATransaction_thenRollback_neverArrives() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            ServiceBusTransactionContext tx = s.createTransaction();
            s.sendMessage(new ServiceBusMessage("rolled back"), tx);
            s.sendMessages(List.of(new ServiceBusMessage("also rolled back")), tx);
            s.rollbackTransaction(tx);
            assertTrue(Sb.receive(r, 1, 3).isEmpty());
        }
    }

    @Test void completeInATransaction_thenCommit_removesTheMessage() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("work"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            ServiceBusTransactionContext tx = r.createTransaction();
            r.complete(m, new CompleteOptions().setTransactionContext(tx));
            r.commitTransaction(tx);
            assertTrue(Sb.receive(r, 1, 2).isEmpty());
        }
    }

    @Test void completeInATransaction_thenRollback_leavesTheMessageInTheQueue() throws Exception {
        String q = admin.createQueue(Emu.dashed("tq"),
            new com.azure.messaging.servicebus.administration.models.CreateQueueOptions().setLockDuration(java.time.Duration.ofSeconds(5))).getName();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q)) {
            s.sendMessage(new ServiceBusMessage("work"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            ServiceBusTransactionContext tx = r.createTransaction();
            r.complete(m, new CompleteOptions().setTransactionContext(tx));
            r.rollbackTransaction(tx);
            // the complete was undone: after the lock runs out the message is delivered again
            Thread.sleep(6000);
            ServiceBusReceivedMessage again = Sb.receiveOne(r);
            assertEquals("work", again.getBody().toString());
            r.complete(again);
        }
    }

    @Test void receiveCompleteAndSendAcrossTwoQueuesInOneTransaction_commitsBothOrNeither() {
        String in = queue();
        String out = queue();
        try (ServiceBusSenderClient inSender = sb.sender().queueName(in).buildClient();
             ServiceBusSenderClient outSender = sb.sender().queueName(out).buildClient();
             ServiceBusReceiverClient inReceiver = Sb.peekLock(sb, in);
             ServiceBusReceiverClient outReceiver = Sb.peekLock(sb, out)) {
            inSender.sendMessage(new ServiceBusMessage("order"));
            ServiceBusReceivedMessage m = Sb.receiveOne(inReceiver);
            ServiceBusTransactionContext tx = inReceiver.createTransaction();
            inReceiver.complete(m, new CompleteOptions().setTransactionContext(tx));
            outSender.sendMessage(new ServiceBusMessage("invoice"), tx);
            assertTrue(Sb.receive(outReceiver, 1, 2).isEmpty(), "invoice invisible before commit");
            inReceiver.commitTransaction(tx);
            ServiceBusReceivedMessage invoice = Sb.receiveOne(outReceiver);
            assertEquals("invoice", invoice.getBody().toString());
            outReceiver.complete(invoice);
            assertTrue(Sb.receive(inReceiver, 1, 2).isEmpty());
        }
    }

    @Test void commitingTheSameTransactionTwice_fails() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient()) {
            ServiceBusTransactionContext tx = s.createTransaction();
            s.sendMessage(new ServiceBusMessage("x"), tx);
            s.commitTransaction(tx);
            assertThrows(RuntimeException.class, () -> s.commitTransaction(tx));
        }
    }

    @Test void scheduleAndDeadLetterInATransaction_thenCommit() {
        String q = queue();
        try (ServiceBusSenderClient s = sb.sender().queueName(q).buildClient();
             ServiceBusReceiverClient r = Sb.peekLock(sb, q);
             ServiceBusReceiverClient dlq = Sb.deadLetter(sb, q)) {
            s.sendMessage(new ServiceBusMessage("poison"));
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            ServiceBusTransactionContext tx = r.createTransaction();
            r.deadLetter(m, new DeadLetterOptions().setDeadLetterReason("tx").setTransactionContext(tx));
            r.commitTransaction(tx);
            ServiceBusReceivedMessage d = Sb.receiveOne(dlq);
            assertEquals("tx", d.getDeadLetterReason());
            dlq.complete(d);
        }
    }
}
