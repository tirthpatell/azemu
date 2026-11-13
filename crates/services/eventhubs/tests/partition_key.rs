//! Partition keys go to the partition Azure chooses.
//!
//! The expected values were computed by Microsoft's own function, not by this crate: the JavaScript SDK's
//! `mapPartitionKeyToId` and `hashPartitionKey` in
//! `tests/sdk-js/node_modules/@azure/event-hubs/dist/esm/impl/partitionKeyToIdMapper.js` (version in
//! `tests/sdk-js/package.json`), run with Node by `tests/partition_key_values.mjs` in this folder. To make them
//! again: `node crates/services/eventhubs/tests/partition_key_values.mjs`. The .NET SDK's `PartitionResolver`
//! implements the same function. The columns are the partition counts in `COUNTS`.

use azemu_eventhubs::model::{hash_key, partition_for_key};

const COUNTS: [usize; 7] = [1, 2, 3, 4, 8, 16, 32];

/// (key, `hashPartitionKey`, partition for each count in `COUNTS`)
const EXPECTED: &[(&str, i16, [usize; 7])] = &[
    ("", 0, [0, 0, 0, 0, 0, 0, 0]),
    ("a", -16220, [0, 0, 2, 0, 4, 12, 28]),
    ("alpha", 873, [0, 1, 0, 1, 1, 9, 9]),
    ("beta", -6771, [0, 1, 0, 3, 3, 3, 19]),
    ("gamma", -11976, [0, 0, 0, 0, 0, 8, 8]),
    ("user-1", -27605, [0, 1, 2, 1, 5, 5, 21]),
    ("order-12345", 22030, [0, 0, 1, 2, 6, 14, 14]),
    ("device/42", 4230, [0, 0, 0, 2, 6, 6, 6]),
    ("sensor-temperature-01", -5782, [0, 0, 1, 2, 6, 6, 22]),
    ("0123456789ab", -17054, [0, 0, 2, 2, 6, 14, 30]),
    ("0123456789abc", -4167, [0, 1, 0, 3, 7, 7, 7]),
    ("hello world", 24431, [0, 1, 2, 3, 7, 15, 15]),
    ("Zürich-ß", 15217, [0, 1, 1, 1, 1, 1, 17]),
    ("日本語のキー", 18957, [0, 1, 0, 1, 5, 13, 13]),
    ("😀", -17126, [0, 0, 2, 2, 6, 6, 6]),
    ("tenant:acme:region:eu-west-1", 17248, [0, 0, 1, 0, 0, 0, 0]),
    ("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", -29224, [0, 0, 1, 0, 0, 8, 8]),
    ("The quick brown fox jumps over the lazy dog", -31937, [0, 1, 2, 1, 1, 1, 1]),
    ("k1", 23816, [0, 0, 2, 0, 0, 8, 8]),
    ("partition-key-with-a-fairly-long-name-1234567890", -6264, [0, 0, 0, 0, 0, 8, 24]),
];

#[test]
fn twenty_keys_land_where_the_javascript_sdk_puts_them() {
    assert_eq!(EXPECTED.len(), 20);
    for (key, hash, parts) in EXPECTED {
        assert_eq!(hash_key(key), *hash, "hash of {key:?}");
        for (count, want) in COUNTS.iter().zip(parts) {
            assert_eq!(partition_for_key(key, *count), *want, "key {key:?} with {count} partitions");
        }
    }
}

#[test]
fn the_review_example_for_four_partitions() {
    // the SDK maps alpha, beta, gamma to 1, 3, 0 (the old FNV hash gave 3, 3, 2)
    assert_eq!([partition_for_key("alpha", 4), partition_for_key("beta", 4), partition_for_key("gamma", 4)], [1, 3, 0]);
}

#[test]
fn a_partition_is_always_inside_the_hub() {
    for n in 1..=32usize {
        for i in 0..200 {
            assert!(partition_for_key(&format!("key-{i}"), n) < n);
        }
    }
}
