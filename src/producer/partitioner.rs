use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher};

use twox_hash::XxHash;

use protocol::PartitionId;
use client::{Cluster, Metadata};

/// A partitioner is given a chance to choose/redefine a partition
/// for a message to be sent to Kafka.
pub trait Partitioner {
    /// Compute the partition for the given record.
    fn partition<K: Hash, V>(&self,
                             topic_name: &str,
                             partition: Option<PartitionId>,
                             key: Option<&K>,
                             value: Option<&V>,
                             metadata: Rc<Metadata>)
                             -> Option<PartitionId>;
}

pub type DefaultHasher = XxHash;

/// The default partitioning strategy:
///
/// - If a partition is specified in the record, use it
/// - If no partition is specified but a key is present choose a partition based on a hash of the key
/// - If no partition or key is present choose a partition in a round-robin fashion
#[derive(Default)]
pub struct DefaultPartitioner<H: BuildHasher = BuildHasherDefault<DefaultHasher>> {
    hash_builder: H,
    records: AtomicUsize,
}

impl DefaultPartitioner {
    pub fn new() -> DefaultPartitioner<BuildHasherDefault<DefaultHasher>> {
        Default::default()
    }

    pub fn with_hasher<B: BuildHasher>(hash_builder: B) -> DefaultPartitioner<B> {
        DefaultPartitioner {
            hash_builder: hash_builder.into(),
            records: AtomicUsize::new(0),
        }
    }

    pub fn records(&self) -> usize {
        self.records.load(Ordering::Relaxed)
    }
}

impl<H> Partitioner for DefaultPartitioner<H>
    where H: BuildHasher
{
    fn partition<K: Hash, V>(&self,
                             topic_name: &str,
                             partition: Option<PartitionId>,
                             key: Option<&K>,
                             _value: Option<&V>,
                             metadata: Rc<Metadata>)
                             -> Option<PartitionId> {
        if let Some(partition) = partition {
            if partition >= 0 {
                // If a partition is specified in the record, use it
                return Some(partition);
            }
        }

        // TODO: use available partitions for topic in cluster
        if let Some(partitions) = metadata.partitions_for_topic(topic_name) {
            let index = if let Some(ref key) = key {
                // If no partition is specified but a key is present choose a partition based on a
                // hash of the key
                let mut hasher = self.hash_builder.build_hasher();
                key.hash(&mut hasher);
                hasher.finish() as usize
            } else {
                // If no partition or key is present choose a partition in a round-robin fashion
                self.records.fetch_add(1, Ordering::Relaxed)
            } % partitions.len();

            trace!("send record to partition #{} base on {}",
                   index,
                   key.map_or("round-robin", |_| "hash-key"));

            Some(partitions[index].partition)
        } else {
            warn!("missed partitions info for topic `{}`, fallback to partition #0",
                  topic_name);

            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::PartitionInfo;

    #[test]
    fn test_skip_partitioning() {
        let metadata = Rc::new(Metadata::default());
        let partitioner = DefaultPartitioner::new();

        // partition without topics
        assert_eq!(partitioner.partition("topic",
                                         None,
                                         Some("key").as_ref(),
                                         Some("value").as_ref(),
                                         metadata),
                   None);
    }

    #[test]
    fn test_key_partitioning() {
        let partitions = (0..3)
            .map(|id| {
                     PartitionInfo {
                         partition: id,
                         ..Default::default()
                     }
                 })
            .collect();
        let metadata = Rc::new(Metadata::with_topics(vec![("topic".to_owned(), partitions)]));

        let partitioner = DefaultPartitioner::new();

        // partition with key
        assert_eq!(partitioner.partition("topic",
                                         None,
                                         Some("key").as_ref(),
                                         Some("value").as_ref(),
                                         metadata.clone()),
                   Some(2));

        // partition without key
        for id in 0..100 {
            assert_eq!(partitioner.partition::<(), &str>("topic",
                                                         None,
                                                         None,
                                                         Some("value").as_ref(),
                                                         metadata.clone()),
                       Some(id % 3));
        }

        assert_eq!(partitioner.records(), 100);
    }
}