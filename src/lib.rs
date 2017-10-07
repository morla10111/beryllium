extern crate base64;
extern crate cryptobox;
extern crate futures;
extern crate hyper;
#[macro_use] extern crate lazy_static;
#[macro_use] extern crate log;
extern crate parking_lot;
extern crate proteus;
extern crate protobuf;
extern crate reqwest;
extern crate rustls;
#[macro_use] extern crate serde_derive;
extern crate serde_json;
extern crate tokio_rustls;
extern crate tokio_proto;

mod client;
mod errors;
mod handlers;
mod otr_manager;
mod service;
mod types;
mod utils;

include!(concat!(env!("OUT_DIR"), "/messages.rs"));

pub use service::BotService;
