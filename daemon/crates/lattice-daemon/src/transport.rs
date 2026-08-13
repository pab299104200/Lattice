use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::TcpStream as AsyncTcpStream;

use crate::transport_credentials::{
    self, IssuedTransportCredential, TransportCredential, TRANSPORT_PROTOCOL_VERSION,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProxyRequest {
    pub workspace_roots: Vec<String>,
    #[serde(default)]
    pub focus_files: Vec<String>,
    #[serde(default)]
    pub focus_dirs: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClientKind {
    StdioProxy,
    Cli,
    Doctor,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProxyHello {
    pub protocol_version: u32,
    pub daemon_epoch: String,
    pub transport_token: String,
    pub client_instance_nonce: String,
    pub client_kind: ClientKind,
    #[serde(flatten)]
    pub request: ProxyRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProxyAck {
    pub protocol_version: u32,
    pub daemon_epoch: String,
    pub connection_id: String,
    pub accepted_features: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ConnectionMetadata {
    pub protocol_version: u32,
    pub daemon_epoch: String,
    pub connection_id: String,
    pub client_instance_nonce: String,
    pub client_kind: ClientKind,
}

#[derive(Debug)]
pub(crate) struct AuthenticatedHello {
    pub request: ProxyRequest,
    pub metadata: ConnectionMetadata,
    pub ack: ProxyAck,
}

pub(crate) struct ServerTransport {
    issued: IssuedTransportCredential,
}

impl ServerTransport {
    pub(crate) fn issue(listener_address: &str) -> Result<Self> {
        Ok(Self {
            issued: transport_credentials::issue(listener_address)?,
        })
    }

    pub(crate) fn authenticate(
        &self,
        peer: SocketAddr,
        hello_line: &str,
    ) -> Result<AuthenticatedHello> {
        if !peer.ip().is_loopback() {
            anyhow::bail!("transport authentication rejected a non-loopback peer");
        }
        let hello: ProxyHello = serde_json::from_str(hello_line)
            .map_err(|_| anyhow::anyhow!("transport hello is malformed"))?;
        let credential = self.issued.credential();
        if hello.protocol_version != TRANSPORT_PROTOCOL_VERSION {
            anyhow::bail!(
                "transport protocol version {} is incompatible with daemon version {}; restart the lattice proxy and daemon with the matching binary",
                hello.protocol_version,
                TRANSPORT_PROTOCOL_VERSION
            );
        }
        if !valid_random_identifier(&hello.client_instance_nonce) {
            anyhow::bail!("transport hello contains an invalid client nonce");
        }
        if !constant_time_eq(
            hello.daemon_epoch.as_bytes(),
            credential.daemon_epoch.as_bytes(),
        ) {
            anyhow::bail!("transport authentication rejected a stale daemon epoch");
        }
        if !constant_time_eq(
            hello.transport_token.as_bytes(),
            credential.transport_token.as_bytes(),
        ) {
            anyhow::bail!("transport authentication failed");
        }
        let connection_id = transport_credentials::random_hex(16)?;
        let metadata = ConnectionMetadata {
            protocol_version: hello.protocol_version,
            daemon_epoch: credential.daemon_epoch.clone(),
            connection_id: connection_id.clone(),
            client_instance_nonce: hello.client_instance_nonce,
            client_kind: hello.client_kind,
        };
        let ack = ProxyAck {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            daemon_epoch: credential.daemon_epoch.clone(),
            connection_id,
            accepted_features: vec!["json_rpc_2_0".to_string()],
        };
        Ok(AuthenticatedHello {
            request: hello.request,
            metadata,
            ack,
        })
    }
}

pub(crate) async fn client_handshake(
    stream: &mut AsyncTcpStream,
    listener_address: &str,
    client_kind: ClientKind,
    request: &ProxyRequest,
) -> Result<ConnectionMetadata> {
    let credential = load_credential_with_retry(listener_address).await?;
    let nonce = transport_credentials::random_hex(16)?;
    let hello = ProxyHello {
        protocol_version: TRANSPORT_PROTOCOL_VERSION,
        daemon_epoch: credential.daemon_epoch.clone(),
        transport_token: credential.transport_token.clone(),
        client_instance_nonce: nonce.clone(),
        client_kind,
        request: request.clone(),
    };
    let mut encoded = serde_json::to_vec(&hello)?;
    encoded.push(b'\n');
    stream.write_all(&encoded).await?;
    stream.flush().await?;

    let mut ack_line = String::new();
    let read = AsyncBufReader::new(&mut *stream)
        .read_line(&mut ack_line)
        .await?;
    if read == 0 {
        anyhow::bail!("daemon closed the connection before transport acknowledgement");
    }
    validate_ack(&credential, &nonce, client_kind, &ack_line)
}

pub(crate) fn client_handshake_sync(
    stream: &mut std::net::TcpStream,
    listener_address: &str,
    client_kind: ClientKind,
    request: &ProxyRequest,
) -> Result<ConnectionMetadata> {
    let credential = transport_credentials::load(listener_address)?;
    let nonce = transport_credentials::random_hex(16)?;
    let hello = ProxyHello {
        protocol_version: TRANSPORT_PROTOCOL_VERSION,
        daemon_epoch: credential.daemon_epoch.clone(),
        transport_token: credential.transport_token.clone(),
        client_instance_nonce: nonce.clone(),
        client_kind,
        request: request.clone(),
    };
    serde_json::to_writer(&mut *stream, &hello)?;
    std::io::Write::write_all(stream, b"\n")?;
    std::io::Write::flush(stream)?;
    let mut ack_line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut *stream), &mut ack_line)?;
    if ack_line.is_empty() {
        anyhow::bail!("daemon closed the connection before transport acknowledgement");
    }
    validate_ack(&credential, &nonce, client_kind, &ack_line)
}

fn validate_ack(
    credential: &TransportCredential,
    client_instance_nonce: &str,
    client_kind: ClientKind,
    ack_line: &str,
) -> Result<ConnectionMetadata> {
    let ack: ProxyAck = serde_json::from_str(ack_line)
        .map_err(|_| anyhow::anyhow!("daemon returned a malformed transport acknowledgement"))?;
    if ack.protocol_version != TRANSPORT_PROTOCOL_VERSION {
        anyhow::bail!("daemon transport acknowledgement has an incompatible protocol version");
    }
    if !constant_time_eq(
        ack.daemon_epoch.as_bytes(),
        credential.daemon_epoch.as_bytes(),
    ) {
        anyhow::bail!("daemon transport acknowledgement has a stale epoch");
    }
    if !valid_random_identifier(&ack.connection_id) {
        anyhow::bail!("daemon transport acknowledgement has an invalid connection id");
    }
    if ack.accepted_features != ["json_rpc_2_0"] {
        anyhow::bail!("daemon transport acknowledgement has unsupported connection features");
    }
    Ok(ConnectionMetadata {
        protocol_version: ack.protocol_version,
        daemon_epoch: ack.daemon_epoch,
        connection_id: ack.connection_id,
        client_instance_nonce: client_instance_nonce.to_string(),
        client_kind,
    })
}

async fn load_credential_with_retry(listener_address: &str) -> Result<TransportCredential> {
    let mut last_error = None;
    for _ in 0..20 {
        match transport_credentials::load(listener_address) {
            Ok(credential) => return Ok(credential),
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("transport credential is unavailable")))
        .context("failed to load protected daemon transport credential")
}

fn valid_random_identifier(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> TransportCredential {
        TransportCredential {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            daemon_epoch: "11".repeat(32),
            transport_token: "22".repeat(32),
            listener_address: "127.0.0.1:1".into(),
            daemon_pid: 1,
        }
    }

    fn transport() -> ServerTransport {
        ServerTransport {
            issued: IssuedTransportCredential {
                credential: credential(),
                path: std::path::PathBuf::from("/nonexistent-test-credential"),
            },
        }
    }

    fn hello() -> ProxyHello {
        ProxyHello {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            daemon_epoch: "11".repeat(32),
            transport_token: "22".repeat(32),
            client_instance_nonce: "33".repeat(16),
            client_kind: ClientKind::Cli,
            request: ProxyRequest {
                workspace_roots: vec!["/secret/claimed/root".into()],
                focus_files: vec![],
                focus_dirs: vec![],
            },
        }
    }

    #[test]
    fn constant_time_comparison_handles_equal_and_different_lengths() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"same", b"same-longer"));
    }

    #[test]
    fn authentication_rejects_bad_auth_before_exposing_roots() {
        let transport = transport();
        let mut hello = hello();
        hello.transport_token = "wrong".into();
        let wire = serde_json::to_string(&hello).unwrap();
        let error = transport
            .authenticate("127.0.0.1:1234".parse().unwrap(), &wire)
            .unwrap_err();
        assert_eq!(error.to_string(), "transport authentication failed");
        assert!(!error.to_string().contains("secret"));
        assert!(!error.to_string().contains("wrong"));
    }

    #[test]
    fn authentication_rejects_malformed_stale_and_incompatible_hellos() {
        let transport = transport();
        assert_eq!(
            transport
                .authenticate("127.0.0.1:1234".parse().unwrap(), "not json")
                .unwrap_err()
                .to_string(),
            "transport hello is malformed"
        );

        let mut stale = hello();
        stale.daemon_epoch = "44".repeat(32);
        assert!(transport
            .authenticate(
                "127.0.0.1:1234".parse().unwrap(),
                &serde_json::to_string(&stale).unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("stale daemon epoch"));

        let mut incompatible = hello();
        incompatible.protocol_version += 1;
        assert!(transport
            .authenticate(
                "127.0.0.1:1234".parse().unwrap(),
                &serde_json::to_string(&incompatible).unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("matching binary"));
    }

    #[test]
    fn authentication_rejects_non_loopback_peers() {
        let error = transport()
            .authenticate(
                "192.0.2.1:1234".parse().unwrap(),
                &serde_json::to_string(&hello()).unwrap(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("non-loopback"));
    }
}
