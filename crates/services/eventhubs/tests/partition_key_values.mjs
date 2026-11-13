// Prints the expected values of tests/partition_key.rs: the JavaScript SDK's own functions, run with Node.
// Run from anywhere: node crates/services/eventhubs/tests/partition_key_values.mjs
import { mapPartitionKeyToId, hashPartitionKey } from "../../../../tests/sdk-js/node_modules/@azure/event-hubs/dist/esm/impl/partitionKeyToIdMapper.js";
const keys = ["", "a", "alpha", "beta", "gamma", "user-1", "order-12345", "device/42", "sensor-temperature-01", "0123456789ab", "0123456789abc", "hello world", "Zürich-ß", "日本語のキー", "😀", "tenant:acme:region:eu-west-1", "x".repeat(40), "The quick brown fox jumps over the lazy dog", "k1", "partition-key-with-a-fairly-long-name-1234567890"];
const counts = [1, 2, 3, 4, 8, 16, 32];
const out = keys.map(k => ({ key: k, hash: hashPartitionKey(k), parts: counts.map(c => mapPartitionKeyToId(k, c)) }));
console.log(JSON.stringify({ counts, rows: out }));
