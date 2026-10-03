//! Stratum V1 endpoint handling and the bounded `probe` diagnostic. The mining
//! session itself lives in `miner.rs`.
use crate::transport::{self, Timeouts, TlsSettings};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};
use url::Url;

const MAX_LINE: usize = 1 << 20;
const MAX_MESSAGES: usize = 1024;
pub const AGENT: &str = concat!("tideminer/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub authorized: bool,
    pub extranonce2_bytes: usize,
    pub job_id: String,
    /// Difficulty effective when this notify arrived, not a later pending update.
    pub difficulty: f64,
    pub clean_jobs: bool,
}

pub(crate) fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "Stratum deadline exceeded");
    Ok(remaining)
}

fn send<S: Write + Timeouts>(stream: &mut S, message: Value, deadline: Instant) -> Result<()> {
    stream.set_write_timeout(Some(remaining(deadline)?))?;
    let mut bytes = serde_json::to_vec(&message)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).context("write Stratum request")
}

/// Limit before allocating the entire line, and refresh the deadline on every read.
fn receive<S: Read + Timeouts>(reader: &mut BufReader<S>, deadline: Instant) -> Result<Value> {
    let mut line = Vec::new();
    loop {
        reader
            .get_ref()
            .set_read_timeout(Some(remaining(deadline)?))?;
        let available = reader.fill_buf().context("read Stratum response")?;
        ensure!(
            !available.is_empty(),
            "pool closed the connection before probe completed"
        );
        let newline = available.iter().position(|&b| b == b'\n');
        let count = newline.map_or(available.len(), |i| i + 1);
        ensure!(
            line.len() + count <= MAX_LINE,
            "Stratum frame exceeds 1 MiB"
        );
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            let value: Value = serde_json::from_slice(&line).context("invalid Stratum JSON")?;
            ensure!(value.is_object(), "Stratum message must be an object");
            return Ok(value);
        }
    }
}

fn hex_field(value: &Value, bytes: Option<usize>) -> Result<()> {
    let text = value.as_str().context("expected hexadecimal string")?;
    ensure!(
        text.len() % 2 == 0 && text.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid hexadecimal field"
    );
    if let Some(bytes) = bytes {
        ensure!(text.len() == bytes * 2, "wrong hexadecimal field length");
    }
    Ok(())
}

fn validate_job(params: &Value, difficulty: f64) -> Result<(String, f64, bool)> {
    let fields = params
        .as_array()
        .context("notify params must be an array")?;
    ensure!(
        fields.len() == 9,
        "expected nine Tidecoin mining.notify fields"
    );
    let id = fields[0].as_str().context("job id must be a string")?;
    ensure!(id.len() <= 1024, "job id too long");
    hex_field(&fields[1], Some(32))?;
    hex_field(&fields[2], None)?;
    hex_field(&fields[3], None)?;
    let branches = fields[4]
        .as_array()
        .context("merkle branches must be an array")?;
    ensure!(branches.len() <= 32, "too many merkle branches");
    for branch in branches {
        hex_field(branch, Some(32))?;
    }
    for field in &fields[5..8] {
        hex_field(field, Some(4))?;
    }
    let clean = fields[8].as_bool().context("clean_jobs must be boolean")?;
    Ok((id.to_owned(), difficulty, clean))
}

/// A validated pool address: `stratum+tcp://` (plain) or `stratum+tls://`,
/// `stratum+ssl://`, `stratum+tcps://` (TLS). A bare `host:port`, as cpuminer
/// accepts, means plain TCP unless `--tls` is given.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl Endpoint {
    pub fn parse(endpoint: &str) -> Result<Self> {
        let text = if endpoint.contains("://") {
            endpoint.to_owned()
        } else {
            format!("stratum+tcp://{endpoint}")
        };
        let url = Url::parse(&text).context("invalid pool URL")?;
        let tls = match url.scheme() {
            "stratum+tcp" | "tcp" => false,
            "stratum+tls" | "stratum+ssl" | "stratum+tcps" | "tls" | "ssl" => true,
            other => {
                bail!("unsupported pool scheme {other:?}: use stratum+tcp:// or stratum+tls://")
            }
        };
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "provide credentials with -u/-p, not in the pool URL"
        );
        ensure!(
            (url.path().is_empty() || url.path() == "/")
                && url.query().is_none()
                && url.fragment().is_none(),
            "pool URL must contain only host and port"
        );
        let host = url.host_str().context("pool URL has no host")?;
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = url.port().context("pool URL must specify a port")?;
        let mut endpoint = Self {
            url: String::new(),
            host,
            port,
            tls,
        };
        endpoint.set_tls(tls);
        Ok(endpoint)
    }

    /// Force TLS on (cpuminer's `--tls`).
    pub fn set_tls(&mut self, tls: bool) {
        self.tls = tls;
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let scheme = if tls { "stratum+tls" } else { "stratum+tcp" };
        self.url = format!("{scheme}://{host}:{}", self.port);
    }

    /// Resolve and connect, trying each address until one answers before the deadline.
    pub fn connect(&self, deadline: Instant) -> Result<TcpStream> {
        let addresses = crate::resolver::resolve(&self.host, self.port, deadline)?;
        let mut last = None;
        for (index, address) in addresses.iter().enumerate() {
            // Give alternate addresses a chance within the overall deadline.
            let budget = remaining(deadline)? / (addresses.len() - index) as u32;
            match TcpStream::connect_timeout(address, budget) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    return Ok(stream);
                }
                Err(error) => last = Some(error),
            }
        }
        match last {
            Some(error) => Err(error).context("could not connect to pool"),
            None => bail!("pool host resolved to no addresses"),
        }
    }
}

pub fn probe(
    endpoint: &Endpoint,
    user: &str,
    password: &str,
    timeout: Duration,
    tls: &TlsSettings,
) -> Result<ProbeReport> {
    ensure!(
        !user.is_empty() && user.len() <= 4096 && password.len() <= 4096,
        "invalid credential length"
    );
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("invalid timeout")?;
    let (stream, _) = transport::open(endpoint, deadline, tls)?;
    session(stream, user, password, deadline)
}

fn session<S: Read + Write + Timeouts>(
    stream: S,
    user: &str,
    password: &str,
    deadline: Instant,
) -> Result<ProbeReport> {
    let mut reader = BufReader::new(stream);
    send(
        reader.get_mut(),
        json!({"id":1,"method":"mining.subscribe","params":[AGENT]}),
        deadline,
    )?;
    let mut subscribed = None;
    let mut authorized = false;
    let mut next_difficulty = 1.0;
    let mut job = None;
    for _ in 0..MAX_MESSAGES {
        let message = receive(&mut reader, deadline)?;
        if let Some(method) = message["method"].as_str() {
            let params = &message["params"];
            match method {
                "mining.set_difficulty" => {
                    let diff = params[0].as_f64().context("invalid pool difficulty")?;
                    ensure!(
                        diff.is_finite() && diff > 0.0,
                        "pool difficulty must be positive and finite"
                    );
                    next_difficulty = diff;
                }
                "mining.notify" => job = Some(validate_job(params, next_difficulty)?),
                "client.get_version" | "mining.ping" if !message["id"].is_null() => {
                    let result = if method == "mining.ping" {
                        "pong"
                    } else {
                        AGENT
                    };
                    send(
                        reader.get_mut(),
                        json!({"id":message["id"],"result":result,"error":null}),
                        deadline,
                    )?;
                }
                _ if !message["id"].is_null() => {
                    send(
                        reader.get_mut(),
                        json!({"id":message["id"],"result":null,
                        "error":[20,"unsupported method",null]}),
                        deadline,
                    )?;
                }
                _ => {}
            }
        } else if message["id"] == 1 {
            ensure!(subscribed.is_none(), "duplicate subscribe response");
            ensure!(message["error"].is_null(), "pool rejected subscription");
            let result = message["result"]
                .as_array()
                .context("invalid subscribe response")?;
            ensure!(
                result.len() == 3 && result[0].is_array(),
                "invalid subscription fields"
            );
            hex_field(&result[1], None)?;
            let size = result[2].as_u64().context("invalid extranonce2 size")?;
            ensure!((1..=32).contains(&size), "unsupported extranonce2 size");
            subscribed = Some(size as usize);
            send(
                reader.get_mut(),
                json!({"id":2,"method":"mining.authorize",
                "params":[user,password]}),
                deadline,
            )?;
        } else if message["id"] == 2 {
            ensure!(
                subscribed.is_some(),
                "authorization response before subscription"
            );
            ensure!(
                message["error"].is_null() && message["result"] == true,
                "pool rejected authorization"
            );
            authorized = true;
        }
        if let (Some(extranonce2_bytes), true, Some((job_id, difficulty, clean_jobs))) =
            (subscribed, authorized, job.as_ref())
        {
            return Ok(ProbeReport {
                authorized,
                extranonce2_bytes,
                job_id: job_id.clone(),
                difficulty: *difficulty,
                clean_jobs: *clean_jobs,
            });
        }
    }
    bail!("too many messages before probe completed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    fn notify() -> Value {
        json!({"id":null,"method":"mining.notify","params":["opaque/job-7", "00".repeat(32),
            "0102","0304",[],"20000000","1d022ab6","61ba60ad",true]})
    }

    #[test]
    fn handshake_handles_interleaving_and_snapshots_difficulty() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("stratum+tcp://{}", listener.local_addr().unwrap());
        let pool = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut reader = BufReader::new(listener.accept().unwrap().0);
            assert_eq!(
                receive(&mut reader, deadline).unwrap()["method"],
                "mining.subscribe"
            );
            // Exercise both fragmented frames and multiple frames in one read.
            let mut frames = serde_json::to_vec(
                &json!({"id":null,"method":"mining.set_difficulty","params":[0.25]}),
            )
            .unwrap();
            frames.push(b'\n');
            frames.extend(serde_json::to_vec(&notify()).unwrap());
            frames.push(b'\n');
            reader.get_mut().write_all(&frames[..13]).unwrap();
            reader.get_mut().write_all(&frames[13..]).unwrap();
            send(
                reader.get_mut(),
                json!({"id":1,"result":[[],"aabb",4],"error":null}),
                deadline,
            )
            .unwrap();
            let auth = receive(&mut reader, deadline).unwrap();
            assert_eq!(auth["method"], "mining.authorize");
            assert_eq!(auth["params"], json!(["worker", "test-password"]));
            send(
                reader.get_mut(),
                json!({"method":"mining.set_difficulty","params":[4.0]}),
                deadline,
            )
            .unwrap();
            send(
                reader.get_mut(),
                json!({"id":2,"result":true,"error":null}),
                deadline,
            )
            .unwrap();
        });
        let report = probe(
            &Endpoint::parse(&endpoint).unwrap(),
            "worker",
            "test-password",
            Duration::from_secs(5),
            &TlsSettings::default(),
        )
        .unwrap();
        pool.join().unwrap();
        assert!(report.authorized);
        assert_eq!(report.difficulty, 0.25);
        assert_eq!(report.extranonce2_bytes, 4);
        assert_eq!(report.job_id, "opaque/job-7");
    }

    #[test]
    fn endpoints_parse_like_cpuminer() {
        let e = Endpoint::parse("na.rplant.xyz:17059").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.tls),
            ("na.rplant.xyz", 17059, false)
        );
        assert_eq!(e.url, "stratum+tcp://na.rplant.xyz:17059");
        let mut e = e;
        e.set_tls(true);
        assert_eq!(e.url, "stratum+tls://na.rplant.xyz:17059");
        for tls_url in [
            "stratum+tls://h:1",
            "stratum+ssl://h:1",
            "stratum+tcps://h:1",
        ] {
            assert!(Endpoint::parse(tls_url).unwrap().tls, "{tls_url}");
        }
        assert!(!Endpoint::parse("stratum+tcp://127.0.0.1:3333").unwrap().tls);
        assert!(Endpoint::parse("http://h:1").is_err());
        assert!(Endpoint::parse("stratum+tcp://h").is_err());
        assert!(Endpoint::parse("stratum+tcp://u:p@h:1").is_err());
    }

    #[test]
    fn malformed_jobs_are_rejected() {
        let mut job = notify();
        job["params"][1] = json!("00");
        assert!(validate_job(&job["params"], 1.0).is_err());
        job = notify();
        job["params"][8] = json!(1);
        assert!(validate_job(&job["params"], 1.0).is_err());
    }

    #[test]
    fn oversized_frame_is_rejected_before_json_parsing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let pool = thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let _ = stream.write_all(&vec![b'x'; MAX_LINE + 1]);
        });
        let error = receive(
            &mut BufReader::new(stream),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap_err();
        pool.join().unwrap();
        assert_eq!(error.to_string(), "Stratum frame exceeds 1 MiB");
    }

    #[test]
    fn expired_deadline_fails_without_waiting_for_pool() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let _pool = listener.accept().unwrap();
        let error = receive(&mut BufReader::new(stream), Instant::now()).unwrap_err();
        assert_eq!(error.to_string(), "Stratum deadline exceeded");
    }

    #[test]
    fn authorization_failure_is_reported_without_server_text() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("stratum+tcp://{}", listener.local_addr().unwrap());
        let pool = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut reader = BufReader::new(listener.accept().unwrap().0);
            receive(&mut reader, deadline).unwrap();
            send(
                reader.get_mut(),
                json!({"id":1,"result":[[],"aa",4],"error":null}),
                deadline,
            )
            .unwrap();
            receive(&mut reader, deadline).unwrap();
            send(
                reader.get_mut(),
                json!({"id":2,"result":false,"error":[24,"secret",null]}),
                deadline,
            )
            .unwrap();
        });
        let error = probe(
            &Endpoint::parse(&endpoint).unwrap(),
            "worker",
            "secret",
            Duration::from_secs(5),
            &TlsSettings::default(),
        )
        .unwrap_err();
        pool.join().unwrap();
        assert_eq!(error.to_string(), "pool rejected authorization");
    }
}
