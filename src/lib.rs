//! vpngate-client library: MS-SSTP + PPP tunnel to VPNGate volunteer
//! nodes, a userspace TCP stack over the tunnel, a local SOCKS5 server,
//! and the VPNGate node list. Shared by the CLI binary (`vpngate-client`)
//! and the desktop GUI binary (`vpngate-gui`).

pub mod config;
pub mod doh;
pub mod filelog;
pub mod net;
pub mod socks5;
pub mod sstp;
pub mod sysproxy;
pub mod tray;
pub mod util;
pub mod vpngate;
