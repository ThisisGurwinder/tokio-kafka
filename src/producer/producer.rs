use std::rc::Rc;
use std::cell::{Ref, RefCell};
use std::borrow::Borrow;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Duration;
use std::net::SocketAddr;

use time;

use futures::{Future, Stream};
use tokio_core::reactor::Handle;
use tokio_retry::Retry;

use errors::Error;
use protocol::ApiKeys;
use network::TopicPartition;
use client::{Cluster, KafkaClient, Metadata, StaticBoxFuture, ToMilliseconds};
use producer::{Accumulator, Partitioner, ProducerBuilder, ProducerConfig, ProducerRecord,
               RecordAccumulator, RecordMetadata, Sender, Serializer};

pub trait Producer<'a> {
    type Key: Hash;
    type Value;

    /// Send the given record asynchronously and
    /// return a future which will eventually contain the response information.
    fn send(&mut self, record: ProducerRecord<Self::Key, Self::Value>) -> SendRecord;

    /// Flush any accumulated records from the producer.
    fn flush(&mut self, force: bool) -> Flush;
}

pub type SendRecord = StaticBoxFuture<RecordMetadata>;
pub type Flush = StaticBoxFuture;

pub struct KafkaProducer<'a, K, V, P> {
    client: Rc<RefCell<KafkaClient<'a>>>,
    config: ProducerConfig,
    accumulator: RecordAccumulator<'a>,
    key_serializer: K,
    value_serializer: V,
    partitioner: P,
}

impl<'a, K, V, P> KafkaProducer<'a, K, V, P>
    where Self: 'static
{
    pub fn new(client: KafkaClient<'a>,
               config: ProducerConfig,
               key_serializer: K,
               value_serializer: V,
               partitioner: P)
               -> Self {
        let accumulator =
            RecordAccumulator::new(config.batch_size, config.compression, config.linger());

        KafkaProducer {
            client: Rc::new(RefCell::new(client)),
            config: config,
            accumulator: accumulator,
            key_serializer: key_serializer,
            value_serializer: value_serializer,
            partitioner: partitioner,
        }
    }

    pub fn client(&self) -> Rc<RefCell<KafkaClient<'a>>> {
        self.client.clone()
    }

    pub fn as_client(&self) -> Ref<KafkaClient<'a>> {
        let client: &RefCell<KafkaClient> = self.client.borrow();

        client.borrow()
    }

    pub fn from_client(client: KafkaClient<'a>) -> ProducerBuilder<'a, K, V, P> {
        ProducerBuilder::from_client(client)
    }

    pub fn from_hosts<I>(hosts: I, handle: Handle) -> ProducerBuilder<'a, K, V, P>
        where I: Iterator<Item = SocketAddr>
    {
        ProducerBuilder::from_config(ProducerConfig::from_hosts(hosts), handle)
    }
}

impl<'a, K, V, P> Producer<'a> for KafkaProducer<'a, K, V, P>
    where K: Serializer,
          K::Item: Debug + Hash,
          V: Serializer,
          V::Item: Debug,
          P: Partitioner,
          Self: 'static
{
    type Key = K::Item;
    type Value = V::Item;

    fn send(&mut self, record: ProducerRecord<Self::Key, Self::Value>) -> SendRecord {
        trace!("sending record {:?}", record);

        let ProducerRecord {
            topic_name,
            partition,
            key,
            value,
            timestamp,
        } = record;

        let cluster: Rc<Metadata> = self.as_client().metadata();

        let partition = self.partitioner
            .partition(&topic_name,
                       partition,
                       key.as_ref(),
                       value.as_ref(),
                       cluster.clone())
            .unwrap_or_default();

        let key = key.and_then(|key| {
                                   self.key_serializer
                                       .serialize(&topic_name, key)
                                       .map_err(|err| warn!("fail to serialize key, {}", err))
                                       .ok()
                               });

        let value =
            value.and_then(|value| {
                               self.value_serializer
                                   .serialize(&topic_name, value)
                                   .map_err(|err| warn!("fail to serialize value, {}", err))
                                   .ok()
                           });

        let tp = TopicPartition {
            topic_name: topic_name.into(),
            partition: partition,
        };

        let timestamp =
            timestamp.unwrap_or_else(|| time::now_utc().to_timespec().as_millis() as i64);

        let api_version = cluster
            .leader_for(&tp)
            .and_then(|broker| broker.api_versions())
            .and_then(|api_versions| api_versions.find(ApiKeys::Produce))
            .map_or(0, |api_version| api_version.max_version);

        SendRecord::new(self.accumulator
                            .push_record(tp, timestamp, key, value, api_version))
    }

    fn flush(&mut self, force: bool) -> Flush {
        if force {
            trace!("force to flush batches");

            self.accumulator.flush();
        }

        let client = self.client.clone();
        let handle = self.as_client().handle().clone();
        let acks = self.config.acks;
        let ack_timeout = self.config.ack_timeout();
        let retry_strategy = self.config.retry_strategy();

        Flush::new(self.accumulator
                       .batches()
                       .for_each(move |(tp, batch)| {
            let sender = Sender::new(client.clone(), acks, ack_timeout, tp, batch);

            Retry::spawn(handle.clone(),
                         retry_strategy.clone(),
                         move || sender.send_batch())
                    .map_err(Error::from)
        }))
    }
}

#[cfg(test)]
pub mod mock {
    use std::hash::Hash;

    use futures::future;

    use producer::{Flush, Producer, ProducerRecord, RecordMetadata, SendRecord};

    #[derive(Debug, Default)]
    pub struct MockProducer<K, V>
        where K: Hash
    {
        pub records: Vec<(Option<K>, Option<V>)>,
    }

    impl<'a, K, V> Producer<'a> for MockProducer<K, V>
        where K: Hash + Clone,
              V: Clone
    {
        type Key = K;
        type Value = V;

        fn send(&mut self, record: ProducerRecord<Self::Key, Self::Value>) -> SendRecord {
            self.records.push((record.key, record.value));

            SendRecord::new(future::ok(RecordMetadata::default()))
        }

        fn flush(&mut self, _force: bool) -> Flush {
            Flush::new(future::ok(()))
        }
    }
}
