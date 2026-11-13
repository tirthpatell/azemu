//! How much one request may name and one answer may hold: a request is the client's text, an answer is built
//! from it, and neither is allowed to be big because the client said so. The Kafka clients never come near these numbers
//! (an event hub namespace has at most 1,000 event hubs of at most 1,024 partitions).

use azemu_kafka::Struct;

/// Topics (or groups, config resources) one request may name. The same as the event hubs of a namespace.
pub const MAX_TOPICS_PER_REQUEST: usize = 1_000;
/// (topic, partition) pairs one request may name, over all its topics.
pub const MAX_PARTITIONS_PER_REQUEST: usize = 20_000;
/// Partitions one Metadata answer lists. Topics past it are answered with `LEADER_NOT_AVAILABLE`, which clients retry.
pub const MAX_PARTITIONS_PER_ANSWER: usize = 20_000;

/// The arrays that hold what a request names, and the arrays inside each entry that hold partitions.
const TOPIC_LISTS: &[&str] = &["Topics", "TopicData", "TopicNames", "Groups", "GroupIds", "Resources"];
const PARTITION_LISTS: &[&str] = &["Partitions", "PartitionData", "PartitionIndexes"];

/// Does the request name more topics or partitions than allowed? The connection is closed for one that does, as Apache Kafka
/// closes it for a request it cannot handle: no answer is built, so no answer can be big.
pub fn names_too_much(body: &Struct) -> bool {
    let mut partitions = 0usize;
    for list in TOPIC_LISTS {
        let items = body.get(list).items();
        if items.len() > MAX_TOPICS_PER_REQUEST {
            return true;
        }
        for t in items {
            if let Some(t) = t.as_struct() {
                for p in PARTITION_LISTS {
                    partitions = partitions.saturating_add(t.get(p).items().len());
                }
            }
        }
    }
    partitions > MAX_PARTITIONS_PER_REQUEST
}
