use std::hash::Hash;

use protocol::{Offset, PartitionId, Timestamp};

/// A key/value pair to be sent to Kafka.
///
/// This consists of a topic name to which the record is being sent,
/// an optional partition number, and an optional key and value.
#[derive(Clone, Debug)]
pub struct ProducerRecord<K, V>
    where K: Hash
{
    /// The topic this record is being sent to
    pub topic_name: String,
    /// The partition to which the record will be sent (or `None` if no partition was specified)
    pub partition: Option<PartitionId>,
    /// The key (or `None` if no key is specified)
    pub key: Option<K>,
    /// The value
    pub value: Option<V>,
    /// The timestamp
    pub timestamp: Option<Timestamp>,
}

impl<K> ProducerRecord<K, ()>
    where K: Hash
{
    pub fn from_key<S: AsRef<str>>(topic_name: S, key: K) -> Self {
        ProducerRecord {
            topic_name: topic_name.as_ref().to_owned(),
            partition: None,
            key: Some(key),
            value: None,
            timestamp: None,
        }
    }
}

impl<V> ProducerRecord<(), V> {
    pub fn from_value<S: AsRef<str>>(topic_name: S, value: V) -> Self {
        ProducerRecord {
            topic_name: topic_name.as_ref().to_owned(),
            partition: None,
            key: None,
            value: Some(value),
            timestamp: None,
        }
    }
}

impl<K, V> ProducerRecord<K, V>
    where K: Hash
{
    pub fn from_key_value<S: AsRef<str>>(topic_name: S, key: K, value: V) -> Self {
        ProducerRecord {
            topic_name: topic_name.as_ref().to_owned(),
            partition: None,
            key: Some(key),
            value: Some(value),
            timestamp: None,
        }
    }

    pub fn from_topic_record<S: AsRef<str>>(topic_name: S, record: TopicRecord<K, V>) -> Self {
        ProducerRecord {
            topic_name: topic_name.as_ref().to_owned(),
            partition: record.partition,
            key: record.key,
            value: record.value,
            timestamp: record.timestamp,
        }
    }

    pub fn from_partition_record<S: AsRef<str>>(topic_name: S,
                                                partition_id: PartitionId,
                                                record: PartitionRecord<K, V>)
                                                -> Self {
        ProducerRecord {
            topic_name: topic_name.as_ref().to_owned(),
            partition: Some(partition_id),
            key: record.key,
            value: record.value,
            timestamp: record.timestamp,
        }
    }

    pub fn with_partition(mut self, partition: PartitionId) -> Self {
        self.partition = Some(partition);
        self
    }

    pub fn with_timestamp(mut self, timestamp: Timestamp) -> Self {
        self.timestamp = Some(timestamp);
        self
    }
}

pub struct TopicRecord<K, V> {
    /// The partition to which the record will be sent (or `None` if no partition was specified)
    pub partition: Option<PartitionId>,
    /// The key (or `None` if no key is specified)
    pub key: Option<K>,
    /// The value
    pub value: Option<V>,
    /// The timestamp
    pub timestamp: Option<Timestamp>,
}

impl<K> TopicRecord<K, ()>
    where K: Hash
{
    pub fn from_key(key: K) -> Self {
        TopicRecord {
            partition: None,
            key: Some(key),
            value: None,
            timestamp: None,
        }
    }
}

impl<V> TopicRecord<(), V> {
    pub fn from_value(value: V) -> Self {
        TopicRecord {
            partition: None,
            key: None,
            value: Some(value),
            timestamp: None,
        }
    }
}

impl<K, V> TopicRecord<K, V>
    where K: Hash
{
    pub fn from_key_value(key: K, value: V) -> Self {
        TopicRecord {
            partition: None,
            key: Some(key),
            value: Some(value),
            timestamp: None,
        }
    }

    pub fn with_partition(mut self, partition: PartitionId) -> Self {
        self.partition = Some(partition);
        self
    }

    pub fn with_timestamp(mut self, timestamp: Timestamp) -> Self {
        self.timestamp = Some(timestamp);
        self
    }
}


pub struct PartitionRecord<K, V> {
    /// The key (or `None` if no key is specified)
    pub key: Option<K>,
    /// The value
    pub value: Option<V>,
    /// The timestamp
    pub timestamp: Option<Timestamp>,
}

impl<K> PartitionRecord<K, ()>
    where K: Hash
{
    pub fn from_key(key: K) -> Self {
        PartitionRecord {
            key: Some(key),
            value: None,
            timestamp: None,
        }
    }
}

impl<V> PartitionRecord<(), V> {
    pub fn from_value(value: V) -> Self {
        PartitionRecord {
            key: None,
            value: Some(value),
            timestamp: None,
        }
    }
}

impl<K, V> PartitionRecord<K, V>
    where K: Hash
{
    pub fn from_key_value(key: K, value: V) -> Self {
        PartitionRecord {
            key: Some(key),
            value: Some(value),
            timestamp: None,
        }
    }

    pub fn with_timestamp(mut self, timestamp: Timestamp) -> Self {
        self.timestamp = Some(timestamp);
        self
    }
}

/// The metadata for a record that has been acknowledged by the server
#[derive(Clone, Debug, Default)]
pub struct RecordMetadata {
    /// The topic the record was appended to
    pub topic_name: String,
    /// The partition the record was sent to
    pub partition: PartitionId,
    /// The offset of the record in the topic/partition.
    pub offset: Offset,
    /// The timestamp of the record in the topic/partition.
    pub timestamp: Timestamp,
    /// The size of the serialized, uncompressed key in bytes.
    pub serialized_key_size: usize,
    /// The size of the serialized, uncompressed value in bytes.
    pub serialized_value_size: usize,
}
