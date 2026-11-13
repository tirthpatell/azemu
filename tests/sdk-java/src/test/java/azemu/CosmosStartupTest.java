package azemu;

import com.azure.cosmos.*;
import com.azure.cosmos.models.*;
import org.junit.jupiter.api.*;

import static org.junit.jupiter.api.Assertions.*;

/** The Java SDK creates a database, a container and an item through the client: the first thing every other Cosmos test needs. */
@EmuTest
class CosmosStartupTest {
    static Emu emu;

    @BeforeAll static void setUp() { emu = Emu.current(); }

    @Test void createItem_inAContainerCreatedThroughTheSdk_works() {
        try (CosmosClient c = Cos.builder(emu, Emu.name("cosf")).buildClient()) {
            String db = Emu.dashed("fdb");
            c.createDatabase(db);
            c.getDatabase(db).createContainer("c", "/pk");
            CosmosItemResponse<?> r = c.getDatabase(db).getContainer("c")
                .createItem(Cos.doc("a", "p"), new PartitionKey("p"), new CosmosItemRequestOptions());
            assertEquals(201, r.getStatusCode());
        }
    }
}
