package azemu;

import com.azure.core.util.BinaryData;
import com.azure.core.util.Context;
import com.azure.storage.blob.*;
import com.azure.storage.blob.models.*;
import com.azure.storage.blob.options.*;
import com.azure.storage.common.StorageSharedKeyCredential;
import org.junit.jupiter.api.*;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.*;

import static org.junit.jupiter.api.Assertions.*;

/**
 * Quick query (Query Blob Contents) with the Java SDK: BlobClient.openQueryInputStream and queryWithResponse. Expectations come
 * from the SDK's own live tests and the REST page "Query Blob Contents".
 */
@EmuTest
class BlobQueryTest {
    static Emu emu;
    static BlobServiceClient svc;

    static final String CSV = "id,name,price\r\n1,apple,0.5\r\n2,banana,0.25\r\n3,\"cherry, red\",4\r\n";
    static final String LINES = CSV.replace("\r\n", "\n");

    @BeforeAll static void setUp() {
        emu = Emu.current();
        String acct = Emu.name("queryacct");
        svc = new BlobServiceClientBuilder().endpoint(emu.blobUrl(acct))
            .credential(new StorageSharedKeyCredential(acct, Emu.DEV_KEY)).buildClient();
    }

    static BlobClient blob(String data) {
        BlobContainerClient c = svc.createBlobContainer(Emu.dashed("c"));
        BlobClient b = c.getBlobClient(Emu.dashed("b"));
        b.upload(BinaryData.fromString(data), true);
        return b;
    }

    static String read(InputStream in) throws IOException {
        try (in) { return new String(in.readAllBytes(), StandardCharsets.UTF_8); }
    }

    static String query(BlobClient b, BlobQueryOptions o) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        BlobQueryOptions with = new BlobQueryOptions(o.getExpression(), out)
            .setInputSerialization(o.getInputSerialization()).setOutputSerialization(o.getOutputSerialization())
            .setErrorConsumer(o.getErrorConsumer()).setProgressConsumer(o.getProgressConsumer())
            .setRequestConditions(o.getRequestConditions());
        b.queryWithResponse(with, null, Context.NONE);
        return out.toString(StandardCharsets.UTF_8);
    }

    @Test void selectStar_returnsTheBlobWithPlainLineEnds() throws IOException {
        BlobClient b = blob(CSV);
        assertEquals(LINES, read(b.openQueryInputStream("SELECT * from BlobStorage")));
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        b.query(out, "SELECT * from BlobStorage");
        assertEquals(LINES, out.toString(StandardCharsets.UTF_8));
    }

    @Test void queryWithResponse_carriesTheBlobHeaders() {
        BlobClient b = blob(CSV);
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        BlobQueryResponse r = b.queryWithResponse(new BlobQueryOptions("SELECT * from BlobStorage", out), null, Context.NONE);
        assertEquals(200, r.getStatusCode());
        assertNotNull(r.getDeserializedHeaders().getETag());
        assertNotNull(r.getDeserializedHeaders().getLastModified());
        assertEquals("avro/binary", r.getHeaders().getValue("Content-Type"));
        assertEquals(LINES, out.toString(StandardCharsets.UTF_8));
    }

    @Test void csvInAndOut_withHeadersWhereAndProjection() {
        BlobClient b = blob(CSV);
        BlobQueryDelimitedSerialization in = new BlobQueryDelimitedSerialization().setRecordSeparator('\n').setHeadersPresent(true);
        BlobQueryDelimitedSerialization out = new BlobQueryDelimitedSerialization().setRecordSeparator('\n');
        String got = query(b, new BlobQueryOptions("SELECT name, price * 2 AS dbl FROM BlobStorage WHERE id > 1")
            .setInputSerialization(in).setOutputSerialization(out));
        assertEquals("banana,0.5\n\"cherry, red\",8\n", got);
    }

    @Test void customOutputSeparators_andAHeaderRow() {
        BlobClient b = blob(CSV);
        BlobQueryDelimitedSerialization in = new BlobQueryDelimitedSerialization().setRecordSeparator('\n').setHeadersPresent(true);
        BlobQueryDelimitedSerialization out = new BlobQueryDelimitedSerialization().setRecordSeparator('|').setColumnSeparator(';')
            .setFieldQuote('\'').setEscapeChar('\\').setHeadersPresent(true);
        String got = query(b, new BlobQueryOptions("SELECT id, name FROM BlobStorage").setInputSerialization(in).setOutputSerialization(out));
        assertEquals("id;name|1;apple|2;banana|3;cherry, red|", got);
    }

    @Test void jsonInAndOut_missingFieldsAreLeftOut() {
        BlobClient b = blob("{\"name\": \"owner\", \"id\": 1}\n{\"name2\": \"owner2\"}\n{\"name\": \"owner\", \"id\": 1}");
        BlobQueryJsonSerialization in = new BlobQueryJsonSerialization().setRecordSeparator('\n');
        BlobQueryJsonSerialization out = new BlobQueryJsonSerialization().setRecordSeparator(';');
        assertEquals("{\"name\":\"owner\"};{};{\"name\":\"owner\"};",
            query(b, new BlobQueryOptions("SELECT name FROM BlobStorage").setInputSerialization(in).setOutputSerialization(out)));
    }

    @Test void jsonPaths() {
        BlobClient b = blob("{\"id\":1,\"warehouses\":[{\"latitude\":41.8},{\"latitude\":1.5}],\"dimensions\":{\"length\":3}}");
        BlobQueryJsonSerialization j = new BlobQueryJsonSerialization().setRecordSeparator('\n');
        assertEquals("{\"latitude\":41.8}\n{\"latitude\":1.5}\n",
            query(b, new BlobQueryOptions("SELECT latitude FROM BlobStorage[*].warehouses[*]").setInputSerialization(j).setOutputSerialization(j)));
        assertEquals("{\"length\":3}\n",
            query(b, new BlobQueryOptions("SELECT length FROM BlobStorage[*].dimensions").setInputSerialization(j).setOutputSerialization(j)));
    }

    @Test void aggregates_andLimit() {
        BlobClient b = blob("1\n2\n3\n4\n");
        assertEquals("4,10,2.5\n", query(b, new BlobQueryOptions("SELECT COUNT(*), SUM(CAST(_1 AS INT)), AVG(CAST(_1 AS INT)) FROM BlobStorage")));
        assertEquals("1\n2\n", query(b, new BlobQueryOptions("SELECT * FROM BlobStorage LIMIT 2")));
    }

    @Test void progressIsReported() {
        BlobClient b = blob(CSV);
        List<Long> seen = new ArrayList<>();
        query(b, new BlobQueryOptions("SELECT * FROM BlobStorage").setProgressConsumer(p -> seen.add(p.getBytesScanned())));
        assertFalse(seen.isEmpty());
        assertEquals((long) CSV.length(), seen.get(seen.size() - 1));
    }

    @Test void errorsArriveInTheErrorConsumer() {
        BlobClient bad = blob("{name: owner}\n");
        List<BlobQueryError> errors = new ArrayList<>();
        String got = query(bad, new BlobQueryOptions("SELECT * FROM BlobStorage")
            .setInputSerialization(new BlobQueryJsonSerialization().setRecordSeparator('\n'))
            .setOutputSerialization(new BlobQueryDelimitedSerialization().setRecordSeparator('\n'))
            .setErrorConsumer(errors::add));
        assertEquals("", got);
        assertEquals(1, errors.size());
        assertTrue(errors.get(0).isFatal());
        assertEquals("ParseError", errors.get(0).getName());
        assertEquals(0, errors.get(0).getPosition());

        List<BlobQueryError> rows = new ArrayList<>();
        String out = query(blob(CSV + "lonely\r\n"), new BlobQueryOptions("SELECT price FROM BlobStorage")
            .setInputSerialization(new BlobQueryDelimitedSerialization().setRecordSeparator('\n').setHeadersPresent(true))
            .setOutputSerialization(new BlobQueryDelimitedSerialization().setRecordSeparator('\n'))
            .setErrorConsumer(rows::add));
        assertEquals("0.5\n0.25\n4\n", out);
        assertEquals(1, rows.size());
        assertFalse(rows.get(0).isFatal());
        assertEquals("InvalidColumnOrdinal", rows.get(0).getName());
        assertEquals(CSV.length(), rows.get(0).getPosition());
    }

    @Test void missingBlob_is404_andAFailedConditionIs412() {
        BlobClient b = blob(CSV);
        BlobStorageException e = assertThrows(BlobStorageException.class,
            () -> b.getContainerClient().getBlobClient("nope").openQueryInputStream("SELECT * FROM BlobStorage"));
        assertEquals(404, e.getStatusCode());
        BlobStorageException e2 = assertThrows(BlobStorageException.class,
            () -> query(b, new BlobQueryOptions("SELECT * FROM BlobStorage").setRequestConditions(new BlobRequestConditions().setIfMatch("\"0x1\""))));
        assertEquals(412, e2.getStatusCode());
    }

    @Test void aSnapshotIsQueriedAsItWas() throws IOException {
        BlobClient b = blob("old\n");
        String snap = b.createSnapshot().getSnapshotId();
        b.upload(BinaryData.fromString("new\n"), true);
        assertEquals("new\n", read(b.openQueryInputStream("SELECT * FROM BlobStorage")));
        assertEquals("old\n", read(b.getSnapshotClient(snap).openQueryInputStream("SELECT * FROM BlobStorage")));
    }

    @Test void aBigResultArrivesWhole() throws IOException {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 150_000; i++) sb.append(i).append(",value-").append(i).append('\n');
        BlobClient b = blob(sb.toString());
        assertEquals(sb.toString(), read(b.openQueryInputStream("SELECT * FROM BlobStorage")));
    }

    @Test void parquetInput_andArrowOutput_areRefusedWith501() {
        BlobClient b = blob(CSV);
        BlobStorageException e = assertThrows(BlobStorageException.class, () -> query(b,
            new BlobQueryOptions("select * from blobstorage").setInputSerialization(new BlobQueryParquetSerialization())));
        assertEquals(501, e.getStatusCode());
        BlobStorageException e2 = assertThrows(BlobStorageException.class, () -> query(b,
            new BlobQueryOptions("SELECT _2 FROM BlobStorage").setOutputSerialization(
                new BlobQueryArrowSerialization().setSchema(List.of(new BlobQueryArrowField(BlobQueryArrowFieldType.DECIMAL).setName("abc").setPrecision(4).setScale(2))))));
        assertEquals(501, e2.getStatusCode());
    }
}
