use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering::Relaxed},
};
use std::time::{Duration, Instant};

#[test]
fn unread_stdout_does_not_prevent_mining_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut socket = loop {
            if server_stop.load(Relaxed) || Instant::now() >= deadline {
                return;
            }
            if let Ok((socket, _)) = listener.accept() {
                break socket;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        for _ in 0..2 {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                return;
            }
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let result = if request["method"] == "mining.subscribe" {
                serde_json::json!([[], "aabbccdd", 4])
            } else {
                serde_json::json!(true)
            };
            if writeln!(
                socket,
                "{}",
                serde_json::json!({"id":request["id"],"result":result,"error":null})
            )
            .is_err()
            {
                return;
            }
        }
        // Protocol logging produces far more output than a pipe can hold.
        let message = format!(
            "{{\"method\":\"test.notice\",\"params\":[\"{}\"]}}\n",
            "x".repeat(4096)
        );
        while !server_stop.load(Relaxed) && Instant::now() < deadline {
            if socket.write_all(message.as_bytes()).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_tideminer"))
        .args([
            "-o",
            &address.to_string(),
            "-u",
            "test",
            "-t",
            "1",
            "--lanes",
            "1",
            "--time-limit",
            "2",
            "-P",
            "--no-color",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Intentionally keep stdout open without reading any bytes.
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    stop.store(true, Relaxed);
    server.join().unwrap();
    assert!(
        status.is_some_and(|s| s.success()),
        "blocked stdout must not stall the coordinator or process exit"
    );
}

#[cfg(feature = "tls")]
#[test]
fn trickling_tls_handshake_has_an_overall_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = tideminer::stratum::Endpoint::parse(&format!(
        "stratum+tls://{}",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        // A TLS handshake record arriving too slowly to finish by the deadline.
        let _ = socket.write_all(&[22, 3, 3, 0, 128]);
        for _ in 0..64 {
            if socket.write_all(&[0]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let started = Instant::now();
    let result = tideminer::transport::open(
        &endpoint,
        started + Duration::from_millis(150),
        &Default::default(),
    );
    let elapsed = started.elapsed();
    server.join().unwrap();
    assert!(result.is_err());
    assert!(
        elapsed >= Duration::from_millis(100) && elapsed < Duration::from_millis(600),
        "elapsed {elapsed:?}"
    );
}
