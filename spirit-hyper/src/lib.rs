#![doc(
    html_root_url = "https://docs.rs/spirit-hyper/0.1.0/spirit_hyper/",
    test(attr(deny(warnings)))
)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate arc_swap;
extern crate failure;
extern crate futures;
extern crate hyper;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate spirit;
extern crate spirit_tokio;
extern crate structopt;
extern crate tokio_io;

use std::borrow::Borrow;
use std::error::Error;
use std::fmt::{Debug, Display};
use std::iter;
use std::sync::Arc;

use arc_swap::ArcSwap;
use failure::Error as FailError;
use futures::future::Shared;
use futures::sync::oneshot::{self, Receiver};
use futures::{Async, Future, IntoFuture, Poll};
use hyper::body::Payload;
use hyper::server::conn::{Connection, Http};
use hyper::service::{self, Service};
use hyper::{Body, Request, Response};
use serde::Deserialize;
use spirit::helpers::{CfgHelper, IteratedCfgHelper};
use spirit::{Builder, Empty, Spirit};
use spirit_tokio::{ResourceMaker, TcpListen};
use structopt::StructOpt;
use tokio_io::{AsyncRead, AsyncWrite};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct HyperServer<Transport> {
    #[serde(flatten)]
    transport: Transport,
}

pub type HttpServer<ExtraCfg = Empty> = HyperServer<TcpListen<ExtraCfg>>;

struct ShutdownConn<T, S: Service> {
    conn: Connection<T, S>,
    shutdown: Option<Shared<Receiver<()>>>,
}

impl<T, S, B> Future for ShutdownConn<T, S>
where
    S: Service<ReqBody = Body, ResBody = B> + 'static,
    S::Error: Into<Box<Error + Send + Sync>>,
    S::Future: Send,
    T: AsyncRead + AsyncWrite + 'static,
    B: Payload + 'static,
{
    type Item = <Connection<T, S> as Future>::Item;
    type Error = <Connection<T, S> as Future>::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let do_shutdown = self
            .shutdown
            .as_mut()
            .map(Future::poll)
            .unwrap_or(Ok(Async::NotReady));
        match do_shutdown {
            Ok(Async::NotReady) => (), // Don't shutdown yet (or already done that)
            _ => {
                self.conn.graceful_shutdown();
                self.shutdown.take();
            }
        }
        self.conn.poll()
    }
}

pub trait ConnAction<S, O, C, ExtraCfg>
where
    S: Borrow<ArcSwap<C>> + Sync + Send + 'static,
{
    type IntoFuture;
    fn action(&self, &Arc<Spirit<S, O, C>>, &ExtraCfg) -> Self::IntoFuture;
}

impl<F, S, O, C, ExtraCfg, R> ConnAction<S, O, C, ExtraCfg> for F
where
    F: Fn(&Arc<Spirit<S, O, C>>, &ExtraCfg) -> R,
    S: Borrow<ArcSwap<C>> + Sync + Send + 'static,
{
    type IntoFuture = R;
    fn action(&self, arc: &Arc<Spirit<S, O, C>>, extra: &ExtraCfg) -> R {
        self(arc, extra)
    }
}

pub fn service_fn_ok<F, S, O, C, ExtraCfg>(
    f: F,
) -> impl ConnAction<
    S,
    O,
    C,
    ExtraCfg,
    IntoFuture = Result<
        (
            impl Service<ReqBody = Body, Future = impl Send> + Send,
            Http,
        ),
        FailError,
    >,
>
where
    // TODO: Make more generic ‒ return future, payload, ...
    F: Fn(&Arc<Spirit<S, O, C>>, &ExtraCfg, Request<Body>) -> Response<Body>
        + Send
        + Sync
        + 'static,
    ExtraCfg: Clone + Debug + PartialEq + Send + 'static,
    S: Borrow<ArcSwap<C>> + Sync + Send + 'static,
    for<'de> C: Deserialize<'de> + Send + Sync + 'static,
    O: Debug + StructOpt + Sync + Send + 'static,
{
    let f = Arc::new(f);
    move |spirit: &_, extra_cfg: &ExtraCfg| -> Result<_, FailError> {
        let spirit = Arc::clone(spirit);
        let extra_cfg = extra_cfg.clone();
        let f = Arc::clone(&f);
        let svc = move |req: Request<Body>| -> Response<Body> { f(&spirit, &extra_cfg, req) };
        Ok((service::service_fn_ok(svc), Http::new()))
    }
}

// TODO: implement service_fn

impl<S, O, C, Transport, Action, Srv, H> IteratedCfgHelper<S, O, C, Action>
    for HyperServer<Transport>
where
    S: Borrow<ArcSwap<C>> + Sync + Send + 'static,
    for<'de> C: Deserialize<'de> + Send + Sync + 'static,
    O: Debug + StructOpt + Sync + Send + 'static,
    Transport: ResourceMaker<S, O, C, ()>,
    Transport::Resource: AsyncRead + AsyncWrite + Send + 'static,
    Action: ConnAction<S, O, C, Transport::ExtraCfg> + Sync + Send + 'static,
    Action::IntoFuture: IntoFuture<Item = (Srv, H), Error = FailError>,
    <Action::IntoFuture as IntoFuture>::Future: Send + 'static,
    Srv: Service<ReqBody = Body> + Send + 'static,
    Srv::Future: Send,
    H: Borrow<Http> + Send + 'static,
{
    fn apply<Extractor, ExtractedIter, Name>(
        mut extractor: Extractor,
        action: Action,
        name: Name,
        builder: Builder<S, O, C>,
    ) -> Builder<S, O, C>
    where
        Extractor: FnMut(&C) -> ExtractedIter + Send + 'static,
        ExtractedIter: IntoIterator<Item = Self>,
        Name: Clone + Display + Send + Sync + 'static,
    {
        let (shutdown_send, shutdown_recv) = oneshot::channel::<()>();
        let shutdown_recv = shutdown_recv.shared();
        let inner_action = move |spirit: &_, resource, extra_cfg: &_, _: &()| {
            let shutdown_recv = shutdown_recv.clone();
            action
                .action(spirit, extra_cfg)
                .into_future()
                .and_then(|(srv, http)| {
                    let conn = http.borrow().serve_connection(resource, srv);
                    let conn = ShutdownConn {
                        shutdown: Some(shutdown_recv),
                        conn,
                    };
                    conn.map_err(FailError::from)
                })
        };
        let inner_extractor = move |cfg: &_| {
            extractor(cfg)
                .into_iter()
                .map(|instance| (instance.transport, ()))
        };
        let mut shutdown_send = Some(shutdown_send);
        Transport::apply(inner_extractor, inner_action, name, builder).on_terminate(move || {
            if let Some(send) = shutdown_send.take() {
                let _ = send.send(());
            }
        })
    }
}

impl<S, O, C, Transport, Action, Srv, H> CfgHelper<S, O, C, Action> for HyperServer<Transport>
where
    S: Borrow<ArcSwap<C>> + Sync + Send + 'static,
    for<'de> C: Deserialize<'de> + Send + Sync + 'static,
    O: Debug + StructOpt + Sync + Send + 'static,
    Transport: ResourceMaker<S, O, C, ()>,
    Transport::Resource: AsyncRead + AsyncWrite + Send + 'static,
    Action: ConnAction<S, O, C, Transport::ExtraCfg> + Sync + Send + 'static,
    Action::IntoFuture: IntoFuture<Item = (Srv, H), Error = FailError>,
    <Action::IntoFuture as IntoFuture>::Future: Send + 'static,
    Srv: Service<ReqBody = Body> + Send + 'static,
    Srv::Future: Send,
    H: Borrow<Http> + Send + 'static,
{
    fn apply<Extractor, Name>(
        mut extractor: Extractor,
        action: Action,
        name: Name,
        builder: Builder<S, O, C>,
    ) -> Builder<S, O, C>
    where
        Extractor: FnMut(&C) -> Self + Send + 'static,
        Name: Clone + Display + Send + Sync + 'static,
    {
        let extractor = move |cfg: &_| iter::once(extractor(cfg));
        <Self as IteratedCfgHelper<S, O, C, Action>>::apply(extractor, action, name, builder)
    }
}
