pub mod signer;

use crate::proto::{ProtocolGroupInit, ProtocolInit, ProtocolType, ServerMessage};
use crate::protocol::*;

use signer::Signer;
use ::musig2::CompactSignature;
use prost::Message;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy)]
struct Setup {
    threshold: u16,
    parties: u16,
    index: u16,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct KeygenContext {
    round: KeygenRound,
    with_card: bool,
}

#[derive(Serialize, Deserialize)]
enum KeygenRound {
    R0,
    R0AwaitKeyFromCard(Setup, Signer), // Get public key from card
    R1(Setup, Signer), // Setup and skey share of the user
    Done(Setup, Signer),
}

impl KeygenContext {
    pub fn with_card() -> Self {
        Self {
            round: KeygenRound::R0,
            with_card: true, 
        }
    }

    fn init(&mut self, data: &[u8]) -> Result<(Vec<u8>, Recipient)> {
        let msg = ProtocolGroupInit::decode(data)?;
        if msg.protocol_type != ProtocolType::Musig2 as i32 {
            return Err("wrong protocol type".into());
        }

        // Musig2 is n-out-of-n scheme. Therefore n = k.
        if msg.parties as u16 != msg.threshold as u16 {
            return Err("number of parties must be equal to the treshold".into());
        }

        let setup = Setup {
            threshold: msg.threshold as u16,
            parties: msg.parties as u16,
            index: msg.index as u16,
        };

        // Key share pair generated
        let signer: Signer = Signer::new();

        let public_key_share = signer.pubkey_serialized();

        let msg = serialize_bcast(&public_key_share, ProtocolType::Musig2)?;
        self.round = KeygenRound::R1(setup, signer);
        Ok((msg, Recipient::Server))
    }

    fn update(&mut self, data: &[u8]) -> Result<(Vec<u8>, Recipient)> {
        let (c, data, rec) = match &mut self.round {
            KeygenRound::R0 => return Err("protocol not initialized".into()),
            KeygenRound::R0AwaitKeyFromCard(setup, signer) => {
                jc::response::setup(data)?;
            }
            KeygenRound::R1(setup, signer) => {
                let data = ServerMessage::decode(data)?.broadcasts;
                let pub_keys_hashmap = deserialize_map(&data)?;

                // Get public key shares of all signers
                let pub_key_shares: Vec<Vec<u8>> = pub_keys_hashmap.values().cloned().collect();

                // Generate key_agg_ctx (together with agg_pubkey). Currently there's no support for tweaks.
                signer.generate_key_agg_ctx_serialized(pub_key_shares, None, false);

                let agg_pubkey = signer.get_agg_pubkey().clone();
                let msg = serialize_bcast(&agg_pubkey, ProtocolType::Musig2)?;

                (
                    KeygenRound::Done(*setup, signer.clone()),
                    msg,
                    Recipient::Server,
                )
            }
            KeygenRound::Done(_, _) => return Err("protocol already finished".into()),
        };
        self.round = c;

        Ok((data, rec))
    }
}

#[typetag::serde(name = "musig2_keygen")]
impl Protocol for KeygenContext {
    fn advance(&mut self, data: &[u8]) -> Result<(Vec<u8>, Recipient)> {
        match self.round {
            KeygenRound::R0 => self.init(data),
            _ => self.update(data),
        }
    }

    fn finish(self: Box<Self>) -> Result<Vec<u8>> {
        match self.round {
            KeygenRound::Done(setup, signer) => {
                Ok(serde_json::to_vec(&(setup, signer))?)
            }
            _ => Err("protocol not finished".into()),
        }
    }
}

impl KeygenProtocol for KeygenContext {
    fn new() -> Self {
        Self {
            round: KeygenRound::R0,
            with_card: false,
        }
    }
}

// By GitHub Copilot
// Deserialize a vector of bytes with pubnonces/partial signatures and internal identifiers of the signers to a hashmap
fn deserialize_musig(data: Vec<Vec<u8>>, chunk_len: usize) -> Result<HashMap<u8, Vec<u8>>> {
    let data = data.concat();
    if data.len() % chunk_len != 0 {
        return Err("Input data length must be a multiple of the input size".into());
    }

    let mut hashmap = HashMap::new();
    for chunk in data.chunks_exact(chunk_len) {
        let key = chunk[chunk_len - 1];
        let mut value = Vec::new();
        value.extend_from_slice(&chunk[..chunk_len - 1]);

        if hashmap.contains_key(&key) {
            return Err("Duplicate key found".into());
        }

        hashmap.insert(key, value);
    }

    Ok(hashmap)
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SignContext {
    setup: Setup,
    initial_signer: Signer,
    message: Option<Vec<u8>>,
    indices: Option<Vec<u16>>,
    round: SignRound,
}

const PUBNONCE_CHUNK_LEN: usize = 67; //(33*2+1)
const SCALAR_CHUNK_LEN: usize = 33; //(32+1)

#[derive(Serialize, Deserialize)]
enum SignRound {
    R0,
    R1(Signer),
    R2(Signer),
    Done(CompactSignature),
}

impl SignContext {
    //fn participants(&self) -> usize {
    //    self.indices.as_ref().unwrap().len()
    //}

    // Format sent is &[u8] + u8 (pubnonce and index of the signer)
    fn init(&mut self, data: &[u8]) -> Result<(Vec<u8>, Recipient)> {
        let msg = ProtocolInit::decode(data)?;
        if msg.protocol_type != ProtocolType::Musig2 as i32 {
            return Err("wrong protocol type".into());
        }

        self.indices = Some(msg.indices.iter().map(|i| *i as u16).collect());
        self.message = Some(msg.data);

        self.initial_signer.first_round();

        let mut out_buffer = self.initial_signer.get_pubnonce_serialized().clone();
        let internal_index: u8 = self.initial_signer.get_index() as u8;

        out_buffer.push(internal_index);

        // Serialize the public nonce and the internal index of the signer. Format: &[u8] + u8
        let msg = serialize_bcast(&out_buffer, ProtocolType::Musig2)?; // TODO: Check if this is correct usage of serialize_bcast

        // TODO: Can I somehow just pass a reference instead of cloning? (Due to Signer having a secret share inside)
        self.round = SignRound::R1(self.initial_signer.clone());

        Ok((msg, Recipient::Server))
    }

    fn update(&mut self, data: &[u8]) -> Result<(Vec<u8>, Recipient)> {
        match &self.round {
            SignRound::R0 => Err("protocol not initialized".into()),
            SignRound::R1(signer) => {
                let data = ServerMessage::decode(data)?.broadcasts;
                let pubnonce_hashmap: HashMap<u32, Vec<u8>> = deserialize_map(&data)?;

                // TODO: Is the pubnonce instance of this Signer object returned by the server?
                // Generate hashmap <internal index of Signer, pubnonce>
                let pubnonces = deserialize_musig(pubnonce_hashmap.values().cloned().collect(), PUBNONCE_CHUNK_LEN)?;

                // Create a vector of tuples (internal index of Signer, pubnonce)
                let pubnonces: Vec<(usize, Vec<u8>)> = pubnonces
                    .into_iter()
                    .map(|(index, pubnonce)| (index as usize, pubnonce.to_vec()))
                    .collect();

                // Get copy of Signer object
                // TODO: Another example of the unnecessary cloning?
                let mut signer: Signer = signer.clone();

                // Establish second round
                if let Some(message) = &mut self.message {
                    signer.second_round(&message, pubnonces);

                    let mut out_buffer = signer.get_partial_signature_serialized().clone();
                    let internal_index = signer.get_index() as u8;

                    out_buffer.push(internal_index);

                    // Serialize the partial signature and the internal index of the signer. Format: &[u8] + u8
                    let msg = serialize_bcast(&out_buffer, ProtocolType::Musig2)?;

                    self.round = SignRound::R2(signer);
                    
                    Ok((msg, Recipient::Server))
                } else {
                    Err("message to sign not initialized".into())
                }
            }
            SignRound::R2(signer) => {
                let data = ServerMessage::decode(data)?.broadcasts;
                let shares_hashmap: HashMap<u32, Vec<u8>> = deserialize_map(&data)?;
                let shares = deserialize_musig(shares_hashmap.values().cloned().collect(), SCALAR_CHUNK_LEN)?;

                // Create a vector of tuples (internal index of Signer, partial signature)
                let partial_signatures: Vec<(usize, Vec<u8>)> = shares
                    .into_iter()
                    .map(|(index, partial_signature)| (index as usize, partial_signature.to_vec()))
                    .collect();

                // Get copy of Signer object
                let mut signer: Signer = signer.clone();

                // Set partial signatures of other signers
                signer.receive_partial_signatures(partial_signatures);
                
                // Get aggregated signature and if successful, return it to the server
                match signer.get_agg_signature() {
                    Ok(signature) => {
                        let msg = serialize_bcast(&signature.serialize().to_vec(), ProtocolType::Musig2)?;
                        self.round = SignRound::Done(signature);
                        Ok((msg, Recipient::Server))
                    }
                    Err(e) => Err(e.into()),
                }
            }
            SignRound::Done(_) => Err("protocol already finished".into()),
        }
    }
}

#[typetag::serde(name = "musig2_sign")]
impl Protocol for SignContext {
    fn advance(&mut self, data: &[u8]) -> Result<(Vec<u8>, Recipient)> {
        match self.round {
            SignRound::R0 => self.init(data),
            _ => self.update(data),
        }
    }

    fn finish(self: Box<Self>) -> Result<Vec<u8>> {
        match self.round {
            SignRound::Done(sig) => Ok(serde_json::to_vec(&sig)?),
            _ => Err("protocol not finished".into()),
        }
    }
}

impl ThresholdProtocol for SignContext {
    fn new(group: &[u8]) -> Self {
        let (setup, initial_signer): (Setup, Signer) =
            serde_json::from_slice(group).expect("could not deserialize group context");
        Self {
            setup,
            initial_signer,
            message: None,
            indices: None,
            round: SignRound::R0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tests::{KeygenProtocolTest, ThresholdProtocolTest};
    use ::musig2::CompactSignature;
    use secp256k1::PublicKey;

    impl KeygenProtocolTest for KeygenContext {
        const PROTOCOL_TYPE: ProtocolType = ProtocolType::Musig2;
        const ROUNDS: usize = 2;
        const INDEX_OFFSET: u32 = 1;
    }

    impl ThresholdProtocolTest for SignContext {
        const PROTOCOL_TYPE: ProtocolType = ProtocolType::Musig2;
        const ROUNDS: usize = 3;
        const INDEX_OFFSET: u32 = 1;
    }

    #[test]
    fn keygen() {
        for parties in 2..6 {
            let (pks, _) =
                <KeygenContext as KeygenProtocolTest>::run(parties as u32, parties as u32);

            let pks: Vec<PublicKey> = pks
                .iter()
                .map(|(_, x)| serde_json::from_slice(&x).unwrap())
                .collect();

            for i in 1..parties {
                assert_eq!(pks[0], pks[i])
            }
        }
    }

    #[test]
    fn sign() {
        for parties in 2..6 {
            let (pks, ctxs) =
                <KeygenContext as KeygenProtocolTest>::run(parties as u32, parties as u32);
            let msg = b"hello";
            let (_, pk) = pks.iter().take(1).collect::<Vec<_>>()[0];
            let pk: PublicKey = serde_json::from_slice(&pk).unwrap();

            let ctxs = ctxs
                .into_iter()
                .collect();
            let results =
                <SignContext as ThresholdProtocolTest>::run(ctxs, msg.to_vec());

            let signature: CompactSignature = serde_json::from_slice(&results[0]).unwrap();

            for result in results {
                assert_eq!(signature, serde_json::from_slice(&result).unwrap());
            }

            assert!(::musig2::verify_single(pk, signature, msg).is_ok());
        }
    }
}

mod jc {
    mod util {
        use crate::protocol::Result;
        use k256::{
            elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint},
            AffinePoint, EncodedPoint,
        };

        pub fn reencode_point(ser: &[u8], compress: bool) -> Result<Box<[u8]>> {
            let encoded = EncodedPoint::from_bytes(ser)?;
            match Option::<AffinePoint>::from(AffinePoint::from_encoded_point(&encoded)) {
                Some(affine) => Ok(affine.to_encoded_point(compress).to_bytes()),
                None => Err("Invalid point".into()),
            }
        }
    }

    pub mod command {
        use secp256k1::PublicKey;

        use super::util::reencode_point;
        use crate::protocol::apdu::CommandBuilder;

        const SCALAR_LEN: usize = 32;
        const POINT_LEN : usize = 33;

        const CLA: u8 = 0xA6;

        const INS_GENERATE_KEYS: u8 = 0xBB;
        const INS_GENERATE_NONCES: u8 = 0x5E;
        const INS_SIGN: u8 = 0x49;

        const INS_GET_XONLY_PUBKEY: u8 = 0x8B;
        const INS_GET_PLAIN_PUBKEY: u8 = 0x5A;
        const INS_GET_PNONCE_SHARE: u8 = 0x35;

        const INS_SET_AGG_PUBKEY: u8 = 0x76;
        const INS_SET_AGG_NONCES: u8 = 0x9A;

        pub fn keygen() -> Vec<u8> {
            CommandBuilder::new(CLA, INS_GENERATE_KEYS)
                .build()
        }

        pub fn get_plain_pubkey() -> Vec<u8> {
            CommandBuilder::new(CLA, INS_GET_PLAIN_PUBKEY)
                .build()
        }

        pub fn set_aggpubkey(aggkey_q: PublicKey, gacc: u8, tacc: u8, coef_a: [u8; SCALAR_LEN]) -> Vec<u8> {

            let mut gacc_array = [0; SCALAR_LEN];
            gacc_array[SCALAR_LEN - 1] = gacc;

            let mut tacc_array = [0; SCALAR_LEN];
            tacc_array[SCALAR_LEN - 1] = tacc;

            CommandBuilder::new(CLA, INS_SET_AGG_PUBKEY)
                .extend(&reencode_point(&aggkey_q.serialize(), true).unwrap())
                .extend(&gacc_array)
                .extend(&tacc_array)
                .extend(&coef_a)
                .build()
        }

        pub fn noncegen() -> Vec<u8> {
            CommandBuilder::new(CLA, INS_GENERATE_NONCES)
                .build()
        }

        pub fn get_pubnonce() -> Vec<u8> {
            CommandBuilder::new(CLA, INS_GET_PNONCE_SHARE)
                .build()
        }

        pub fn set_agg_nonces(pubnonces: &[PublicKey; 2]) -> Vec<u8> {
            CommandBuilder::new(CLA, INS_SET_AGG_NONCES)
                .extend(&reencode_point(&pubnonces[0].serialize(), true).unwrap())
                .extend(&reencode_point(&pubnonces[1].serialize(), true).unwrap())
                .build()
        }

        pub fn sign(message: &[u8]) -> Vec<u8> {
            CommandBuilder::new(CLA, INS_SIGN)
                .extend(message)
                .build()
        }
    }

    pub mod response {
        use musig2::CompactSignature;
        use secp256k1::PublicKey;

        use super::util::reencode_point;
        use crate::protocol::apdu::parse_response;
        use crate::protocol::Result;
        use std::convert::TryInto;

        pub fn keygen(raw: &[u8]) -> Result<()> {
            parse_response(raw)?;
            Ok(())
        }

        pub fn get_plain_pubkey(raw: &[u8]) -> Result<PublicKey> {
            let data = parse_response(raw)?;
            let pubkey = PublicKey::from_slice(&reencode_point(data, false)?)?;
            Ok(pubkey)
        }

        pub fn set_aggpubkey(raw: &[u8]) -> Result<()> {
            parse_response(raw)?;
            Ok(())
        }

        pub fn noncegen(raw: &[u8]) -> Result<()> {
            parse_response(raw)?;
            Ok(())
        }

        pub fn get_pubnonce(raw: &[u8]) -> Result<[PublicKey; 2]> {
            let data = parse_response(raw)?;
            let pubkey = data.split_at(data.len() / 2);
            let pubkey = [
                PublicKey::from_slice(&reencode_point(pubkey.0, false)?)?,
                PublicKey::from_slice(&reencode_point(pubkey.1, false)?)?,
            ];

            Ok(pubkey)
        }

        pub fn set_agg_nonces(raw: &[u8]) -> Result<()> {
            parse_response(raw)?;
            Ok(())
        }

        pub fn sign(raw: &[u8]) -> Result<CompactSignature> {
            let data = parse_response(raw)?;
            let signature = CompactSignature::from_bytes(&data)?;
            Ok(signature)
        }
    }

    #[cfg(test)]
    mod tests {
        use std::{
            collections::{BTreeMap, HashMap},
            convert::TryFrom,
            error::Error,
        };

        use crate::protocol::apdu::{parse_response, CommandBuilder};

        use super::{super::musig2, command, response};
        use pcsc;
        use rand::rngs::OsRng;
        use secp256k1::PublicKey;
        use ::musig2::PartialSignature;

        #[test]
        fn sign_card() -> Result<(), Box<dyn Error>> {
            // connect to card
            let ctx = pcsc::Context::establish(pcsc::Scope::User)?;
            let mut readers_buf = [0; 2048];
            let reader = ctx
                .list_readers(&mut readers_buf)?
                .next()
                .ok_or("no reader")?;
            let card = ctx.connect(reader, pcsc::ShareMode::Shared, pcsc::Protocols::ANY)?;
            let mut resp_buf = [0; pcsc::MAX_BUFFER_SIZE];

            // select MUSIG2
            let aid = b"\x01\xff\xff\x04\x05\x06\x07\x08\x11\x01";
            let select = CommandBuilder::new(0x00, 0xa4).p1(0x04).extend(aid).build();
            let resp = card.transmit(&select, &mut resp_buf)?;
            parse_response(resp)?;

            // Must equal
            let t: u8 = 3;
            let n: u8 = 3;
            let tacc: u8 = 0;
            let gacc: u8 = 1;
            let mut pubkeys: [PublicKey; 3];
            let mut pupnonces: [[PublicKey; 2]; 3];
            let mut pubshares: [PartialSignature; 3];
            let mut signers: [musig2::Signer; 3];

            // keygen
            let cmd = command::keygen();
            let resp = card.transmit(&cmd, &mut resp_buf)?;
            response::keygen(resp)?;

            // get pubkey
            let cmd = command::get_plain_pubkey();
            let resp = card.transmit(&cmd, &mut resp_buf)?;
            let card_pubkey = response::get_plain_pubkey(resp)?;

            // Initialize signers
            signers[0] = musig2::Signer::new_from_card(card_pubkey);

            for i in 1..(n) {
                signers[i as usize] = musig2::Signer::new();
            }

            // Get pubkeys
            let mut pubkey_array: Vec<Vec<u8>> = Vec::new();
            
            for i in 0..n {
                pubkey_array.push(signers[i as usize].pubkey_serialized());
            }

            // sort and assign public keys
            for i in 0..n {
                let local_pubkeys = pubkey_array.clone().remove(i as usize);
                signers[i as usize].generate_key_agg_ctx_serialized(pubkey_array, None, false);
            }

            // set agg pubkey
            let agg_pubkey = signers[0].get_agg_pubkey();
            let cmd = command::set_aggpubkey(agg_pubkey, gacc, tacc, signers[0].get_coef_a().serialize());

            // Generate nonces
            let cmd = command::noncegen();
            let resp = card.transmit(&cmd, &mut resp_buf)?;
            response::noncegen(resp)?;

            for i in 1..n {
                signers[i as usize].first_round();
            }

            // Get pubnonces
            

            for (id, key_pkg) in &key_pkgs { 
                let (nonces, commitments) =
                    frost::round1::commit(key_pkg.signing_share(), &mut OsRng);
                nonces_map.insert(*id, nonces);
                commitments_map.insert(*id, commitments);
            }

            // commitments
            for (i, commitments) in commitments_map.values().enumerate() {
                let cmd = command::commitment((i + 1) as u8, commitments);
                let resp = card.transmit(&cmd, &mut resp_buf)?;
                response::commitment(resp)?;
            }

            // sign
            let message = vec![0xff; 16];
            let signing_package = frost::SigningPackage::new(commitments_map, &message);

            let cmd = command::sign(&message);
            let resp = card.transmit(&cmd, &mut resp_buf)?;
            let card_share = response::sign(resp)?;

            let mut share_map = BTreeMap::new();
            share_map.insert(card_id, card_share);
            for (id, key_pkg) in &key_pkgs {
                let share = frost::round2::sign(&signing_package, &nonces_map[id], key_pkg)?;
                share_map.insert(*id, share);
            }

            frost::aggregate(&signing_package, &share_map, &pubkey_pkg)?;

            Ok(())
        }
    }
}