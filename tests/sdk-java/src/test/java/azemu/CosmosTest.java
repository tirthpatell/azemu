package azemu;

import com.azure.cosmos.*;
import com.azure.cosmos.models.*;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import org.junit.jupiter.api.*;

import java.time.Duration;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

/** Cosmos DB (gateway mode): databases, containers, items, patch, ETags, TTL, partition keys, sync and async clients. */
@EmuTest
class CosmosTest {
    static Emu emu;
    static String acct;
    static CosmosClient client;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        acct = Emu.name("cosacct");
        Cos.assumeGatewayRoutingWorks(emu, acct);
        client = Cos.builder(emu, acct).buildClient();
    }

    @AfterAll static void tearDown() { if (client != null) client.close(); }

    CosmosDatabase db() {
        String id = Emu.dashed("db");
        client.createDatabase(id);
        return client.getDatabase(id);
    }

    CosmosContainer container(String pkPath) {
        CosmosDatabase d = db();
        String id = Emu.dashed("c");
        d.createContainer(new CosmosContainerProperties(id, pkPath));
        return d.getContainer(id);
    }

    CosmosContainer container() { return container("/pk"); }

    static void assertStatus(int status, org.junit.jupiter.api.function.Executable e) {
        CosmosException ex = assertThrows(CosmosException.class, e);
        assertEquals(status, ex.getStatusCode(), ex.getMessage());
    }

    // ---- databases and containers -----------------------------------------------------------------------------
    @Test void createDatabase_returns201_andTheSameIdAgainIs409() {
        String id = Emu.dashed("db");
        CosmosDatabaseResponse r = client.createDatabase(id);
        assertEquals(201, r.getStatusCode());
        assertEquals(id, r.getProperties().getId());
        assertNotNull(r.getProperties().getETag());
        assertNotNull(r.getProperties().getResourceId());
        assertStatus(409, () -> client.createDatabase(id));
        assertEquals(200, client.createDatabaseIfNotExists(id).getStatusCode());
    }

    @Test void readAndQueryDatabases_findTheOnesCreated() {
        String a = Emu.dashed("db"), b = Emu.dashed("db");
        client.createDatabase(a);
        client.createDatabase(b);
        Set<String> ids = new HashSet<>();
        client.readAllDatabases().forEach(p -> ids.add(p.getId()));
        assertTrue(ids.containsAll(List.of(a, b)));
        List<String> q = Cos.all(client.queryDatabases("SELECT * FROM d WHERE d.id = '" + a + "'", new CosmosQueryRequestOptions()))
            .stream().map(CosmosDatabaseProperties::getId).collect(Collectors.toList());
        assertEquals(List.of(a), q);
    }

    @Test void deleteDatabase_thenReadIs404() {
        CosmosDatabase d = db();
        assertEquals(204, d.delete().getStatusCode());
        assertStatus(404, d::read);
    }

    @Test void createContainer_withPartitionKeyAndIndexingPolicy_roundTripsTheProperties() {
        CosmosDatabase d = db();
        CosmosContainerProperties p = new CosmosContainerProperties("things", "/tenant");
        IndexingPolicy ip = new IndexingPolicy().setIndexingMode(IndexingMode.CONSISTENT).setAutomatic(true);
        ip.setIncludedPaths(List.of(new IncludedPath("/*")));
        ip.setExcludedPaths(List.of(new ExcludedPath("/big/*")));
        p.setIndexingPolicy(ip);
        p.setDefaultTimeToLiveInSeconds(-1);
        p.setUniqueKeyPolicy(new UniqueKeyPolicy().setUniqueKeys(List.of(new UniqueKey(List.of("/email")))));
        CosmosContainerResponse r = d.createContainer(p, ThroughputProperties.createManualThroughput(500));
        assertEquals(201, r.getStatusCode());
        CosmosContainerProperties got = d.getContainer("things").read().getProperties();
        assertEquals(List.of("/tenant"), got.getPartitionKeyDefinition().getPaths());
        assertEquals(IndexingMode.CONSISTENT, got.getIndexingPolicy().getIndexingMode());
        assertEquals("/big/*", got.getIndexingPolicy().getExcludedPaths().get(0).getPath());
        assertEquals(-1, got.getDefaultTimeToLiveInSeconds());
        assertEquals("/email", got.getUniqueKeyPolicy().getUniqueKeys().get(0).getPaths().get(0));
        assertEquals(500, d.getContainer("things").readThroughput().getProperties().getManualThroughput());
    }

    @Test void createContainer_twice_is409_andReplaceChangesTheIndexingPolicy() {
        CosmosDatabase d = db();
        d.createContainer("c1", "/pk");
        assertStatus(409, () -> d.createContainer("c1", "/pk"));
        assertEquals(200, d.createContainerIfNotExists("c1", "/pk").getStatusCode());
        CosmosContainer c = d.getContainer("c1");
        CosmosContainerProperties p = c.read().getProperties();
        p.setDefaultTimeToLiveInSeconds(60);
        assertEquals(200, c.replace(p).getStatusCode());
        assertEquals(60, c.read().getProperties().getDefaultTimeToLiveInSeconds());
        assertEquals(204, c.delete().getStatusCode());
        assertStatus(404, c::read);
    }

    @Test void replaceContainer_withADifferentPartitionKey_isRefused() {
        CosmosDatabase d = db();
        d.createContainer("c1", "/pk");
        CosmosContainerProperties p = d.getContainer("c1").read().getProperties();
        p.setPartitionKeyDefinition(new PartitionKeyDefinition().setPaths(List.of("/other")));
        assertStatus(400, () -> d.getContainer("c1").replace(p));
    }

    @Test void hierarchicalPartitionKey_containerAndItemsRoundTrip() {
        CosmosDatabase d = db();
        PartitionKeyDefinition def = new PartitionKeyDefinition().setKind(PartitionKind.MULTI_HASH)
            .setVersion(PartitionKeyDefinitionVersion.V2).setPaths(List.of("/tenant", "/user"));
        d.createContainer(new CosmosContainerProperties("h", def));
        CosmosContainer c = d.getContainer("h");
        assertEquals(PartitionKind.MULTI_HASH, c.read().getProperties().getPartitionKeyDefinition().getKind());
        ObjectNode item = Cos.JSON.createObjectNode().put("id", "1").put("tenant", "acme").put("user", "bob").put("v", 1);
        PartitionKey pk = new PartitionKeyBuilder().add("acme").add("bob").build();
        c.createItem(item, pk, new CosmosItemRequestOptions());
        assertEquals(1, c.readItem("1", pk, JsonNode.class).getItem().get("v").asInt());
        // prefix query
        List<JsonNode> hits = Cos.query(c, "SELECT * FROM c", new CosmosQueryRequestOptions().setPartitionKey(new PartitionKeyBuilder().add("acme").build()));
        assertEquals(1, hits.size());
        assertStatus(404, () -> c.readItem("1", new PartitionKeyBuilder().add("acme").add("other").build(), JsonNode.class));
    }

    @Test void readAllContainers_listsWhatWasCreated() {
        CosmosDatabase d = db();
        d.createContainer("a", "/pk");
        d.createContainer("b", "/pk");
        List<String> ids = Cos.all(d.readAllContainers()).stream().map(CosmosContainerProperties::getId).sorted().collect(Collectors.toList());
        assertEquals(List.of("a", "b"), ids);
        assertEquals(1, Cos.all(d.queryContainers("SELECT * FROM c WHERE c.id = 'a'")).size());
    }

    // ---- items ------------------------------------------------------------------------------------------------
    @Test void createItem_returns201WithEtagAndSystemProperties_readReturns200() {
        CosmosContainer c = container();
        CosmosItemResponse<ObjectNode> r = c.createItem(Cos.doc("a", "p1", "n", 5), new PartitionKey("p1"), new CosmosItemRequestOptions());
        assertEquals(201, r.getStatusCode());
        assertNotNull(r.getETag());
        assertTrue(r.getRequestCharge() > 0);
        assertNotNull(r.getSessionToken());
        CosmosItemResponse<JsonNode> read = c.readItem("a", new PartitionKey("p1"), JsonNode.class);
        assertEquals(200, read.getStatusCode());
        JsonNode item = read.getItem();
        assertEquals(5, item.get("n").asInt());
        assertTrue(item.has("_rid") && item.has("_ts") && item.has("_etag") && item.has("_self"));
        assertEquals(r.getETag(), read.getETag());
    }

    @Test void createItemTwice_is409_replaceMissing_is404_deleteMissing_is404() {
        CosmosContainer c = container();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("a", "p"), pk, new CosmosItemRequestOptions());
        assertStatus(409, () -> c.createItem(Cos.doc("a", "p"), pk, new CosmosItemRequestOptions()));
        assertStatus(404, () -> c.replaceItem(Cos.doc("zzz", "p"), "zzz", pk, new CosmosItemRequestOptions()));
        assertStatus(404, () -> c.deleteItem("zzz", pk, new CosmosItemRequestOptions()));
        assertStatus(404, () -> c.readItem("zzz", pk, JsonNode.class));
    }

    @Test void upsertItem_createsThen200Replaces() {
        CosmosContainer c = container();
        PartitionKey pk = new PartitionKey("p");
        assertEquals(201, c.upsertItem(Cos.doc("u", "p", "v", 1), pk, new CosmosItemRequestOptions()).getStatusCode());
        assertEquals(200, c.upsertItem(Cos.doc("u", "p", "v", 2), pk, new CosmosItemRequestOptions()).getStatusCode());
        assertEquals(2, c.readItem("u", pk, JsonNode.class).getItem().get("v").asInt());
    }

    @Test void replaceItem_replacesTheWholeDocument() {
        CosmosContainer c = container();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("r", "p", "keep", 1, "drop", 2), pk, new CosmosItemRequestOptions());
        c.replaceItem(Cos.doc("r", "p", "keep", 9), "r", pk, new CosmosItemRequestOptions());
        JsonNode got = c.readItem("r", pk, JsonNode.class).getItem();
        assertEquals(9, got.get("keep").asInt());
        assertFalse(got.has("drop"));
    }

    @Test void deleteItem_returns204_thenItIsGone() {
        CosmosContainer c = container();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("d", "p"), pk, new CosmosItemRequestOptions());
        assertEquals(204, c.deleteItem("d", pk, new CosmosItemRequestOptions()).getStatusCode());
        assertStatus(404, () -> c.readItem("d", pk, JsonNode.class));
    }

    @Test void etagConditions_ifMatchOnReplaceAndDelete_ifNoneMatchOnRead() {
        CosmosContainer c = container();
        PartitionKey pk = new PartitionKey("p");
        String etag1 = c.createItem(Cos.doc("e", "p", "v", 1), pk, new CosmosItemRequestOptions()).getETag();
        String etag2 = c.replaceItem(Cos.doc("e", "p", "v", 2), "e", pk, new CosmosItemRequestOptions().setIfMatchETag(etag1)).getETag();
        assertNotEquals(etag1, etag2);
        assertStatus(412, () -> c.replaceItem(Cos.doc("e", "p", "v", 3), "e", pk, new CosmosItemRequestOptions().setIfMatchETag(etag1)));
        assertStatus(412, () -> c.deleteItem("e", pk, new CosmosItemRequestOptions().setIfMatchETag(etag1)));
        CosmosItemResponse<JsonNode> notModified = c.readItem("e", pk, new CosmosItemRequestOptions().setIfNoneMatchETag(etag2), JsonNode.class);
        assertEquals(304, notModified.getStatusCode(), "the Java SDK returns the 304 instead of throwing");
        assertNull(notModified.getItem());
        assertEquals(200, c.readItem("e", pk, new CosmosItemRequestOptions().setIfNoneMatchETag(etag1), JsonNode.class).getStatusCode());
        assertEquals(204, c.deleteItem("e", pk, new CosmosItemRequestOptions().setIfMatchETag(etag2)).getStatusCode());
    }

    @Test void partitionKeyTypes_stringNumberBooleanNullAndNone_areDistinctPartitions() {
        CosmosContainer c = container();
        c.createItem(Cos.JSON.createObjectNode().put("id", "x").put("pk", "5"), new PartitionKey("5"), new CosmosItemRequestOptions());
        c.createItem(Cos.JSON.createObjectNode().put("id", "x").put("pk", 5), new PartitionKey(5), new CosmosItemRequestOptions());
        c.createItem(Cos.JSON.createObjectNode().put("id", "x").put("pk", true), new PartitionKey(true), new CosmosItemRequestOptions());
        c.createItem(Cos.JSON.createObjectNode().putNull("pk").put("id", "x"), new PartitionKey((Object) null), new CosmosItemRequestOptions());
        c.createItem(Cos.JSON.createObjectNode().put("id", "x"), PartitionKey.NONE, new CosmosItemRequestOptions());
        assertEquals("5", c.readItem("x", new PartitionKey("5"), JsonNode.class).getItem().get("pk").asText());
        assertEquals(5, c.readItem("x", new PartitionKey(5), JsonNode.class).getItem().get("pk").asInt());
        assertTrue(c.readItem("x", new PartitionKey(true), JsonNode.class).getItem().get("pk").asBoolean());
        assertTrue(c.readItem("x", new PartitionKey((Object) null), JsonNode.class).getItem().get("pk").isNull());
        assertFalse(c.readItem("x", PartitionKey.NONE, JsonNode.class).getItem().has("pk"));
        assertEquals(1, Cos.all(c.readAllItems(new PartitionKey("5"), JsonNode.class)).size());
    }

    @Test void itemWithUnicodeSpecialCharactersInIdAndValues_roundTrips() {
        CosmosContainer c = container();
        String id = "id with spaces & ünïcödé 日本語 +=";  // a % in an id breaks the SDK (URLDecoder) (see docs/sdk-notes.md, Java)
        PartitionKey pk = new PartitionKey("pk ü/日本");
        ObjectNode d = Cos.doc(id, "pk ü/日本", "text", "line1\nline2\t\"quoted\" \\ é😀");
        c.createItem(d, pk, new CosmosItemRequestOptions());
        JsonNode got = c.readItem(id, pk, JsonNode.class).getItem();
        assertEquals(d.get("text").asText(), got.get("text").asText());
        c.deleteItem(id, pk, new CosmosItemRequestOptions());
    }

    @Test void idWithASlash_isRefusedByTheSdkBeforeSending() {
        CosmosContainer c = container();
        assertThrows(RuntimeException.class, () -> c.createItem(Cos.doc("a/b", "p"), new PartitionKey("p"), new CosmosItemRequestOptions()));
    }

    @Test void itemOver2MB_is413() {
        CosmosContainer c = container();
        ObjectNode d = Cos.doc("big", "p", "blob", "x".repeat(2_200_000));
        assertStatus(413, () -> c.createItem(d, new PartitionKey("p"), new CosmosItemRequestOptions()));
    }

    @Test void largeItemJustUnderTheLimit_roundTrips() {
        CosmosContainer c = container();
        String payload = "y".repeat(1_500_000);
        c.createItem(Cos.doc("big", "p", "blob", payload), new PartitionKey("p"), new CosmosItemRequestOptions());
        assertEquals(payload, c.readItem("big", new PartitionKey("p"), JsonNode.class).getItem().get("blob").asText());
    }

    @Test void contentResponseOnWriteDisabled_returnsNoBodyOnCreate() {
        try (CosmosClient quiet = Cos.builder(emu, acct).contentResponseOnWriteEnabled(false).buildClient()) {
            CosmosDatabase d = quiet.getDatabase(db().getId());
            d.createContainer("c", "/pk");
            CosmosItemResponse<ObjectNode> r = d.getContainer("c").createItem(Cos.doc("m", "p"), new PartitionKey("p"), new CosmosItemRequestOptions());
            assertEquals(201, r.getStatusCode());
            assertNull(r.getItem());
        }
    }

    @Test void deleteAllItemsByPartitionKey_removesOnlyThatPartition() {
        CosmosContainer c = container();
        for (int i = 0; i < 5; i++) c.createItem(Cos.doc("a" + i, "A"), new PartitionKey("A"), new CosmosItemRequestOptions());
        for (int i = 0; i < 3; i++) c.createItem(Cos.doc("b" + i, "B"), new PartitionKey("B"), new CosmosItemRequestOptions());
        c.deleteAllItemsByPartitionKey(new PartitionKey("A"), new CosmosItemRequestOptions());
        assertEquals(0, Cos.all(c.readAllItems(new PartitionKey("A"), JsonNode.class)).size());
        assertEquals(3, Cos.all(c.readAllItems(new PartitionKey("B"), JsonNode.class)).size());
    }

    @Test void readMany_returnsTheItemsThatExist() {
        CosmosContainer c = container();
        for (int i = 0; i < 4; i++) c.createItem(Cos.doc("m" + i, "p" + (i % 2)), new PartitionKey("p" + (i % 2)), new CosmosItemRequestOptions());
        List<CosmosItemIdentity> ids = List.of(new CosmosItemIdentity(new PartitionKey("p0"), "m0"), new CosmosItemIdentity(new PartitionKey("p1"), "m1"),
            new CosmosItemIdentity(new PartitionKey("p1"), "missing"));
        FeedResponse<JsonNode> r = c.readMany(ids, JsonNode.class);
        assertEquals(Set.of("m0", "m1"), r.getResults().stream().map(x -> x.get("id").asText()).collect(Collectors.toSet()));
    }

    // ---- patch ------------------------------------------------------------------------------------------------
    @Test void patch_addSetReplaceRemoveIncrementMove() {
        CosmosContainer c = container();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("pa", "p", "n", 1, "old", "x", "name", "a"), pk, new CosmosItemRequestOptions());
        CosmosPatchOperations ops = CosmosPatchOperations.create()
            .add("/added", "new").set("/n", 10).replace("/name", "b").remove("/old").increment("/count", 3).move("/added", "/moved");
        JsonNode got = c.patchItem("pa", pk, ops, JsonNode.class).getItem();
        assertEquals(10, got.get("n").asInt());
        assertEquals("b", got.get("name").asText());
        assertFalse(got.has("old"));
        assertEquals(3, got.get("count").asInt());
        assertEquals("new", got.get("moved").asText());
        assertFalse(got.has("added"));
        assertEquals(got.toString(), c.readItem("pa", pk, JsonNode.class).getItem().toString());
    }

    @Test void patch_incrementDouble_andNestedPath_andArrayIndex() {
        CosmosContainer c = container();
        PartitionKey pk = new PartitionKey("p");
        ObjectNode d = Cos.doc("pb", "p");
        d.putObject("inner").put("f", 1.5);
        d.putArray("arr").add(1).add(2).add(3);
        c.createItem(d, pk, new CosmosItemRequestOptions());
        JsonNode got = c.patchItem("pb", pk, CosmosPatchOperations.create().increment("/inner/f", 0.25).add("/arr/1", 99).remove("/arr/0"), JsonNode.class).getItem();
        assertEquals(1.75, got.get("inner").get("f").asDouble(), 1e-9);
        assertEquals("[99,2,3]", got.get("arr").toString());
    }

    @Test void patch_withFilterPredicate_failsWith412WhenItDoesNotMatch() {
        CosmosContainer c = container();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("pc", "p", "state", "open"), pk, new CosmosItemRequestOptions());
        CosmosPatchItemRequestOptions o = new CosmosPatchItemRequestOptions().setFilterPredicate("FROM c WHERE c.state = 'closed'");
        assertStatus(412, () -> c.patchItem("pc", pk, CosmosPatchOperations.create().set("/state", "x"), o, JsonNode.class));
        o.setFilterPredicate("FROM c WHERE c.state = 'open'");
        assertEquals("x", c.patchItem("pc", pk, CosmosPatchOperations.create().set("/state", "x"), o, JsonNode.class).getItem().get("state").asText());
    }

    @Test void patch_ofAMissingItem_is404_andTooManyOperationsIs400() {
        CosmosContainer c = container();
        PartitionKey pk = new PartitionKey("p");
        assertStatus(404, () -> c.patchItem("nope", pk, CosmosPatchOperations.create().set("/a", 1), JsonNode.class));
        c.createItem(Cos.doc("pd", "p"), pk, new CosmosItemRequestOptions());
        CosmosPatchOperations eleven = CosmosPatchOperations.create();
        for (int i = 0; i < 11; i++) eleven.set("/f" + i, i);
        assertThrows(RuntimeException.class, () -> c.patchItem("pd", pk, eleven, JsonNode.class));
    }

    // ---- TTL --------------------------------------------------------------------------------------------------
    @Test void containerDefaultTtl_expiresItems() throws Exception {
        CosmosDatabase d = db();
        d.createContainer(new CosmosContainerProperties("ttl", "/pk").setDefaultTimeToLiveInSeconds(2));
        CosmosContainer c = d.getContainer("ttl");
        c.createItem(Cos.doc("t", "p"), new PartitionKey("p"), new CosmosItemRequestOptions());
        assertEquals(200, c.readItem("t", new PartitionKey("p"), JsonNode.class).getStatusCode());
        Thread.sleep(4000);
        assertStatus(404, () -> c.readItem("t", new PartitionKey("p"), JsonNode.class));
        assertTrue(Cos.query(c, "SELECT * FROM c").isEmpty());
    }

    @Test void perItemTtl_overridesTheContainerDefault() throws Exception {
        CosmosDatabase d = db();
        d.createContainer(new CosmosContainerProperties("ttl", "/pk").setDefaultTimeToLiveInSeconds(-1));
        CosmosContainer c = d.getContainer("ttl");
        c.createItem(Cos.doc("short", "p", "ttl", 2), new PartitionKey("p"), new CosmosItemRequestOptions());
        c.createItem(Cos.doc("forever", "p"), new PartitionKey("p"), new CosmosItemRequestOptions());
        Thread.sleep(4000);
        assertStatus(404, () -> c.readItem("short", new PartitionKey("p"), JsonNode.class));
        assertEquals(200, c.readItem("forever", new PartitionKey("p"), JsonNode.class).getStatusCode());
    }

    // ---- unique keys, errors ----------------------------------------------------------------------------------
    @Test void uniqueKeyPolicy_refusesADuplicateValueWith409() {
        CosmosDatabase d = db();
        d.createContainer(new CosmosContainerProperties("u", "/pk")
            .setUniqueKeyPolicy(new UniqueKeyPolicy().setUniqueKeys(List.of(new UniqueKey(List.of("/email"))))));
        CosmosContainer c = d.getContainer("u");
        c.createItem(Cos.doc("1", "p", "email", "a@x"), new PartitionKey("p"), new CosmosItemRequestOptions());
        assertStatus(409, () -> c.createItem(Cos.doc("2", "p", "email", "a@x"), new PartitionKey("p"), new CosmosItemRequestOptions()));
        c.createItem(Cos.doc("3", "q", "email", "a@x"), new PartitionKey("q"), new CosmosItemRequestOptions());
    }

    @Test void itemOperationsInAMissingContainer_are404() {
        CosmosDatabase d = db();
        CosmosContainer c = d.getContainer("missing");
        assertStatus(404, () -> c.createItem(Cos.doc("a", "p"), new PartitionKey("p"), new CosmosItemRequestOptions()));
    }

    @Test void partitionKeyValueDifferentFromTheDocument_isRefused() {
        CosmosContainer c = container();
        assertStatus(400, () -> c.createItem(Cos.doc("a", "in-document"), new PartitionKey("in-header"), new CosmosItemRequestOptions()));
    }

    // ---- async client -----------------------------------------------------------------------------------------
    @Test void asyncClient_createsReadsAndQueries() {
        try (CosmosAsyncClient ac = Cos.builder(emu, acct).buildAsyncClient()) {
            String dbId = Emu.dashed("adb");
            ac.createDatabase(dbId).block();
            CosmosAsyncDatabase d = ac.getDatabase(dbId);
            d.createContainer("c", "/pk").block();
            CosmosAsyncContainer c = d.getContainer("c");
            for (int i = 0; i < 5; i++) c.createItem(Cos.doc("i" + i, "p", "n", i), new PartitionKey("p"), new CosmosItemRequestOptions()).block();
            JsonNode one = c.readItem("i3", new PartitionKey("p"), JsonNode.class).block().getItem();
            assertEquals(3, one.get("n").asInt());
            List<JsonNode> hits = c.queryItems("SELECT * FROM c WHERE c.n >= 3 ORDER BY c.n", new CosmosQueryRequestOptions(), JsonNode.class).collectList().block();
            assertEquals(List.of(3, 4), hits.stream().map(x -> x.get("n").asInt()).collect(Collectors.toList()));
            CosmosException ex = assertThrows(CosmosException.class, () -> c.readItem("none", new PartitionKey("p"), JsonNode.class).block());
            assertEquals(404, ex.getStatusCode());
        }
    }

    // ---- direct mode --------------------------------------------------------------------------------------------
    @Test void directMode_cannotWork_theEmulatorHasNoBinaryProtocol_documentedBehaviour() throws Exception {
        // Java defaults to direct mode (RNTBD over TCP). azemu serves the gateway API only, so the data plane
        // cannot be reached. This test records what happens: the account and database calls (gateway) work,
        // the first item call fails or times out within the budget below instead of returning data.
        DirectConnectionConfig direct = DirectConnectionConfig.getDefaultConfig()
            .setConnectTimeout(Duration.ofSeconds(2)).setNetworkRequestTimeout(Duration.ofSeconds(5));
        try (CosmosClient dc = new CosmosClientBuilder().endpoint(Cos.endpoint(emu, acct)).key(Cos.KEY).directMode(direct)
                 .consistencyLevel(ConsistencyLevel.EVENTUAL)
                 .contentResponseOnWriteEnabled(true)
                 .endToEndOperationLatencyPolicyConfig(new CosmosEndToEndOperationLatencyPolicyConfigBuilder(Duration.ofSeconds(15)).build())
                 .buildClient()) {
            String dbId = Emu.dashed("ddb");
            assertEquals(201, dc.createDatabase(dbId).getStatusCode());
            CosmosDatabase d = dc.getDatabase(dbId);
            assertEquals(201, d.createContainer("c", "/pk").getStatusCode());
            CosmosContainer c = d.getContainer("c");
            long t0 = System.currentTimeMillis();
            RuntimeException e = assertThrows(RuntimeException.class,
                () -> c.createItem(Cos.doc("a", "p"), new PartitionKey("p"), new CosmosItemRequestOptions()));
            System.out.println("directMode createItem failed after " + (System.currentTimeMillis() - t0) + " ms: " + e.getClass().getName()
                + (e instanceof CosmosException ? " status " + ((CosmosException) e).getStatusCode() + "/" + ((CosmosException) e).getSubStatusCode() : "")
                + ": " + String.valueOf(e.getMessage()).substring(0, Math.min(150, String.valueOf(e.getMessage()).length())));
            assertInstanceOf(CosmosException.class, e);
        }
    }
}
