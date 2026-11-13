package azemu;

import com.azure.messaging.servicebus.*;
import com.azure.messaging.servicebus.models.*;
import org.apache.qpid.proton.amqp.transaction.TransactionalState;
import org.junit.jupiter.api.*;

import java.nio.file.*;
import java.util.*;

import static org.junit.jupiter.api.Assertions.*;

/**
 * What the Azure SDK for Java (azure-messaging-servicebus) puts on the wire for a transaction, seen by a small AMQP server
 * of our own ({@link MockAmqpServer}), so that the exact frames can be read. Not a test of azemu. The event list and
 * frames go to target/tx-wire.txt.
 */
class ServiceBusTransactionWireTest {
    static MockAmqpServer server;
    static ServiceBusClientBuilder sb;

    @BeforeAll static void setUp() throws Exception {
        server = new MockAmqpServer();
        sb = new ServiceBusClientBuilder()
            .connectionString("Endpoint=sb://localhost:" + server.port + ";SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true")
            .retryOptions(Sb.FAST_RETRY);
    }

    @AfterAll static void tearDown() throws Exception {
        if (server != null) {
            Files.createDirectories(Paths.get("target"));
            List<String> out = new ArrayList<>(server.events);
            out.add("---- frames seen by the server ----");
            out.addAll(server.frameLog);
            Files.write(Paths.get("target", "tx-wire.txt"), out);
            server.close();
        }
    }

    @Test void createTransaction_sendsDeclareToACoordinatorLink_andCommitSendsDischargeWithFailFalse() {
        try (ServiceBusSenderClient s = sb.sender().queueName("wire-q1").buildClient()) {
            ServiceBusTransactionContext tx = s.createTransaction();
            assertNotNull(tx.getTransactionId());
            s.sendMessage(new ServiceBusMessage("tx message"), tx);
            s.commitTransaction(tx);
        }
        assertFalse(server.declaresSeen.isEmpty(), "events: " + server.events);
        assertFalse(server.transferStates.isEmpty(), "the transfer carries a TransactionalState: " + server.events);
        TransactionalState ts = server.transferStates.get(server.transferStates.size() - 1);
        assertTrue(new String(ts.getTxnId().getArray()).startsWith("txn-"));
        assertFalse(server.discharges.isEmpty());
        assertFalse(server.discharges.get(server.discharges.size() - 1).getFail(), "commit is discharge with fail=false");
    }

    @Test void rollbackSendsDischargeWithFailTrue() {
        int before = server.discharges.size();
        try (ServiceBusSenderClient s = sb.sender().queueName("wire-q2").buildClient()) {
            ServiceBusTransactionContext tx = s.createTransaction();
            s.sendMessage(new ServiceBusMessage("rolled back"), tx);
            s.rollbackTransaction(tx);
        }
        assertEquals(before + 1, server.discharges.size(), server.events.toString());
        assertTrue(server.discharges.get(server.discharges.size() - 1).getFail(), "rollback is discharge with fail=true");
    }

    @Test void completeInATransaction_sendsATransactionalStateDisposition() {
        server.queueMessageForReceivers("work item");
        try (ServiceBusReceiverClient r = Sb.peekLock(sb, "wire-q3")) {
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            ServiceBusTransactionContext tx = r.createTransaction();
            r.complete(m, new CompleteOptions().setTransactionContext(tx));
            r.commitTransaction(tx);
        }
        assertFalse(server.dispositionStates.isEmpty(), "events: " + server.events);
        TransactionalState ts = server.dispositionStates.get(server.dispositionStates.size() - 1);
        assertEquals(org.apache.qpid.proton.amqp.messaging.Accepted.getInstance(), ts.getOutcome());
    }
}
