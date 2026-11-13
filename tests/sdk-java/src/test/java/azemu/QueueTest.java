package azemu;

import com.azure.core.http.rest.Response;
import com.azure.core.util.Context;
import com.azure.storage.common.StorageSharedKeyCredential;
import com.azure.storage.queue.*;
import com.azure.storage.queue.models.*;
import com.azure.storage.queue.sas.*;
import org.junit.jupiter.api.*;

import java.time.Duration;
import java.time.OffsetDateTime;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

@EmuTest
class QueueTest {
    static Emu emu;
    static String acct;
    static StorageSharedKeyCredential cred;
    static QueueServiceClient svc;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        acct = Emu.name("qacct");
        cred = new StorageSharedKeyCredential(acct, Emu.DEV_KEY);
        svc = new QueueServiceClientBuilder().endpoint(emu.queueUrl(acct)).credential(cred).buildClient();
    }

    QueueClient queue() { return svc.createQueue(Emu.dashed("q")); }

    static QueueStorageException err(org.junit.jupiter.api.function.Executable e) { return assertThrows(QueueStorageException.class, e); }

    // ---- lifecycle ----------------------------------------------------------------------------------------
    @Test void createQueue_withSameMetadataTwice_isIdempotent_withDifferentMetadata_is409QueueAlreadyExists() {
        String n = Emu.dashed("q");
        QueueClient q = svc.getQueueClient(n);
        q.createWithResponse(Map.of("a", "1"), null, Context.NONE);
        Response<Void> again = q.createWithResponse(Map.of("a", "1"), null, Context.NONE);
        assertEquals(204, again.getStatusCode());
        QueueStorageException e = err(() -> q.createWithResponse(Map.of("a", "2"), null, Context.NONE));
        assertEquals(409, e.getStatusCode());
        assertEquals(QueueErrorCode.QUEUE_ALREADY_EXISTS, e.getErrorCode());
    }

    @Test void queueMetadataAndApproximateCount_areReturnedByGetProperties() {
        QueueClient q = queue();
        q.setMetadata(Map.of("owner", "java", "env", "test"));
        q.sendMessage("a");
        q.sendMessage("b");
        QueueProperties p = q.getProperties();
        assertEquals(Map.of("owner", "java", "env", "test"), p.getMetadata());
        assertEquals(2, p.getApproximateMessagesCount());
    }

    @Test void operationsOnMissingQueue_are404QueueNotFound() {
        QueueClient q = svc.getQueueClient(Emu.dashed("missing"));
        QueueStorageException e = err(() -> q.sendMessage("x"));
        assertEquals(404, e.getStatusCode());
        assertEquals(QueueErrorCode.QUEUE_NOT_FOUND, e.getErrorCode());
        assertEquals(404, err(q::getProperties).getStatusCode());
        assertEquals(404, err(q::delete).getStatusCode());
        assertFalse(q.deleteIfExists());
    }

    @Test void listQueues_withPrefixMetadataAndPaging() {
        String p = Emu.name("lq");
        for (int i = 0; i < 5; i++) svc.createQueueWithResponse(p + i, Map.of("i", "" + i), null, Context.NONE);
        List<QueueItem> items = svc.listQueues(new QueuesSegmentOptions().setPrefix(p).setIncludeMetadata(true), null, Context.NONE).stream().collect(Collectors.toList());
        assertEquals(5, items.size());
        assertEquals("3", items.get(3).getMetadata().get("i"));
        int pages = 0;
        for (var pg : svc.listQueues(new QueuesSegmentOptions().setPrefix(p), null, Context.NONE).iterableByPage(2)) pages++;
        assertEquals(3, pages);
    }

    // ---- messages -----------------------------------------------------------------------------------------
    @Test void sendReceiveDelete_roundTripsTextAndCountsDequeues() {
        QueueClient q = queue();
        SendMessageResult s = q.sendMessage("hello queue");
        assertNotNull(s.getMessageId());
        assertNotNull(s.getPopReceipt());
        QueueMessageItem m = q.receiveMessage();
        assertEquals("hello queue", m.getMessageText());
        assertEquals(s.getMessageId(), m.getMessageId());
        assertEquals(1, m.getDequeueCount());
        q.deleteMessage(m.getMessageId(), m.getPopReceipt());
        assertNull(q.receiveMessage());
        assertEquals(0, q.getProperties().getApproximateMessagesCount());
    }

    @Test void receivedMessage_isInvisibleUntilTheVisibilityTimeoutEnds_thenDequeueCountIs2() throws Exception {
        QueueClient q = queue();
        q.sendMessage("vis");
        QueueMessageItem m = q.receiveMessages(1, Duration.ofSeconds(1), null, Context.NONE).iterator().next();
        assertEquals(1, m.getDequeueCount());
        assertFalse(q.receiveMessages(1, Duration.ofSeconds(1), null, Context.NONE).iterator().hasNext());
        Thread.sleep(1300);
        QueueMessageItem again = q.receiveMessages(1, Duration.ofSeconds(30), null, Context.NONE).iterator().next();
        assertEquals(m.getMessageId(), again.getMessageId());
        assertEquals(2, again.getDequeueCount());
        assertNotEquals(m.getPopReceipt(), again.getPopReceipt());
    }

    @Test void deleteWithStalePopReceipt_is400PopReceiptMismatch() throws Exception {
        QueueClient q = queue();
        q.sendMessage("stale");
        QueueMessageItem first = q.receiveMessages(1, Duration.ofSeconds(1), null, Context.NONE).iterator().next();
        Thread.sleep(1300);
        q.receiveMessages(1, Duration.ofSeconds(30), null, Context.NONE).iterator().next();
        QueueStorageException e = err(() -> q.deleteMessage(first.getMessageId(), first.getPopReceipt()));
        assertEquals(400, e.getStatusCode());
        assertEquals(QueueErrorCode.POP_RECEIPT_MISMATCH, e.getErrorCode());
    }

    @Test void receiveMultiple_returnsAtMostRequested_inFifoOrder() {
        QueueClient q = queue();
        for (int i = 0; i < 6; i++) q.sendMessage("m" + i);
        List<String> got = q.receiveMessages(4).stream().map(QueueMessageItem::getMessageText).collect(Collectors.toList());
        assertEquals(List.of("m0", "m1", "m2", "m3"), got);
    }

    @Test void peekMessages_doesNotHideMessagesOrChangeDequeueCount() {
        QueueClient q = queue();
        q.sendMessage("peek me");
        PeekedMessageItem p = q.peekMessage();
        assertEquals("peek me", p.getMessageText());
        assertEquals(0, p.getDequeueCount());
        assertEquals(0, q.peekMessage().getDequeueCount());
        QueueMessageItem m = q.receiveMessage();
        assertEquals("peek me", m.getMessageText());
    }

    @Test void updateMessage_changesTextAndGivesANewPopReceipt() {
        QueueClient q = queue();
        q.sendMessage("before");
        QueueMessageItem m = q.receiveMessages(1, Duration.ofSeconds(30), null, Context.NONE).iterator().next();
        UpdateMessageResult u = q.updateMessage(m.getMessageId(), m.getPopReceipt(), "after", Duration.ofSeconds(0));
        assertNotEquals(m.getPopReceipt(), u.getPopReceipt());
        QueueMessageItem again = q.receiveMessage();
        assertEquals("after", again.getMessageText());
    }

    @Test void messageWithVisibilityDelay_isHiddenThenAppears() throws Exception {
        QueueClient q = queue();
        q.sendMessageWithResponse("later", Duration.ofSeconds(1), null, null, Context.NONE);
        assertNull(q.peekMessage());
        Thread.sleep(1300);
        assertEquals("later", q.peekMessage().getMessageText());
    }

    @Test void messageTimeToLive_isReflectedInExpirationTime_andMinusOneMeansNever() {
        QueueClient q = queue();
        SendMessageResult s = q.sendMessageWithResponse("ttl", null, Duration.ofSeconds(120), null, Context.NONE).getValue();
        assertTrue(Duration.between(s.getInsertionTime(), s.getExpirationTime()).getSeconds() == 120);
        SendMessageResult never = q.sendMessageWithResponse("forever", null, Duration.ofSeconds(-1), null, Context.NONE).getValue();
        // Azure answers year 9999 for a message that never expires
        assertEquals(9999, never.getExpirationTime().getYear());
    }

    @Test void expiredMessage_isNotReturned() throws Exception {
        QueueClient q = queue();
        q.sendMessageWithResponse("short", null, Duration.ofSeconds(1), null, Context.NONE);
        Thread.sleep(1500);
        assertNull(q.receiveMessage());
    }

    @Test void clearMessages_emptiesTheQueue() {
        QueueClient q = queue();
        for (int i = 0; i < 5; i++) q.sendMessage("x" + i);
        q.clearMessages();
        assertNull(q.peekMessage());
    }

    @Test void messageTextWithXmlCharactersAndUnicode_roundTrips() {
        QueueClient q = queue();
        String text = "<tag attr=\"v\">&amp; 'quote' é日本😀 \n line2</tag>";
        q.sendMessage(text);
        assertEquals(text, q.receiveMessage().getMessageText());
    }

    @Test void base64MessageEncoding_isStoredAsBase64OnTheWireAndDecodedByTheClient() {
        String n = Emu.dashed("q");
        QueueClient b64 = new QueueClientBuilder().endpoint(emu.queueUrl(acct)).queueName(n).credential(cred).messageEncoding(QueueMessageEncoding.BASE64).buildClient();
        b64.create();
        b64.sendMessage("encode me é");
        QueueClient raw = new QueueClientBuilder().endpoint(emu.queueUrl(acct)).queueName(n).credential(cred).buildClient();
        String onWire = raw.peekMessage().getMessageText();
        assertEquals(Base64.getEncoder().encodeToString("encode me é".getBytes(java.nio.charset.StandardCharsets.UTF_8)), onWire);
        assertEquals("encode me é", b64.receiveMessage().getMessageText());
    }

    @Test void base64Client_readingANonBase64Message_reportsADecodingError_viaTheHandler() {
        String n = Emu.dashed("q");
        QueueClient raw = new QueueClientBuilder().endpoint(emu.queueUrl(acct)).queueName(n).credential(cred).buildClient();
        raw.create();
        raw.sendMessage("not base64 !!");
        List<String> errors = new ArrayList<>();
        QueueClient b64 = new QueueClientBuilder().endpoint(emu.queueUrl(acct)).queueName(n).credential(cred).messageEncoding(QueueMessageEncoding.BASE64)
            .processMessageDecodingError(e -> errors.add(e.getQueueMessageItem().getMessageText())).buildClient();
        assertNull(b64.receiveMessage());
        assertEquals(List.of("not base64 !!"), errors);
    }

    @Test void messageOf64KiBPlus_is413RequestBodyTooLarge() {
        QueueClient q = queue();
        QueueStorageException e = err(() -> q.sendMessage("x".repeat(65 * 1024)));
        assertEquals(413, e.getStatusCode());
        assertEquals(QueueErrorCode.REQUEST_BODY_TOO_LARGE, e.getErrorCode());
        q.sendMessage("y".repeat(60 * 1024));
    }

    // ---- access -------------------------------------------------------------------------------------------
    @Test void queueSas_readOnly_peeksButCannotSend_forDevAccount() {
        StorageSharedKeyCredential dev = new StorageSharedKeyCredential(Emu.DEV_ACCOUNT, Emu.DEV_KEY);
        QueueClient q = new QueueServiceClientBuilder().endpoint(emu.queueUrl(Emu.DEV_ACCOUNT)).credential(dev).buildClient().createQueue(Emu.dashed("q"));
        q.sendMessage("via sas");
        String sas = q.generateSas(new QueueServiceSasSignatureValues(OffsetDateTime.now().plusHours(1), new QueueSasPermission().setReadPermission(true)));
        QueueClient viaSas = new QueueClientBuilder().endpoint(emu.queueUrl(Emu.DEV_ACCOUNT)).queueName(q.getQueueName()).sasToken(sas).buildClient();
        assertEquals("via sas", viaSas.peekMessage().getMessageText());
        QueueStorageException e = err(() -> viaSas.sendMessage("nope"));
        assertEquals(403, e.getStatusCode());
        assertEquals(QueueErrorCode.AUTHORIZATION_PERMISSION_MISMATCH, e.getErrorCode());
    }

    @Test void queueSas_tamperedSignature_is403AuthenticationFailed() {
        StorageSharedKeyCredential dev = new StorageSharedKeyCredential(Emu.DEV_ACCOUNT, Emu.DEV_KEY);
        QueueClient q = new QueueServiceClientBuilder().endpoint(emu.queueUrl(Emu.DEV_ACCOUNT)).credential(dev).buildClient().createQueue(Emu.dashed("q"));
        String sas = q.generateSas(new QueueServiceSasSignatureValues(OffsetDateTime.now().plusHours(1), new QueueSasPermission().setReadPermission(true)));
        String tampered = sas.replace("sp=r", "sp=rapu");
        QueueClient viaSas = new QueueClientBuilder().endpoint(emu.queueUrl(Emu.DEV_ACCOUNT)).queueName(q.getQueueName()).sasToken(tampered).buildClient();
        QueueStorageException e = err(() -> viaSas.sendMessage("x"));
        assertEquals(403, e.getStatusCode());
        assertEquals(QueueErrorCode.AUTHENTICATION_FAILED, e.getErrorCode());
    }

    @Test void sharedKey_wrongKey_is403() {
        String wrong = Base64.getEncoder().encodeToString(Util.bytes(64, 9));
        QueueServiceClient bad = new QueueServiceClientBuilder().endpoint(emu.queueUrl(Emu.DEV_ACCOUNT)).credential(new StorageSharedKeyCredential(Emu.DEV_ACCOUNT, wrong)).buildClient();
        QueueStorageException e = err(() -> bad.createQueue(Emu.dashed("q")));
        assertEquals(403, e.getStatusCode());
        assertEquals(QueueErrorCode.AUTHENTICATION_FAILED, e.getErrorCode());
    }

    @Test void storedAccessPolicy_roundTrips() {
        QueueClient q = queue();
        q.setAccessPolicy(List.of(new QueueSignedIdentifier().setId("p1").setAccessPolicy(new QueueAccessPolicy().setPermissions("r").setExpiresOn(OffsetDateTime.now().plusDays(1)))));
        List<QueueSignedIdentifier> ids = q.getAccessPolicy().stream().collect(Collectors.toList());
        assertEquals(1, ids.size());
        assertEquals("p1", ids.get(0).getId());
        assertEquals("r", ids.get(0).getAccessPolicy().getPermissions());
    }

    @Test void serviceProperties_roundTrip() {
        QueueServiceProperties p = svc.getProperties();
        p.setCors(List.of(new QueueCorsRule().setAllowedOrigins("https://example.com").setAllowedMethods("GET").setAllowedHeaders("x-ms-*").setExposedHeaders("x-ms-*").setMaxAgeInSeconds(30)));
        svc.setProperties(p);
        assertEquals("https://example.com", svc.getProperties().getCors().get(0).getAllowedOrigins());
    }

    @Test void asyncClient_sendAndReceive() {
        QueueAsyncClient q = new QueueClientBuilder().endpoint(emu.queueUrl(acct)).queueName(Emu.dashed("q")).credential(cred).buildAsyncClient();
        q.create().block();
        q.sendMessage("async").block();
        QueueMessageItem m = q.receiveMessages(1).blockFirst();
        assertEquals("async", m.getMessageText());
    }
}
