//! Throwaway experiment for issue #954: how does a reqwest probe of
//! `http://localhost:<port>` behave on Windows when the listener is bound to
//! `127.0.0.1` only, alone and under a burst of fresh probe processes?
//!
//! Modes (argv[1]):
//!   (none)            – orchestrator: runs every measurement, prints a report
//!   probe <url> <ms>  – child: one GET with a whole-request timeout, prints a line
//!   hog <secs>        – child: burns CPU for <secs>

use std::error::Error as _;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(next) = cur {
        s.push_str(" -> ");
        s.push_str(&next.to_string());
        cur = next.source();
    }
    s
}

async fn serve_http(listener: TcpListener) {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut n = 0;
            loop {
                match sock.read(&mut buf[n..]).await {
                    Ok(0) => return,
                    Ok(k) => {
                        n += k;
                        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                        if n >= buf.len() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
            let body = r#"{"Value":[]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
    }
}

async fn probe_once(url: &str, timeout_ms: u64) -> (Duration, Result<u16, String>) {
    let client = reqwest::Client::builder().build().unwrap();
    let start = Instant::now();
    let res = client
        .get(url)
        .timeout(Duration::from_millis(timeout_ms))
        .send()
        .await;
    let elapsed = start.elapsed();
    match res {
        Ok(r) => (elapsed, Ok(r.status().as_u16())),
        Err(e) => (
            elapsed,
            Err(format!(
                "timeout={} connect={} chain=[{}]",
                e.is_timeout(),
                e.is_connect(),
                chain(&e)
            )),
        ),
    }
}

fn stats(label: &str, samples: &[Duration], errors: &[String]) {
    if samples.is_empty() {
        println!("  {label}: no samples, {} errors", errors.len());
    } else {
        let mut v: Vec<u128> = samples.iter().map(|d| d.as_millis()).collect();
        v.sort_unstable();
        let sum: u128 = v.iter().sum();
        let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        println!(
            "  {label}: n={} min={}ms p50={}ms p90={}ms max={}ms mean={}ms errors={}",
            v.len(),
            v[0],
            p(0.5),
            p(0.9),
            v[v.len() - 1],
            sum / v.len() as u128,
            errors.len()
        );
    }
    for e in errors.iter().take(8) {
        println!("    err: {e}");
    }
}

async fn child_burst(exe: &std::path::Path, url: &str, children: usize, hogs: usize, timeout_ms: u64) {
    let mut hog_handles = Vec::new();
    for _ in 0..hogs {
        let h = tokio::process::Command::new(exe)
            .arg("hog")
            .arg("60")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn hog");
        hog_handles.push(h);
    }
    let start = Instant::now();
    let mut joins = Vec::new();
    for _ in 0..children {
        let exe = exe.to_path_buf();
        let url = url.to_string();
        joins.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let out = tokio::process::Command::new(&exe)
                .arg("probe")
                .arg(&url)
                .arg(timeout_ms.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .expect("spawn probe");
            (t0.elapsed(), String::from_utf8_lossy(&out.stdout).trim().to_string())
        }));
    }
    let mut samples = Vec::new();
    let mut errors = Vec::new();
    let mut spawn_to_exit = Vec::new();
    for j in joins {
        let (wall, line) = j.await.unwrap();
        spawn_to_exit.push(wall);
        // line: "elapsed_ms=<n> ok=<status>" or "elapsed_ms=<n> err=<...>"
        let mut ms = 0u64;
        let mut err = None;
        for part in line.splitn(2, ' ') {
            if let Some(v) = part.strip_prefix("elapsed_ms=") {
                ms = v.parse().unwrap_or(0);
            } else if let Some(v) = part.strip_prefix("err=") {
                err = Some(v.to_string());
            }
        }
        match err {
            None => samples.push(Duration::from_millis(ms)),
            Some(e) => errors.push(format!("after {ms}ms: {e}")),
        }
    }
    println!(
        "  burst children={children} hogs={hogs} url={url} total_wall={}ms",
        start.elapsed().as_millis()
    );
    stats("in-child request time", &samples, &errors);
    stats("spawn-to-exit wall", &spawn_to_exit, &[]);
    for mut h in hog_handles {
        let _ = h.kill().await;
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("probe") => {
            let url = &args[2];
            let ms: u64 = args[3].parse().unwrap();
            let (elapsed, res) = probe_once(url, ms).await;
            match res {
                Ok(status) => println!("elapsed_ms={} ok={status}", elapsed.as_millis()),
                Err(e) => println!("elapsed_ms={} err={e}", elapsed.as_millis()),
            }
            return;
        }
        Some("hog") => {
            let secs: u64 = args[2].parse().unwrap();
            let end = Instant::now() + Duration::from_secs(secs);
            let mut x = 0u64;
            while Instant::now() < end {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(x);
            }
            return;
        }
        _ => {}
    }

    let exe = std::env::current_exe().unwrap();
    println!("== localhost probe experiment on {} ({} cpus)", std::env::consts::OS, std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0));

    // 1. name resolution
    for i in 0..3 {
        let t = Instant::now();
        let addrs: Vec<_> = tokio::net::lookup_host("localhost:80").await.unwrap().collect();
        println!("  lookup_host(localhost) #{i}: {:?} in {}ms", addrs, t.elapsed().as_millis());
    }

    // 2. listener bound to 127.0.0.1 only
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_http(listener));
    println!("  listener on 127.0.0.1:{port}");

    // 3. connect timing to the unlistened v6 side and to port 1
    for target in [format!("[::1]:{port}"), "[::1]:1".to_string(), "127.0.0.1:1".to_string()] {
        let mut ds = Vec::new();
        let mut last = String::new();
        for _ in 0..5 {
            let t = Instant::now();
            let r = tokio::net::TcpStream::connect(&target).await;
            ds.push(t.elapsed());
            if let Err(e) = r {
                last = e.to_string();
            } else {
                last = "CONNECTED?!".to_string();
            }
        }
        println!("  connect {target}: {:?} last={last}", ds.iter().map(|d| d.as_millis()).collect::<Vec<_>>());
    }

    // 4. in-process sequential probes
    for host in ["localhost", "127.0.0.1"] {
        let url = format!("http://{host}:{port}/management/v1/configureddevices");
        let mut samples = Vec::new();
        let mut errors = Vec::new();
        for _ in 0..20 {
            let (d, r) = probe_once(&url, 5000).await;
            match r {
                Ok(_) => samples.push(d),
                Err(e) => errors.push(e),
            }
        }
        stats(&format!("in-process sequential {url}"), &samples, &errors);
    }

    // 5. child bursts: fresh process per probe, with and without CPU hogs
    for hogs in [0usize, 8, 32] {
        for host in ["localhost", "127.0.0.1"] {
            let url = format!("http://{host}:{port}/management/v1/configureddevices");
            for _round in 0..2 {
                child_burst(&exe, &url, 64, hogs, 5000).await;
            }
        }
    }
    println!("== done");
}
