package azemu;

import org.apache.qpid.proton.Proton;
import org.apache.qpid.proton.amqp.Binary;
import org.apache.qpid.proton.amqp.Symbol;
import org.apache.qpid.proton.amqp.messaging.*;
import org.apache.qpid.proton.amqp.transaction.*;
import org.apache.qpid.proton.amqp.transport.DeliveryState;
import org.apache.qpid.proton.engine.*;
import org.apache.qpid.proton.message.Message;
import org.apache.qpid.proton.reactor.Acceptor;
import org.apache.qpid.proton.reactor.Reactor;

import java.io.IOException;
import java.net.ServerSocket;
import java.util.*;
import java.util.concurrent.*;

/**
 * A tiny AMQP 1.0 server made with qpid-proton-j, only to record what the Azure SDK for Java sends for a Service Bus
 * transaction (azemu refuses the coordinator link, so the real emulator cannot show it). It accepts every SASL login,
 * answers $cbs put-token with 202, plays a transaction coordinator (Declare answered with Declared, Discharge with
 * accepted), accepts transfers on sender links and delivers one queued message on receiver links.
 * Everything it sees is appended to {@link #events}; the frames are also printed by proton's own tracer (PN_TRACE_FRM=1
 * for the client side, always on for this side, written to {@link #frameLog}).
 */
final class MockAmqpServer implements AutoCloseable {
    final List<String> events = new CopyOnWriteArrayList<>();
    final List<String> frameLog = new CopyOnWriteArrayList<>();
    final BlockingQueue<Object> declares = new LinkedBlockingQueue<>();
    final int port;
    private final Reactor reactor;
    private final Thread thread;
    private volatile byte[] queuedMessage;
    private int txnCounter = 0;

    /** Last transactional delivery states seen, by kind. */
    final List<TransactionalState> transferStates = new CopyOnWriteArrayList<>();
    final List<TransactionalState> dispositionStates = new CopyOnWriteArrayList<>();
    final List<Discharge> discharges = new CopyOnWriteArrayList<>();
    final List<Declare> declaresSeen = new CopyOnWriteArrayList<>();

    MockAmqpServer() throws IOException {
        try (ServerSocket s = new ServerSocket(0)) { port = s.getLocalPort(); }
        reactor = Proton.reactor(new org.apache.qpid.proton.engine.BaseHandler());
        Handler h = new Handler();
        Acceptor acceptor = reactor.acceptor("127.0.0.1", port, h);
        thread = new Thread(() -> {
            try {
                reactor.setTimeout(100);
                reactor.start();
                while (reactor.process()) { /* run */ }
            } catch (Exception e) {
                events.add("reactor stopped: " + e);
            }
        }, "mock-amqp");
        thread.setDaemon(true);
        thread.start();
    }

    void queueMessageForReceivers(String body) {
        Message m = Proton.message();
        m.setBody(new AmqpValue(body));
        m.setMessageId("mock-msg-1");
        Header header = new Header();
        header.setDurable(false);
        header.setDeliveryCount(org.apache.qpid.proton.amqp.UnsignedInteger.ZERO);
        m.setHeader(header);   // the SDK dereferences the header without a null check
        Map<Symbol, Object> ann = new LinkedHashMap<>();
        ann.put(Symbol.valueOf("x-opt-sequence-number"), 1L);
        ann.put(Symbol.valueOf("x-opt-enqueued-time"), new Date());
        ann.put(Symbol.valueOf("x-opt-locked-until"), new Date(System.currentTimeMillis() + 60_000));
        m.setMessageAnnotations(new MessageAnnotations(ann));
        byte[] buf = new byte[1024];
        int n = m.encode(buf, 0, buf.length);
        queuedMessage = Arrays.copyOf(buf, n);
    }

    @Override public void close() {
        try { reactor.stop(); } catch (Exception ignored) { }
        try { reactor.free(); } catch (Exception ignored) { }
    }

    private final class Handler extends org.apache.qpid.proton.engine.BaseHandler {
        private final Map<String, Sender> senders = new HashMap<>();

        @Override public void onConnectionBound(Event e) {
            Transport t = e.getTransport();
            Sasl sasl = t.sasl();
            sasl.server();
            sasl.setMechanisms("ANONYMOUS");
            sasl.done(Sasl.SaslOutcome.PN_SASL_OK);
            t.trace(Transport.TRACE_FRM);
            org.apache.qpid.proton.engine.impl.TransportImpl impl = (org.apache.qpid.proton.engine.impl.TransportImpl) t;
            impl.setProtocolTracer(new org.apache.qpid.proton.engine.impl.ProtocolTracer() {
                @Override public void receivedFrame(org.apache.qpid.proton.framing.TransportFrame f) { frameLog.add("<- " + f.getBody() + payloadText(f)); }
                @Override public void sentFrame(org.apache.qpid.proton.framing.TransportFrame f) { frameLog.add("-> " + f.getBody()); }
            });
        }

        private String payloadText(org.apache.qpid.proton.framing.TransportFrame f) {
            return f.getPayload() == null ? "" : " payload=" + f.getPayload().getLength() + "B";
        }

        @Override public void onConnectionRemoteOpen(Event e) { e.getConnection().setContainer("mock"); e.getConnection().open(); }
        @Override public void onSessionRemoteOpen(Event e) { e.getSession().setIncomingCapacity(1 << 20); e.getSession().open(); }

        @Override public void onLinkRemoteOpen(Event e) {
            Link l = e.getLink();
            l.setSource(l.getRemoteSource());
            l.setTarget(l.getRemoteTarget());
            String kind = l.getRemoteTarget() instanceof Coordinator ? "coordinator" : "address=" + addr(l);
            events.add("attach " + (l instanceof Receiver ? "from-client-sender " : "from-client-receiver ") + l.getName() + " " + kind
                + " rcvSettleMode=" + l.getRemoteReceiverSettleMode() + " sndSettleMode=" + l.getRemoteSenderSettleMode());
            if (l instanceof Receiver) l.setMaxMessageSize(org.apache.qpid.proton.amqp.UnsignedLong.valueOf(1048576));
            l.open();
            if (l instanceof Receiver) ((Receiver) l).flow(100);
            if (l instanceof Sender) senders.put(l.getName(), (Sender) l);
        }

        private String addr(Link l) {
            if (l instanceof Receiver && l.getRemoteTarget() instanceof Target) return ((Target) l.getRemoteTarget()).getAddress();
            if (l.getRemoteSource() instanceof Source) return ((Source) l.getRemoteSource()).getAddress();
            return "?";
        }

        @Override public void onLinkFlow(Event e) {
            Link l = e.getLink();
            if (l instanceof Sender && l.getCredit() > 0 && queuedMessage != null && !"cbs:receiver".equals(l.getName())
                && !addr(l).equals("$cbs")) {
                Sender s = (Sender) l;
                byte[] data = queuedMessage;
                queuedMessage = null;
                UUID lock = UUID.randomUUID();   // Service Bus uses the 16 byte delivery tag as the lock token
                java.nio.ByteBuffer tag = java.nio.ByteBuffer.allocate(16).putLong(lock.getMostSignificantBits()).putLong(lock.getLeastSignificantBits());
                Delivery d = s.delivery(tag.array());
                s.send(data, 0, data.length);
                s.advance();
                events.add("sent message to receiver link " + l.getName());
            }
        }

        @Override public void onDelivery(Event e) {
            Delivery d = e.getDelivery();
            Link l = d.getLink();
            if (l instanceof Receiver) {
                if (!d.isReadable() || d.isPartial()) return;
                Receiver r = (Receiver) l;
                byte[] buf = new byte[d.pending()];
                r.recv(buf, 0, buf.length);
                Message m = Proton.message();
                m.decode(buf, 0, buf.length);
                DeliveryState st = d.getRemoteState();
                if (l.getRemoteTarget() instanceof Coordinator) {
                    Object body = ((AmqpValue) m.getBody()).getValue();
                    if (body instanceof Declare) {
                        declaresSeen.add((Declare) body);
                        Declared declared = new Declared();
                        byte[] id = ("txn-" + (++txnCounter)).getBytes();
                        declared.setTxnId(new Binary(id));
                        events.add("coordinator: Declare(globalId=" + ((Declare) body).getGlobalId() + ") -> Declared(" + new String(id) + ")");
                        d.disposition(declared);
                    } else if (body instanceof Discharge) {
                        Discharge dc = (Discharge) body;
                        discharges.add(dc);
                        events.add("coordinator: Discharge(txnId=" + new String(dc.getTxnId().getArray()) + ", fail=" + dc.getFail() + ") -> Accepted");
                        d.disposition(Accepted.getInstance());
                    } else {
                        events.add("coordinator: unexpected body " + body);
                        d.disposition(new Rejected());
                    }
                    d.settle();
                    return;
                }
                String address = addr(l);
                if ("$cbs".equals(address)) {
                    String op = String.valueOf(m.getApplicationProperties().getValue().get("operation"));
                    events.add("$cbs " + op + " name=" + m.getApplicationProperties().getValue().get("name"));
                    Sender back = senders.get("cbs:receiver");
                    if (back != null) {
                        Message reply = Proton.message();
                        reply.setCorrelationId(m.getMessageId());
                        reply.setAddress("cbs-client-reply-to");
                        Map<String, Object> props = new LinkedHashMap<>();
                        props.put("status-code", 202);
                        props.put("status-description", "Accepted");
                        reply.setApplicationProperties(new ApplicationProperties(props));
                        byte[] rb = new byte[512];
                        int n = reply.encode(rb, 0, rb.length);
                        back.delivery(("cbs" + (++txnCounter)).getBytes());
                        back.send(rb, 0, n);
                        back.advance();
                    }
                    d.disposition(Accepted.getInstance());
                    d.settle();
                    return;
                }
                if (st instanceof TransactionalState) {
                    transferStates.add((TransactionalState) st);
                    events.add("transfer to " + address + " with TransactionalState(txnId=" + new String(((TransactionalState) st).getTxnId().getArray()) + ", outcome=" + ((TransactionalState) st).getOutcome() + ")");
                } else {
                    events.add("transfer to " + address + " state=" + st);
                }
                // answer inside the transaction like Service Bus does: TransactionalState with the accepted outcome
                if (st instanceof TransactionalState) {
                    TransactionalState ts = new TransactionalState();
                    ts.setTxnId(((TransactionalState) st).getTxnId());
                    ts.setOutcome(Accepted.getInstance());
                    d.disposition(ts);
                } else {
                    d.disposition(Accepted.getInstance());
                }
                d.settle();
            } else {
                // a disposition from the client for a message we sent
                DeliveryState st = d.getRemoteState();
                if (st instanceof TransactionalState) {
                    dispositionStates.add((TransactionalState) st);
                    TransactionalState ts = (TransactionalState) st;
                    events.add("disposition for " + l.getName() + " TransactionalState(txnId=" + new String(ts.getTxnId().getArray()) + ", outcome=" + ts.getOutcome() + ")");
                } else if (st != null) {
                    events.add("disposition for " + l.getName() + " " + st);
                }
                if (d.remotelySettled() || st != null) {
                    d.disposition(st);
                    d.settle();
                }
            }
        }
    }
}
