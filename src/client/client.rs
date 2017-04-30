use std::io;
use std::rc::Rc;
use std::fmt::Debug;
use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::net::{SocketAddr, ToSocketAddrs};
use std::collections::HashMap;

use bytes::BytesMut;

use futures::{Async, Poll, Stream};
use futures::future::{self, Future};
use futures::unsync::oneshot;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_core::reactor::Handle;
use tokio_proto::BindClient;
use tokio_proto::streaming::{Message, Body};
use tokio_proto::streaming::pipeline::ClientProto;
use tokio_proto::util::client_proxy::ClientProxy;
use tokio_service::Service;

use errors::{Error, ErrorKind};
use network::{KafkaConnection, KafkaConnector, Pool, Pooled};
use protocol::{ApiKeys, KafkaCode, FetchOffset};
use network::{KafkaRequest, KafkaResponse, KafkaCodec};
use client::{KafkaConfig, KafkaState, Metadata, DEFAULT_MAX_CONNECTION_TIMEOUT};

impl From<FetchOffset> for i64 {
    fn from(offset: FetchOffset) -> Self {
        match offset {
            FetchOffset::Earliest => -2,
            FetchOffset::Latest => -1,
            FetchOffset::ByTime(t) => t.sec * 1000 + t.nsec as i64 / 1000_000,
        }
    }
}

/// A retrieved offset for a particular partition in the context of an already known topic.
#[derive(Clone, Debug)]
pub struct PartitionOffset {
    pub partition: i32,
    pub offset: i64,
}

pub struct KafkaClient {
    config: KafkaConfig,
    handle: Handle,
    connector: KafkaConnector,
    pool: Pool<SocketAddr, TokioClient>,
    state: Rc<RefCell<KafkaState>>,
}

impl Deref for KafkaClient {
    type Target = KafkaConfig;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl DerefMut for KafkaClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.config
    }
}

impl KafkaClient {
    pub fn from_config(config: KafkaConfig, handle: &Handle) -> Self {
        debug!("client with config: {:?}", config);

        let max_connection_idle = config
            .max_connection_idle()
            .unwrap_or(*DEFAULT_MAX_CONNECTION_TIMEOUT);

        KafkaClient {
            config: config,
            handle: handle.clone(),
            connector: KafkaConnector::new(handle),
            pool: Pool::new(max_connection_idle),
            state: Rc::new(RefCell::new(KafkaState::new())),
        }
    }

    pub fn from_hosts<A: ToSocketAddrs + Clone>(hosts: &[A], handle: &Handle) -> Self {
        KafkaClient::from_config(KafkaConfig::from_hosts(hosts), handle)
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    pub fn metadata(&self) -> Rc<Metadata> {
        self.state.borrow().metadata()
    }

    pub fn fetch_offsets<S: AsRef<str>>(&mut self,
                                        topic_names: &[S],
                                        offset: FetchOffset)
                                        -> FetchOffsets {
        let topics = {
            let metadata = self.state.borrow().metadata();

            let mut topics = HashMap::new();

            for topic_name in topic_names {
                if let Some(partitions) = metadata.partitions_for(topic_name.as_ref()) {
                    for (id, partition) in partitions {
                        if let Some(broker) = metadata.find_broker(partition.broker()) {
                            let addr = broker
                                .addr()
                                .to_socket_addrs()
                                .unwrap()
                                .next()
                                .unwrap(); // TODO
                            let api_version = if let Some(api_versions) = broker.api_versions() {
                                api_versions
                                    .get(ApiKeys::ListOffsets as usize)
                                    .map(|api_version| api_version.max_version)
                                    .unwrap_or(0)
                            } else {
                                0
                            };

                            topics
                                .entry((addr, api_version))
                                .or_insert_with(|| HashMap::new())
                                .entry(topic_name.as_ref().to_owned())
                                .or_insert_with(|| Vec::new())
                                .push(id);
                        }
                    }
                }
            }

            topics
        };

        let responses = {
            let mut correlation_ids = topics
                .iter()
                .map(|_| self.state.borrow_mut().next_correlation_id())
                .collect::<Vec<i32>>();
            let client_id = self.config.client_id();

            let mut responses = Vec::new();

            for ((addr, api_version), ref topics) in topics {
                let request = KafkaRequest::list_offsets(api_version,
                                                         correlation_ids.pop().unwrap(),
                                                         client_id.clone(),
                                                         topics.iter(),
                                                         offset);
                let response =
                    self.send_request(&addr, request)
                        .and_then(|res| {
                            if let KafkaResponse::ListOffsets(res) = res {
                                let topics =
                                    res.topics
                                        .iter()
                                        .map(|topic| {
                                            let partitions = topic
                                 .partitions
                                 .iter()
                                 .flat_map(|partition| {
                                if partition.error_code == KafkaCode::None as i16 {
                                    Ok(PartitionOffset {
                                           partition: partition.partition,
                                           offset: *partition.offsets.iter().next().unwrap(), //TODO
                                       })
                                } else {
                                    Err(ErrorKind::KafkaError(partition.error_code.into()))
                                }
                            }).collect();

                                            (topic.topic_name.clone(), partitions)
                                        })
                                        .collect::<Vec<(String, Vec<PartitionOffset>)>>();

                                Ok(topics)
                            } else {
                                bail!(ErrorKind::InvalidResponse)
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
                            .or_insert_with(|| Vec::new())
                            .extend(partitions.iter().map(|partition| partition.clone()))
                    }
                    offsets
                })
        });

        FetchOffsets::new(offsets)
    }

    pub fn load_metadata(&mut self) -> LoadMetadata {
        debug!("loading metadata...");

        let state = self.state.clone();

        StaticBoxFuture::new(self.fetch_metadata::<&str>(&[])
                                 .and_then(move |metadata| {
                                               state.borrow_mut().update_metadata(metadata);

                                               future::ok(())
                                           }))
    }

    fn fetch_metadata<S>(&mut self, topic_names: &[S]) -> FetchMetadata
        where S: AsRef<str> + Debug
    {
        debug!("fetch metadata for toipcs: {:?}", topic_names);

        let addrs = self.config.brokers().unwrap(); // TODO
        let addr = addrs.iter().next().unwrap();

        let api_version = 0;
        let correlation_id = self.state.borrow_mut().next_correlation_id();
        let client_id = self.config.client_id();
        let request =
            KafkaRequest::fetch_metadata(api_version, correlation_id, client_id, topic_names);

        let response = self.send_request(addr, request)
            .and_then(|res| if let KafkaResponse::Metadata(res) = res {
                          future::ok(Metadata::from(res))
                      } else {
                          future::err(ErrorKind::InvalidResponse.into())
                      });

        FetchMetadata::new(response)
    }

    fn send_request(&mut self, addr: &SocketAddr, request: KafkaRequest) -> FutureResponse {
        let checkout = self.pool.checkout(addr);
        let connect = {
            let handle = self.handle.clone();
            let connection_id = self.state.borrow_mut().next_connection_id();
            let pool = self.pool.clone();
            let key = Rc::new(addr.clone());

            self.connector
                .tcp(addr.clone())
                .map(move |io| {
                    let (tx, rx) = oneshot::channel();
                    let client = RemoteClient {
                            connection_id: connection_id,
                            client_rx: RefCell::new(Some(rx)),
                        }
                        .bind_client(&handle, io);
                    let pooled = pool.pooled(key, client);
                    drop(tx.send(pooled.clone()));
                    pooled
                })
        };

        let race = checkout
            .select(connect)
            .map(|(conn, _work)| conn)
            .map_err(|(err, _work)| {
                warn!("fail to checkout connection, {}", err);
                // the Pool Checkout cannot error, so the only error
                // is from the Connector
                // XXX: should wait on the Checkout? Problem is
                // that if the connector is failing, it may be that we
                // never had a pooled stream at all
                err.into()
            });

        let response = race.and_then(move |client| client.call(Message::WithoutBody(request)))
            .map(|msg| {
                     debug!("received message: {:?}", msg);

                     match msg {
                         Message::WithoutBody(res) => res,
                         Message::WithBody(res, _) => res,
                     }
                 })
            .map_err(Error::from);

        FutureResponse::new(response)
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

pub type SendRequest = StaticBoxFuture;
pub type FetchOffsets = StaticBoxFuture<HashMap<String, Vec<PartitionOffset>>>;
pub type LoadMetadata = StaticBoxFuture;
pub type FetchMetadata = StaticBoxFuture<Metadata>;
pub type FutureResponse = StaticBoxFuture<KafkaResponse>;

type TokioBody = Body<BytesMut, io::Error>;

pub struct KafkaBody(TokioBody);

impl Stream for KafkaBody {
    type Item = BytesMut;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<BytesMut>, io::Error> {
        self.0.poll()
    }
}

type TokioClient = ClientProxy<Message<KafkaRequest, KafkaBody>,
                               Message<KafkaResponse, TokioBody>,
                               io::Error>;

struct RemoteClient {
    connection_id: u32,
    client_rx: RefCell<Option<oneshot::Receiver<Pooled<SocketAddr, TokioClient>>>>,
}

impl<T> ClientProto<T> for RemoteClient
    where T: AsyncRead + AsyncWrite + Debug + 'static
{
    type Request = KafkaRequest;
    type RequestBody = <KafkaBody as Stream>::Item;
    type Response = KafkaResponse;
    type ResponseBody = BytesMut;
    type Error = io::Error;
    type Transport = KafkaConnection<T>;
    type BindTransport = BindingClient<T>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        trace!("bind transport for {:?}", io);

        BindingClient {
            connection_id: self.connection_id,
            rx: self.client_rx
                .borrow_mut()
                .take()
                .expect("client_rx was lost"),
            io: Some(io),
        }
    }
}

struct BindingClient<T> {
    connection_id: u32,
    rx: oneshot::Receiver<Pooled<SocketAddr, TokioClient>>,
    io: Option<T>,
}

impl<T> Future for BindingClient<T>
    where T: AsyncRead + AsyncWrite + Debug + 'static
{
    type Item = KafkaConnection<T>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.rx.poll() {
            Ok(Async::Ready(client)) => {
                trace!("got connection #{} for {:?}, client {:?}",
                       self.connection_id,
                       self.io,
                       client);

                let codec = KafkaCodec::new();

                Ok(Async::Ready(KafkaConnection::new(self.connection_id, self.io.take().expect("binding client io lost"), codec)))
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_canceled) => unreachable!(),
        }
    }
}