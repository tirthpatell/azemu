package azemu;

import org.apache.kafka.clients.producer.KafkaProducer;
import org.apache.kafka.clients.producer.ProducerRecord;
import org.junit.jupiter.api.*;

import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.*;

/**
 * The Kafka endpoint makes the event hub for a producer that names a topic nobody created, whatever AZEMU_EVENTHUBS_AUTOCREATE says
 * (it is about AMQP): one partition, as a real namespace does. The same is tested in KafkaTest with the default settings; this class
 * runs it with the setting on too, to show that the setting changes nothing here.
 */
@EmuTest(env = {"AZEMU_EVENTHUBS_AUTOCREATE=1"})
class KafkaAutoCreateTest {
    @BeforeAll static void setUp() { KafkaTest.emu = Emu.current(); }

    @Test void producer_toAMissingTopic_createsTheEventHub_withOnePartition() throws Exception {
        String t = KafkaTest.topic();
        try (KafkaProducer<String, String> p = KafkaTest.producer(KafkaTest.plain())) {
            assertEquals(t, p.send(new ProducerRecord<>(t, "k", "v")).get(30, TimeUnit.SECONDS).topic());
        }
        String info = KafkaTest.hubInfo("default", t);
        assertNotNull(info);
        assertTrue(info.contains("\"partitionCount\":1"), info);
    }
}
