//! Bounded OS name resolution. A stuck getaddrinfo must not hold the caller past
//! its deadline or create a new leaked thread on every reconnect attempt.
use anyhow::{Context, Result, bail};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Instant;

struct Lookup {
    host: String,
    port: u16,
    deadline: Instant,
    reply: mpsc::Sender<std::io::Result<Vec<SocketAddr>>>,
}

struct Resolver(mpsc::SyncSender<Lookup>);

impl Resolver {
    fn start(
        resolve: impl Fn(&str, u16) -> std::io::Result<Vec<SocketAddr>> + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Lookup>(4);
        let rx = Arc::new(Mutex::new(rx));
        let resolve = Arc::new(resolve);
        for _ in 0..2 {
            let (rx, resolve) = (rx.clone(), resolve.clone());
            std::thread::Builder::new()
                .name("tm-dns".into())
                .spawn(move || {
                    loop {
                        let request = rx.lock().unwrap().recv();
                        let Ok(request) = request else {
                            return;
                        };
                        if Instant::now() >= request.deadline {
                            continue;
                        }
                        let result = resolve(&request.host, request.port);
                        let _ = request.reply.send(result);
                    }
                })?;
        }
        Ok(Self(tx))
    }

    fn lookup(&self, host: &str, port: u16, deadline: Instant) -> Result<Vec<SocketAddr>> {
        let (reply, rx) = mpsc::channel();
        self.0
            .try_send(Lookup {
                host: host.to_owned(),
                port,
                deadline,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("DNS resolver busy or unavailable"))?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        rx.recv_timeout(remaining)
            .context("pool DNS deadline exceeded")?
            .context("resolve pool host")
    }
}

pub(crate) fn resolve(host: &str, port: u16, deadline: Instant) -> Result<Vec<SocketAddr>> {
    if Instant::now() >= deadline {
        bail!("pool connect deadline exceeded");
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    static RESOLVER: OnceLock<Result<Resolver, String>> = OnceLock::new();
    let resolver = RESOLVER
        .get_or_init(|| {
            Resolver::start(|host, port| (host, port).to_socket_addrs().map(Iterator::collect))
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("start DNS resolver: {e}"))?;
    resolver.lookup(host, port, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn slow_lookup_cannot_exceed_callers_deadline() {
        let resolver = Resolver::start(|_, port| {
            std::thread::sleep(Duration::from_millis(300));
            Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
        })
        .unwrap();
        let started = Instant::now();
        assert!(
            resolver
                .lookup("test.invalid", 1, started + Duration::from_millis(30))
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_millis(250));
    }
}
