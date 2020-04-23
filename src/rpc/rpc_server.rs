use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use bytes::IntoBuf;
use futures::future::Future;
use futures_util::future::*;
use futures_util::stream::*;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

use crate::config::TlsConfig;
use crate::data::*;
use crate::error::Error;

use crate::rpc::tls_util;

pub trait RpcMessage: Serialize + for<'de> Deserialize<'de> + Send + Sync {}

type ResponseFuture = Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send>>;
type Handler = Box<dyn Fn(Request<Body>, SocketAddr) -> ResponseFuture + Send + Sync>;

pub struct RpcServer {
	pub bind_addr: SocketAddr,
	pub tls_config: Option<TlsConfig>,

	handlers: HashMap<String, Handler>,
}

async fn handle_func<M, F, Fut>(
	handler: Arc<F>,
	req: Request<Body>,
	sockaddr: SocketAddr,
	name: Arc<String>,
) -> Result<Response<Body>, Error>
where
	M: RpcMessage + 'static,
	F: Fn(M, SocketAddr) -> Fut + Send + Sync + 'static,
	Fut: Future<Output = Result<M, Error>> + Send + 'static,
{
	let begin_time = Instant::now();
	let whole_body = hyper::body::to_bytes(req.into_body()).await?;
	let msg = rmp_serde::decode::from_read::<_, M>(whole_body.into_buf())?;
	match handler(msg, sockaddr).await {
		Ok(resp) => {
			let resp_bytes = rmp_to_vec_all_named::<Result<M, String>>(&Ok(resp))?;
			let rpc_duration = (Instant::now() - begin_time).as_millis();
			if rpc_duration > 100 {
				debug!("RPC {} ok, took long: {} ms", name, rpc_duration,);
			}
			Ok(Response::new(Body::from(resp_bytes)))
		}
		Err(e) => {
			let err_str = format!("{}", e);
			let rep_bytes = rmp_to_vec_all_named::<Result<M, String>>(&Err(err_str))?;
			let mut err_response = Response::new(Body::from(rep_bytes));
			*err_response.status_mut() = e.http_status_code();
			warn!(
				"RPC error ({}): {} ({} ms)",
				name,
				e,
				(Instant::now() - begin_time).as_millis(),
			);
			Ok(err_response)
		}
	}
}

impl RpcServer {
	pub fn new(bind_addr: SocketAddr, tls_config: Option<TlsConfig>) -> Self {
		Self {
			bind_addr,
			tls_config,
			handlers: HashMap::new(),
		}
	}

	pub fn add_handler<M, F, Fut>(&mut self, name: String, handler: F)
	where
		M: RpcMessage + 'static,
		F: Fn(M, SocketAddr) -> Fut + Send + Sync + 'static,
		Fut: Future<Output = Result<M, Error>> + Send + 'static,
	{
		let name2 = Arc::new(name.clone());
		let handler_arc = Arc::new(handler);
		let handler = Box::new(move |req: Request<Body>, sockaddr: SocketAddr| {
			let handler2 = handler_arc.clone();
			let b: ResponseFuture = Box::pin(handle_func(handler2, req, sockaddr, name2.clone()));
			b
		});
		self.handlers.insert(name, handler);
	}

	async fn handler(
		self: Arc<Self>,
		req: Request<Body>,
		addr: SocketAddr,
	) -> Result<Response<Body>, Error> {
		if req.method() != &Method::POST {
			let mut bad_request = Response::default();
			*bad_request.status_mut() = StatusCode::BAD_REQUEST;
			return Ok(bad_request);
		}

		let path = &req.uri().path()[1..];
		let handler = match self.handlers.get(path) {
			Some(h) => h,
			None => {
				let mut not_found = Response::default();
				*not_found.status_mut() = StatusCode::NOT_FOUND;
				return Ok(not_found);
			}
		};

		let resp_waiter = tokio::spawn(handler(req, addr));
		match resp_waiter.await {
			Err(err) => {
				warn!("Handler await error: {}", err);
				let mut ise = Response::default();
				*ise.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
				Ok(ise)
			}
			Ok(Err(err)) => {
				let mut bad_request = Response::new(Body::from(format!("{}", err)));
				*bad_request.status_mut() = StatusCode::BAD_REQUEST;
				Ok(bad_request)
			}
			Ok(Ok(resp)) => Ok(resp),
		}
	}

	pub async fn run(
		self: Arc<Self>,
		shutdown_signal: impl Future<Output = ()>,
	) -> Result<(), Error> {
		if let Some(tls_config) = self.tls_config.as_ref() {
			let ca_certs = tls_util::load_certs(&tls_config.ca_cert)?;
			let node_certs = tls_util::load_certs(&tls_config.node_cert)?;
			let node_key = tls_util::load_private_key(&tls_config.node_key)?;

			let mut ca_store = rustls::RootCertStore::empty();
			for crt in ca_certs.iter() {
				ca_store.add(crt)?;
			}

			let mut config =
				rustls::ServerConfig::new(rustls::AllowAnyAuthenticatedClient::new(ca_store));
			config.set_single_cert([&node_certs[..], &ca_certs[..]].concat(), node_key)?;
			let tls_acceptor = Arc::new(TlsAcceptor::from(Arc::new(config)));

			let mut listener = TcpListener::bind(&self.bind_addr).await?;
			let incoming = listener.incoming().filter_map(|socket| async {
				match socket {
					Ok(stream) => match tls_acceptor.clone().accept(stream).await {
						Ok(x) => Some(Ok::<_, hyper::Error>(x)),
						Err(_e) => None,
					},
					Err(_) => None,
				}
			});
			let incoming = hyper::server::accept::from_stream(incoming);

			let self_arc = self.clone();
			let service = make_service_fn(|conn: &TlsStream<TcpStream>| {
				let client_addr = conn
					.get_ref()
					.0
					.peer_addr()
					.unwrap_or(([0, 0, 0, 0], 0).into());
				let self_arc = self_arc.clone();
				async move {
					Ok::<_, Error>(service_fn(move |req: Request<Body>| {
						self_arc.clone().handler(req, client_addr).map_err(|e| {
							warn!("RPC handler error: {}", e);
							e
						})
					}))
				}
			});

			let server = Server::builder(incoming).serve(service);

			let graceful = server.with_graceful_shutdown(shutdown_signal);
			info!("RPC server listening on http://{}", self.bind_addr);

			graceful.await?;
		} else {
			let self_arc = self.clone();
			let service = make_service_fn(move |conn: &AddrStream| {
				let client_addr = conn.remote_addr();
				let self_arc = self_arc.clone();
				async move {
					Ok::<_, Error>(service_fn(move |req: Request<Body>| {
						self_arc.clone().handler(req, client_addr).map_err(|e| {
							warn!("RPC handler error: {}", e);
							e
						})
					}))
				}
			});

			let server = Server::bind(&self.bind_addr).serve(service);

			let graceful = server.with_graceful_shutdown(shutdown_signal);
			info!("RPC server listening on http://{}", self.bind_addr);

			graceful.await?;
		}

		Ok(())
	}
}