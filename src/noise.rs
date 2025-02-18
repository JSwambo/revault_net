//! Noise Protocol Framework API
//!
//! This module is a wrapper for noise functionality provided by snow to build
//! and use secure communication channels between revault infrastructure machines.
//!

use crate::error::NoiseError;

use std::convert::TryInto;

use snow::{resolvers::SodiumResolver, Builder, HandshakeState, TransportState};

/// The static public key used to enact Noise authenticated and encrypted channels
pub use sodiumoxide::crypto::box_::curve25519xsalsa20poly1305::PublicKey;
/// The static secret key used to enact Noise authenticated and encrypted channels
pub use sodiumoxide::crypto::box_::curve25519xsalsa20poly1305::SecretKey;

/// The size of a key, either public or private, on the Curve25519
pub const KEY_SIZE: usize = 32;
/// Size of the poly1305 MAC
pub const MAC_SIZE: usize = 16;
/// Max message size pecified by Noise Protocol Framework
pub const NOISE_MESSAGE_MAX_SIZE: usize = 65535;
/// Two bytes are used for the message length prefix
pub const LENGTH_PREFIX_SIZE: usize = 2;
/// Message header length plus its MAC
pub const NOISE_MESSAGE_HEADER_SIZE: usize = LENGTH_PREFIX_SIZE + MAC_SIZE;
/// Maximum size of a message before being encrypted; limited by Noise Protocol Framework
pub const NOISE_PLAINTEXT_MAX_SIZE: usize =
    NOISE_MESSAGE_MAX_SIZE - NOISE_MESSAGE_HEADER_SIZE - MAC_SIZE;
/// e, es, ss
pub const KK_MSG_1_SIZE: usize = KEY_SIZE + HANDSHAKE_MESSAGE.len() + MAC_SIZE;
/// e, ee, se
pub const KK_MSG_2_SIZE: usize = KEY_SIZE + MAC_SIZE;
/// Sent for versioning and identification during handshake
pub const HANDSHAKE_MESSAGE: &[u8] = b"practical_revault_0";

/// First round of the KK handshake
#[derive(Debug)]
pub struct KKHandshakeActOne {
    state: HandshakeState,
}

/// Message sent during the first round of the KK handshake (e, es, ss)
pub struct KKMessageActOne(pub(crate) [u8; KK_MSG_1_SIZE]);

impl KKHandshakeActOne {
    /// Start the first act of the handshake as an initiator (sharing e, es, ss)
    pub fn initiator(
        my_privkey: &SecretKey,
        their_pubkey: &PublicKey,
    ) -> Result<(KKHandshakeActOne, KKMessageActOne), NoiseError> {
        // Build the initial initiator state
        let builder = Builder::with_resolver(
            "Noise_KK_25519_ChaChaPoly_SHA256"
                .parse()
                .expect("Valid params"),
            Box::new(SodiumResolver::default()),
        );
        let mut state = builder
            .local_private_key(&my_privkey.0)
            .remote_public_key(&their_pubkey.0)
            .build_initiator()?;

        // Write the first message
        let mut msg = [0u8; KK_MSG_1_SIZE];
        state.write_message(HANDSHAKE_MESSAGE, &mut msg)?;

        Ok((KKHandshakeActOne { state }, KKMessageActOne(msg)))
    }

    /// Start the first act of the handshake as a responder (reading e, es, ss and doing wizardry with it)
    pub fn responder(
        my_privkey: &SecretKey,
        their_possible_pubkeys: &[PublicKey],
        message: &KKMessageActOne,
    ) -> Result<KKHandshakeActOne, NoiseError> {
        // TODO: estimate how inefficient it is.
        for their_pubkey in their_possible_pubkeys {
            // Build the initial responder state
            let builder = Builder::with_resolver(
                "Noise_KK_25519_ChaChaPoly_SHA256"
                    .parse()
                    .expect("Valid params"),
                Box::new(SodiumResolver::default()),
            );
            let mut state = builder
                .local_private_key(&my_privkey.0)
                .remote_public_key(&their_pubkey.0)
                .build_responder()?;

            let mut msg = [0u8; KK_MSG_1_SIZE];
            if state.read_message(&message.0, &mut msg).is_err() {
                continue;
            }
            if &msg[..HANDSHAKE_MESSAGE.len()] != HANDSHAKE_MESSAGE {
                return Err(NoiseError::BadHandshake);
            }

            return Ok(KKHandshakeActOne { state });
        }

        Err(NoiseError::MissingStaticKey)
    }
}

/// Final round of the KK handshake
#[derive(Debug)]
pub struct KKHandshakeActTwo {
    /// Inner snow Noise KK handshake state
    state: HandshakeState,
}

/// Content of the message from the final round of the KK handshake (e, ee, se)
pub struct KKMessageActTwo(pub(crate) [u8; KK_MSG_2_SIZE]);

impl KKHandshakeActTwo {
    /// Start the second act of the handshake as a responder (read e, ee, se)
    pub fn initiator(
        mut handshake: KKHandshakeActOne,
        message: &KKMessageActTwo,
    ) -> Result<KKHandshakeActTwo, NoiseError> {
        // In handshake mode we don't actually care about the message
        let mut _m = [0u8; KK_MSG_2_SIZE];
        handshake.state.read_message(&message.0, &mut _m)?;

        Ok(KKHandshakeActTwo {
            state: handshake.state,
        })
    }

    /// Start the second act of the handshake as a responder (write e, ee, se)
    pub fn responder(
        mut handshake: KKHandshakeActOne,
    ) -> Result<(KKHandshakeActTwo, KKMessageActTwo), NoiseError> {
        let mut msg = [0u8; KK_MSG_2_SIZE];
        handshake.state.write_message(&[], &mut msg)?;

        Ok((
            KKHandshakeActTwo {
                state: handshake.state,
            },
            KKMessageActTwo(msg),
        ))
    }
}

/// A cypertext encrypted with [encrypt_message] containing the length prefix of
/// a plaintext message.
#[derive(Debug, Clone, Copy)]
pub struct NoiseEncryptedHeader(pub [u8; LENGTH_PREFIX_SIZE + MAC_SIZE]);

/// A cyphertext encrypted with [encrypt_message] containing the body of a Noise
/// message.
#[derive(Debug)]
pub struct NoiseEncryptedMessage(pub Vec<u8>);

/// A wrapper over Snow's transport state for a KK Noise communication channel.
#[derive(Debug)]
pub struct KKChannel {
    transport_state: TransportState,
}

fn encrypted_msg_size(plaintext_size: usize) -> usize {
    // Length prefix + MAC    ||   Message + MAC
    NOISE_MESSAGE_HEADER_SIZE + plaintext_size + MAC_SIZE
}

impl KKChannel {
    /// Constructs the KK Noise channel from a final stage KK handshake
    pub fn from_handshake(state: KKHandshakeActTwo) -> Result<KKChannel, NoiseError> {
        let transport_state = state.state.into_transport_mode()?;

        Ok(KKChannel { transport_state })
    }

    /// Use the channel to encrypt a message shorter than [NOISE_PLAINTEXT_MAX_SIZE].
    /// Pre-fixes the message with a 2-bytes big-endian length field MAC'ed on its own to permit
    /// incremental reads.
    /// On success, returns the ciphertext.
    pub fn encrypt_message(&mut self, message: &[u8]) -> Result<NoiseEncryptedMessage, NoiseError> {
        if message.len() > NOISE_PLAINTEXT_MAX_SIZE {
            return Err(NoiseError::InvalidPlaintext);
        }
        let mut output = vec![0u8; encrypted_msg_size(message.len())];

        let message_len: u16 = (MAC_SIZE + message.len())
            .try_into()
            .expect("We just checked it was < NOISE_PLAINTEXT_MAX_SIZE");
        let prefix = message_len.to_be_bytes().to_vec();
        self.transport_state
            .write_message(&prefix, &mut output[..NOISE_MESSAGE_HEADER_SIZE])?;

        self.transport_state
            .write_message(&message, &mut output[NOISE_MESSAGE_HEADER_SIZE..])?;

        Ok(NoiseEncryptedMessage(output))
    }

    /// Get the size of the message following this header
    pub fn decrypt_header(&mut self, header: &NoiseEncryptedHeader) -> Result<u16, NoiseError> {
        let mut buf = [0u8; NOISE_MESSAGE_HEADER_SIZE];
        self.transport_state.read_message(&header.0, &mut buf)?;

        let len_be: [u8; 2] = buf[..NOISE_MESSAGE_HEADER_SIZE - MAC_SIZE]
            .try_into()
            .expect("NOISE_MESSAGE_HEADER_SIZE - MAC_SIZE == LENGTH_PREFIX_SIZE");
        Ok(u16::from_be_bytes(len_be))
    }

    /// Get plaintext bytes from a Noise-encrypted message
    pub fn decrypt_message(
        &mut self,
        message: &NoiseEncryptedMessage,
    ) -> Result<Vec<u8>, NoiseError> {
        // TODO: could be in NoiseEncryptedMessage's constructor?
        if message.0.len() > NOISE_MESSAGE_MAX_SIZE {
            return Err(NoiseError::InvalidCiphertext);
        }
        if message.0.len() < MAC_SIZE {
            return Err(NoiseError::InvalidCiphertext);
        }
        let mut plaintext = vec![0u8; message.0.len()];

        self.transport_state
            .read_message(&message.0, &mut plaintext)?;

        // We read the MAC, but caller doesn't care about it
        // FIXME: add a test for invalid MAC getting refused
        plaintext.truncate(plaintext.len() - MAC_SIZE);
        Ok(plaintext)
    }

    /// Get the static public key of the peer
    pub fn remote_static(&self) -> PublicKey {
        PublicKey(
            self.transport_state
                .get_remote_static()
                .expect(
                    "We could not have settled the KK channel without their key. \
                     And if we could, better to crash now!",
                )
                .try_into()
                .expect("Our keys aren't 32 bytes anymore?"),
        )
    }
}

#[cfg(test)]
pub mod tests {
    use crate::noise::{
        KKChannel, KKHandshakeActOne, KKHandshakeActTwo, KKMessageActOne, KKMessageActTwo,
        NoiseEncryptedHeader, NoiseEncryptedMessage, KK_MSG_1_SIZE, KK_MSG_2_SIZE, MAC_SIZE,
        NOISE_MESSAGE_HEADER_SIZE, NOISE_MESSAGE_MAX_SIZE, NOISE_PLAINTEXT_MAX_SIZE,
    };
    use sodiumoxide::crypto::box_::curve25519xsalsa20poly1305::gen_keypair;
    use std::convert::TryInto;

    #[test]
    fn test_bidirectional_roundtrip() {
        let (initiator_pubkey, initiator_privkey) = gen_keypair();
        let (responder_pubkey, responder_privkey) = gen_keypair();

        // client
        let (cli_act_1, msg_1) =
            KKHandshakeActOne::initiator(&initiator_privkey, &responder_pubkey).unwrap();

        // server
        let serv_act_1 =
            KKHandshakeActOne::responder(&responder_privkey, &[initiator_pubkey], &msg_1).unwrap();
        let (serv_act_2, msg_2) = KKHandshakeActTwo::responder(serv_act_1).unwrap();
        let mut server_channel = KKChannel::from_handshake(serv_act_2).unwrap();

        // client
        let cli_act_2 = KKHandshakeActTwo::initiator(cli_act_1, &msg_2).unwrap();
        let mut client_channel = KKChannel::from_handshake(cli_act_2).unwrap();

        // client encrypts message for server
        let msg = "Hello".as_bytes();
        let encrypted_msg = client_channel.encrypt_message(&msg).unwrap();
        assert_eq!(
            encrypted_msg.0.len(),
            msg.len() + NOISE_MESSAGE_HEADER_SIZE + MAC_SIZE
        );
        let (header, body) = (
            &encrypted_msg.0[..NOISE_MESSAGE_HEADER_SIZE],
            &encrypted_msg.0[NOISE_MESSAGE_HEADER_SIZE..],
        );
        eprintln!("{:x?}", header);
        let msg_len = server_channel
            .decrypt_header(&NoiseEncryptedHeader(header.try_into().unwrap()))
            .unwrap();
        assert_eq!(msg_len as usize, msg.len() + MAC_SIZE);
        let decrypted_msg = server_channel
            .decrypt_message(&NoiseEncryptedMessage(body.to_vec()))
            .unwrap();
        assert_eq!(msg.to_vec(), decrypted_msg);

        // server encrypts message for client
        let msg = "Goodbye".as_bytes();
        let encrypted_msg = server_channel.encrypt_message(&msg).unwrap();
        let (header, body) = (
            &encrypted_msg.0[..NOISE_MESSAGE_HEADER_SIZE],
            &encrypted_msg.0[NOISE_MESSAGE_HEADER_SIZE..],
        );
        let msg_len = client_channel
            .decrypt_header(&NoiseEncryptedHeader(header.try_into().unwrap()))
            .unwrap();
        assert_eq!(msg_len as usize, msg.len() + MAC_SIZE);
        let decrypted_msg = client_channel
            .decrypt_message(&NoiseEncryptedMessage(body.to_vec()))
            .unwrap();
        assert_eq!(msg.to_vec(), decrypted_msg);
    }

    #[test]
    fn test_message_size_limit() {
        let (initiator_pubkey, initiator_privkey) = gen_keypair();
        let (responder_pubkey, responder_privkey) = gen_keypair();

        // client
        let (_, msg_1) =
            KKHandshakeActOne::initiator(&initiator_privkey, &responder_pubkey).unwrap();

        // server
        let serv_act_1 =
            KKHandshakeActOne::responder(&responder_privkey, &[initiator_pubkey], &msg_1).unwrap();
        let (serv_act_2, _msg_2) = KKHandshakeActTwo::responder(serv_act_1).unwrap();
        let mut server_channel = KKChannel::from_handshake(serv_act_2).unwrap();

        // Hit the limit
        let msg = [0u8; NOISE_PLAINTEXT_MAX_SIZE];
        server_channel
            .encrypt_message(&msg)
            .expect("Maximum allowed");

        // Fail if msg too large
        let msg = [0u8; NOISE_MESSAGE_MAX_SIZE - NOISE_MESSAGE_HEADER_SIZE + 1];
        server_channel
            .encrypt_message(&msg)
            .expect_err("Limit exceeded");

        // We can encrypt an empty message
        let msg = b"";
        server_channel
            .encrypt_message(msg)
            .expect("Empty message is fine to encrypt");

        // We cannot decrypt an empty message
        server_channel
            .decrypt_message(&NoiseEncryptedMessage(msg.to_vec()))
            .expect_err("Encrypted message with no header");
    }

    #[test]
    fn test_bad_messages() {
        let (initiator_pubkey, initiator_privkey) = gen_keypair();
        let (responder_pubkey, responder_privkey) = gen_keypair();

        // KK handshake fails if messages are badly formed.
        // Without a valid cli_act_2 nor serv_act_2, no KKChannel can be constructed.
        let (cli_act_1, _) = KKHandshakeActOne::initiator(&initiator_privkey, &responder_pubkey)
            .expect("The first act is valid.");

        let bad_msg = KKMessageActOne([1u8; KK_MSG_1_SIZE]);
        KKHandshakeActOne::responder(&responder_privkey, &[initiator_pubkey], &bad_msg)
            .expect_err("This one is invalid as bad_msg cannot be decrypted.");

        let bad_msg = KKMessageActTwo([1u8; KK_MSG_2_SIZE]);
        KKHandshakeActTwo::initiator(cli_act_1, &bad_msg).expect_err("So is this one.");
    }
}
