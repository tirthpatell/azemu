package azemu;

import com.azure.core.amqp.AmqpRetryOptions;
import com.azure.core.amqp.AmqpTransportType;
import com.azure.core.http.policy.HttpPipelinePolicy;
import com.azure.messaging.servicebus.*;
import com.azure.messaging.servicebus.administration.ServiceBusAdministrationClient;
import com.azure.messaging.servicebus.administration.ServiceBusAdministrationClientBuilder;
import com.azure.messaging.servicebus.models.ServiceBusReceiveMode;
import com.azure.messaging.servicebus.models.SubQueue;

import java.time.Duration;
import java.util.ArrayList;
import java.util.List;

/** Shared helpers for the Service Bus tests. Not a test. */
final class Sb {
    private Sb() {}

    static final AmqpRetryOptions FAST_RETRY = new AmqpRetryOptions()
        .setMaxRetries(1).setDelay(Duration.ofMillis(200)).setTryTimeout(Duration.ofSeconds(20));

    static ServiceBusClientBuilder client(Emu emu, String ns) {
        return new ServiceBusClientBuilder().connectionString(emu.sbConnStr(ns)).retryOptions(FAST_RETRY);
    }

    /** AMQP over WebSocket, plain ws: the emulator flag in the connection string keeps TLS off and the port of Endpoint=. */
    static ServiceBusClientBuilder wsClient(Emu emu, String ns) {
        return new ServiceBusClientBuilder().connectionString(emu.sbConnStr(ns))
            .transportType(AmqpTransportType.AMQP_WEB_SOCKETS).retryOptions(FAST_RETRY);
    }

    /** AMQP over WebSocket inside TLS (wss): no emulator flag, a custom endpoint carries the port. */
    static ServiceBusClientBuilder wssClient(Emu emu, String ns) {
        return new ServiceBusClientBuilder()
            .connectionString("Endpoint=sb://" + ns + ".servicebus.localhost;SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0")
            .customEndpointAddress("https://" + ns + ".servicebus.localhost:" + emu.port)
            .transportType(AmqpTransportType.AMQP_WEB_SOCKETS)
            .retryOptions(FAST_RETRY);
    }

    /**
     * The administration client builds its URL as https://{host} and drops the port of the connection string
     * (see docs/sdk-notes.md), so a pipeline policy puts the emulator's port back.
     */
    static ServiceBusAdministrationClient admin(Emu emu, String ns) {
        HttpPipelinePolicy setPort = (context, next) -> {
            try {
                java.net.URL u = context.getHttpRequest().getUrl();
                context.getHttpRequest().setUrl(new java.net.URL(u.getProtocol(), u.getHost(), emu.port, u.getFile()));
            } catch (java.net.MalformedURLException e) {
                return reactor.core.publisher.Mono.error(e);
            }
            return next.process();
        };
        return new ServiceBusAdministrationClientBuilder()
            .connectionString(emu.sbConnStr(ns))
            .addPolicy(setPort)
            .buildClient();
    }

    static ServiceBusReceiverClient peekLock(ServiceBusClientBuilder b, String queue) {
        return b.receiver().queueName(queue).disableAutoComplete().buildClient();
    }

    /** Peek-lock without the SDK's automatic lock renewal (needed to see a lock expire). */
    static ServiceBusReceiverClient peekLockNoRenew(ServiceBusClientBuilder b, String queue) {
        return b.receiver().queueName(queue).disableAutoComplete().maxAutoLockRenewDuration(Duration.ZERO).buildClient();
    }

    static ServiceBusReceiverClient peekLockSub(ServiceBusClientBuilder b, String topic, String sub) {
        return b.receiver().topicName(topic).subscriptionName(sub).disableAutoComplete().buildClient();
    }

    static ServiceBusReceiverClient deadLetter(ServiceBusClientBuilder b, String queue) {
        return b.receiver().queueName(queue).subQueue(SubQueue.DEAD_LETTER_QUEUE).disableAutoComplete().buildClient();
    }

    static ServiceBusReceiverClient receiveAndDelete(ServiceBusClientBuilder b, String queue) {
        return b.receiver().queueName(queue).receiveMode(ServiceBusReceiveMode.RECEIVE_AND_DELETE).buildClient();
    }

    static List<ServiceBusReceivedMessage> receive(ServiceBusReceiverClient r, int max, int seconds) {
        List<ServiceBusReceivedMessage> out = new ArrayList<>();
        r.receiveMessages(max, Duration.ofSeconds(seconds)).forEach(out::add);
        return out;
    }

    static ServiceBusReceivedMessage receiveOne(ServiceBusReceiverClient r) {
        List<ServiceBusReceivedMessage> l = receive(r, 1, 10);
        if (l.isEmpty()) throw new AssertionError("no message within 10 s");
        return l.get(0);
    }
}
