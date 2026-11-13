package azemu;

import org.apache.kafka.clients.consumer.KafkaConsumer;
import org.apache.kafka.clients.producer.KafkaProducer;
import org.apache.kafka.clients.producer.ProducerRecord;
import org.apache.kafka.clients.producer.RecordMetadata;
import org.junit.jupiter.api.*;

import java.time.Duration;
import java.util.List;
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.*;

/**
 * Auto-creation of event hubs by the Kafka endpoint. Azure does not have it, so the emulator does not do it unless it is
 * started with AZEMU_EVENTHUBS_AUTOCREATE=1 (or AZEMU_LENIENT=1): this class starts it that way. The helpers are those of
 * KafkaTest (the same client settings); the default behaviour is tested there.
 */
@EmuTest(env = {"AZEMU_EVENTHUBS_AUTOCREATE=1"})
class KafkaAutoCreateTest {
    @BeforeAll static void setUp() { KafkaTest.emu = Emu.current(); }

    @Test void producer_toAMissingTopic_createsTheEventHub_whenAutoCreationIsOn() throws Exception {
        String t = KafkaTest.topic();
        try (KafkaProducer<String, String> p = KafkaTest.producer(KafkaTest.plain())) {
            RecordMetadata md = p.send(new ProducerRecord<>(t, "k", "v")).get(30, TimeUnit.SECONDS);
            assertEquals(t, md.topic());
        }
        String info = KafkaTest.hubInfo("default", t);
        assertNotNull(info);
        assertTrue(info.contains("\"partitionCount\":4"), info);
    }

    @Test void consumer_subscribingToAMissingTopic_createsIt_becauseTheJavaConsumerAllowsAutoCreation() throws Exception {
        String t = KafkaTest.topic();
        try (KafkaConsumer<String, String> c = KafkaTest.consumer(KafkaTest.plain(), Emu.dashed("g"))) {
            c.subscribe(List.of(t));
            long end = System.currentTimeMillis() + 20_000;
            while (KafkaTest.hubInfo("default", t) == null && System.currentTimeMillis() < end) c.poll(Duration.ofMillis(300));
            assertNotNull(KafkaTest.hubInfo("default", t));
        }
    }
}
