use errors::{BerylliumError, BerylliumResult};
use futures::{Async, Future, Poll};
use handlers::{BotHandler, Handler};
use hyper::server::Http;
use rustls::{Certificate, PrivateKey, ServerConfig};
use rustls::internal::pemfile;
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use tokio_core::reactor::Core;
use tokio_rustls::proto::Server;
use tokio_proto::TcpServer;
use utils;

pub struct BotService {
    config: ServerConfig,
}

impl BotService {
    fn load_certs<P>(path: P) -> BerylliumResult<Vec<Certificate>>
        where P: AsRef<Path>
    {
        info!("Loading certificate from {}", path.as_ref().display());
        let cert = File::open(path)?;
        let mut reader = BufReader::new(cert);
        pemfile::certs(&mut reader).map_err(|_| BerylliumError::PemFileError)
    }

    fn load_private_key<P>(path: P) -> BerylliumResult<PrivateKey>
        where P: AsRef<Path>
    {
        info!("Loading private key from {}", path.as_ref().display());
        let key = File::open(path)?;
        let mut reader = BufReader::new(key);
        let mut keys = pemfile::rsa_private_keys(&mut reader)
                               .map_err(|_| BerylliumError::PemFileError)?;
        keys.truncate(1);
        if keys.is_empty() {
            return Err(BerylliumError::PemFileError)
        }

        Ok(keys.pop().unwrap())
    }

    pub fn new<P>(auth: String, store_path: P, key_path: P, cert_path: P)
                  -> BerylliumResult<BotService>
        where P: AsRef<Path>
    {
        let certs = Self::load_certs(cert_path)?;
        let key = Self::load_private_key(key_path)?;
        let mut tls_config = ServerConfig::new();
        tls_config.set_single_cert(certs, key);
        utils::set_auth_token(auth);
        utils::set_store_path(store_path);

        Ok(BotService {
            config: tls_config,
        })
    }

    pub fn start_listening<H>(self, addr: &SocketAddr, handler: H)
        where H: Handler
    {
        let mut core = Core::new().expect("event loop creation");
        let https_server = Server::new(Http::new(), Arc::new(self.config));
        let tcp_server = TcpServer::new(https_server, addr.clone());
        let handler = Arc::new(handler);
        let remote = core.remote();

        // Separate thread has the HTTPS server itself. It has the
        // handle to spawn closures into the main event loop.
        let _ = thread::spawn(move || {
            tcp_server.serve(move || {
                Ok(BotHandler::new(handler.clone(), remote.clone()))
            });
        });

        // The event loop runs only for making hyper client requests.
        core.run(NeverEndingFuture).expect("running event loop");
    }
}

/// Zero-sized struct just to keep the event loop running.
struct NeverEndingFuture;

impl Future for NeverEndingFuture {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        Ok(Async::NotReady)
    }
}
