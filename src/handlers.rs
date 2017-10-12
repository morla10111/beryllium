use {serde_json, utils};
use client::{BotClient, BotData};
use errors::{BerylliumError, BerylliumResult};
use futures::{Future, Stream};
use futures::future;
use futures_cpupool::{Builder, CpuPool};
use hyper::{Body, Error as HyperError, Method, StatusCode};
use hyper::header::{Authorization, Bearer, ContentLength};
use hyper::server::{Service, Request, Response};
use parking_lot::Mutex;
use storage::StorageManager;
use std::collections::HashMap;
use std::sync::Arc;
use types::{BotCreationData, BotCreationResponse, Event, EventData};
use types::{ConversationData, ConversationEventType, MessageData, MessageRequest};
use types::{Devices, Member};

pub trait Handler: Send + Sync + 'static {
    type Item: Send + 'static;
    type Error: Send + 'static;
    type Future: Future<Item=Self::Item, Error=Self::Error> + Send + 'static;

    fn handle(&self, data: EventData, client: BotClient) -> Self::Future;
}

// FIXME: I *know* that Arc has an overhead, but I'm not entirely
// sure about the performance impact of this in our case (i.e., HTTP requests)
pub struct BotHandler<H> {
    handler: Arc<H>,
    pool: Arc<CpuPool>,
    bot_data: Arc<Mutex<HashMap<String, BotData>>>,
}

impl<H: Handler> BotHandler<H> {
    pub fn new(handler: Arc<H>) -> BotHandler<H> {
        BotHandler {
            handler: handler,
            pool: Arc::new(Builder::new().create()),
            bot_data: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<H: Handler> Service for BotHandler<H> {
    type Request = Request;
    type Response = Response;
    type Error = HyperError;
    type Future = Box<Future<Item=Self::Response, Error=Self::Error>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let mut resp = Response::new();
        let (method, uri, _version, headers, body) = req.deconstruct();

        if method != Method::Post {     // only allow POST
            resp.set_status(StatusCode::MethodNotAllowed);
            return Box::new(future::ok(resp))
        } else {        // all requests should have Bearer token auth
            match headers.get::<Authorization<Bearer>>() {
                Some(header) if utils::check_auth_token(&header.to_string()[7..]) => (),
                _ => {
                    resp.set_status(StatusCode::Unauthorized);
                    return Box::new(future::ok(resp))
                }
            }
        }

        macro_rules! parse_json_and {
            ($call:expr $( , $arg:expr )*) => {{
                let mut bytes = vec![];
                if let Some(len) = headers.get::<ContentLength>() {
                    bytes = Vec::with_capacity(**len as usize);
                }

                // FIXME: Prone to DDoS
                let f = body.fold(bytes, |mut acc, ref chunk| {
                    acc.extend_from_slice(chunk);
                    future::ok::<_, Self::Error>(acc)
                }).map(|vec| {
                    if let Ok(value) = serde_json::from_slice(&vec) {
                        if let Err(e) = $call($( $arg, )* value, &mut resp) {
                            error!("{}", e);
                            resp.set_status(StatusCode::InternalServerError);
                        }
                    } else {
                        resp.set_status(StatusCode::BadRequest);
                    }

                    resp
                });

                Box::new(f)
            }};
        }

        let rel_url = uri.path();
        let mut split = rel_url.split('/');

        if rel_url == "/bots" {
            parse_json_and!(create_bot)
        } else {
            // FIXME: Better way to detect relative URL paths?
            match (split.next(), split.next(), split.next(), split.next(), split.next()) {
                (Some(""), Some("bots"), Some(id), Some("messages"), None) => {
                    let pool = self.pool.clone();
                    let handler = self.handler.clone();
                    let bot_id = String::from(id);
                    let bot_data = self.bot_data.clone();
                    parse_json_and!(handle_events, pool, bot_data, bot_id, handler)
                },
                _ => Box::new(future::ok(resp.with_status(StatusCode::NotFound))),
            }
        }
    }
}

fn create_bot(data: BotCreationData, resp: &mut Response) -> BerylliumResult<()> {
    info!("Creating new bot...");
    let storage = StorageManager::new(&data.id)?;
    let mut prekeys = storage.initialize_prekeys(data.conversation.members.len())?;
    // There will always be a final prekey corresponding to u16::MAX
    let final_key = prekeys.pop().unwrap();
    storage.save_state(&data)?;

    let data = BotCreationResponse {
        prekeys: prekeys,
        last_prekey: final_key,
    };

    let bytes = serde_json::to_vec(&data)?;
    resp.set_body(Body::from(bytes));
    resp.set_status(StatusCode::Created);
    Ok(())
}

fn handle_events<H>(pool: Arc<CpuPool>,
                    bot_data: Arc<Mutex<HashMap<String, BotData>>>,
                    bot_id: String, handler: Arc<H>,
                    data: MessageData, resp: &mut Response)
                   -> BerylliumResult<()>
    where H: Handler
{
    // NOTE: parking_lot's Mutex is suitable for fine-grained locks, so we
    // acquire the lock, check (and possibly refresh) the data and release it
    // immediately. We do this a lot below.

    if bot_data.lock().get(&bot_id).is_none() {
        let mut this_bot_data = BotData::from_storage(&bot_id)?;
        let (devices, status): (Devices, _) = this_bot_data.client.send_message(MessageRequest {
            sender: this_bot_data.data.client.clone(),
            recipients: HashMap::new(),
        }, false)?;

        // This happens only when we haven't sent the encrypted message
        // for all the devices in the conversation (i.e., we don't have all the devices).
        if status == StatusCode::PreconditionFailed {
            this_bot_data.devices = Some(devices);
        }

        bot_data.lock().insert(bot_id.clone(), this_bot_data);
    }

    // TODO:
    // - Isolate events into their own functions.
    // - Revisit `clone` usage on various types.
    let event = match (data.type_, &data.data) {
        (ConversationEventType::MemberJoin,
         &ConversationData::LeavingOrJoiningMembers { ref user_ids }) => {
            // FIXME: What if we don't have the devices of these members?
            let (conversation, client) = {
                let mut lock = bot_data.lock();
                let old_data = lock.get_mut(&bot_id).unwrap();
                // Add users to existing data
                for id in user_ids {
                    old_data.data.conversation.members.insert(Member {
                        id: id.clone(),
                        status: 0,
                    });
                }

                (old_data.data.conversation.clone(), old_data.client.clone())
            };

            let members_joined = user_ids.clone();
            Some((EventData {
                bot_id: bot_id,
                event: Event::ConversationMemberJoin { members_joined, conversation },
            }, client))
        },

        (ConversationEventType::MemberLeave,
         &ConversationData::LeavingOrJoiningMembers { ref user_ids }) => {
            let (conversation, client) = {
                let mut lock = bot_data.lock();
                let old_data = lock.get_mut(&bot_id).unwrap();
                // Remove users from existing data
                for id in user_ids {
                    old_data.data.conversation.members.remove(id.as_str());
                }

                (old_data.data.conversation.clone(), old_data.client.clone())
            };

            // If our bot has left, then remove the entire data.
            if user_ids.iter().find(|&id| id == &bot_id).is_some() {
                bot_data.lock().remove(&bot_id).unwrap();
            }

            let members_left = user_ids.clone();
            Some((EventData {
                bot_id: bot_id,
                event: Event::ConversationMemberLeave { members_left, conversation },
            }, client))
        },

        (ConversationEventType::Rename,
         &ConversationData::Rename { ref name }) => {
            let (conversation, client) = {
                let mut lock = bot_data.lock();
                let old_data = lock.get_mut(&bot_id).unwrap();
                old_data.data.conversation.name = name.clone();
                (old_data.data.conversation.clone(), old_data.client.clone())
            };

            Some((EventData {
                bot_id: bot_id,
                event: Event::ConversationRename { conversation },
            }, client))
        },

        _ => {
            debug!("Unknown type {:?} and data {:?}", data.type_, data.data);
            return Err(BerylliumError::Unreachable)
        },
    };

    if let Some((event_data, client)) = event {
        let _ = pool.spawn_fn(move || {
            handler.handle(event_data, client.into())
        });
    }

    resp.set_status(StatusCode::Ok);
    Ok(())
}
