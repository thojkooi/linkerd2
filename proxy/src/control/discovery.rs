use std::borrow::Borrow;
use std::collections::VecDeque;
use std::collections::hash_map::{self, Entry, HashMap};
use std::hash::{Hash, Hasher};
use std::iter::{self, IntoIterator};
use std::net::SocketAddr;
use std::sync::Arc;
use std::{fmt, ops};

use futures::{Async, Future, Poll, Stream};
use futures::sync::mpsc;
use http;
use tower::Service;
use tower_h2::{HttpService, BoxBody, RecvBody};
use tower_discover::{Change, Discover};
use tower_grpc as grpc;

use fully_qualified_authority::FullyQualifiedAuthority;

use conduit_proxy_controller_grpc::common::{Destination, TcpAddress};
use conduit_proxy_controller_grpc::destination::{
    Update as PbUpdate,
    WeightedAddr,
};
use conduit_proxy_controller_grpc::destination::update::Update as PbUpdate2;
use conduit_proxy_controller_grpc::destination::client::{Destination as DestinationSvc};
use transport::DnsNameAndPort;

use control::cache::{Cache, CacheChange, Exists};

/// A handle to start watching a destination for address changes.
#[derive(Clone, Debug)]
pub struct Discovery {
    tx: mpsc::UnboundedSender<(DnsNameAndPort, mpsc::UnboundedSender<Update>)>,
}

/// A `tower_discover::Discover`, given to a `tower_balance::Balance`.
#[derive(Debug)]
pub struct Watch<B> {
    rx: mpsc::UnboundedReceiver<Update>,
    bind: B,
}

/// A background handle to eventually bind on the controller thread.
#[derive(Debug)]
pub struct Background {
    rx: mpsc::UnboundedReceiver<(DnsNameAndPort, mpsc::UnboundedSender<Update>)>,
    default_destination_namespace: String,
}

/// A future returned from `Background::work()`, doing the work of talking to
/// the controller destination API.
// TODO: debug impl
pub struct DiscoveryWork<T: HttpService<ResponseBody = RecvBody>> {
    default_destination_namespace: String,
    destinations: HashMap<DnsNameAndPort, DestinationSet<T>>,
    /// A queue of authorities that need to be reconnected.
    reconnects: VecDeque<DnsNameAndPort>,
    /// The Destination.Get RPC client service.
    /// Each poll, records whether the rpc service was till ready.
    rpc_ready: bool,
    /// A receiver of new watch requests.
    rx: mpsc::UnboundedReceiver<(DnsNameAndPort, mpsc::UnboundedSender<Update>)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DstLabels {
    set: Arc<HashMap<String, String>>,
    addr: Arc<HashMap<String, String>>,
}

/// A service that adds a label extension to all requests transiting it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelRequest<T> {
    labels: Option<DstLabels>,
    inner: T,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Labeled<T> {
    metric_labels: DstLabels,
    inner: T,
}

struct DestinationSet<T: HttpService<ResponseBody = RecvBody>> {
    addrs: Exists<Cache<Labeled<SocketAddr>>>,
    query: Option<DestinationServiceQuery<T>>,
    txs: Vec<mpsc::UnboundedSender<Update>>,
}

enum DestinationServiceQuery<T: HttpService<ResponseBody = RecvBody>> {
    NeedsReconnect,
    ConnectedOrConnecting {
        rx: UpdateRx<T>
    },
}

/// Receiver for destination set updates.
///
/// The destination RPC returns a `ResponseFuture` whose item is a
/// `Response<Stream>`, so this type holds the state of that RPC call ---
/// either we're waiting for the future, or we have a stream --- and allows
/// us to implement `Stream` regardless of whether the RPC has returned yet
/// or not.
///
/// Polling an `UpdateRx` polls the wrapped future while we are
/// `Waiting`, and the `Stream` if we are `Streaming`. If the future is `Ready`,
/// then we switch states to `Streaming`.
enum UpdateRx<T: HttpService<ResponseBody = RecvBody>> {
    Waiting(UpdateRsp<T::Future>),
    Streaming(grpc::Streaming<PbUpdate, T::ResponseBody>),
}

type UpdateRsp<F> =
    grpc::client::server_streaming::ResponseFuture<PbUpdate, F>;

/// Wraps the error types returned by `UpdateRx` polls.
///
/// An `UpdateRx` error is either the error type of the `Future` in the
/// `UpdateRx::Waiting` state, or the `Stream` in the `UpdateRx::Streaming`
/// state.
// TODO: impl Error?
#[derive(Debug)]
enum RxError<T> {
    Future(grpc::Error<T>),
    Stream(grpc::Error),
}

#[derive(Debug)]
enum Update {
    Insert(Labeled<SocketAddr>),
    Remove(Labeled<SocketAddr>),
}

/// Bind a `SocketAddr` with a protocol.
pub trait Bind {
    /// Requests handled by the discovered services
    type Request;

    /// Responses given by the discovered services
    type Response;

    /// Errors produced by the discovered services
    type Error;

    type BindError;

    /// The discovered `Service` instance.
    type Service: Service<Request = Self::Request, Response = Self::Response, Error = Self::Error>;

    /// Bind a socket address with a service.
    fn bind(&self, addr: &SocketAddr) -> Result<Self::Service, Self::BindError>;
}

/// Creates a "channel" of `Discovery` to `Background` handles.
///
/// The `Discovery` is used by a listener, the `Background` is consumed
/// on the controller thread.
pub fn new(default_destination_namespace: String) -> (Discovery, Background) {
    let (tx, rx) = mpsc::unbounded();
    (
        Discovery {
            tx,
        },
        Background {
            rx,
            default_destination_namespace,
        },
    )
}

// ==== impl Discovery =====

impl Discovery {
    /// Start watching for address changes for a certain authority.
    pub fn resolve<B>(&self, authority: &DnsNameAndPort, bind: B) -> Watch<B> {
        trace!("resolve; authority={:?}", authority);
        let (tx, rx) = mpsc::unbounded();
        self.tx
            .unbounded_send((authority.clone(), tx))
            .expect("unbounded can't fail");

        Watch {
            rx,
            bind,
        }
    }
}

// ==== impl Watch =====

impl<B, A> Discover for Watch<B>
where
    B: Bind<Request = http::Request<A>>,
{
    type Key = SocketAddr;
    type Request = B::Request;
    type Response = B::Response;
    type Error = B::Error;
    type Service = LabelRequest<B::Service>;
    type DiscoverError = ();

    fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::DiscoverError> {
        let up = self.rx.poll();
        trace!("watch: {:?}", up);
        let update = match up {
            Ok(Async::Ready(Some(update))) => update,
            Ok(Async::Ready(None)) => unreachable!(),
            Ok(Async::NotReady) => return Ok(Async::NotReady),
            Err(_) => return Err(()),
        };

        match update {
            Update::Insert(Labeled { metric_labels, inner: addr }) => {
                let service = self.bind.bind(&addr)
                    .map(|svc| LabelRequest::new(metric_labels, svc))
                    .map_err(|_| ())?;

                Ok(Async::Ready(Change::Insert(addr, service)))
            }
            Update::Remove(addr) => Ok(Async::Ready(Change::Remove(*addr))),
        }
    }
}

// ==== impl Background =====

impl Background {
    /// Bind this handle to start talking to the controller API.
    pub fn work<T>(self) -> DiscoveryWork<T>
    where T: HttpService<RequestBody = BoxBody, ResponseBody = RecvBody>,
          T::Error: fmt::Debug,
    {
        DiscoveryWork {
            default_destination_namespace: self.default_destination_namespace,
            destinations: HashMap::new(),
            reconnects: VecDeque::new(),
            rpc_ready: false,
            rx: self.rx,
        }
    }
}

// ==== impl DiscoveryWork =====

impl<T> DiscoveryWork<T>
where
    T: HttpService<RequestBody = BoxBody, ResponseBody = RecvBody>,
    T::Error: fmt::Debug,
{
    pub fn poll_rpc(&mut self, client: &mut T) {
        // This loop is make sure any streams that were found disconnected
        // in `poll_destinations` while the `rpc` service is ready should
        // be reconnected now, otherwise the task would just sleep...
        loop {
            self.poll_new_watches(client);
            self.poll_destinations();

            if self.reconnects.is_empty() || !self.rpc_ready {
                break;
            }
        }
    }

    fn poll_new_watches(&mut self, client: &mut T) {
        loop {
            // if rpc service isn't ready, not much we can do...
            match client.poll_ready() {
                Ok(Async::Ready(())) => {
                    self.rpc_ready = true;
                }
                Ok(Async::NotReady) => {
                    self.rpc_ready = false;
                    break;
                }
                Err(err) => {
                    warn!("Destination.Get poll_ready error: {:?}", err);
                    self.rpc_ready = false;
                    break;
                }
            }

            // handle any pending reconnects first
            if self.poll_reconnect(client) {
                continue;
            }

            // check for any new watches
            match self.rx.poll() {
                Ok(Async::Ready(Some((auth, tx)))) => {
                    trace!("Destination.Get {:?}", auth);
                    match self.destinations.entry(auth) {
                        Entry::Occupied(mut occ) => {
                            let set = occ.get_mut();
                            // we may already know of some addresses here, so push
                            // them onto the new watch first
                            match set.addrs {
                                Exists::Yes(ref mut cache) => {
                                    for addr in cache.values().iter().cloned() {
                                        tx.unbounded_send(Update::Insert(addr))
                                            .expect("unbounded_send does not fail");
                                    }
                                },
                                Exists::No | Exists::Unknown => (),
                            }
                            set.txs.push(tx);
                        }
                        Entry::Vacant(vac) => {
                            let query =
                                DestinationServiceQuery::connect_maybe(
                                    &self.default_destination_namespace,
                                    client,
                                    vac.key(),
                                    "connect");
                            vac.insert(DestinationSet {
                                addrs: Exists::Unknown,
                                query,
                                txs: vec![tx],
                            });
                        }
                    }
                }
                Ok(Async::Ready(None)) => {
                    trace!("Discover tx is dropped, shutdown?");
                    return;
                }
                Ok(Async::NotReady) => break,
                Err(_) => unreachable!("unbounded receiver doesn't error"),
            }
        }
    }

    /// Tries to reconnect next watch stream. Returns true if reconnection started.
    fn poll_reconnect(&mut self, client: &mut T) -> bool {
        debug_assert!(self.rpc_ready);

        while let Some(auth) = self.reconnects.pop_front() {
            if let Some(set) = self.destinations.get_mut(&auth) {
                set.query = DestinationServiceQuery::connect_maybe(
                    &self.default_destination_namespace,
                    client,
                    &auth,
                    "reconnect");
                return true;
            } else {
                trace!("reconnect no longer needed: {:?}", auth);
            }
        }
        false
    }

    fn poll_destinations(&mut self) {
        for (auth, set) in &mut self.destinations {
            set.query = match set.query.take() {
                Some(DestinationServiceQuery::ConnectedOrConnecting{ rx }) => {
                    let new_query = set.poll_destination(auth, rx);
                    if let DestinationServiceQuery::NeedsReconnect = new_query {
                        set.reset_on_next_modification();
                        self.reconnects.push_back(auth.clone());
                    }
                    Some(new_query)
                },
                query => query,
            };
        }
    }
}


// ===== impl DestinationServiceQuery =====

impl<T: HttpService<RequestBody = BoxBody, ResponseBody = RecvBody>> DestinationServiceQuery<T> {
    // Initiates a query `query` to the Destination service and returns it as `Some(query)` if the
    // given authority's host is of a form suitable for using to query the Destination service.
    // Otherwise, returns `None`.
    fn connect_maybe(
        default_destination_namespace: &str,
        client: &mut T,
        auth: &DnsNameAndPort,
        connect_or_reconnect: &str)
        -> Option<Self>
    {
        trace!("DestinationServiceQuery {} {:?}", connect_or_reconnect, auth);
        FullyQualifiedAuthority::normalize(auth, default_destination_namespace)
            .map(|auth| {
                let req = Destination {
                    scheme: "k8s".into(),
                    path: auth.without_trailing_dot().to_owned(),
                };
                // TODO: Can grpc::Request::new be removed?
                let mut svc = DestinationSvc::new(client.lift_ref());
                let response = svc.get(grpc::Request::new(req));
                DestinationServiceQuery::ConnectedOrConnecting { rx: UpdateRx::Waiting(response) }
            })
    }
}

// ===== impl DestinationSet =====

impl<T> DestinationSet<T>
    where T: HttpService<RequestBody = BoxBody, ResponseBody = RecvBody>,
          T::Error: fmt::Debug
{
    fn poll_destination(&mut self, auth: &DnsNameAndPort, mut rx: UpdateRx<T>)
                        -> DestinationServiceQuery<T>
    {
        loop {
            match rx.poll() {
                Ok(Async::Ready(Some(update))) => match update.update {
                    Some(PbUpdate2::Add(a_set)) => {
                        let set_labels = Arc::new(a_set.metric_labels);
                        let addrs = a_set.addrs.into_iter()
                                .filter_map(|addr| {
                                    Labeled::from_pb(addr, &set_labels)
                                });
                        set.add(auth, addrs)
                    },
                    Some(PbUpdate2::Remove(r_set)) =>
                        self.remove(
                            auth,
                            r_set.addrs.iter().filter_map(|addr| pb_to_sock_addr(addr.clone()))),
                    Some(PbUpdate2::NoEndpoints(no_endpoints)) =>
                        self.no_endpoints(auth, no_endpoints.exists),
                    None => (),
                },
                Ok(Async::Ready(None)) => {
                    trace!(
                        "Destination.Get stream ended for {:?}, must reconnect",
                        auth
                    );
                    return DestinationServiceQuery::NeedsReconnect;
                },
                Ok(Async::NotReady) => {
                    return DestinationServiceQuery::ConnectedOrConnecting { rx };
                },
                Err(err) => {
                    warn!("Destination.Get stream errored for {:?}: {:?}", auth, err);
                    return DestinationServiceQuery::NeedsReconnect;
                }
            };
        }
    }
}

impl <T: HttpService<ResponseBody = RecvBody>> DestinationSet<T> {
    fn reset_on_next_modification(&mut self) {
        match self.addrs {
            Exists::Yes(ref mut cache) => {
                cache.set_reset_on_next_modification();
            },
            Exists::No |
            Exists::Unknown => (),
        }
    }

    fn add<A>(&mut self, authority_for_logging: &DnsNameAndPort, addrs_to_add: A)
        where A: Iterator<Item = Labeled<SocketAddr>>
    {
        let mut cache = match self.addrs.take() {
            Exists::Yes(mut cache) => cache,
            Exists::Unknown | Exists::No => Cache::new(),
        };
        cache.extend(
            addrs_to_add,
            &mut |addr, change| Self::on_change(&mut self.txs, authority_for_logging, addr,
                                                change));
        self.addrs = Exists::Yes(cache);
    }

    fn remove<A>(&mut self, authority_for_logging: &DnsNameAndPort, addrs_to_remove: A)
        where A: Iterator<Item = SocketAddr>
    {
        let cache = match self.addrs.take() {
            Exists::Yes(mut cache) => {
                cache.remove(
                    addrs_to_remove,
                    &mut |addr, change| Self::on_change(
                        &mut self.txs,
                        authority_for_logging,
                        // Wrap the removed addr in a fake label to placate the
                        // type system.
                        addr,
                        change
                    ));
                cache
            },
            Exists::Unknown | Exists::No => Cache::new(),
        };
        self.addrs = Exists::Yes(cache);
    }

    fn no_endpoints(&mut self, authority_for_logging: &DnsNameAndPort, exists: bool) {
        trace!("no endpoints for {:?} that is known to {}", authority_for_logging,
               if exists { "exist" } else { "not exist" });
        match self.addrs.take() {
            Exists::Yes(mut cache) => {
                cache.clear(
                    &mut |addr, change| Self::on_change(&mut self.txs, authority_for_logging, addr,
                                                        change));
            },
            Exists::Unknown | Exists::No => (),
        };
        self.addrs = if exists {
            Exists::Yes(Cache::new())
        } else {
            Exists::No
        };
    }

    fn on_change(txs: &mut Vec<mpsc::UnboundedSender<Update>>,
                 authority_for_logging: &DnsNameAndPort,
                 addr: Labeled<SocketAddr>,
                 change: CacheChange) {
        let (update_str, update_constructor): (&'static str, fn(Labeled<SocketAddr>) -> Update) =
            match change {
                CacheChange::Insertion => ("insert", Update::Insert),
                CacheChange::Removal => ("remove", Update::Remove),
            };
        trace!("{} {:?} for {:?}", update_str, addr, authority_for_logging);
        // retain is used to drop any senders that are dead
        txs.retain(|tx| {
            tx.unbounded_send(update_constructor(addr.clone())).is_ok()
        });
    }
}

// ===== impl Bind =====

impl<F, S, E> Bind for F
where
    F: Fn(&SocketAddr) -> Result<S, E>,
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;
    type BindError = E;

    fn bind(&self, addr: &SocketAddr) -> Result<Self::Service, Self::BindError> {
        (*self)(addr)
    }
}

// ===== impl UpdateRx =====

impl<T> Stream for UpdateRx<T>
where T: HttpService<RequestBody = BoxBody, ResponseBody = RecvBody>,
      T::Error: fmt::Debug,
{
    type Item = PbUpdate;
    type Error = RxError<T::Error>;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // this is not ideal.
        let stream = match *self {
            UpdateRx::Waiting(ref mut future) => match future.poll() {
                Ok(Async::Ready(response)) => response.into_inner(),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Err(e) => return Err(RxError::Future(e)),
            },
            UpdateRx::Streaming(ref mut stream) =>
                return stream.poll().map_err(RxError::Stream),
        };
        *self = UpdateRx::Streaming(stream);
        self.poll()
    }
}

// ===== impl DstLabels ====

impl Hash for DstLabels {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for pair in self {
            pair.hash(state);
        }
    }
}

impl<'a> IntoIterator for &'a DstLabels {
    type Item = (&'a str, &'a str);
    type IntoIter = iter::Map<
        iter::Chain<
            hash_map::Iter<'a, String, String>,
            hash_map::Iter<'a, String, String>,
        >,
        fn((&'a String, &'a String)) -> (&'a str, &'a str)
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn borrow_pair<'a>((k, v): (&'a String, &'a String))
            -> (&'a str, &'a str)
        {
            (k.as_ref(), v.as_ref())
        }
        self.set.iter()
            .chain(self.addr.iter())
            .map(borrow_pair)

    }
}

impl DstLabels {
    pub fn is_empty(&self) -> bool {
        self.addr.is_empty() && self.set.is_empty()
    }
}


impl Labeled<SocketAddr> {
    fn from_pb(pb: WeightedAddr, set_labels: &Arc<HashMap<String, String>>)
               -> Option<Self> {
        let inner = pb.addr.and_then(pb_to_sock_addr)?;
        let metric_labels = DstLabels {
            addr: Arc::new(pb.metric_labels),
            set: set_labels.clone(),
        };
        Some(Labeled { inner, metric_labels })
    }
}

impl<T> ops::Deref for Labeled<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> Borrow<T> for Labeled<T> {
    fn borrow(&self) -> &T {
        &self.inner
    }
}

impl<T> LabelRequest<T> {
    fn new(labels: DstLabels, inner: T) -> Self {
        Self { labels: Some(labels), inner }
    }

    pub fn none(inner: T) -> Self {
        Self { labels: None, inner }
    }
}

impl<T, A> Service for LabelRequest<T>
where
    T: Service<Request=http::Request<A>>,
{
    type Request = T::Request;
    type Response= T::Response;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let mut req = req;
        if let Some(ref labels) = self.labels {
            req.extensions_mut().insert(labels.clone());
        }
        self.inner.call(req)
    }

}

fn pb_to_sock_addr(pb: TcpAddress) -> Option<SocketAddr> {
    use conduit_proxy_controller_grpc::common::ip_address::Ip;
    use std::net::{Ipv4Addr, Ipv6Addr};
    /*
    current structure is:
    TcpAddress {
        ip: Option<IpAddress {
            ip: Option<enum Ip {
                Ipv4(u32),
                Ipv6(IPv6 {
                    first: u64,
                    last: u64,
                }),
            }>,
        }>,
        port: u32,
    }
    */
    match pb.ip {
        Some(ip) => match ip.ip {
            Some(Ip::Ipv4(octets)) => {
                let ipv4 = Ipv4Addr::from(octets);
                Some(SocketAddr::from((ipv4, pb.port as u16)))
            }
            Some(Ip::Ipv6(v6)) => {
                let octets = [
                    (v6.first >> 56) as u8,
                    (v6.first >> 48) as u8,
                    (v6.first >> 40) as u8,
                    (v6.first >> 32) as u8,
                    (v6.first >> 24) as u8,
                    (v6.first >> 16) as u8,
                    (v6.first >> 8) as u8,
                    v6.first as u8,
                    (v6.last >> 56) as u8,
                    (v6.last >> 48) as u8,
                    (v6.last >> 40) as u8,
                    (v6.last >> 32) as u8,
                    (v6.last >> 24) as u8,
                    (v6.last >> 16) as u8,
                    (v6.last >> 8) as u8,
                    v6.last as u8,
                ];
                let ipv6 = Ipv6Addr::from(octets);
                Some(SocketAddr::from((ipv6, pb.port as u16)))
            }
            None => None,
        },
        None => None,
    }
}
