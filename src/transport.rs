//! TCP wrapper API
//!
//! This module is a wrapper for TCP functionality that uses noise API internally
//! to automagically provide encrypted and authenticated channels.
//!

use crate::{
    error::Error,
    noise::{
        KKChannel, KKHandshakeActOne, KKHandshakeActTwo, KKMessageActOne, KKMessageActTwo,
        NoiseEncryptedHeader, NoiseEncryptedMessage, PublicKey, SecretKey, KK_MSG_1_SIZE,
        KK_MSG_2_SIZE, NOISE_MESSAGE_HEADER_SIZE,
    },
};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::{thread, time::Duration};

/// Wrapper type for a TcpStream and KKChannel that automatically enforces authenticated and
/// encrypted channels when communicating
#[derive(Debug)]
pub struct KKTransport {
    stream: TcpStream,
    channel: KKChannel,
}

impl KKTransport {
    /// Connect to server at given address, and enact Noise handshake with given private key.
    pub fn connect(
        addr: SocketAddr,
        my_noise_privkey: &SecretKey,
        their_noise_pubkey: &PublicKey,
    ) -> Result<KKTransport, Error> {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;

        let (cli_act_1, msg_1) =
            KKHandshakeActOne::initiator(my_noise_privkey, their_noise_pubkey)?;

        // write msg_1 to stream (e, es, ss)
        stream.write_all(&msg_1.0)?;

        // read msg_2 from stream (e, ee, se)
        let mut msg_2 = [0u8; KK_MSG_2_SIZE];
        stream.read_exact(&mut msg_2)?;

        let msg_act_2 = KKMessageActTwo(msg_2);
        let cli_act_2 = KKHandshakeActTwo::initiator(cli_act_1, &msg_act_2)?;
        let channel = KKChannel::from_handshake(cli_act_2)?;
        Ok(KKTransport { stream, channel })
    }

    /// Accept an incoming connection and immediately perform the noise KK handshake
    /// as a responder with our single private key and a set of possible public key for them.
    /// This is used by servers to identify the origin of the message.
    pub fn accept(
        listener: &TcpListener,
        my_noise_privkey: &SecretKey,
        their_possible_pubkeys: &[PublicKey],
    ) -> Result<KKTransport, Error> {
        let (mut stream, _) = listener.accept().map_err(|e| Error::Transport(e))?;

        // read msg_1 from stream
        let mut msg_1 = [0u8; KK_MSG_1_SIZE];
        stream.read_exact(&mut msg_1)?;
        let msg_act_1 = KKMessageActOne(msg_1);

        let serv_act_1 =
            KKHandshakeActOne::responder(&my_noise_privkey, their_possible_pubkeys, &msg_act_1)?;
        let (serv_act_2, msg_2) = KKHandshakeActTwo::responder(serv_act_1)?;
        let channel = KKChannel::from_handshake(serv_act_2)?;

        // write msg_2 to stream
        stream.write_all(&msg_2.0)?;

        Ok(KKTransport { stream, channel })
    }

    /// Write a message to the other end of the encrypted communication channel. Attempts
    /// to recover from certain kinds of error.
    pub fn write(&mut self, msg: &[u8]) -> Result<(), Error> {
        let encrypted_msg = self.channel.encrypt_message(msg)?.0;
        let mut attempts = 0;
        loop {
            match self.stream.write_all(&encrypted_msg) {
                Ok(n) => return Ok(n),
                // write_all returns the first error of non-ErrorKind::Interrupted kind that
                // write returns, in which case no bytes were written to the writer, and can
                // try again. Here we try up to 5 times.
                Err(e) => {
                    attempts += 1;
                    if attempts == 5 {
                        return Err(Error::from(e));
                    } else {
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                }
            }
        }
    }

    /// Read a message from the other end of the encrypted communication channel.
    fn _read(&mut self) -> Result<Vec<u8>, Error> {
        let mut cypherheader = [0u8; NOISE_MESSAGE_HEADER_SIZE];
        self.stream.read_exact(&mut cypherheader)?;
        let msg_len = self
            .channel
            .decrypt_header(&NoiseEncryptedHeader(cypherheader))?;

        // Note that `msg_len` cannot be > 65K (2 bytes)
        let mut cypherbody = vec![0u8; msg_len as usize];
        self.stream.read_exact(&mut cypherbody)?;
        self.channel
            .decrypt_message(&NoiseEncryptedMessage(cypherbody))
            .map_err(|e| e.into())
    }

    /// Read a message from the other end of the encrypted communication channel.
    /// Will recover from certain kinds of error, those for which no bytes are
    /// read from the stream, by retrying up to 5 times with a 1s sleep between
    /// attempts. After 5 attempts, or an unrecoverable error, will return an
    /// error.  
    pub fn read(&mut self) -> Result<Vec<u8>, Error> {
        let mut attempts = 0;
        loop {
            match self._read() {
                Ok(msg) => return Ok(msg),
                Err(error) => match error {
                    e if attempts == 4 => return Err(e),
                    Error::Transport(e) => match e.kind() {
                        ErrorKind::UnexpectedEof => return Err(Error::Transport(e)),
                        ErrorKind::Interrupted => return Err(Error::Transport(e)),
                        _ => {
                            thread::sleep(Duration::from_secs(1));
                            continue;
                        }
                    },
                    e => e,
                },
            };
            attempts += 1;
        }
    }

    /// Get the static public key of the peer
    pub fn remote_static(&self) -> PublicKey {
        self.channel.remote_static()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sodiumoxide::crypto::box_::curve25519xsalsa20poly1305::gen_keypair;
    use std::thread;

    #[test]
    fn test_transport_kk() {
        let ((client_pubkey, client_privkey), (server_pubkey, server_privkey)) =
            (gen_keypair(), gen_keypair());

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // client thread
        let cli_thread = thread::spawn(move || {
            let my_noise_privkey = client_privkey;
            let their_noise_pubkey = server_pubkey;

            let mut cli_channel =
                KKTransport::connect(addr, &my_noise_privkey, &their_noise_pubkey)
                    .expect("Client channel connecting");
            let msg = "Test message".as_bytes();
            cli_channel.write(&msg).expect("Sending test message");
            msg
        });

        let mut server_transport =
            KKTransport::accept(&listener, &server_privkey, &[client_pubkey])
                .expect("Server channel binding and accepting");

        let sent_msg = cli_thread.join().unwrap();
        let received_msg = server_transport.read().unwrap();
        assert_eq!(sent_msg.to_vec(), received_msg);
    }
}
