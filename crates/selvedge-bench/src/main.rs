//! A closed-loop HTTP/1.1 load generator: `-c` clients, each on its own
//! keep-alive connection, send GETs back to back for `-d` seconds and record
//! every latency. Prints one JSON object. Used by `test/bench/bench.sh`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use clap::Parser;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;

#[derive(Parser)]
struct Args {
    /// `http://host:port/path` to request.
    url: http::Uri,
    /// Host header to send (the Worker's hostname).
    #[arg(long)]
    host: Option<String>,
    /// Concurrent clients.
    #[arg(short, long, default_value_t = 16)]
    concurrency: usize,
    /// Seconds to measure.
    #[arg(short, long, default_value_t = 10)]
    duration: u64,
    /// Seconds to run before measuring.
    #[arg(long, default_value_t = 2)]
    warmup: u64,
    /// A name for this run in the output.
    #[arg(long, default_value = "")]
    label: String,
}

#[derive(Default)]
struct Tally {
    latencies_us: Vec<u64>,
    errors: u64,
    non_2xx: u64,
}

async fn client(args: &Args, warm: Instant, end: Instant) -> Tally {
    let mut tally = Tally::default();
    let authority = args.url.authority().expect("checked in main").as_str().to_owned();
    let host = args.host.clone().unwrap_or_else(|| authority.clone());
    let path = args.url.path_and_query().map_or("/", |p| p.as_str()).to_owned();
    let mut sender = None;
    while Instant::now() < end {
        if sender.is_none() {
            match tokio::net::TcpStream::connect(&authority).await {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    match hyper::client::conn::http1::handshake(TokioIo::new(s)).await {
                        Ok((s, conn)) => {
                            tokio::spawn(conn);
                            sender = Some(s);
                        }
                        Err(_) => {
                            tally.errors += 1;
                            continue;
                        }
                    }
                }
                Err(_) => {
                    tally.errors += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            }
        }
        let s: &mut hyper::client::conn::http1::SendRequest<Empty<Bytes>> =
            sender.as_mut().expect("set above");
        let req = http::Request::get(path.as_str())
            .header(http::header::HOST, host.as_str())
            .body(Empty::<Bytes>::new())
            .expect("valid request");
        let t0 = Instant::now();
        let ok = match s.send_request(req).await {
            Ok(resp) => {
                let status = resp.status();
                match resp.into_body().collect().await {
                    Ok(_) => Some(status.is_success()),
                    Err(_) => None,
                }
            }
            Err(_) => None,
        };
        let took = t0.elapsed();
        if t0 < warm {
            if ok.is_none() {
                sender = None;
            }
            continue;
        }
        match ok {
            Some(true) => tally.latencies_us.push(took.as_micros() as u64),
            Some(false) => {
                tally.latencies_us.push(took.as_micros() as u64);
                tally.non_2xx += 1;
            }
            None => {
                tally.errors += 1;
                sender = None;
            }
        }
    }
    tally
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i] as f64 / 1000.0
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::sync::Arc::new(Args::parse());
    args.url.authority().context("the URL needs a host and port")?;
    let start = Instant::now();
    let warm = start + Duration::from_secs(args.warmup);
    let end = warm + Duration::from_secs(args.duration);
    let tasks: Vec<_> = (0..args.concurrency.max(1))
        .map(|_| {
            let args = args.clone();
            tokio::spawn(async move { client(&args, warm, end).await })
        })
        .collect();
    let mut all = Tally::default();
    for t in tasks {
        let t = t.await?;
        all.latencies_us.extend(t.latencies_us);
        all.errors += t.errors;
        all.non_2xx += t.non_2xx;
    }
    all.latencies_us.sort_unstable();
    let n = all.latencies_us.len();
    let secs = args.duration as f64;
    let out = serde_json::json!({
        "label": args.label,
        "concurrency": args.concurrency,
        "seconds": args.duration,
        "requests": n,
        "rps": (n as f64 / secs * 10.0).round() / 10.0,
        "errors": all.errors,
        "non_2xx": all.non_2xx,
        "p50_ms": percentile(&all.latencies_us, 0.50),
        "p90_ms": percentile(&all.latencies_us, 0.90),
        "p99_ms": percentile(&all.latencies_us, 0.99),
        "max_ms": percentile(&all.latencies_us, 1.0),
    });
    println!("{out}");
    Ok(())
}
