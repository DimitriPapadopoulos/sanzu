//! Implementation of message serialization/de-serialization using Tunnel
//!
//! Tunnel doesn't include a packet size by design. As our messages will be
//! sent using a stream, we won't be able to detect serialized message stop. To
//! overcome this, we will add a little header which will embed the serialized
//! message length.

use anyhow::{Context, Result};
use byteorder::ByteOrder;
use byteorder::{LittleEndian, ReadBytesExt};
use std::io::Cursor;
use std::io::{Read, Write};

pub const VERSION: &str = "0.1.1";

/// Code generated by Tunnel
pub mod tunnel {
    include!(concat!(env!("OUT_DIR"), "/tunnel.rs"));
}

const MAX_PACKET_LEN: usize = 100 * 1024 * 1024; // 100 Mo

/// Read + Write trait used to send protobuf serialized messages
pub trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

pub struct Stdio {}

impl Write for Stdio {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        let ret = std::io::stdout().write(buf);
        self.flush()?;
        ret
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        std::io::stdout().flush()
    }
}

impl Read for Stdio {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        std::io::stdin().read(buf)
    }
}

/// Tunnel object
pub struct Tunnel {}

impl Tunnel {
    /// Send a serialized message through `stream`
    pub fn send<T>(stream: &mut dyn ReadWrite, req: T) -> Result<()>
    where
        T: prost::Message,
    {
        // Send length, on 8 bytes
        let mut buffer = vec![0u8; 8];
        LittleEndian::write_u64(&mut buffer, req.encoded_len() as u64);
        // Encode request
        let mut req_buf = vec![];
        req.encode(&mut req_buf).context("Cannot encode pkt")?;
        buffer.append(&mut req_buf);
        // Send request
        stream.write_all(&buffer).context("Cannot write pkt")?;
        Ok(())
    }

    /// Receive a serialized message from `stream`
    pub fn recv<T>(stream: &mut dyn ReadWrite) -> Result<T>
    where
        T: prost::Message + Default,
    {
        let mut buffer = vec![0u8; 0x8];
        stream.read_exact(&mut buffer).context("Cannot read pkt")?;

        let mut rdr = Cursor::new(buffer);
        let len =
            ReadBytesExt::read_u64::<LittleEndian>(&mut rdr).context("Cannot read len")? as usize;
        if len > MAX_PACKET_LEN {
            return Err(anyhow!("Packet too big!"));
        }
        let mut req_buffer = vec![0u8; len];
        stream
            .read_exact(&mut req_buffer)
            .context("Cannot read pkt")?;
        match prost::Message::decode(req_buffer.as_slice()) {
            Ok(pkt) => Ok(pkt),
            Err(_) => {
                // Try to parse as Error msg
                let msg: tunnel::EventError =
                    prost::Message::decode(req_buffer.as_slice()).context("Cannot decode pkt")?;
                let mut err = anyhow!("Error from server");
                for error in msg.errors {
                    err = err.context(error);
                }
                Err(err)
            }
        }
    }
}

/// Only send first chain error to avoid internal error leaks to client
pub fn send_server_err_event(sock: &mut dyn ReadWrite, err: anyhow::Error) -> anyhow::Error {
    let mut errors = vec![];
    if let Some(err) = err.chain().next() {
        errors.push(format!("{}", err));
    }

    let err_msg = tunnel::EventError { errors };
    let srv_err_msg = tunnel::ServerMsgOrErr {
        msg: Some(tunnel::server_msg_or_err::Msg::Err(err_msg)),
    };

    if let Err(err) = Tunnel::send(sock, srv_err_msg) {
        anyhow!("Error in send: Peer has closed connection? ({:?})", err)
    } else {
        err
    }
}

pub fn recv_client_msg_or_error(
    stream: &mut dyn ReadWrite,
) -> Result<tunnel::message_client_ok::Msg> {
    let msg: tunnel::ClientMsgOrErr = Tunnel::recv(stream).context("Error in recv pkt")?;
    match msg.msg {
        Some(tunnel::client_msg_or_err::Msg::Ok(msg_ok)) => {
            // Message is ok
            if let Some(msg) = msg_ok.msg {
                Ok(msg)
            } else {
                Err(anyhow!("Empty pkt from client"))
            }
        }
        Some(tunnel::client_msg_or_err::Msg::Err(msg)) => {
            // Message is err
            let mut error = Err(anyhow!("[end err]"));
            for err in msg.errors.iter().rev() {
                error = error.context(err.to_string());
            }
            error = error.context("Error from client:");
            error
        }
        _ => Err(anyhow!("Bad pkt from client")),
    }
}

#[macro_export]
macro_rules! send_server_msg_type {
    (
        $sock: expr, $msg: expr, $name: ident
    ) => {{
        let msg_ok = tunnel::MessageServerOk {
            msg: Some(tunnel::message_server_ok::Msg::$name($msg)),
        };
        let msgsrv_ok = tunnel::ServerMsgOrErr {
            msg: Some(tunnel::server_msg_or_err::Msg::Ok(msg_ok)),
        };
        Tunnel::send($sock, msgsrv_ok).context("Error in send: Peer has closed connection?")
    }};
}

#[macro_export]
macro_rules! recv_client_msg_type {
    (
        $sock: expr, $name: ident
    ) => {{
        match recv_client_msg_or_error($sock) {
            Err(err) => Err(err.context(anyhow!("Received error msg"))),
            Ok(msg) => {
                if let tunnel::message_client_ok::Msg::$name(msg) = msg {
                    Ok(msg)
                } else {
                    Err(anyhow!("Bad packet type"))
                }
            }
        }
    }};
}

pub fn recv_server_msg_or_error(
    stream: &mut dyn ReadWrite,
) -> Result<tunnel::message_server_ok::Msg> {
    let msg: tunnel::ServerMsgOrErr = Tunnel::recv(stream).context("Error in recv pkt")?;
    match msg.msg {
        Some(tunnel::server_msg_or_err::Msg::Ok(msg_ok)) => {
            // Message is ok
            if let Some(msg) = msg_ok.msg {
                Ok(msg)
            } else {
                Err(anyhow!("Empty pkt from server"))
            }
        }
        Some(tunnel::server_msg_or_err::Msg::Err(msg)) => {
            // Message is err
            let mut error = Err(anyhow!("[end err]"));
            for err in msg.errors.iter().rev() {
                error = error.context(err.to_string());
            }
            error = error.context("Error from server:");
            error
        }
        _ => Err(anyhow!("Bad pkt from server")),
    }
}

/// Only send first chain error to avoid internal error leaks to client
pub fn send_client_err_event(sock: &mut dyn ReadWrite, err: anyhow::Error) -> anyhow::Error {
    let mut errors = vec![];
    if let Some(err) = err.chain().next() {
        errors.push(format!("{}", err));
    }

    let err_msg = tunnel::EventError { errors };
    let client_err_msg = tunnel::ClientMsgOrErr {
        msg: Some(tunnel::client_msg_or_err::Msg::Err(err_msg)),
    };

    if let Err(err) = Tunnel::send(sock, client_err_msg) {
        anyhow!("Error in send: Peer has closed connection? ({:?})", err)
    } else {
        err
    }
}

#[macro_export]
macro_rules! recv_server_msg_type {
    (
        $sock: expr, $name: ident
    ) => {{
        match recv_server_msg_or_error($sock) {
            Err(err) => Err(err.context(anyhow!("Received error msg"))),
            Ok(msg) => {
                if let tunnel::message_server_ok::Msg::$name(msg) = msg {
                    Ok(msg)
                } else {
                    Err(anyhow!("Bad packet type"))
                }
            }
        }
    }};
}

#[macro_export]
macro_rules! send_client_msg_type {
    (
        $sock: expr, $msg: expr, $name: ident
    ) => {{
        let msg_ok = tunnel::MessageClientOk {
            msg: Some(tunnel::message_client_ok::Msg::$name($msg)),
        };
        let msgclient_ok = tunnel::ClientMsgOrErr {
            msg: Some(tunnel::client_msg_or_err::Msg::Ok(msg_ok)),
        };
        Tunnel::send($sock, msgclient_ok)
            .map_err(|err| anyhow!("Error in send: Peer has closed connection? ({:?})", err,))
    }};
}
