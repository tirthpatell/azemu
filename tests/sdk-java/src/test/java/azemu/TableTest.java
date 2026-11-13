package azemu;

import com.azure.core.credential.AzureNamedKeyCredential;
import com.azure.core.http.rest.PagedResponse;
import com.azure.core.util.Context;
import com.azure.data.tables.*;
import com.azure.data.tables.models.*;
import com.azure.data.tables.sas.*;
import org.junit.jupiter.api.*;

import java.time.OffsetDateTime;
import java.time.ZoneOffset;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

@EmuTest
class TableTest {
    static Emu emu;
    static String acct;
    static AzureNamedKeyCredential cred;
    static TableServiceClient svc;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        acct = Emu.name("tacct");
        cred = new AzureNamedKeyCredential(acct, Emu.DEV_KEY);
        svc = new TableServiceClientBuilder().endpoint(emu.tableUrl(acct)).credential(cred).buildClient();
    }

    static String tname() { return "t" + Emu.name("tbl"); }
    TableClient table() { return svc.createTable(tname()); }

    static TableServiceException err(org.junit.jupiter.api.function.Executable e) { return assertThrows(TableServiceException.class, e); }

    // ---- tables -------------------------------------------------------------------------------------------
    @Test void createTable_twice_is409TableAlreadyExists() {
        String n = tname();
        svc.createTable(n);
        TableServiceException e = err(() -> svc.createTable(n));
        assertEquals(409, e.getResponse().getStatusCode());
        assertEquals("TableAlreadyExists", e.getValue().getErrorCode().toString());
    }

    @Test void createTableIfNotExists_secondCallDoesNotFail_andListTablesWithFilter() {
        String n = tname();
        svc.createTableIfNotExists(n);
        svc.createTableIfNotExists(n);
        List<String> names = svc.listTables(new ListTablesOptions().setFilter("TableName eq '" + n + "'"), null, Context.NONE).stream().map(TableItem::getName).collect(Collectors.toList());
        assertEquals(List.of(n), names);
    }

    @Test void deleteTable_thenEntityOperations_are404TableNotFound() {
        TableClient t = table();
        t.deleteTable();
        TableServiceException e = err(() -> t.createEntity(new TableEntity("p", "r")));
        assertEquals(404, e.getResponse().getStatusCode());
        assertEquals("TableNotFound", e.getValue().getErrorCode().toString());
    }

    @Test void invalidTableName_is400InvalidResourceName() {
        TableServiceException e = err(() -> svc.createTable("1-bad_name"));
        assertEquals(400, e.getResponse().getStatusCode());
    }

    // ---- entities -----------------------------------------------------------------------------------------
    @Test void createGetEntity_withEveryEdmType_roundTripsValuesAndTypes() {
        TableClient t = table();
        UUID guid = UUID.randomUUID();
        OffsetDateTime when = OffsetDateTime.of(2024, 5, 6, 7, 8, 9, 123_000_000, ZoneOffset.UTC);
        byte[] bin = Util.bytes(40, 1);
        TableEntity e = new TableEntity("pk", "rk")
            .addProperty("S", "text é日")
            .addProperty("I", 42)
            .addProperty("L", Long.MAX_VALUE - 1)
            .addProperty("D", 3.14159)
            .addProperty("B", true)
            .addProperty("G", guid)
            .addProperty("X", bin)
            .addProperty("T", when);
        t.createEntity(e);
        TableEntity got = t.getEntity("pk", "rk");
        assertEquals("text é日", got.getProperty("S"));
        assertEquals(42, got.getProperty("I"));
        assertEquals(Long.MAX_VALUE - 1, got.getProperty("L"));
        assertEquals(3.14159, (Double) got.getProperty("D"), 0.0);
        assertEquals(true, got.getProperty("B"));
        assertEquals(guid, got.getProperty("G"));
        assertArrayEquals(bin, (byte[]) got.getProperty("X"));
        assertEquals(when, got.getProperty("T"));
        assertNotNull(got.getETag());
        assertNotNull(got.getTimestamp());
    }

    @Test void createEntity_twice_is409EntityAlreadyExists() {
        TableClient t = table();
        t.createEntity(new TableEntity("p", "r"));
        TableServiceException e = err(() -> t.createEntity(new TableEntity("p", "r")));
        assertEquals(409, e.getResponse().getStatusCode());
        assertEquals("EntityAlreadyExists", e.getValue().getErrorCode().toString());
    }

    @Test void getMissingEntity_is404ResourceNotFound() {
        TableClient t = table();
        TableServiceException e = err(() -> t.getEntity("p", "nope"));
        assertEquals(404, e.getResponse().getStatusCode());
        assertEquals("ResourceNotFound", e.getValue().getErrorCode().toString());
    }

    @Test void updateEntity_mergeKeepsOtherProperties_replaceDropsThem() {
        TableClient t = table();
        t.createEntity(new TableEntity("p", "r").addProperty("a", 1).addProperty("b", 2));
        t.updateEntity(new TableEntity("p", "r").addProperty("b", 20).addProperty("c", 30), TableEntityUpdateMode.MERGE);
        TableEntity m = t.getEntity("p", "r");
        assertEquals(1, m.getProperty("a"));
        assertEquals(20, m.getProperty("b"));
        assertEquals(30, m.getProperty("c"));
        t.updateEntity(new TableEntity("p", "r").addProperty("z", 9), TableEntityUpdateMode.REPLACE);
        TableEntity r = t.getEntity("p", "r");
        assertNull(r.getProperty("a"));
        assertEquals(9, r.getProperty("z"));
    }

    @Test void updateEntity_withStaleEtag_is412UpdateConditionNotSatisfied() {
        TableClient t = table();
        t.createEntity(new TableEntity("p", "r").addProperty("v", 1));
        TableEntity mine = t.getEntity("p", "r");
        t.updateEntity(new TableEntity("p", "r").addProperty("v", 2), TableEntityUpdateMode.REPLACE);   // someone else changes it
        TableServiceException e = err(() -> t.updateEntityWithResponse(mine.addProperty("v", 3), TableEntityUpdateMode.REPLACE, true, null, Context.NONE));
        assertEquals(412, e.getResponse().getStatusCode());
        assertEquals("UpdateConditionNotSatisfied", e.getValue().getErrorCode().toString());
        assertEquals(2, t.getEntity("p", "r").getProperty("v"));
    }

    @Test void upsertEntity_createsThenMerges() {
        TableClient t = table();
        t.upsertEntity(new TableEntity("p", "r").addProperty("a", 1));
        t.upsertEntity(new TableEntity("p", "r").addProperty("b", 2));
        TableEntity got = t.getEntity("p", "r");
        assertEquals(1, got.getProperty("a"));
        assertEquals(2, got.getProperty("b"));
    }

    @Test void deleteEntity_thenDeleteAgain_isIdempotentForTheSdk_andGetIs404() {
        TableClient t = table();
        t.createEntity(new TableEntity("p", "r"));
        t.deleteEntity("p", "r");
        assertEquals(404, err(() -> t.getEntity("p", "r")).getResponse().getStatusCode());
        assertDoesNotThrow(() -> t.deleteEntity("p", "r"));   // the SDK ignores 404 on delete
    }

    @Test void keysWithForbiddenCharacters_are400() {
        TableClient t = table();
        for (String bad : List.of("a/b", "a\\b", "a#b", "a?b")) {
            TableServiceException e = err(() -> t.createEntity(new TableEntity(bad, "r")));
            assertEquals(400, e.getResponse().getStatusCode(), bad);
        }
    }

    @Test void keysWithSpacesPercentAndUnicode_roundTrip() {
        TableClient t = table();
        for (String k : List.of("with space", "per%cent", "pl+us", "ünï日", "quote'single", "amp&eq=")) {
            t.createEntity(new TableEntity(k, k + "-r").addProperty("v", k));
            assertEquals(k, t.getEntity(k, k + "-r").getProperty("v"), k);
        }
    }

    // ---- queries ------------------------------------------------------------------------------------------
    TableClient seeded() {
        TableClient t = table();
        for (int i = 0; i < 25; i++) {
            t.createEntity(new TableEntity(i % 2 == 0 ? "even" : "odd", String.format("r%02d", i))
                .addProperty("N", i).addProperty("L", 10_000_000_000L + i).addProperty("S", "s" + (i % 5))
                .addProperty("D", i / 2.0).addProperty("F", i % 3 == 0));
        }
        return t;
    }

    List<String> keys(TableClient t, ListEntitiesOptions o) {
        return t.listEntities(o, null, Context.NONE).stream().map(e -> e.getRowKey()).collect(Collectors.toList());
    }

    @Test void filter_onPartitionKeyAndNumber_returnsMatchingRows() {
        TableClient t = seeded();
        assertEquals(List.of("r14", "r16", "r18", "r20", "r22", "r24"), keys(t, new ListEntitiesOptions().setFilter("PartitionKey eq 'even' and N gt 12")));
    }

    @Test void filter_onInt64AndDouble_andBoolean() {
        TableClient t = seeded();
        assertEquals(List.of("r22", "r24", "r23"), keys(t, new ListEntitiesOptions().setFilter("L gt 10000000021L")));   // ordered by partition (even, odd), then row key
        assertEquals(List.of("r00", "r02", "r01"), keys(t, new ListEntitiesOptions().setFilter("D lt 1.5")));
        assertEquals(9, keys(t, new ListEntitiesOptions().setFilter("F eq true")).size());
    }

    @Test void filter_onGuidAndDateTimeAndBinary() {
        TableClient t = table();
        UUID g = UUID.randomUUID();
        OffsetDateTime when = OffsetDateTime.of(2023, 1, 2, 3, 4, 5, 0, ZoneOffset.UTC);
        t.createEntity(new TableEntity("p", "hit").addProperty("G", g).addProperty("T", when).addProperty("X", new byte[]{1, 2, 3}));
        t.createEntity(new TableEntity("p", "miss").addProperty("G", UUID.randomUUID()).addProperty("T", when.plusDays(5)).addProperty("X", new byte[]{9}));
        assertEquals(List.of("hit"), keys(t, new ListEntitiesOptions().setFilter("G eq guid'" + g + "'")));
        assertEquals(List.of("hit"), keys(t, new ListEntitiesOptions().setFilter("T lt datetime'2023-01-03T00:00:00Z'")));
        assertEquals(List.of("hit"), keys(t, new ListEntitiesOptions().setFilter("X eq X'010203'")));
    }

    @Test void filter_withSpacesInStringValues_andNotAndOr() {
        TableClient t = table();
        t.createEntity(new TableEntity("p", "a").addProperty("S", "hello world"));
        t.createEntity(new TableEntity("p", "b").addProperty("S", "it's"));
        t.createEntity(new TableEntity("p", "c").addProperty("S", "other"));
        assertEquals(List.of("a"), keys(t, new ListEntitiesOptions().setFilter("S eq 'hello world'")));
        assertEquals(List.of("b"), keys(t, new ListEntitiesOptions().setFilter("S eq 'it''s'")));
        assertEquals(List.of("a", "c"), keys(t, new ListEntitiesOptions().setFilter("not (S eq 'it''s')")));
        assertEquals(List.of("b", "c"), keys(t, new ListEntitiesOptions().setFilter("RowKey eq 'b' or RowKey eq 'c'")));
    }

    @Test void select_returnsOnlyTheRequestedProperties() {
        TableClient t = seeded();
        TableEntity e = t.listEntities(new ListEntitiesOptions().setFilter("RowKey eq 'r05'").setSelect(List.of("N", "S")), null, Context.NONE).iterator().next();
        assertEquals(5, e.getProperty("N"));
        assertEquals("s0", e.getProperty("S"));
        assertNull(e.getProperty("L"));
    }

    @Test void paging_withTop_returnsContinuationAcrossPagesInKeyOrder() {
        TableClient t = seeded();
        List<Integer> sizes = new ArrayList<>();
        List<String> all = new ArrayList<>();
        for (PagedResponse<TableEntity> p : t.listEntities(new ListEntitiesOptions().setTop(10), null, Context.NONE).iterableByPage()) {
            sizes.add(p.getValue().size());
            p.getValue().forEach(e -> all.add(e.getPartitionKey() + "/" + e.getRowKey()));
        }
        assertEquals(List.of(10, 10, 5), sizes);
        List<String> sorted = new ArrayList<>(all);
        Collections.sort(sorted);
        assertEquals(sorted, all, "entities come back ordered by partition key then row key");
        assertEquals(25, new HashSet<>(all).size());
    }

    @Test void listEntities_over1000Rows_followsServerContinuation() {
        TableClient t = table();
        List<TableTransactionAction> batch = new ArrayList<>();
        for (int i = 0; i < 1100; i++) {
            batch.add(new TableTransactionAction(TableTransactionActionType.CREATE, new TableEntity("p", String.format("r%04d", i)).addProperty("v", i)));
            if (batch.size() == 100) { t.submitTransaction(batch); batch = new ArrayList<>(); }
        }
        List<Integer> sizes = new ArrayList<>();
        for (PagedResponse<TableEntity> p : t.listEntities().iterableByPage()) sizes.add(p.getValue().size());
        assertEquals(1100, sizes.stream().mapToInt(Integer::intValue).sum());
        assertEquals(1000, sizes.get(0));
    }

    // ---- transactions -------------------------------------------------------------------------------------
    @Test void transaction_mixedActions_commitTogether() {
        TableClient t = table();
        t.createEntity(new TableEntity("p", "existing").addProperty("v", 1));
        t.createEntity(new TableEntity("p", "gone"));
        List<TableTransactionAction> actions = List.of(
            new TableTransactionAction(TableTransactionActionType.CREATE, new TableEntity("p", "new").addProperty("v", 1)),
            new TableTransactionAction(TableTransactionActionType.UPDATE_MERGE, new TableEntity("p", "existing").addProperty("w", 2)),
            new TableTransactionAction(TableTransactionActionType.UPSERT_REPLACE, new TableEntity("p", "upserted").addProperty("v", 3)),
            new TableTransactionAction(TableTransactionActionType.DELETE, new TableEntity("p", "gone")));
        TableTransactionResult r = t.submitTransaction(actions);
        assertEquals(4, r.getTransactionActionResponses().size());
        // the Java SDK sends "Prefer: return-no-content" on the create, so 204 for every action
        assertEquals(List.of(204, 204, 204, 204), r.getTransactionActionResponses().stream().map(a -> a.getStatusCode()).collect(Collectors.toList()));
        assertEquals(List.of("existing", "new", "upserted"), keys(t, new ListEntitiesOptions()));
        assertEquals(1, t.getEntity("p", "existing").getProperty("v"));
        assertEquals(2, t.getEntity("p", "existing").getProperty("w"));
    }

    @Test void transaction_withAFailingAction_rollsEverythingBack_andNamesTheFailedIndex() {
        TableClient t = table();
        t.createEntity(new TableEntity("p", "dup"));
        List<TableTransactionAction> actions = List.of(
            new TableTransactionAction(TableTransactionActionType.CREATE, new TableEntity("p", "fresh")),
            new TableTransactionAction(TableTransactionActionType.CREATE, new TableEntity("p", "dup")));
        TableTransactionFailedException e = assertThrows(TableTransactionFailedException.class, () -> t.submitTransaction(actions));
        assertEquals(1, e.getFailedTransactionActionIndex());
        assertEquals("EntityAlreadyExists", e.getValue().getErrorCode().toString());
        assertEquals(List.of("dup"), keys(t, new ListEntitiesOptions()));
    }

    @Test void transaction_of101Actions_isRefused_andOfDifferentPartitions_is400() {
        TableClient t = table();
        List<TableTransactionAction> many = new ArrayList<>();
        for (int i = 0; i < 101; i++) many.add(new TableTransactionAction(TableTransactionActionType.CREATE, new TableEntity("p", "r" + i)));
        assertThrows(RuntimeException.class, () -> t.submitTransaction(many));
        assertEquals(0, keys(t, new ListEntitiesOptions()).size());
        List<TableTransactionAction> twoPartitions = List.of(
            new TableTransactionAction(TableTransactionActionType.CREATE, new TableEntity("p1", "r")),
            new TableTransactionAction(TableTransactionActionType.CREATE, new TableEntity("p2", "r")));
        assertThrows(RuntimeException.class, () -> t.submitTransaction(twoPartitions));
        assertEquals(0, keys(t, new ListEntitiesOptions()).size());
    }

    // ---- access -------------------------------------------------------------------------------------------
    @Test void tableSas_readOnly_queriesButCannotAdd_forDevAccount() {
        AzureNamedKeyCredential dev = new AzureNamedKeyCredential(Emu.DEV_ACCOUNT, Emu.DEV_KEY);
        TableClient t = new TableServiceClientBuilder().endpoint(emu.tableUrl(Emu.DEV_ACCOUNT)).credential(dev).buildClient().createTable(tname());
        t.createEntity(new TableEntity("p", "r").addProperty("v", 1));
        String sas = t.generateSas(new TableSasSignatureValues(OffsetDateTime.now().plusHours(1), new TableSasPermission().setReadPermission(true)));
        TableClient viaSas = new TableClientBuilder().endpoint(emu.tableUrl(Emu.DEV_ACCOUNT)).tableName(t.getTableName()).sasToken(sas).buildClient();
        assertEquals(1, viaSas.getEntity("p", "r").getProperty("v"));
        TableServiceException e = err(() -> viaSas.createEntity(new TableEntity("p", "r2")));
        assertEquals(403, e.getResponse().getStatusCode());
    }

    @Test void sharedKey_wrongKey_is403() {
        String wrong = Base64.getEncoder().encodeToString(Util.bytes(64, 4));
        TableServiceClient bad = new TableServiceClientBuilder().endpoint(emu.tableUrl(Emu.DEV_ACCOUNT)).credential(new AzureNamedKeyCredential(Emu.DEV_ACCOUNT, wrong)).buildClient();
        TableServiceException e = err(() -> bad.createTable(tname()));
        assertEquals(403, e.getResponse().getStatusCode());
    }

    @Test void connectionString_client_andAsyncClient_work() {
        String a = Emu.name("tcs");
        TableServiceClient s = new TableServiceClientBuilder().connectionString(emu.storageConnStr(a)).buildClient();
        TableClient t = s.createTable(tname());
        t.createEntity(new TableEntity("p", "r").addProperty("v", "cs"));
        TableAsyncClient ta = new TableServiceClientBuilder().connectionString(emu.storageConnStr(a)).buildAsyncClient().getTableClient(t.getTableName());
        assertEquals("cs", ta.getEntity("p", "r").block().getProperty("v"));
    }

    @Test void tableAcl_roundTrips() {
        TableClient t = table();
        t.setAccessPolicies(List.of(new TableSignedIdentifier("id1").setAccessPolicy(new TableAccessPolicy().setPermissions("r").setExpiresOn(OffsetDateTime.now().plusDays(1)))));
        assertEquals("id1", t.getAccessPolicies().getIdentifiers().get(0).getId());
    }
}
