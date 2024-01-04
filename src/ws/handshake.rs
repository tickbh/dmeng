// Copyright 2022 - 2024 Wenmeng See the COPYRIGHT
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
//
// Author: tickbh
// -----
// Created Date: 2024/01/04 11:12:31

use std::net::SocketAddr;

use sha1::{Digest, Sha1};
use webparse::{Response, WebError, WsError};

use crate::{Body, ProtError, ProtResult, RecvRequest, RecvResponse};

static MAGIC_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub struct WsHandshake {
    /// The HTTP request sent to begin the handshake.
    pub request: RecvRequest,
    /// The HTTP response from the server confirming the handshake.
    pub response: RecvResponse,
    /// The socket address of the other endpoint. This address may
    /// be an intermediary such as a proxy server.
    pub peer_addr: Option<SocketAddr>,
    /// The socket address of this endpoint.
    pub local_addr: Option<SocketAddr>,
}

impl WsHandshake {
    pub fn new(
        request: RecvRequest,
        response: RecvResponse,
        peer_addr: Option<SocketAddr>,
    ) -> Self {
        Self {
            request,
            response,
            peer_addr,
            local_addr: None,
        }
    }

    pub fn build_accept(key: &str) -> ProtResult<String> {
        match base64::decode(key) {
            Ok(vec) => {
                if vec.len() != 16 {
                    return Err(ProtError::from(WebError::Ws(WsError::ProtocolError(
                        "Sec-WebSocket-Key must be 16 bytes",
                    ))));
                }
                let mut array = [0u8; 16];
                array[..16].clone_from_slice(&vec[..16]);

                let mut concat_key = String::with_capacity(array.len() + 36);
                concat_key.push_str(&key[..]);
                concat_key.push_str(MAGIC_GUID);
                let hash = Sha1::digest(concat_key.as_bytes());
                let key: [u8; 20] = hash.into();
                Ok(base64::encode(key))
            }
            Err(_) => {
                return Err(ProtError::from(WebError::Ws(WsError::ProtocolError(
                    "Invalid Sec-WebSocket-Accept",
                ))))
            }
        }
    }

    pub fn build_request(req: &RecvRequest) -> ProtResult<RecvResponse> {
        let key = req.headers().get_str_value(&"Sec-WebSocket-Key");
        let protocol = req.headers().get_str_value(&"Sec-WebSocket-Protocol");
        let version = req.headers().get_str_value(&"Sec-WebSocket-Version");
        println!("key = {:?}, protocol = {:?} version = {:?}", key, protocol, version);
        if key.is_none() || version.as_ref().map(|s| &**s) != Some("13") {
            return Ok(Response::builder()
                .status(400)
                .body("invalid websocket version")
                .unwrap()
                .into_type());
        }
        let (key, protocol) = (key.unwrap(), protocol.unwrap_or("chat".to_string()));
        let accept = Self::build_accept(&key)?;
        let protocols: Vec<&str> = protocol
            .split(|c| c == ',' || c == ' ')
            .filter(|s| !s.is_empty())
            .collect();
        return Ok(Response::builder()
            .status(101)
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Accept", accept)
            .header("Sec-WebSocket-Protocol", protocols[0].to_string())
            .body(Body::empty())
            .unwrap());
    }
}
