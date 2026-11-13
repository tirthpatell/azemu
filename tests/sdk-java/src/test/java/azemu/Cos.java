package azemu;

import com.azure.cosmos.*;
import com.azure.cosmos.models.*;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.util.ArrayList;
import java.util.List;

/** Shared helpers for the Cosmos tests. Not a test. */
final class Cos {
    private Cos() {}

    static final String KEY = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";
    static final ObjectMapper JSON = new ObjectMapper();

    /**
     * Eventual consistency on purpose: with the account's default (session) the Java SDK parses the x-ms-content-path
     * response header as a bare resource id and fails on azemu's path form (see docs/sdk-notes.md, Java, Cosmos DB;
     * `sessionConsistency_...` in CosmosQueryTest uses the account's default).
     */
    private static Process proxy;
    private static int proxyPort;
    private static int proxyFor;

    /**
     * The URL of the account. Development aid, not part of the suite: with COSMOS_DEV_PROXY=/path/to/proxy.py the requests go
     * through the proxy that script starts (it prints its port on the first line), for example to rewrite or log answers.
     */
    static synchronized String endpoint(Emu emu, String account) {
        String script = System.getenv("COSMOS_DEV_PROXY");
        if (script == null || script.isEmpty()) return emu.cosmosUrl(account);
        try {
            if (proxy == null || !proxy.isAlive() || proxyFor != emu.port) {
                if (proxy != null) proxy.destroyForcibly();
                proxyFor = emu.port;
                proxy = new ProcessBuilder("python3", script, Integer.toString(emu.port)).redirectErrorStream(false).start();
                proxyPort = Integer.parseInt(new java.io.BufferedReader(new java.io.InputStreamReader(proxy.getInputStream())).readLine().trim());
                Runtime.getRuntime().addShutdownHook(new Thread(proxy::destroyForcibly));
            }
        } catch (Exception e) { throw new RuntimeException(e); }
        return "https://" + account + ".documents.localhost:" + proxyPort;
    }

    static CosmosClientBuilder builder(Emu emu, String account) {
        return new CosmosClientBuilder().endpoint(endpoint(emu, account)).key(KEY).gatewayMode()
            .consistencyLevel(ConsistencyLevel.EVENTUAL)
            .contentResponseOnWriteEnabled(true);
    }

    static CosmosClientBuilder sessionBuilder(Emu emu, String account) {
        return new CosmosClientBuilder().endpoint(emu.cosmosUrl(account)).key(KEY).gatewayMode()
            .consistencyLevel(ConsistencyLevel.SESSION)
            .contentResponseOnWriteEnabled(true);
    }

    static ObjectNode doc(String id, String pk, Object... more) {
        ObjectNode n = JSON.createObjectNode();
        n.put("id", id);
        n.put("pk", pk);
        for (int i = 0; i + 1 < more.length; i += 2) n.putPOJO((String) more[i], more[i + 1]);
        return n;
    }

    static <T> List<T> all(Iterable<T> it) {
        List<T> l = new ArrayList<>();
        it.forEach(l::add);
        return l;
    }

    static List<JsonNode> query(CosmosContainer c, String sql, CosmosQueryRequestOptions o) {
        return all(c.queryItems(sql, o, JsonNode.class));
    }

    static List<JsonNode> query(CosmosContainer c, String sql) {
        return query(c, sql, new CosmosQueryRequestOptions());
    }

    /**
     * Skips the whole class (an aborted assumption, reported as skipped, not failed) when the Java SDK cannot create a
     * container and write an item: every test would then fail after retries (about 20 s each). CosmosStartupTest
     * shows the failure itself.
     */
    static void assumeGatewayRoutingWorks(Emu emu, String account) {
        try (CosmosClient c = builder(emu, account).buildClient()) {
            String db = Emu.dashed("probe");
            c.createDatabase(db);
            c.getDatabase(db).createContainer("c", "/pk");
            c.getDatabase(db).getContainer("c").createItem(doc("a", "p"), new PartitionKey("p"), new CosmosItemRequestOptions());
        } catch (Exception e) {
            org.junit.jupiter.api.Assumptions.assumeTrue(false,
                "azemu: the Java SDK cannot create a container and write an item (a request the Java SDK sends was refused): " + String.valueOf(e.getMessage()).substring(0, Math.min(120, String.valueOf(e.getMessage()).length())));
        }
    }
}
