use std::time::{Duration, Instant};
use std::collections::{HashMap, VecDeque};

use bytes::Bytes;

use futures::{Async, Future, Poll, Stream, future};
use futures::unsync::oneshot::{Canceled, Receiver, Sender, channel};

use errors::{Error, ErrorKind, Result};
use compression::Compression;
use protocol::{ApiVersion, MessageSet, MessageSetBuilder, Timestamp};
use client::{StaticBoxFuture, TopicPartition};
use producer::RecordMetadata;

/// Accumulator acts as a queue that accumulates records
pub trait Accumulator<'a> {
    /// Add a record to the accumulator, return the append result
    fn push_record(&mut self,
                   tp: TopicPartition<'a>,
                   timestamp: Timestamp,
                   key: Option<Bytes>,
                   value: Option<Bytes>,
                   api_version: ApiVersion)
                   -> PushRecord;
}

/// RecordAccumulator acts as a queue that accumulates records into ProducerRecord instances to be sent to the server.
pub struct RecordAccumulator<'a> {
    /// The size to use when allocating ProducerRecord instances
    batch_size: usize,
    /// The compression codec for the records
    compression: Compression,
    /// An artificial delay time to add before declaring a records instance that isn't full ready for sending.
    ///
    /// This allows time for more records to arrive.
    /// Setting a non-zero lingerMs will trade off some latency for potentially better throughput
    /// due to more batching (and hence fewer, larger requests).
    linger: Duration,
    /// An artificial delay time to retry the produce request upon receiving an error.
    ///
    /// This avoids exhausting all retries in a short period of time.
    retry_backoff: Duration,

    batches: HashMap<TopicPartition<'a>, VecDeque<ProducerBatch>>,
}

impl<'a> RecordAccumulator<'a> {
    pub fn new(batch_size: usize,
               compression: Compression,
               linger: Duration,
               retry_backoff: Duration)
               -> Self {
        RecordAccumulator {
            batch_size: batch_size,
            compression: compression,
            linger: linger,
            retry_backoff: retry_backoff,
            batches: HashMap::new(),
        }
    }
}

impl<'a> Accumulator<'a> for RecordAccumulator<'a> {
    fn push_record(&mut self,
                   tp: TopicPartition<'a>,
                   timestamp: Timestamp,
                   key: Option<Bytes>,
                   value: Option<Bytes>,
                   api_version: ApiVersion)
                   -> PushRecord {
        let batches = self.batches
            .entry(tp)
            .or_insert_with(|| VecDeque::new());

        if let Some(batch) = batches.back_mut() {
            let result = batch.push_record(timestamp, key.clone(), value.clone());

            if let Ok(push_recrod) = result {
                return PushRecord::new(push_recrod);
            }
        }

        let mut batch = ProducerBatch::new(api_version, self.compression, self.batch_size);

        let result = batch.push_record(timestamp, key, value);

        batches.push_back(batch);

        match result {
            Ok(push_recrod) => PushRecord::new(push_recrod),
            Err(err) => PushRecord::new(future::err(err)),
        }
    }
}

pub type PushRecord = StaticBoxFuture<RecordMetadata>;

impl<'a> Stream for RecordAccumulator<'a> {
    type Item = (TopicPartition<'a>, ProducerBatch);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        for (tp, batches) in self.batches.iter_mut() {
            let is_full = batches.len() > 1 ||
                          batches.back().map_or(false, |batches| batches.is_full());

            if is_full {
                if let Some(batch) = batches.pop_front() {
                    return Ok(Async::Ready(Some((tp.clone(), batch))));
                }
            }
        }

        Ok(Async::NotReady)
    }
}

pub struct Thunk {
    sender: Sender<RecordMetadata>,
}

pub struct ProducerBatch {
    builder: MessageSetBuilder,
    thunks: Vec<Thunk>,
    create_time: Instant,
    last_push_time: Instant,
}

impl ProducerBatch {
    pub fn new(api_version: ApiVersion, compression: Compression, write_limit: usize) -> Self {
        let now = Instant::now();

        ProducerBatch {
            builder: MessageSetBuilder::new(api_version, compression, write_limit, 0),
            thunks: vec![],
            create_time: now,
            last_push_time: now,
        }
    }

    pub fn is_full(&self) -> bool {
        self.builder.is_full()
    }

    pub fn push_record(&mut self,
                       timestamp: Timestamp,
                       key: Option<Bytes>,
                       value: Option<Bytes>)
                       -> Result<FutureRecordMetadata> {
        self.builder.push(timestamp, key, value)?;

        let (sender, receiver) = channel();

        self.thunks.push(Thunk { sender: sender });
        self.last_push_time = Instant::now();

        Ok(FutureRecordMetadata { receiver: receiver })
    }

    pub fn build(self) -> MessageSet {
        self.builder.build()
    }
}

pub struct FutureRecordMetadata {
    receiver: Receiver<RecordMetadata>,
}

impl Future for FutureRecordMetadata {
    type Item = RecordMetadata;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.receiver.poll() {
            Ok(result) => Ok(result),
            Err(Canceled) => bail!(ErrorKind::Canceled),
        }
    }
}
