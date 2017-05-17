use std::mem;
use std::rc::Rc;
use std::borrow::Cow;
use std::fmt::Debug;
use std::cell::RefCell;
use std::ops::Deref;
use std::net::{SocketAddr, ToSocketAddrs};
use std::collections::HashMap;
use std::iter::FromIterator;
use std::time::Duration;

use futures::{Async, Poll};
use futures::future::{self, Future};
use futures::unsync::oneshot;
use tokio_core::reactor::{Handle, Timeout};
use tokio_service::Service;
use tokio_middleware::{Log as LogMiddleware, Timeout as TimeoutMiddleware};

use errors::{Error, ErrorKind};
use protocol::{ApiKeys, ApiVersion, CorrelationId, ErrorCode, FetchOffset, KafkaCode, MessageSet,
               Offset, PartitionId, RequiredAcks, UsableApiVersions};
use network::{KafkaRequest, KafkaResponse, TopicPartition};
use client::{Broker, BrokerRef, ClientConfig, Cluster, KafkaService, Metadata, Metrics};

pub trait Client<'a>: 'static {
    fn produce_records(&self,
                       acks: RequiredAcks,
                       timeout: Duration,
                       TopicPartition<'a>,
                       records: Vec<Cow<'a, MessageSet>>)
                       -> ProduceRecords;

    fn fetch_offsets<S: AsRef<str>>(&self, topic_names: &[S], offset: FetchOffset) -> FetchOffsets;

    fn load_metadata(&mut self) -> LoadMetadata<'a>;
}

#[derive(Clone)]
pub struct KafkaClient<'a> {
    inner: Rc<Inner<'a>>,
}

struct Inner<'a> {
    config: ClientConfig,
    handle: Handle,
    service: LogMiddleware<TimeoutMiddleware<KafkaService<'a>>>,
    metrics: Option<Rc<Metrics>>,
    state: Rc<RefCell<State>>,
}

#[derive(Default)]
struct State {
    correlation_id: CorrelationId,
    metadata: MetadataStatus,
}

enum MetadataStatus {
    Loading(Rc<RefCell<Vec<oneshot::Sender<Rc<Metadata>>>>>),
    Loaded(Rc<Metadata>),
}

impl Default for MetadataStatus {
    fn default() -> Self {
        MetadataStatus::Loading(Rc::new(RefCell::new(Vec::new())))
    }
}

impl<'a> Deref for KafkaClient<'a> {
    type Target = ClientConfig;

    fn deref(&self) -> &Self::Target {
        &self.inner.config
    }
}

impl<'a> KafkaClient<'a>
    where Self: 'static
{
    pub fn from_hosts<I>(hosts: I, handle: Handle) -> KafkaClient<'a>
        where I: Iterator<Item = SocketAddr>
    {
        KafkaClient::from_config(ClientConfig::from_hosts(hosts), handle)
    }

    pub fn from_config(config: ClientConfig, handle: Handle) -> KafkaClient<'a> {
        trace!("create client from config: {:?}", config);

        let metrics = if config.metrics {
            Some(Rc::new(Metrics::new().expect("fail to register metrics")))
        } else {
            None
        };
        let service = LogMiddleware::new(TimeoutMiddleware::new(KafkaService::new(handle.clone(),
                                                              config.max_connection_idle(),
                                                              metrics.clone()),
                                            config.timer(),
                                            config.request_timeout()));

        let inner = Rc::new(Inner {
                                config: config,
                                handle: handle.clone(),
                                service: service,
                                metrics: metrics,
                                state: Rc::new(RefCell::new(State::default())),
                            });

        let mut client = KafkaClient { inner: inner };

        handle.spawn(client
                         .load_metadata()
                         .map(|metadata| {
                                  trace!("auto loaded metadata, {:?}", metadata);
                              })
                         .map_err(|err| {
                                      warn!("fail to load metadata, {}", err);
                                  }));

        client
    }

    pub fn handle(&self) -> &Handle {
        &self.inner.handle
    }

    pub fn metrics(&self) -> Option<Rc<Metrics>> {
        self.inner.metrics.clone()
    }

    pub fn metadata(&self) -> GetMetadata {
        (*self.inner.state).borrow().metadata()
    }
}

impl<'a> Client<'a> for KafkaClient<'a>
    where Self: 'static
{
    fn produce_records(&self,
                       required_acks: RequiredAcks,
                       timeout: Duration,
                       tp: TopicPartition<'a>,
                       records: Vec<Cow<'a, MessageSet>>)
                       -> ProduceRecords {
        let inner = self.inner.clone();
        let future = self.metadata()
            .and_then(move |metadata| {
                          inner.produce_records(metadata, required_acks, timeout, tp, records)
                      });
        ProduceRecords::new(future)
    }

    fn fetch_offsets<S: AsRef<str>>(&self, topic_names: &[S], offset: FetchOffset) -> FetchOffsets {
        let inner = self.inner.clone();
        let topic_names: Vec<String> = topic_names
            .iter()
            .map(|s| s.as_ref().to_owned())
            .collect();
        let future = self.metadata()
            .and_then(move |metadata| {
                          let topics = inner.topics_by_broker(metadata, &topic_names);

                          inner.fetch_offsets(topics, offset)
                      });
        FetchOffsets::new(future)
    }

    fn load_metadata(&mut self) -> LoadMetadata<'a> {
        debug!("loading metadata...");

        if self.inner.config.metadata_max_age > 0 {
            let handle = self.inner.handle.clone();

            let timeout = Timeout::new(self.inner.config.metadata_max_age(), &handle);

            match timeout {
                Ok(timeout) => {
                    let inner = self.inner.clone();
                    let future = timeout
                        .map_err(Error::from)
                        .and_then(move |_| LoadMetadata::new(inner.clone()))
                        .map(|_| ())
                        .map_err(|_| ());

                    handle.spawn(future);
                }
                Err(err) => {
                    warn!("fail to create timeout, {}", err);
                }
            }
        }

        LoadMetadata::new(self.inner.clone())
    }
}

impl<'a> Inner<'a>
    where Self: 'static
{
    fn next_correlation_id(&self) -> CorrelationId {
        (*self.state).borrow_mut().next_correlation_id()
    }

    fn client_id(&self) -> Option<Cow<'a, str>> {
        self.config.client_id.clone().map(Cow::from)
    }

    pub fn metadata(&self) -> GetMetadata {
        (*self.state).borrow().metadata()
    }

    fn fetch_metadata<S>(&self, topic_names: &[S]) -> FetchMetadata
        where S: AsRef<str> + Debug
    {
        debug!("fetch metadata for toipcs: {:?}", topic_names);

        let responses = {
            let mut responses = Vec::new();

            for addr in &self.config.hosts {
                let request = KafkaRequest::fetch_metadata(0, // api_version
                                                           self.next_correlation_id(),
                                                           self.client_id(),
                                                           topic_names);

                let response = self.service
                    .call((*addr, request))
                    .and_then(|res| if let KafkaResponse::Metadata(res) = res {
                                  future::ok(Rc::new(Metadata::from(res)))
                              } else {
                                  future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                              });

                responses.push(response);
            }

            responses
        };

        FetchMetadata::new(future::select_ok(responses).map(|(metadata, _)| metadata))
    }

    fn fetch_api_versions(&self, broker: &Broker) -> FetchApiVersions {
        debug!("fetch API versions for broker: {:?}", broker);

        let addr = broker
            .addr()
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap(); // TODO

        let request = KafkaRequest::fetch_api_versions(0, // api_version,
                                                       self.next_correlation_id(),
                                                       self.client_id());

        let response = self.service
            .call((addr, request))
            .and_then(|res| if let KafkaResponse::ApiVersions(res) = res {
                          future::ok(UsableApiVersions::new(res.api_versions))
                      } else {
                          future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                      });

        FetchApiVersions::new(response)
    }

    fn load_api_versions(&self, metadata: Rc<Metadata>) -> LoadApiVersions {
        trace!("load API versions from brokers, {:?}", metadata.brokers());

        let responses = {
            let mut responses = Vec::new();

            for broker in metadata.brokers() {
                let broker_ref = broker.as_ref();
                let response = self.fetch_api_versions(broker)
                    .map(move |api_versions| (broker_ref, api_versions));

                responses.push(response);
            }

            responses
        };
        let responses = future::join_all(responses).map(HashMap::from_iter);

        LoadApiVersions::new(StaticBoxFuture::new(responses))
    }

    fn produce_records(&self,
                       metadata: Rc<Metadata>,
                       required_acks: RequiredAcks,
                       timeout: Duration,
                       tp: TopicPartition<'a>,
                       records: Vec<Cow<'a, MessageSet>>)
                       -> ProduceRecords {
        let (api_version, addr) = metadata
            .leader_for(&tp)
            .map_or_else(|| (0, *self.config.hosts.iter().next().unwrap()),
                         |broker| {
                (broker.api_version(ApiKeys::Produce).unwrap_or_default(),
                 broker
                     .addr()
                     .to_socket_addrs()
                     .unwrap()
                     .next()
                     .unwrap())
            });

        let request = KafkaRequest::produce_records(api_version,
                                                    self.next_correlation_id(),
                                                    self.client_id(),
                                                    required_acks,
                                                    timeout,
                                                    &tp,
                                                    records);

        let response = self.service
            .call((addr, request))
            .and_then(|res| if let KafkaResponse::Produce(res) = res {
                          let produce = res.topics
                              .iter()
                              .map(|topic| {
                    (topic.topic_name.to_owned(),
                     topic
                         .partitions
                         .iter()
                         .map(|partition| {
                                  (partition.partition, partition.error_code, partition.offset)
                              })
                         .collect())
                })
                              .collect();

                          future::ok(produce)
                      } else {
                          future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                      });

        ProduceRecords::new(response)
    }

    fn topics_by_broker<S>(&self, metadata: Rc<Metadata>, topic_names: &[S]) -> Topics<'a>
        where S: AsRef<str>
    {
        let mut topics = HashMap::new();

        for topic_name in topic_names.iter() {
            if let Some(partitions) = metadata.partitions_for_topic(topic_name.as_ref()) {
                for topic_partition in partitions {
                    if let Some(broker) = metadata.leader_for(&topic_partition) {
                        let addr = broker
                            .addr()
                            .to_socket_addrs()
                            .unwrap()
                            .next()
                            .unwrap(); // TODO
                        let api_version = broker
                            .api_version(ApiKeys::ListOffsets)
                            .unwrap_or_default();
                        topics
                            .entry((addr, api_version))
                            .or_insert_with(HashMap::new)
                            .entry(topic_name.as_ref().to_owned().into())
                            .or_insert_with(Vec::new)
                            .push(topic_partition.partition);
                    }
                }
            }
        }

        topics
    }

    fn fetch_offsets(&self, topics: Topics<'a>, offset: FetchOffset) -> FetchOffsets {
        let responses = {
            let mut responses = Vec::new();

            for ((addr, api_version), topics) in topics {
                let request = KafkaRequest::list_offsets(api_version,
                                                         self.next_correlation_id(),
                                                         self.client_id(),
                                                         topics,
                                                         offset);
                let response = self.service
                    .call((addr, request))
                    .and_then(|res| {
                        if let KafkaResponse::ListOffsets(res) = res {
                            let topics = res.topics
                                .iter()
                                .map(|topic| {
                                    let partitions = topic
                                        .partitions
                                        .iter()
                                        .flat_map(|partition| {
                                            if partition.error_code ==
                                               KafkaCode::None as ErrorCode {
                                                Ok(PartitionOffset {
                                                       partition: partition.partition,
                                                       offset: *partition
                                                                    .offsets
                                                                    .iter()
                                                                    .next()
                                                                    .unwrap(), // TODO
                                                   })
                                            } else {
                                                Err(ErrorKind::KafkaError(partition
                                                                              .error_code
                                                                              .into()))
                                            }
                                        })
                                        .collect();

                                    (topic.topic_name.clone(), partitions)
                                })
                                .collect::<Vec<(String, Vec<PartitionOffset>)>>();

                            Ok(topics)
                        } else {
                            bail!(ErrorKind::UnexpectedResponse(res.api_key()))
                        }
                    });

                responses.push(response);
            }

            responses
        };

        let offsets = future::join_all(responses).map(|responses| {
            responses
                .iter()
                .fold(HashMap::new(), |mut offsets, topics| {
                    for &(ref topic_name, ref partitions) in topics {
                        offsets
                            .entry(topic_name.clone())
                            .or_insert_with(Vec::new)
                            .extend(partitions.iter().cloned())
                    }
                    offsets
                })
        });

        FetchOffsets::new(offsets)
    }
}

type Topics<'a> = HashMap<(SocketAddr, ApiVersion), HashMap<Cow<'a, str>, Vec<PartitionId>>>;

impl State {
    pub fn next_correlation_id(&mut self) -> CorrelationId {
        self.correlation_id = self.correlation_id.wrapping_add(1);
        self.correlation_id - 1
    }

    pub fn metadata(&self) -> GetMetadata {
        let (sender, receiver) = oneshot::channel();

        match self.metadata {
            MetadataStatus::Loading(ref senders) => (*senders).borrow_mut().push(sender),
            MetadataStatus::Loaded(ref metadata) => drop(sender.send(metadata.clone())),
        }

        GetMetadata::new(receiver.map_err(|_| ErrorKind::Canceled("load metadata canceled").into()))
    }

    pub fn set_metadata(&mut self, metadata: Rc<Metadata>) {
        let status = mem::replace(&mut self.metadata, MetadataStatus::Loaded(metadata.clone()));

        if let MetadataStatus::Loading(senders) = status {
            for sender in Rc::try_unwrap(senders)
                    .unwrap()
                    .into_inner()
                    .into_iter() {
                drop(sender.send(metadata.clone()));
            }
        }
    }
}

/// A retrieved offset for a particular partition in the context of an already known topic.
#[derive(Clone, Debug)]
pub struct PartitionOffset {
    pub partition: PartitionId,
    pub offset: Offset,
}

pub struct LoadMetadata<'a> {
    state: Loading,
    inner: Rc<Inner<'a>>,
}

pub enum Loading {
    Metadata(FetchMetadata),
    ApiVersions(Rc<Metadata>, LoadApiVersions),
    Finished(Rc<Metadata>),
}

impl<'a> LoadMetadata<'a>
    where Self: 'static
{
    fn new(inner: Rc<Inner<'a>>) -> LoadMetadata<'a> {
        let fetch_metadata = inner.fetch_metadata::<&str>(&[]);

        LoadMetadata {
            state: Loading::Metadata(fetch_metadata),
            inner: inner,
        }
    }
}

impl<'a> Future for LoadMetadata<'a>
    where Self: 'static
{
    type Item = Rc<Metadata>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let state;

            match self.state {
                Loading::Metadata(ref mut future) => {
                    match future.poll() {
                        Ok(Async::Ready(metadata)) => {
                            let inner = self.inner.clone();

                            if inner.config.api_version_request {
                                state = Loading::ApiVersions(metadata.clone(),
                                                             inner.load_api_versions(metadata));
                            } else {
                                let fallback_api_versions =
                                    inner.config.broker_version_fallback.api_versions();

                                let metadata = Rc::new(metadata.with_fallback_api_versions(fallback_api_versions));

                                trace!("use fallback API versions from {:?}, {:?}",
                                       inner.config.broker_version_fallback,
                                       fallback_api_versions);

                                state = Loading::Finished(metadata);
                            }
                        }
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(err) => return Err(err),
                    }
                }
                Loading::ApiVersions(ref metadata, ref mut future) => {
                    match future.poll() {
                        Ok(Async::Ready(api_versions)) => {
                            let metadata = Rc::new(metadata.with_api_versions(api_versions));

                            state = Loading::Finished(metadata);
                        }
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(err) => return Err(err),
                    }
                }
                Loading::Finished(ref metadata) => {
                    (*self.inner.state)
                        .borrow_mut()
                        .set_metadata(metadata.clone());

                    return Ok(Async::Ready(metadata.clone()));
                }
            }

            self.state = state;
        }
    }
}


pub struct StaticBoxFuture<F = (), E = Error>(Box<Future<Item = F, Error = E> + 'static>);

impl<F, E> StaticBoxFuture<F, E> {
    pub fn new<T>(inner: T) -> Self
        where T: Future<Item = F, Error = E> + 'static
    {
        StaticBoxFuture(Box::new(inner))
    }
}

impl<F, E> Future for StaticBoxFuture<F, E> {
    type Item = F;
    type Error = E;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

pub type GetMetadata = StaticBoxFuture<Rc<Metadata>>;
pub type ProduceRecords = StaticBoxFuture<HashMap<String, Vec<(PartitionId, ErrorCode, Offset)>>>;
pub type FetchOffsets = StaticBoxFuture<HashMap<String, Vec<PartitionOffset>>>;
pub type FetchMetadata = StaticBoxFuture<Rc<Metadata>>;
pub type FetchApiVersions = StaticBoxFuture<UsableApiVersions>;
pub type LoadApiVersions = StaticBoxFuture<HashMap<BrokerRef, UsableApiVersions>>;
