//! Loopback listeners for the stub endpoints the scenarios stand up.
//!
//! Every stub here is reached by *name*: doctor's active-service probe
//! dials `localhost` on a self-signed install (`aggregate::probe_host`),
//! and so do the steps that check an issued certificate over HTTPS. A
//! listener bound only to `127.0.0.1` does not own that name — see
//! [`bind_loopback_pair`].

use tokio::net::TcpListener;

/// How many ports [`bind_loopback_pair`] draws before giving up. Only a
/// port whose IPv6 half is already spoken for costs a try, so the ceiling
/// is never approached.
const BIND_TRIES: usize = 16;

/// Bind one loopback port in **both** address families — `127.0.0.1` and
/// `[::1]`, the same port number — and return it with its listeners, the
/// IPv4 one first.
///
/// IPv4 and IPv6 are separate port spaces, so binding `127.0.0.1:0`
/// leaves `[::1]:<port>` free for any other process in the run to take.
/// `localhost` resolves to `::1` first on every host the suite runs on,
/// so a stub owning only the IPv4 half can be answered *for* by whatever
/// holds the IPv6 half: the client never falls back, and the scenario
/// asserts against a stranger's reply — a non-HTTP one surfacing as a
/// transport error, an HTTP one as the wrong status. Owning both halves
/// makes the stub the only thing `localhost:<port>` can reach. Note that
/// `rusty_photon_tls::server::bind_dual_stack` does not do this: it
/// widens an IPv6 bind and returns a plain IPv4 socket for an IPv4
/// address.
///
/// Rejected IPv4 listeners are held until a pair lands, so a retry is
/// handed a fresh port instead of the one just freed.
///
/// # Panics
///
/// Panics when no port is free in both families within [`BIND_TRIES`]
/// draws.
pub async fn bind_loopback_pair() -> (u16, Vec<TcpListener>) {
    let mut rejected: Vec<(u16, TcpListener)> = Vec::new();
    for _ in 0..BIND_TRIES {
        let v4 = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("stub endpoint bind");
        let port = v4.local_addr().expect("stub addr").port();
        match TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).await {
            Ok(v6) => return (port, vec![v4, v6]),
            // Exactly the squatter the pairing exists to shut out. Draw
            // another port rather than serve one we only half own — and
            // keep this listener, so the kernel cannot hand the port
            // straight back.
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => rejected.push((port, v4)),
            // No IPv6 on this host: nothing else can hold `[::1]` either,
            // and the probe's `localhost` connect falls straight back to
            // the IPv4 half.
            Err(_) => return (port, vec![v4]),
        }
    }
    let taken: Vec<u16> = rejected.iter().map(|(port, _)| *port).collect();
    panic!(
        "no loopback port free in both address families in {BIND_TRIES} draws — \
         [::1] was already held at ports {taken:?}"
    );
}
