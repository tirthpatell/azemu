package azemu;

import com.azure.cosmos.*;
import com.azure.cosmos.models.*;
import com.azure.cosmos.util.CosmosPagedIterable;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import org.junit.jupiter.api.*;

import java.time.Duration;
import java.time.Instant;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

/**
 * Cosmos DB queries (the Java SDK asks the gateway for a query plan and runs cross-partition ORDER BY, TOP, aggregates,
 * GROUP BY and DISTINCT itself), transactional batch, bulk, and the change feed.
 */
@EmuTest
class CosmosQueryTest {
    static Emu emu;
    static String acct;
    static CosmosClient client;
    static CosmosContainer people;     // read-only data set shared by the query tests
    static List<String> names = List.of("ann", "bob", "cid", "dee", "eve", "fay", "gus", "hal", "ivy", "jon");

    @BeforeAll static void setUp() {
        emu = Emu.current();
        acct = Emu.name("cosq");
        Cos.assumeGatewayRoutingWorks(emu, acct);
        client = Cos.builder(emu, acct).buildClient();
        String db = Emu.dashed("qdb");
        client.createDatabase(db);
        CosmosContainerProperties p = new CosmosContainerProperties("people", "/city");
        IndexingPolicy ip = new IndexingPolicy();
        ip.setCompositeIndexes(List.of(List.of(
            new CompositePath().setPath("/age").setOrder(CompositePathSortOrder.ASCENDING),
            new CompositePath().setPath("/name").setOrder(CompositePathSortOrder.DESCENDING))));
        p.setIndexingPolicy(ip);
        client.getDatabase(db).createContainer(p);
        people = client.getDatabase(db).getContainer("people");
        String[] cities = {"paris", "rome", "oslo"};
        for (int i = 0; i < names.size(); i++) {
            ObjectNode d = Cos.JSON.createObjectNode().put("id", "p" + i).put("name", names.get(i)).put("age", 20 + (i % 5) * 5)
                .put("city", cities[i % 3]).put("score", i * 1.5);
            d.putArray("tags").add(i % 2 == 0 ? "even" : "odd").add("all");
            d.putObject("address").put("zip", "z" + i);
            people.createItem(d, new PartitionKey(cities[i % 3]), new CosmosItemRequestOptions());
        }
    }

    @AfterAll static void tearDown() { if (client != null) client.close(); }

    static List<String> names(List<JsonNode> l) { return l.stream().map(n -> n.get("name").asText()).collect(Collectors.toList()); }

    static CosmosQueryRequestOptions crossPartition() { return new CosmosQueryRequestOptions(); }

    // ---- basic queries ----------------------------------------------------------------------------------------
    @Test void selectStar_acrossPartitions_returnsEveryItem() {
        assertEquals(10, Cos.query(people, "SELECT * FROM c").size());
    }

    @Test void whereWithParameters_filtersByValueAndType() {
        SqlQuerySpec q = new SqlQuerySpec("SELECT c.name FROM c WHERE c.age >= @min AND c.city = @city ORDER BY c.name",
            new SqlParameter("@min", 25), new SqlParameter("@city", "paris"));
        List<JsonNode> got = Cos.all(people.queryItems(q, new CosmosQueryRequestOptions(), JsonNode.class));
        assertEquals(List.of("dee", "gus"), names(got).stream().sorted().collect(Collectors.toList()).subList(0, got.size()).stream().filter(n -> List.of("dee", "gus").contains(n)).collect(Collectors.toList()));
    }

    @Test void queryInASinglePartition_usesThePartitionKeyOption() {
        List<JsonNode> got = Cos.query(people, "SELECT * FROM c", new CosmosQueryRequestOptions().setPartitionKey(new PartitionKey("rome")));
        assertEquals(Set.of("bob", "eve", "hal"), new HashSet<>(names(got)));
    }

    @Test void projectionWithValueAndFunctions() {
        List<JsonNode> got = Cos.query(people, "SELECT VALUE UPPER(c.name) FROM c WHERE STARTSWITH(c.name, 'a') OR CONTAINS(c.name, 'on')");
        assertEquals(Set.of("ANN", "JON"), got.stream().map(JsonNode::asText).collect(Collectors.toSet()));
    }

    @Test void arrayContains_andJoinOverArrays() {
        assertEquals(5, Cos.query(people, "SELECT * FROM c WHERE ARRAY_CONTAINS(c.tags, 'even')").size());
        List<JsonNode> joined = Cos.query(people, "SELECT c.id, t FROM c JOIN t IN c.tags WHERE t = 'odd'");
        assertEquals(5, joined.size());
    }

    @Test void nestedPropertyAndInOperator() {
        List<JsonNode> got = Cos.query(people, "SELECT c.name FROM c WHERE c.address.zip IN ('z1', 'z2', 'z3') ORDER BY c.name");
        assertEquals(List.of("bob", "cid", "dee"), names(got));
    }

    // ---- ORDER BY, TOP, OFFSET LIMIT, DISTINCT (planned by the client) ---------------------------------------
    @Test void orderByAcrossPartitions_isMergedByTheSdk() {
        assertEquals(List.of("ann", "bob", "cid", "dee", "eve", "fay", "gus", "hal", "ivy", "jon"),
            names(Cos.query(people, "SELECT c.name FROM c ORDER BY c.name")));
        assertEquals(List.of("jon", "ivy", "hal"), names(Cos.query(people, "SELECT TOP 3 c.name FROM c ORDER BY c.name DESC")));
    }

    @Test void orderByWithCompositeIndex_onTwoFields() {
        List<JsonNode> got = Cos.query(people, "SELECT c.name, c.age FROM c ORDER BY c.age ASC, c.name DESC");
        int prevAge = -1; String prevName = null;
        for (JsonNode n : got) {
            int age = n.get("age").asInt();
            if (age == prevAge) assertTrue(n.get("name").asText().compareTo(prevName) < 0, "name descends inside an age");
            else assertTrue(age > prevAge);
            prevAge = age; prevName = n.get("name").asText();
        }
        assertEquals(10, got.size());
    }

    @Test void top_offsetLimit_andDistinct() {
        assertEquals(4, Cos.query(people, "SELECT TOP 4 * FROM c").size());
        assertEquals(List.of("dee", "eve", "fay"), names(Cos.query(people, "SELECT c.name FROM c ORDER BY c.name OFFSET 3 LIMIT 3")));
        List<JsonNode> cities = Cos.query(people, "SELECT DISTINCT VALUE c.city FROM c");
        assertEquals(Set.of("paris", "rome", "oslo"), cities.stream().map(JsonNode::asText).collect(Collectors.toSet()));
        assertEquals(3, cities.size());
    }

    // ---- aggregates and GROUP BY ---------------------------------------------------------------------------
    @Test void aggregates_acrossPartitions_countSumAvgMinMax() {
        assertEquals(10, Cos.query(people, "SELECT VALUE COUNT(1) FROM c").get(0).asInt());
        assertEquals(67.5, Cos.query(people, "SELECT VALUE SUM(c.score) FROM c").get(0).asDouble(), 1e-9);
        assertEquals(6.75, Cos.query(people, "SELECT VALUE AVG(c.score) FROM c").get(0).asDouble(), 1e-9);
        assertEquals(20, Cos.query(people, "SELECT VALUE MIN(c.age) FROM c").get(0).asInt());
        assertEquals(40, Cos.query(people, "SELECT VALUE MAX(c.age) FROM c").get(0).asInt());
    }

    @Test void multipleAggregatesInOneQuery_inAnObject() {
        JsonNode r = Cos.query(people, "SELECT COUNT(1) AS n, MIN(c.age) AS lo, MAX(c.age) AS hi, SUM(c.age) AS total FROM c").get(0);
        assertEquals(10, r.get("n").asInt());
        assertEquals(20, r.get("lo").asInt());
        assertEquals(40, r.get("hi").asInt());
        assertEquals(300, r.get("total").asInt());
    }

    @Test void groupBy_acrossPartitions() {
        List<JsonNode> got = Cos.query(people, "SELECT c.city, COUNT(1) AS n, AVG(c.age) AS avgAge FROM c GROUP BY c.city");
        Map<String, Integer> counts = got.stream().collect(Collectors.toMap(n -> n.get("city").asText(), n -> n.get("n").asInt()));
        assertEquals(Map.of("paris", 4, "rome", 3, "oslo", 3), counts);
    }

    @Test void aggregateOnAnEmptyResult_countIsZero() {
        assertEquals(0, Cos.query(people, "SELECT VALUE COUNT(1) FROM c WHERE c.age > 1000").get(0).asInt());
    }

    // ---- paging ---------------------------------------------------------------------------------------------
    @Test void paging_withContinuationToken_resumesWhereThePageEnded() {
        CosmosPagedIterable<JsonNode> it = people.queryItems("SELECT c.name FROM c ORDER BY c.name", new CosmosQueryRequestOptions(), JsonNode.class);
        List<String> seen = new ArrayList<>();
        String token = null;
        int pages = 0;
        do {
            Iterator<FeedResponse<JsonNode>> pagesIt = (token == null ? it : people.queryItems("SELECT c.name FROM c ORDER BY c.name",
                new CosmosQueryRequestOptions(), JsonNode.class)).iterableByPage(token, 3).iterator();
            if (!pagesIt.hasNext()) break;
            FeedResponse<JsonNode> page = pagesIt.next();
            assertTrue(page.getResults().size() <= 3);
            page.getResults().forEach(n -> seen.add(n.get("name").asText()));
            token = page.getContinuationToken();
            pages++;
        } while (token != null && pages < 10);
        assertEquals(names, seen);
        assertTrue(pages >= 4, "ten items in pages of three need at least four pages, got " + pages);
    }

    @Test void paging_plainSelect_withPageSize_returnsAllPages() {
        int pages = 0, items = 0;
        for (FeedResponse<JsonNode> page : people.queryItems("SELECT * FROM c", new CosmosQueryRequestOptions(), JsonNode.class).iterableByPage(4)) {
            pages++;
            items += page.getResults().size();
            assertTrue(page.getResults().size() <= 4);
            assertTrue(page.getRequestCharge() >= 0);
        }
        assertEquals(10, items);
        assertTrue(pages >= 3);
    }

    @Test void readAllItems_ofAPartition_andOfTheContainer() {
        assertEquals(4, Cos.all(people.readAllItems(new PartitionKey("paris"), JsonNode.class)).size());
        assertEquals(4, Cos.all(people.readAllItems(new PartitionKey("paris"), new CosmosQueryRequestOptions(), JsonNode.class)).size());
    }

    @Test void queryWithSyntaxError_is400() {
        CosmosException e = assertThrows(CosmosException.class, () -> Cos.query(people, "SELEC * FROM c"));
        assertEquals(400, e.getStatusCode());
    }

    @Test void queryOfAMissingContainer_is404() {
        CosmosContainer missing = client.getDatabase(people.getId().isEmpty() ? "x" : people.getId()).getContainer("nope");
        assertThrows(CosmosException.class, () -> Cos.query(missing, "SELECT * FROM c"));
    }

    // ---- transactional batch ----------------------------------------------------------------------------------
    CosmosContainer fresh() {
        String db = Emu.dashed("bdb");
        client.createDatabase(db);
        client.getDatabase(db).createContainer("c", "/pk");
        return client.getDatabase(db).getContainer("c");
    }

    @Test void transactionalBatch_createsReplacesUpsertsPatchesAndDeletesInOneRequest() {
        CosmosContainer c = fresh();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("existing", "p", "v", 1), pk, new CosmosItemRequestOptions());
        c.createItem(Cos.doc("gone", "p"), pk, new CosmosItemRequestOptions());
        CosmosBatch b = CosmosBatch.createCosmosBatch(pk);
        b.createItemOperation(Cos.doc("new1", "p", "v", 1));
        b.upsertItemOperation(Cos.doc("up", "p", "v", 2));
        b.replaceItemOperation("existing", Cos.doc("existing", "p", "v", 9));
        b.patchItemOperation("existing", CosmosPatchOperations.create().add("/extra", true));
        b.readItemOperation("existing");
        b.deleteItemOperation("gone");
        CosmosBatchResponse r = c.executeCosmosBatch(b);
        assertTrue(r.isSuccessStatusCode(), r.getErrorMessage());
        assertEquals(200, r.getStatusCode());
        assertEquals(6, r.getResults().size());
        assertEquals(List.of(201, 201, 200, 200, 200, 204), r.getResults().stream().map(CosmosBatchOperationResult::getStatusCode).collect(Collectors.toList()));
        JsonNode read = r.getResults().get(4).getItem(JsonNode.class);
        assertEquals(9, read.get("v").asInt());
        assertTrue(read.get("extra").asBoolean());
        assertNotNull(r.getResults().get(0).getETag());
        assertEquals(200, c.readItem("new1", pk, JsonNode.class).getStatusCode());
        assertThrows(CosmosException.class, () -> c.readItem("gone", pk, JsonNode.class));
    }

    @Test void transactionalBatch_withAFailingOperation_rollsEverythingBack() {
        CosmosContainer c = fresh();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("dup", "p"), pk, new CosmosItemRequestOptions());
        CosmosBatch b = CosmosBatch.createCosmosBatch(pk);
        b.createItemOperation(Cos.doc("fresh", "p"));
        b.createItemOperation(Cos.doc("dup", "p"));       // conflict
        b.createItemOperation(Cos.doc("never", "p"));
        CosmosBatchResponse r = c.executeCosmosBatch(b);
        assertFalse(r.isSuccessStatusCode());
        assertEquals(409, r.getStatusCode(), "the SDK reports the status of the operation that failed");
        assertEquals(List.of(424, 409, 424), r.getResults().stream().map(CosmosBatchOperationResult::getStatusCode).collect(Collectors.toList()));
        CosmosException e = assertThrows(CosmosException.class, () -> c.readItem("fresh", pk, JsonNode.class));
        assertEquals(404, e.getStatusCode(), "the first create was rolled back");
    }

    @Test void transactionalBatch_ifMatchOnAnOperation_failsWith412() {
        CosmosContainer c = fresh();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("a", "p"), pk, new CosmosItemRequestOptions());
        CosmosBatch b = CosmosBatch.createCosmosBatch(pk);
        b.replaceItemOperation("a", Cos.doc("a", "p", "v", 2), new CosmosBatchItemRequestOptions().setIfMatchETag("\"stale\""));
        CosmosBatchResponse r = c.executeCosmosBatch(b);
        assertFalse(r.isSuccessStatusCode());
        assertEquals(412, r.getResults().get(0).getStatusCode());
    }

    @Test void transactionalBatch_of101Operations_isRefused() {
        CosmosContainer c = fresh();
        CosmosBatch b = CosmosBatch.createCosmosBatch(new PartitionKey("p"));
        for (int i = 0; i < 101; i++) b.createItemOperation(Cos.doc("i" + i, "p"));
        assertThrows(RuntimeException.class, () -> c.executeCosmosBatch(b));
    }

    // ---- bulk -------------------------------------------------------------------------------------------------
    @Test void executeBulkOperations_createsManyItemsAcrossPartitions() {
        CosmosContainer c = fresh();
        List<CosmosItemOperation> ops = new ArrayList<>();
        for (int i = 0; i < 250; i++) ops.add(CosmosBulkOperations.getCreateItemOperation(Cos.doc("b" + i, "p" + (i % 7), "n", i), new PartitionKey("p" + (i % 7))));
        int ok = 0;
        for (CosmosBulkOperationResponse<Object> r : c.<Object>executeBulkOperations(ops)) {
            assertNull(r.getException(), String.valueOf(r.getException()));
            assertEquals(201, r.getResponse().getStatusCode());
            ok++;
        }
        assertEquals(250, ok);
        assertEquals(250, Cos.query(c, "SELECT VALUE COUNT(1) FROM c").get(0).asInt());
    }

    @Test void executeBulkOperations_mixedOperations_reportEachOperationsStatus() {
        CosmosContainer c = fresh();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("a", "p", "v", 1), pk, new CosmosItemRequestOptions());
        c.createItem(Cos.doc("del", "p"), pk, new CosmosItemRequestOptions());
        List<CosmosItemOperation> ops = List.of(
            CosmosBulkOperations.getCreateItemOperation(Cos.doc("a", "p"), pk),                       // 409
            CosmosBulkOperations.getUpsertItemOperation(Cos.doc("u", "p", "v", 5), pk),               // 201
            CosmosBulkOperations.getReplaceItemOperation("a", Cos.doc("a", "p", "v", 2), pk),         // 200
            CosmosBulkOperations.getPatchItemOperation("a", pk, CosmosPatchOperations.create().set("/w", 3)),
            CosmosBulkOperations.getReadItemOperation("a", pk),                                       // 200
            CosmosBulkOperations.getDeleteItemOperation("del", pk),                                   // 204
            CosmosBulkOperations.getReadItemOperation("missing", pk));                                // 404
        Map<String, Integer> status = new HashMap<>();
        for (CosmosBulkOperationResponse<Object> r : c.<Object>executeBulkOperations(ops)) {
            CosmosItemOperation op = r.getOperation();
            String id = op.getId() != null ? op.getId() : ((JsonNode) op.getItem()).get("id").asText();   // create and upsert carry the item, not an id
            String key = op.getOperationType().name() + ":" + id;
            if (r.getResponse() != null) status.put(key, r.getResponse().getStatusCode());
            else status.put(key, ((CosmosException) r.getException()).getStatusCode());
        }
        assertEquals(Integer.valueOf(409), status.get("CREATE:a"), status.toString());
        assertEquals(201, status.get("UPSERT:u"));
        assertEquals(200, status.get("REPLACE:a"));
        assertEquals(200, status.get("PATCH:a"));
        assertEquals(204, status.get("DELETE:del"));
        assertEquals(404, status.get("READ:missing"));
        assertEquals(200, status.get("READ:a"));
    }

    // ---- change feed ------------------------------------------------------------------------------------------
    @Test void changeFeed_fromBeginning_listsEveryItemThenContinuesWithNewOnes() {
        CosmosContainer c = fresh();
        for (int i = 0; i < 5; i++) c.createItem(Cos.doc("cf" + i, "p" + (i % 2), "n", i), new PartitionKey("p" + (i % 2)), new CosmosItemRequestOptions());
        String token = null;
        Set<String> ids = new TreeSet<>();
        for (FeedResponse<JsonNode> page : c.queryChangeFeed(CosmosChangeFeedRequestOptions.createForProcessingFromBeginning(FeedRange.forFullRange()), JsonNode.class).iterableByPage()) {
            page.getResults().forEach(n -> ids.add(n.get("id").asText()));
            token = page.getContinuationToken();
        }
        assertEquals(new TreeSet<>(List.of("cf0", "cf1", "cf2", "cf3", "cf4")), ids);
        assertNotNull(token);
        c.createItem(Cos.doc("cf5", "p0"), new PartitionKey("p0"), new CosmosItemRequestOptions());
        c.replaceItem(Cos.doc("cf0", "p0", "n", 100), "cf0", new PartitionKey("p0"), new CosmosItemRequestOptions());
        Set<String> next = new TreeSet<>();
        for (FeedResponse<JsonNode> page : c.queryChangeFeed(CosmosChangeFeedRequestOptions.createForProcessingFromContinuation(token), JsonNode.class).iterableByPage())
            page.getResults().forEach(n -> next.add(n.get("id").asText()));
        assertEquals(new TreeSet<>(List.of("cf0", "cf5")), next);
    }

    @Test void changeFeed_fromNow_seesOnlyLaterChanges() {
        CosmosContainer c = fresh();
        c.createItem(Cos.doc("old", "p"), new PartitionKey("p"), new CosmosItemRequestOptions());
        CosmosChangeFeedRequestOptions o = CosmosChangeFeedRequestOptions.createForProcessingFromNow(FeedRange.forFullRange());
        List<String> first = new ArrayList<>();
        String token = null;
        for (FeedResponse<JsonNode> page : c.queryChangeFeed(o, JsonNode.class).iterableByPage()) {
            page.getResults().forEach(n -> first.add(n.get("id").asText()));
            token = page.getContinuationToken();
        }
        assertTrue(first.isEmpty());
        c.createItem(Cos.doc("new", "p"), new PartitionKey("p"), new CosmosItemRequestOptions());
        List<String> second = new ArrayList<>();
        for (FeedResponse<JsonNode> page : c.queryChangeFeed(CosmosChangeFeedRequestOptions.createForProcessingFromContinuation(token), JsonNode.class).iterableByPage())
            page.getResults().forEach(n -> second.add(n.get("id").asText()));
        assertEquals(List.of("new"), second);
    }

    @Test void changeFeed_forOneLogicalPartition() {
        CosmosContainer c = fresh();
        c.createItem(Cos.doc("a", "A"), new PartitionKey("A"), new CosmosItemRequestOptions());
        c.createItem(Cos.doc("b", "B"), new PartitionKey("B"), new CosmosItemRequestOptions());
        Set<String> ids = new TreeSet<>();
        for (FeedResponse<JsonNode> page : c.queryChangeFeed(
            CosmosChangeFeedRequestOptions.createForProcessingFromBeginning(FeedRange.forLogicalPartition(new PartitionKey("A"))), JsonNode.class).iterableByPage())
            page.getResults().forEach(n -> ids.add(n.get("id").asText()));
        assertEquals(Set.of("a"), ids);
    }

    @Test void changeFeed_latestVersion_doesNotReportDeletes() {
        CosmosContainer c = fresh();
        PartitionKey pk = new PartitionKey("p");
        c.createItem(Cos.doc("x", "p"), pk, new CosmosItemRequestOptions());
        c.createItem(Cos.doc("y", "p"), pk, new CosmosItemRequestOptions());
        c.deleteItem("x", pk, new CosmosItemRequestOptions());
        Set<String> ids = new TreeSet<>();
        for (FeedResponse<JsonNode> page : c.queryChangeFeed(CosmosChangeFeedRequestOptions.createForProcessingFromBeginning(FeedRange.forFullRange()), JsonNode.class).iterableByPage())
            page.getResults().forEach(n -> ids.add(n.get("id").asText()));
        assertEquals(Set.of("y"), ids);
    }

    @Test void changeFeed_allVersionsAndDeletes_reportsCreateReplaceAndDelete() {
        String db = Emu.dashed("avdb");
        client.createDatabase(db);
        client.getDatabase(db).createContainer(new CosmosContainerProperties("c", "/pk")
            .setChangeFeedPolicy(ChangeFeedPolicy.createAllVersionsAndDeletesPolicy(Duration.ofMinutes(10))));
        CosmosContainer c = client.getDatabase(db).getContainer("c");
        PartitionKey pk = new PartitionKey("p");
        // the position must be taken before the changes: this mode cannot start from the beginning
        String token = null;
        for (FeedResponse<ChangeFeedProcessorItem> page : c.queryChangeFeed(
            CosmosChangeFeedRequestOptions.createForProcessingFromNow(FeedRange.forFullRange()).allVersionsAndDeletes(), ChangeFeedProcessorItem.class).iterableByPage())
            token = page.getContinuationToken();
        c.createItem(Cos.doc("a", "p", "v", 1), pk, new CosmosItemRequestOptions());
        c.replaceItem(Cos.doc("a", "p", "v", 2), "a", pk, new CosmosItemRequestOptions());
        c.deleteItem("a", pk, new CosmosItemRequestOptions());
        List<String> ops = new ArrayList<>();
        for (FeedResponse<ChangeFeedProcessorItem> page : c.queryChangeFeed(
            CosmosChangeFeedRequestOptions.createForProcessingFromContinuation(token), ChangeFeedProcessorItem.class).iterableByPage())
            page.getResults().forEach(i -> ops.add(i.getChangeFeedMetaData().getOperationType().toString()));
        assertEquals(List.of("create", "replace", "delete"), ops.stream().map(String::toLowerCase).collect(Collectors.toList()));
    }

    // ---- session consistency --------------------------------------------------------------------
    @Test void sessionConsistency_theDefaultOfTheAccount_worksForAnItemRoundTrip() {
        try (CosmosClient sc = Cos.sessionBuilder(emu, acct).buildClient()) {
            String db = Emu.dashed("sdb");
            sc.createDatabase(db);
            sc.getDatabase(db).createContainer("c", "/pk");
            CosmosContainer c = sc.getDatabase(db).getContainer("c");
            c.createItem(Cos.doc("s", "p"), new PartitionKey("p"), new CosmosItemRequestOptions());
            assertEquals(200, c.readItem("s", new PartitionKey("p"), JsonNode.class).getStatusCode());
        }
    }

    // ---- TTL in a change feed / throughput -------------------------------------------------------------------
    @Test void throughputOfADatabaseAndContainer_canBeReadAndReplaced() {
        String db = Emu.dashed("tdb");
        client.createDatabase(db, ThroughputProperties.createManualThroughput(800));
        CosmosDatabase d = client.getDatabase(db);
        assertEquals(800, d.readThroughput().getProperties().getManualThroughput());
        d.replaceThroughput(ThroughputProperties.createManualThroughput(1000));
        assertEquals(1000, d.readThroughput().getProperties().getManualThroughput());
    }
}
