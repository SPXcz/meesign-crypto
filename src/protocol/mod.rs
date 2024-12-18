#[cfg(feature = "elgamal")]
pub mod elgamal;
#[cfg(feature = "frost")]
pub mod frost;
#[cfg(feature = "gg18")]
pub mod gg18;
#[cfg(feature = "musig2meesign")]
pub mod musig2meesign;

mod signer;
mod apdu;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

use crate::proto::{ClientMessage, ProtocolType};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub enum Recipient {
    Card,
    Server,
}

#[typetag::serde]
pub trait Protocol {
    fn advance(&mut self, data: &[u8]) -> Result<(Vec<u8>, Recipient)>;
    fn finish(self: Box<Self>) -> Result<Vec<u8>>;
}

pub trait KeygenProtocol: Protocol {
    fn new() -> Self
    where
        Self: Sized;
}

pub trait ThresholdProtocol: Protocol {
    fn new(group: &[u8]) -> Self
    where
        Self: Sized;
}

/// Deserializes values in a `HashMap`
fn deserialize_map<'de, T: Deserialize<'de>>(
    map: &'de HashMap<u32, Vec<u8>>,
) -> serde_json::Result<HashMap<u32, T>> {
    map.iter()
        .map(|(k, v)| Ok((*k, serde_json::from_slice::<T>(v.as_slice())?)))
        .collect()
}

/// Encode a broadcast message to protobuf format
fn encode_raw_bcast(message: Vec<u8>, protocol_type: ProtocolType) -> Vec<u8> {
    ClientMessage {
        protocol_type: protocol_type.into(),
        unicasts: HashMap::new(),
        broadcast: Some(message),
    }
    .encode_to_vec()
}

/// Serialize and encode a broadcast message to protobuf format
fn serialize_bcast<T: Serialize>(
    value: &T,
    protocol_type: ProtocolType,
) -> serde_json::Result<Vec<u8>> {
    let message = serde_json::to_vec(value)?;
    Ok(encode_raw_bcast(message, protocol_type))
}

/// Encode unicast messages to protobuf format
///
/// Each message is associated with an index as used by a respective protocol
fn encode_raw_uni(messages: HashMap<u32, Vec<u8>>, protocol_type: ProtocolType) -> Vec<u8> {
    ClientMessage {
        protocol_type: protocol_type.into(),
        unicasts: messages,
        broadcast: None,
    }
    .encode_to_vec()
}

/// Serialize and encode unicast messages to protobuf format
///
/// Each message is associated with an index as used by a respective protocol
fn serialize_uni<T, I>(kvs: I, protocol_type: ProtocolType) -> serde_json::Result<Vec<u8>>
where
    I: IntoIterator<Item = (u32, T)>,
    T: Serialize,
{
    let messages = kvs
        .into_iter()
        .map(|(k, v)| Ok((k, serde_json::to_vec(&v)?)))
        .collect::<serde_json::Result<_>>()?;
    Ok(encode_raw_uni(messages, protocol_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    use prost::bytes::Bytes;

    use crate::{
        proto::{ProtocolGroupInit, ProtocolInit, ServerMessage},
        protocol::{KeygenProtocol, ThresholdProtocol},
    };

    pub(super) trait KeygenProtocolTest: KeygenProtocol + Sized {
        // Cannot be added in Protocol (yet) due to typetag Trait limitations
        const PROTOCOL_TYPE: ProtocolType;
        const ROUNDS: usize;
        const INDEX_OFFSET: u32 = 0;

        fn run(threshold: u32, parties: u32) -> (HashMap<u32, Vec<u8>>, HashMap<u32, Vec<u8>>) {
            assert!(threshold <= parties);

            // initialize
            let mut ctxs: HashMap<u32, Self> = (0..parties)
                .map(|i| (i as u32 + Self::INDEX_OFFSET, Self::new()))
                .collect();

            let mut messages: HashMap<u32, _> = ctxs
                .iter_mut()
                .map(|(&index, ctx)| {
                    let msg = ClientMessage::decode::<Bytes>(
                        ctx.advance(
                            &(ProtocolGroupInit {
                                protocol_type: Self::PROTOCOL_TYPE as i32,
                                index,
                                parties,
                                threshold,
                            })
                            .encode_to_vec(),
                        )
                        .unwrap()
                        .0
                        .into(),
                    )
                    .unwrap();
                    (index, msg)
                })
                .collect();

            // protocol rounds
            for _ in 0..(Self::ROUNDS - 1) {
                messages = ctxs
                    .iter_mut()
                    .map(|(&idx, ctx)| {
                        let mut unicasts = HashMap::new();
                        let mut broadcasts = HashMap::new();

                        for (&sender, msg) in &messages {
                            if sender == idx {
                                continue;
                            }
                            if let Some(broadcast) = &msg.broadcast {
                                broadcasts.insert(sender, broadcast.clone());
                            }
                            if let Some(unicast) = msg.unicasts.get(&idx) {
                                unicasts.insert(sender, unicast.clone());
                            }
                        }

                        let msg = ClientMessage::decode::<Bytes>(
                            ctx.advance(
                                &(ServerMessage {
                                    protocol_type: Self::PROTOCOL_TYPE as i32,
                                    unicasts,
                                    broadcasts,
                                })
                                .encode_to_vec(),
                            )
                            .unwrap()
                            .0
                            .into(),
                        )
                        .unwrap();
                        (idx, msg)
                    })
                    .collect();
            }

            let pks = messages
                .into_iter()
                .map(|(i, msgs)| (i, msgs.broadcast.unwrap()))
                .collect();

            let results = ctxs
                .into_iter()
                .map(|(i, ctx)| (i, Box::new(ctx).finish().unwrap()))
                .collect();

            (pks, results)
        }
    }

    pub(super) trait ThresholdProtocolTest: ThresholdProtocol + Sized {
        // Cannot be added in Protocol (yet) due to typetag Trait limitations
        const PROTOCOL_TYPE: ProtocolType;
        const ROUNDS: usize;
        const INDEX_OFFSET: u32 = 0;

        fn run(ctxs: HashMap<u32, Vec<u8>>, data: Vec<u8>) -> Vec<Vec<u8>> {
            // initialize
            let mut ctxs: HashMap<u32, _> = ctxs
                .into_iter()
                .map(|(i, ctx)| (i, Self::new(&ctx)))
                .collect();

            let mut indices: Vec<_> = ctxs.keys().cloned().collect();
            indices.sort();

            let mut messages: HashMap<u32, _> = ctxs
                .iter_mut()
                .map(|(&index, ctx)| {
                    let msg = ClientMessage::decode::<Bytes>(
                        ctx.advance(
                            &(ProtocolInit {
                                protocol_type: Self::PROTOCOL_TYPE as i32,
                                indices: indices.clone(),
                                index,
                                data: data.clone(),
                            })
                            .encode_to_vec(),
                        )
                        .unwrap()
                        .0
                        .into(),
                    )
                    .unwrap();
                    (index, msg)
                })
                .collect();

            // protocol rounds
            for _ in 0..(Self::ROUNDS - 1) {
                messages = ctxs
                    .iter_mut()
                    .map(|(&idx, ctx)| {
                        let mut unicasts = HashMap::new();
                        let mut broadcasts = HashMap::new();

                        for (&sender, msg) in &messages {
                            if sender == idx {
                                continue;
                            }
                            if let Some(broadcast) = &msg.broadcast {
                                broadcasts.insert(sender, broadcast.clone());
                            }
                            if let Some(unicast) = msg.unicasts.get(&idx) {
                                unicasts.insert(sender, unicast.clone());
                            }
                        }

                        let msg = ClientMessage::decode::<Bytes>(
                            ctx.advance(
                                &(ServerMessage {
                                    protocol_type: Self::PROTOCOL_TYPE as i32,
                                    unicasts,
                                    broadcasts,
                                })
                                .encode_to_vec(),
                            )
                            .unwrap()
                            .0
                            .into(),
                        )
                        .unwrap();
                        (idx, msg)
                    })
                    .collect();
            }

            ctxs.into_iter()
                .map(|(_, ctx)| Box::new(ctx).finish().unwrap())
                .collect()
        }
    }
}
