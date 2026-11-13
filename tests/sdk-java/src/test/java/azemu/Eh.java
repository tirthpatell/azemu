package azemu;

import com.azure.core.amqp.AmqpRetryOptions;
import com.azure.core.amqp.AmqpTransportType;
import com.azure.messaging.eventhubs.*;
import com.azure.messaging.eventhubs.models.EventPosition;
import com.azure.messaging.eventhubs.models.PartitionEvent;

import java.time.Duration;
import java.util.ArrayList;
import java.util.List;

/** Shared helpers for the Event Hubs tests. Not a test. */
final class Eh {
    private Eh() {}

    static final AmqpRetryOptions FAST_RETRY = new AmqpRetryOptions()
        .setMaxRetries(1).setDelay(Duration.ofMillis(200)).setTryTimeout(Duration.ofSeconds(20));

    /**
     * Create an event hub (4 partitions, and the consumer groups named) through the emulator's admin API and return its name. The
     * emulator does not create event hubs by being named (AZEMU_EVENTHUBS_AUTOCREATE is off by default), as Azure does not.
     */
    static String newHub(Emu emu, String ns, String... groups) {
        String hub = Emu.dashed("eh");
        createHub(emu, ns, hub, groups);
        return hub;
    }

    static void createHub(Emu emu, String ns, String hub, String... groups) {
        try {
            String body = groups.length == 0 ? "{}" : "{\"consumerGroups\":[\"" + String.join("\",\"", groups) + "\"]}";
            java.net.HttpURLConnection c = (java.net.HttpURLConnection) new java.net.URI(emu.gateway() + "/_azemu/eventhubs/" + ns + "/hubs/" + hub).toURL().openConnection();
            c.setRequestMethod("PUT");
            c.setDoOutput(true);
            c.setRequestProperty("Content-Type", "application/json");
            c.getOutputStream().write(body.getBytes(java.nio.charset.StandardCharsets.UTF_8));
            int code = c.getResponseCode();
            c.disconnect();
            if (code != 200 && code != 201) throw new IllegalStateException("create event hub " + ns + "/" + hub + ": HTTP " + code);
        } catch (RuntimeException e) {
            throw e;
        } catch (Exception e) {
            throw new IllegalStateException(e);
        }
    }

    static EventHubClientBuilder builder(Emu emu, String ns, String hub) {
        return new EventHubClientBuilder().connectionString(emu.ehConnStr(ns, hub)).retryOptions(FAST_RETRY);
    }

    /** WebSocket: connection string without the emulator flag plus a custom endpoint (the SDK ignores the port otherwise). */
    static EventHubClientBuilder wsBuilder(Emu emu, String ns, String hub) {
        return new EventHubClientBuilder()
            .connectionString("Endpoint=sb://" + ns + ".eventhub.localhost;SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;EntityPath=" + hub)
            .customEndpointAddress("sb://" + ns + ".eventhub.localhost:" + emu.port)
            .transportType(AmqpTransportType.AMQP_WEB_SOCKETS)
            .retryOptions(FAST_RETRY);
    }

    static List<PartitionEvent> read(EventHubConsumerClient c, String partition, int max, EventPosition pos, int seconds) {
        List<PartitionEvent> out = new ArrayList<>();
        c.receiveFromPartition(partition, max, pos, Duration.ofSeconds(seconds)).forEach(out::add);
        return out;
    }
}
