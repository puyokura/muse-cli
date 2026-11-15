//! Noise_XX_25519_AESGCM_SHA256 initiator.
//!
//! Muse's Hatch gateway performs a standard Noise XX handshake over the
//! WebSocket before any protobuf-framed RPC is allowed. The state machine here
//! mirrors the wire behaviour observed in the Android client
//! (`NoiseGatewayHatchWebSocket`, `TigonNoiseContext`) and the web bundle:
//! the initiator commits an explicit `mix_hash("")` right after
//! `InitializeSymmetric`.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

type HmacSha256 = Hmac<Sha256>;

const PROTOCOL: &[u8] = b"Noise_XX_25519_AESGCM_SHA256";
const HASHLEN: usize = 32;

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    buf
}

fn hkdf(chaining_key: &[u8], ikm: &[u8], outputs: usize) -> Vec<[u8; 32]> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(chaining_key).expect("hmac key");
    mac.update(ikm);
    let temp = mac.finalize().into_bytes();

    let mut out = Vec::with_capacity(outputs);
    let mut previous: Vec<u8> = Vec::new();
    for index in 0..outputs {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&temp).expect("hmac key");
        mac.update(&previous);
        mac.update(&[(index + 1) as u8]);
        let block = mac.finalize().into_bytes();
        previous = block.to_vec();
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&block);
        out.push(buf);
    }
    out
}

fn nonce_bytes(counter: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[4..].copy_from_slice(&counter.to_be_bytes());
    out
}

/// One direction of the transport cipher.
pub struct CipherState {
    cipher: Option<Aes256Gcm>,
    nonce: u64,
}

impl CipherState {
    fn empty() -> Self {
        Self {
            cipher: None,
            nonce: 0,
        }
    }

    fn with_key(key: &[u8; 32]) -> Self {
        Self {
            cipher: Some(Aes256Gcm::new_from_slice(key).expect("aes key")),
            nonce: 0,
        }
    }

    pub fn seal(&mut self, ad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        match self.cipher.as_ref() {
            None => plaintext.to_vec(),
            Some(cipher) => {
                let nonce = nonce_bytes(self.nonce);
                self.nonce += 1;
                cipher
                    .encrypt(
                        Nonce::from_slice(&nonce),
                        Payload {
                            msg: plaintext,
                            aad: ad,
                        },
                    )
                    .expect("aes-gcm seal")
            }
        }
    }

    pub fn open(&mut self, ad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
        match self.cipher.as_ref() {
            None => Ok(ciphertext.to_vec()),
            Some(cipher) => {
                let nonce = nonce_bytes(self.nonce);
                self.nonce += 1;
                cipher
                    .decrypt(
                        Nonce::from_slice(&nonce),
                        Payload {
                            msg: ciphertext,
                            aad: ad,
                        },
                    )
                    .map_err(|_| "noise: AEAD authentication failed".to_string())
            }
        }
    }
}

struct SymmetricState {
    ck: [u8; 32],
    h: [u8; 32],
    cipher: CipherState,
}

impl SymmetricState {
    fn new() -> Self {
        let mut initial = [0u8; HASHLEN];
        if PROTOCOL.len() <= HASHLEN {
            initial[..PROTOCOL.len()].copy_from_slice(PROTOCOL);
        } else {
            initial = sha256(PROTOCOL);
        }
        let mut state = Self {
            ck: initial,
            h: initial,
            cipher: CipherState::empty(),
        };
        state.mix_hash(b"");
        state
    }

    fn mix_hash(&mut self, data: &[u8]) {
        let mut buf = Vec::with_capacity(HASHLEN + data.len());
        buf.extend_from_slice(&self.h);
        buf.extend_from_slice(data);
        self.h = sha256(&buf);
    }

    fn mix_key(&mut self, ikm: &[u8]) {
        let out = hkdf(&self.ck, ikm, 2);
        self.ck = out[0];
        self.cipher = CipherState::with_key(&out[1]);
    }

    fn encrypt_and_hash(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let ad = self.h;
        let ct = self.cipher.seal(&ad, plaintext);
        self.mix_hash(&ct);
        ct
    }

    fn decrypt_and_hash(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, String> {
        let ad = self.h;
        let pt = self.cipher.open(&ad, ciphertext)?;
        self.mix_hash(ciphertext);
        Ok(pt)
    }

    fn split(&mut self) -> (CipherState, CipherState) {
        let out = hkdf(&self.ck, b"", 2);
        (
            CipherState::with_key(&out[0]),
            CipherState::with_key(&out[1]),
        )
    }
}

pub struct Initiator {
    symmetric: SymmetricState,
    ephemeral: Option<StaticSecret>,
    remote_ephemeral: Option<[u8; 32]>,
    phase: u8,
}

impl Initiator {
    pub fn new() -> Self {
        Self {
            symmetric: SymmetricState::new(),
            ephemeral: None,
            remote_ephemeral: None,
            phase: 1,
        }
    }

    fn dh(secret: &StaticSecret, peer: &[u8; 32]) -> Result<[u8; 32], String> {
        let shared = secret.diffie_hellman(&PublicKey::from(*peer));
        if !shared.was_contributory() {
            return Err("noise: all-zero DH output".to_string());
        }
        Ok(*shared.as_bytes())
    }

    pub fn write_message1(&mut self) -> Vec<u8> {
        let secret = StaticSecret::from(rand::random::<[u8; 32]>());
        let public = PublicKey::from(&secret);
        let mut message = public.as_bytes().to_vec();
        self.symmetric.mix_hash(public.as_bytes());
        let payload = random_nonce_payload();
        message.extend_from_slice(&self.symmetric.encrypt_and_hash(&payload));
        self.ephemeral = Some(secret);
        self.phase = 2;
        message
    }

    pub fn read_message2(&mut self, message: &[u8]) -> Result<Vec<u8>, String> {
        if message.len() < 96 {
            return Err(format!("noise: message2 too short ({} bytes)", message.len()));
        }
        let secret = self
            .ephemeral
            .as_ref()
            .ok_or_else(|| "noise: missing ephemeral secret".to_string())?
            .clone();

        let mut remote = [0u8; 32];
        remote.copy_from_slice(&message[..32]);
        self.symmetric.mix_hash(&remote);
        self.symmetric.mix_key(&Self::dh(&secret, &remote)?);

        let static_key = self.symmetric.decrypt_and_hash(&message[32..80])?;
        let mut remote_static = [0u8; 32];
        if static_key.len() != 32 {
            return Err("noise: responder static key is not 32 bytes".to_string());
        }
        remote_static.copy_from_slice(&static_key);
        self.symmetric.mix_key(&Self::dh(&secret, &remote_static)?);

        let payload = self.symmetric.decrypt_and_hash(&message[80..])?;
        self.remote_ephemeral = Some(remote);
        self.phase = 3;
        Ok(payload)
    }

    pub fn write_message3(&mut self) -> Result<Vec<u8>, String> {
        let remote = self
            .remote_ephemeral
            .ok_or_else(|| "noise: missing responder ephemeral".to_string())?;
        let secret = StaticSecret::from(rand::random::<[u8; 32]>());
        let public = PublicKey::from(&secret);
        let mut message = self.symmetric.encrypt_and_hash(public.as_bytes());
        self.symmetric.mix_key(&Self::dh(&secret, &remote)?);
        message.extend_from_slice(&self.symmetric.encrypt_and_hash(b""));
        self.phase = 5;
        Ok(message)
    }

    pub fn split(&mut self) -> (CipherState, CipherState) {
        self.symmetric.split()
    }
}

/// Protobuf `ClientNonce { bytes nonce = 1; }` with a random 32-byte nonce.
fn random_nonce_payload() -> Vec<u8> {
    let nonce: [u8; 32] = rand::random();
    let mut payload = Vec::with_capacity(34);
    payload.push(0x0a);
    payload.push(0x20);
    payload.extend_from_slice(&nonce);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive both sides of the XX handshake with matching states so the
    /// initiator can be checked without a network peer.
    struct Responder {
        symmetric: SymmetricState,
        ephemeral: StaticSecret,
        initiator_ephemeral: Option<[u8; 32]>,
    }

    impl Responder {
        fn new() -> Self {
            Self {
                symmetric: SymmetricState::new(),
                ephemeral: StaticSecret::from(rand::random::<[u8; 32]>()),
                initiator_ephemeral: None,
            }
        }

        fn read_message1(&mut self, message: &[u8]) -> Vec<u8> {
            let mut remote = [0u8; 32];
            remote.copy_from_slice(&message[..32]);
            self.symmetric.mix_hash(&remote);
            self.initiator_ephemeral = Some(remote);
            let payload = self
                .symmetric
                .decrypt_and_hash(&message[32..])
                .expect("message1 payload");
            assert_eq!(payload.len(), 34, "client nonce payload is a 32-byte field");
            assert_eq!(payload[0], 0x0a);
            assert_eq!(payload[1], 0x20);
            payload
        }

        fn write_message2(&mut self) -> Vec<u8> {
            let public = PublicKey::from(&self.ephemeral);
            let mut message = public.as_bytes().to_vec();
            self.symmetric.mix_hash(public.as_bytes());
            let initiator = self.initiator_ephemeral.expect("initiator ephemeral");
            let shared = self.ephemeral.diffie_hellman(&PublicKey::from(initiator));
            self.symmetric.mix_key(shared.as_bytes());

            let static_secret = StaticSecret::from(rand::random::<[u8; 32]>());
            let static_public = PublicKey::from(&static_secret);
            message.extend_from_slice(&self.symmetric.encrypt_and_hash(static_public.as_bytes()));

            let shared_static = static_secret.diffie_hellman(&PublicKey::from(initiator));
            self.symmetric.mix_key(shared_static.as_bytes());
            message.extend_from_slice(&self.symmetric.encrypt_and_hash(b"attestation"));
            message
        }

        fn read_message3(&mut self, message: &[u8]) {
            let remote_static = self
                .symmetric
                .decrypt_and_hash(&message[..48])
                .expect("initiator static");
            let shared = self
                .ephemeral
                .diffie_hellman(&{
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&remote_static);
                    PublicKey::from(key)
                });
            self.symmetric.mix_key(shared.as_bytes());
            let payload = self
                .symmetric
                .decrypt_and_hash(&message[48..])
                .expect("final payload");
            assert!(payload.is_empty());
        }

        fn split(&mut self) -> (CipherState, CipherState) {
            // Responder's send cipher is the initiator's receive cipher.
            let (initiator_send, initiator_receive) = self.symmetric.split();
            (initiator_receive, initiator_send)
        }
    }

    #[test]
    fn full_handshake_agrees_on_keys() {
        let mut initiator = Initiator::new();
        let message1 = initiator.write_message1();
        // Message 1 has no key yet: the payload is emitted as plaintext even
        // though it goes through encrypt_and_hash.
        assert_eq!(message1.len(), 32 + 34, "ephemeral public key + client nonce");

        let mut responder = Responder::new();
        responder.read_message1(&message1);

        let message2 = responder.write_message2();
        let attestation = initiator.read_message2(&message2).expect("message2");
        assert_eq!(attestation, b"attestation");
        assert!(initiator.phase >= 3, "message2 must advance the handshake");

        let message3 = initiator.write_message3().expect("message3");
        responder.read_message3(&message3);

        let (mut initiator_send, mut initiator_receive) = initiator.split();
        let (mut responder_send, mut responder_receive) = responder.split();

        let ciphertext = initiator_send.seal(b"", b"hello gateway");
        assert_ne!(ciphertext, b"hello gateway");
        assert_eq!(
            responder_receive.open(b"", &ciphertext).expect("decrypt"),
            b"hello gateway"
        );

        let reverse = responder_send.seal(b"", b"hello client");
        assert_eq!(
            initiator_receive.open(b"", &reverse).expect("decrypt"),
            b"hello client"
        );
    }

    #[test]
    fn nonce_counter_advances_per_message() {
        let key = [7u8; 32];
        let mut a = CipherState::with_key(&key);
        let mut b = CipherState::with_key(&key);
        let first = a.seal(b"aad", b"one");
        let second = a.seal(b"aad", b"two");
        assert_eq!(b.open(b"aad", &first).unwrap(), b"one");
        assert_eq!(b.open(b"aad", &second).unwrap(), b"two");
    }

    #[test]
    fn associated_data_is_enforced() {
        let key = [9u8; 32];
        let mut a = CipherState::with_key(&key);
        let mut b = CipherState::with_key(&key);
        let ciphertext = a.seal(b"right-ad", b"payload");
        assert!(b.open(b"wrong-ad", &ciphertext).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = [3u8; 32];
        let mut a = CipherState::with_key(&key);
        let mut b = CipherState::with_key(&key);
        let mut ciphertext = a.seal(b"", b"payload");
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xff;
        assert!(b.open(b"", &ciphertext).is_err());
    }

    #[test]
    fn keyless_cipherstate_is_a_pass_through() {
        let mut empty = CipherState::empty();
        assert_eq!(empty.seal(b"", b"plain"), b"plain");
        assert_eq!(empty.open(b"", b"plain").unwrap(), b"plain");
    }

    #[test]
    fn protocol_name_is_padded_to_hashlen() {
        let state = SymmetricState::new();
        // InitializeSymmetric pads a short protocol name with zeros to HASHLEN.
        let mut padded = [0u8; 32];
        padded[..PROTOCOL.len()].copy_from_slice(PROTOCOL);
        assert_eq!(state.ck, padded);
        // Then the initiator applies mix_hash(""), i.e. h = SHA256(h || "").
        assert_eq!(state.h, sha256(&padded));
    }

    #[test]
    fn message2_shorter_than_a_handshake_is_rejected() {
        let mut initiator = Initiator::new();
        initiator.write_message1();
        assert!(initiator.read_message2(&[0u8; 64]).is_err());
    }
}
