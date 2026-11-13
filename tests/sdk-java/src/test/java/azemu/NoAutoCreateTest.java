package azemu;

import com.azure.core.amqp.exception.AmqpException;
import com.azure.messaging.eventhubs.*;
import com.azure.messaging.eventhubs.models.*;
import com.azure.messaging.servicebus.*;
import com.azure.messaging.servicebus.administration.ServiceBusAdministrationClient;
import com.azure.messaging.servicebus.administration.models.*;
import org.junit.jupiter.api.*;

import java.util.List;

import static org.junit.jupiter.api.Assertions.*;

/** With auto-creation off (as on Azure) a missing queue or event hub is an error. */
@EmuTest(env = {"AZEMU_SERVICEBUS_AUTOCREATE=0", "AZEMU_EVENTHUBS_AUTOCREATE=0"})
class NoAutoCreateTest {
    static Emu emu;
    static String ns;

    @BeforeAll static void setUp() { emu = Emu.current(); ns = Emu.name("nac"); }

    @Test void serviceBus_sendToAMissingQueue_failsWithMessagingEntityNotFound() {
        ServiceBusClientBuilder b = Sb.client(emu, ns);
        try (ServiceBusSenderClient s = b.sender().queueName(Emu.dashed("missing")).buildClient()) {
            ServiceBusException e = assertThrows(ServiceBusException.class, () -> s.sendMessage(new ServiceBusMessage("x")));
            assertEquals(ServiceBusFailureReason.MESSAGING_ENTITY_NOT_FOUND, e.getReason());
            assertTrue(e.getMessage().contains("could not be found"), e.getMessage());
        }
    }

    @Test void serviceBus_receiveFromAMissingQueue_failsWithTheNotFoundCondition() {
        ServiceBusClientBuilder b = Sb.client(emu, ns);
        try (ServiceBusReceiverClient r = Sb.peekLock(b, Emu.dashed("missing"))) {
            // the sync receiver wraps the error in "The receiver client is terminated"; the cause is the service's error
            RuntimeException e = assertThrows(RuntimeException.class, () -> Sb.receive(r, 1, 3));
            Throwable c = e;
            while (c != null && !(c instanceof AmqpException)) c = c.getCause();
            assertNotNull(c, String.valueOf(e));
            assertEquals(com.azure.core.amqp.exception.AmqpErrorCondition.NOT_FOUND, ((AmqpException) c).getErrorCondition());
            assertTrue(c.getMessage().contains("could not be found"), c.getMessage());
        }
    }

    @Test void serviceBus_queueCreatedWithTheAdministrationClient_isUsable_andDisablingItRefusesSends() {
        ServiceBusAdministrationClient admin = Sb.admin(emu, ns);
        String q = Emu.dashed("q");
        admin.createQueue(q);
        ServiceBusClientBuilder b = Sb.client(emu, ns);
        try (ServiceBusSenderClient s = b.sender().queueName(q).buildClient()) {
            s.sendMessage(new ServiceBusMessage("ok"));
            QueueProperties p = admin.getQueue(q);
            p.setStatus(EntityStatus.SEND_DISABLED);
            admin.updateQueue(p);
            ServiceBusException e = assertThrows(ServiceBusException.class, () -> s.sendMessage(new ServiceBusMessage("refused")));
            assertEquals(ServiceBusFailureReason.MESSAGING_ENTITY_DISABLED, e.getReason());
        }
    }

    @Test void eventHubs_aMissingEventHub_failsInsteadOfBeingCreated() {
        try (EventHubProducerClient p = Eh.builder(emu, Emu.name("nacehns"), Emu.dashed("missinghub")).buildProducerClient()) {
            RuntimeException e = assertThrows(RuntimeException.class, () -> p.getEventHubProperties());
            String all = String.valueOf(e) + " " + String.valueOf(e.getCause());
            assertTrue(all.toLowerCase().contains("not be found") || all.toLowerCase().contains("not found") || e instanceof AmqpException || e.getCause() instanceof AmqpException, all);
        }
    }
}
