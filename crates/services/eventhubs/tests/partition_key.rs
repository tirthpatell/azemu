//! Partition keys go to the partition Azure chooses.
//!
//! `partition_key_azure.jsonl` holds `[key, partitions, partition]` as a real Standard namespace answered in November 2026:
//! 475 samples, each an event sent with that key (no partition id) to an event hub with 2, 3, 4, 5, 6, 7, 8, 12, 16, 24, 28, 30, 31
//! or 32 partitions and read back. They include upper and lower case of one letter, characters that are not ASCII, and keys picked
//! to fall where the ways of dividing the ring differ (28, 30 and 31 partitions). The function is not the one in Microsoft's SDKs,
//! which their buffered producers use and the service does not: see `model::partition_for_key`.

use azemu_eventhubs::model::{key_bytes, key_position, partition_for_key};

fn samples() -> Vec<(String, usize, usize)> {
    include_str!("partition_key_azure.jsonl")
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            (v[0].as_str().unwrap().to_string(), v[1].as_u64().unwrap() as usize, v[2].as_u64().unwrap() as usize)
        })
        .collect()
}

#[test]
fn every_key_lands_where_azure_put_it() {
    let all = samples();
    assert_eq!(all.len(), 475);
    let wrong: Vec<_> = all.iter().filter(|(k, n, p)| partition_for_key(k, *n) != *p).map(|(k, n, p)| (k.as_str(), *n, *p)).collect();
    assert!(wrong.is_empty(), "keys that go elsewhere than on Azure (key, partitions, Azure's partition): {wrong:?}");
}

#[test]
fn the_samples_cover_the_partition_counts_that_divide_the_ring_unevenly() {
    let counts: std::collections::BTreeSet<usize> = samples().into_iter().map(|s| s.1).collect();
    assert_eq!(counts.into_iter().collect::<Vec<_>>(), vec![2, 3, 4, 5, 6, 7, 8, 12, 16, 24, 28, 30, 31, 32]);
}

#[test]
fn case_does_not_matter_and_a_letter_outside_ascii_is_a_question_mark() {
    assert_eq!(key_bytes("Alpha-1"), b"ALPHA-1");
    assert_eq!(key_bytes("é"), b"?");
    assert_eq!(key_bytes("日本"), b"??");
    // a character outside the Basic Multilingual Plane is two UTF-16 units
    assert_eq!(key_bytes("😀"), b"??");
    for n in [2usize, 4, 7, 32] {
        assert_eq!(partition_for_key("device-A7", n), partition_for_key("DEVICE-a7", n));
    }
    assert_eq!(key_position("ü"), key_position("é"));
}

#[test]
fn a_partition_is_always_inside_the_hub() {
    for n in 1..=32usize {
        for i in 0..200 {
            assert!(partition_for_key(&format!("key-{i}"), n) < n);
        }
    }
    assert_eq!(partition_for_key("anything", 1), 0);
    assert_eq!(partition_for_key("anything", 0), 0);
}
