// Embedded TURN relay for clients that can't establish direct/srflx ICE
// connectivity - e.g. browsers' mDNS-obfuscated host candidates can't be
// resolved across a netbird (WireGuard) tunnel, since mDNS needs multicast.

use anyhow::{Context, Result};
use rand::distributions::Alphanumeric;
use rand::Rng;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::time::Duration;
use turn::auth::{generate_auth_key, AuthHandler};
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::server::config::{ConnConfig, ServerConfig};
use turn::server::Server;
use turn::Error as TurnError;
use webrtc_util::vnet::net::Net;

const REALM: &str = "waylandwebstream";

/// STUN + TURN server list, and the TURN credentials, shared between the
/// Rust-side `RTCConfiguration` and the `/ice-config` endpoint served to the
/// browser client.
#[derive(Clone)]
pub struct IceServerConfig {
    pub stun_url: String,
    pub turn_url: String,
    pub turn_username: String,
    pub turn_password: String,
}

/// Pick the IP address to advertise as the TURN relay address. Prefers an
/// address in netbird's CGNAT range (100.64.0.0/10), since that's the
/// address reachable from peers connected only via the netbird mesh; falls
/// back to the first non-loopback IPv4 address otherwise.
pub fn detect_relay_address() -> Option<IpAddr> {
    let ifaces = local_ip_address::list_afinet_netifas().ok()?;

    let is_netbird_cgnat = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        }
        IpAddr::V6(_) => false,
    };

    ifaces
        .iter()
        .find(|(_, ip)| !ip.is_loopback() && is_netbird_cgnat(ip))
        .or_else(|| {
            ifaces
                .iter()
                .find(|(_, ip)| !ip.is_loopback() && matches!(ip, IpAddr::V4(_)))
        })
        .map(|(_, ip)| *ip)
}

/// Credentials handed out to clients for the embedded TURN server.
#[derive(Clone)]
pub struct TurnCredentials {
    pub username: String,
    pub password: String,
}

impl TurnCredentials {
    /// Generate a fresh random username/password pair for this run.
    pub fn generate() -> Self {
        let random_string = |len: usize| -> String {
            let mut rng = rand::thread_rng();
            (0..len).map(|_| rng.sample(Alphanumeric) as char).collect()
        };
        Self {
            username: random_string(12),
            password: random_string(24),
        }
    }
}

struct StaticAuthHandler {
    username: String,
    key: Vec<u8>,
}

impl AuthHandler for StaticAuthHandler {
    fn auth_handle(
        &self,
        username: &str,
        _realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, TurnError> {
        if username == self.username {
            Ok(self.key.clone())
        } else {
            Err(TurnError::ErrFakeErr)
        }
    }
}

/// Start the embedded TURN server, bound to `port` on all interfaces and
/// advertising `relay_address` as the address clients should send relayed
/// media to (this must be an address the browser can actually reach, e.g.
/// the host's netbird IP).
pub async fn spawn_turn_server(
    port: u16,
    relay_address: IpAddr,
    credentials: &TurnCredentials,
) -> Result<Server> {
    let conn = Arc::new(
        UdpSocket::bind(format!("0.0.0.0:{port}"))
            .await
            .context("Failed to bind TURN UDP listener")?,
    );

    let key = generate_auth_key(&credentials.username, REALM, &credentials.password);

    let server = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn,
            relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                relay_address,
                address: "0.0.0.0".to_owned(),
                net: Arc::new(Net::new(None)),
            }),
        }],
        realm: REALM.to_owned(),
        auth_handler: Arc::new(StaticAuthHandler {
            username: credentials.username.clone(),
            key,
        }),
        channel_bind_timeout: Duration::from_secs(0),
        alloc_close_notify: None,
    })
    .await
    .context("Failed to start embedded TURN server")?;

    Ok(server)
}
