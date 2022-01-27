//! A simple benchmarking utility for Arti.
//!
//! This works by establishing a simple TCP server, and having Arti connect back to it via
//! a `chutney` network of Tor nodes, benchmarking the upload and download bandwidth while doing so.

#![deny(missing_docs)]
#![warn(noop_method_call)]
#![deny(unreachable_pub)]
#![deny(clippy::all)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::cargo_common_metadata)]
#![deny(clippy::cast_lossless)]
#![deny(clippy::checked_conversions)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::cognitive_complexity)]
#![deny(clippy::debug_assert_with_mut_call)]
#![deny(clippy::exhaustive_enums)]
#![deny(clippy::exhaustive_structs)]
#![deny(clippy::expl_impl_clone_on_copy)]
#![deny(clippy::fallible_impl_from)]
#![deny(clippy::implicit_clone)]
#![deny(clippy::large_stack_arrays)]
#![warn(clippy::manual_ok_or)]
#![deny(clippy::missing_docs_in_private_items)]
#![deny(clippy::missing_panics_doc)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_pass_by_value)]
#![warn(clippy::option_option)]
#![warn(clippy::rc_buffer)]
#![deny(clippy::ref_option_ref)]
#![warn(clippy::semicolon_if_nothing_returned)]
#![warn(clippy::trait_duplication_in_bounds)]
#![deny(clippy::unnecessary_wraps)]
#![warn(clippy::unseparated_literal_suffix)]
// This file uses `unwrap()` a fair deal, but this is fine in test/bench code
// because it's OK if tests and benchmarks simply crash if things go wrong.
#![allow(clippy::unwrap_used)]

use anyhow::{anyhow, Result};
use arti_client::{TorAddr, TorClient, TorClientConfig};
use arti_config::ArtiConfig;
use clap::{App, Arg};
use futures::stream::Stream;
use futures::StreamExt;
use rand::distributions::Standard;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fmt::Formatter;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::ops::Deref;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_socks::tcp::Socks5Stream;
use tor_rtcompat::Runtime;
use tracing::info;

/// Generate a random payload of bytes of the given size
fn random_payload(size: usize) -> Vec<u8> {
    rand::thread_rng()
        .sample_iter(Standard)
        .take(size)
        .collect()
}

/// Timing information from the benchmarking server.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ServerTiming {
    /// When the connection was accepted.
    accepted_ts: SystemTime,
    /// When the payload was successfully written to the client.
    copied_ts: SystemTime,
    /// When the server received the first byte from the client.
    first_byte_ts: SystemTime,
    /// When the server finished reading the client's payload.
    read_done_ts: SystemTime,
}

/// Timing information from the benchmarking client.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientTiming {
    /// When the client's connection succeeded.
    started_ts: SystemTime,
    /// When the client received the first byte from the server.
    first_byte_ts: SystemTime,
    /// When the client finsihed reading the server's payload.
    read_done_ts: SystemTime,
    /// When the payload was successfully written to the server.
    copied_ts: SystemTime,
    /// The server's copy of the timing information.
    server: ServerTiming,
    /// The size of the payload downloaded from the server.
    download_size: usize,
    /// The size of the payload uploaded to the server.
    upload_size: usize,
}

/// A summary of benchmarking results, generated from `ClientTiming`.
#[derive(Debug, Copy, Clone, Serialize)]
pub struct TimingSummary {
    /// The time to first byte (TTFB) for the download benchmark.
    download_ttfb_sec: f64,
    /// The average download speed, in megabits per second.
    download_rate_megabit: f64,
    /// The time to first byte (TTFB) for the upload benchmark.
    upload_ttfb_sec: f64,
    /// The average upload speed, in megabits per second.
    upload_rate_megabit: f64,
}

impl fmt::Display for TimingSummary {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:.2} Mbit/s up (ttfb {:.2}ms), {:.2} Mbit/s down (ttfb {:.2}ms)",
            self.upload_rate_megabit,
            self.upload_ttfb_sec * 1000.0,
            self.download_rate_megabit,
            self.download_ttfb_sec * 1000.0
        )
    }
}

impl TimingSummary {
    /// Generate a `TimingSummary` from the `ClientTiming` returned by a benchmark run.
    pub fn generate(ct: &ClientTiming) -> Result<Self> {
        let download_ttfb = ct.first_byte_ts.duration_since(ct.server.accepted_ts)?;
        let download_time = ct.read_done_ts.duration_since(ct.first_byte_ts)?;
        let download_rate_bps = ct.download_size as f64 / download_time.as_secs_f64();

        let upload_ttfb = ct.server.first_byte_ts.duration_since(ct.read_done_ts)?;
        let upload_time = ct
            .server
            .read_done_ts
            .duration_since(ct.server.first_byte_ts)?;
        let upload_rate_bps = ct.upload_size as f64 / upload_time.as_secs_f64();

        Ok(Self {
            download_ttfb_sec: download_ttfb.as_secs_f64(),
            download_rate_megabit: download_rate_bps / 125_000.0,
            upload_ttfb_sec: upload_ttfb.as_secs_f64(),
            upload_rate_megabit: upload_rate_bps / 125_000.0,
        })
    }
}

/// Run the timing routine
fn run_timing(mut stream: TcpStream, send: &Arc<[u8]>, receive: &Arc<[u8]>) -> Result<()> {
    let peer_addr = stream.peer_addr()?;
    // Do this potentially costly allocation before we do all the timing stuff.
    let mut received = vec![0_u8; receive.len()];

    info!("Accepted connection from {}", peer_addr);
    let accepted_ts = SystemTime::now();
    let mut data: &[u8] = send.deref();
    let copied = std::io::copy(&mut data, &mut stream)?;
    stream.flush()?;
    let copied_ts = SystemTime::now();
    assert_eq!(copied, send.len() as u64);
    info!("Copied {} bytes payload to {}.", copied, peer_addr);
    let read = stream.read(&mut received)?;
    if read == 0 {
        panic!("unexpected EOF");
    }
    let first_byte_ts = SystemTime::now();
    stream.read_exact(&mut received[read..])?;
    let read_done_ts = SystemTime::now();
    info!(
        "Received {} bytes payload from {}.",
        received.len(),
        peer_addr
    );
    // Check we actually got what we thought we would get.
    if received != receive.deref() {
        panic!("Received data doesn't match expected; potential corruption?");
    }
    let st = ServerTiming {
        accepted_ts,
        copied_ts,
        first_byte_ts,
        read_done_ts,
    };
    serde_json::to_writer(&mut stream, &st)?;
    info!("Wrote timing payload to {}.", peer_addr);
    Ok(())
}

/// Runs the benchmarking TCP server, using the provided TCP listener and set of payloads.
fn serve_payload(
    listener: &TcpListener,
    send: &Arc<[u8]>,
    receive: &Arc<[u8]>,
) -> Vec<JoinHandle<Result<()>>> {
    info!("Listening for clients...");

    listener
        .incoming()
        .into_iter()
        .map(|stream| {
            let send = Arc::clone(send);
            let receive = Arc::clone(receive);
            std::thread::spawn(move || run_timing(stream?, &send, &receive))
        })
        .collect()
}

/// Runs the benchmarking client on the provided socket.
async fn client<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: S,
    send: Arc<[u8]>,
    receive: Arc<[u8]>,
) -> Result<ClientTiming> {
    // Do this potentially costly allocation before we do all the timing stuff.
    let mut received = vec![0_u8; receive.len()];
    let started_ts = SystemTime::now();

    let read = socket.read(&mut received).await?;
    if read == 0 {
        anyhow!("unexpected EOF");
    }
    let first_byte_ts = SystemTime::now();
    socket.read_exact(&mut received[read..]).await?;
    let read_done_ts = SystemTime::now();
    info!("Received {} bytes payload.", received.len());
    let mut send_data = &send as &[u8];

    tokio::io::copy(&mut send_data, &mut socket).await?;
    socket.flush().await?;
    info!("Sent {} bytes payload.", send.len());
    let copied_ts = SystemTime::now();

    // Check we actually got what we thought we would get.
    if received != receive.deref() {
        panic!("Received data doesn't match expected; potential corruption?");
    }
    let mut json_buf = Vec::new();
    socket.read_to_end(&mut json_buf).await?;
    let server: ServerTiming = serde_json::from_slice(&json_buf)?;
    Ok(ClientTiming {
        started_ts,
        first_byte_ts,
        read_done_ts,
        copied_ts,
        server,
        download_size: receive.len(),
        upload_size: send.len(),
    })
}

#[allow(clippy::cognitive_complexity)]
fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let matches = App::new("arti-bench")
        .version(env!("CARGO_PKG_VERSION"))
        .author("The Tor Project Developers")
        .about("A simple benchmarking utility for Arti.")
        .arg(
            Arg::with_name("arti-config")
                .short("c")
                .long("arti-config")
                .takes_value(true)
                .required(true)
                .value_name("CONFIG")
                .help(
                    "Path to the Arti configuration to use (usually, a Chutney-generated config).",
                ),
        )
        .arg(
            Arg::with_name("num-samples")
                .short("s")
                .long("num-samples")
                .takes_value(true)
                .required(true)
                .value_name("COUNT")
                .default_value("3")
                .help("How many samples to take per benchmark run.")
        )
        .arg(
            Arg::with_name("num-parallel")
                .short("p")
                .long("num-parallel")
                .takes_value(true)
                .required(true)
                .value_name("COUNT")
                .default_value("3")
                .help("How many simultaneous streams per benchmark run.")
        )
        .arg(
            Arg::with_name("output")
                .short("o")
                .takes_value(true)
                .value_name("/path/to/output.json")
                .help("A path to write benchmark results to, in JSON format.")
        )
        .arg(
            Arg::with_name("download-bytes")
                .short("d")
                .long("download-bytes")
                .takes_value(true)
                .required(true)
                .value_name("SIZE")
                .default_value("10485760")
                .help("How much fake payload data to generate for the download benchmark."),
        )
        .arg(
            Arg::with_name("upload-bytes")
                .short("u")
                .long("upload-bytes")
                .takes_value(true)
                .required(true)
                .value_name("SIZE")
                .default_value("10485760")
                .help("How much fake payload data to generate for the upload benchmark."),
        )
        .arg(
            Arg::with_name("socks-proxy")
                .long("socks5")
                .takes_value(true)
                .value_name("addr:port")
                .help("SOCKS5 proxy address for a node to benchmark through as well (usually a Chutney node). Optional."),
        )
        .get_matches();
    info!("Parsing Arti configuration...");
    let config_files = matches
        .values_of_os("arti-config")
        .expect("no config files provided")
        .into_iter()
        .map(|x| (PathBuf::from(x), true))
        .collect::<Vec<_>>();
    let cfg = arti_config::load(&config_files, vec![])?;
    let config: ArtiConfig = cfg.try_into()?;
    let tcc = config.tor_client_config()?;
    info!("Binding local TCP listener...");
    let listener = TcpListener::bind("0.0.0.0:0")?;
    let local_addr = listener.local_addr()?;
    let connect_addr = SocketAddr::new(IpAddr::from_str("127.0.0.1").unwrap(), local_addr.port());
    info!("Bound to {}.", local_addr);
    let upload_bytes = matches.value_of("upload-bytes").unwrap().parse::<usize>()?;
    let download_bytes = matches
        .value_of("download-bytes")
        .unwrap()
        .parse::<usize>()?;
    let samples = matches.value_of("num-samples").unwrap().parse::<usize>()?;
    let parallel = matches.value_of("num-parallel").unwrap().parse::<usize>()?;
    info!("Generating test payloads, please wait...");
    let upload_payload = random_payload(upload_bytes).into();
    let download_payload = random_payload(download_bytes).into();
    info!(
        "Generated payloads ({} upload, {} download)",
        upload_bytes, download_bytes
    );
    let up = Arc::clone(&upload_payload);
    let dp = Arc::clone(&download_payload);
    let _handle = std::thread::spawn(move || -> Result<()> {
        serve_payload(&listener, &dp, &up)
            .into_iter()
            .try_for_each(|handle| handle.join().expect("failed to join thread"))
    });

    let mut benchmark = Benchmark {
        connect_addr,
        samples,
        concurrent: parallel,
        upload_payload,
        download_payload,
        runtime: tor_rtcompat::tokio::TokioNativeTlsRuntime::create()?,
        results: Default::default(),
    };

    benchmark.without_arti()?;
    if let Some(addr) = matches.value_of("socks-proxy") {
        benchmark.with_proxy(addr)?;
    }
    benchmark.with_arti(tcc)?;

    info!("Benchmarking complete.");

    for (ty, results) in benchmark.results.iter() {
        info!(
            "Information for benchmark type {:?} ({} samples taken):",
            ty, benchmark.samples
        );
        info!("median: {}", results.results_median);
        info!("  mean: {}", results.results_mean);
        info!(" worst: {}", results.results_worst);
        info!("  best: {}", results.results_best);
    }

    if let Some(output) = matches.value_of("output") {
        info!("Writing benchmark results to {}...", output);
        let file = std::fs::File::create(output)?;
        serde_json::to_writer(
            &file,
            &BenchmarkSummary {
                crate_version: env!("CARGO_PKG_VERSION").to_string(),
                results: benchmark.results,
            },
        )?;
    }

    Ok(())
}

/// A helper struct for running benchmarks
#[allow(clippy::missing_docs_in_private_items)]
struct Benchmark<R>
where
    R: Runtime,
{
    runtime: R,
    connect_addr: SocketAddr,
    samples: usize,
    concurrent: usize,
    upload_payload: Arc<[u8]>,
    download_payload: Arc<[u8]>,
    /// All benchmark results conducted, indexed by benchmark type.
    results: HashMap<BenchmarkType, BenchmarkResults>,
}

/// The type of benchmark conducted.
#[derive(Clone, Copy, Serialize, Deserialize, Hash, Debug, PartialEq, Eq)]
enum BenchmarkType {
    /// Use the benchmark server on its own, without using any proxy.
    ///
    /// This is useful to get an idea of how well the benchmarking utility performs on its own.
    RawLoopback,
    /// Benchmark via a SOCKS5 proxy (usually that of a chutney node).
    Socks,
    /// Benchmark via Arti.
    Arti,
}

/// A set of benchmark results for a given `BenchmarkType`, including information about averages.
#[derive(Clone, Serialize, Debug)]
struct BenchmarkResults {
    /// The type of benchmark conducted.
    ty: BenchmarkType,
    /// The number of times the benchmark was run.
    samples: usize,
    /// The number of concurrent connections used during the run.
    connections: usize,
    /// The mean average of all metrics throughout all benchmark runs.
    results_mean: TimingSummary,
    /// The "low-median" average of all metrics throughout all benchmark runs.
    ///
    /// # Important note
    ///
    /// This is only the median if `samples` is an odd number, else it is the
    /// `samples / 2`th sample of each set of metrics in sorted order.
    results_median: TimingSummary,
    /// The best value recorded for each metric throughout all benchmark runs.
    results_best: TimingSummary,
    /// The worst value recorded for each metric throughout all benchmark runs.
    results_worst: TimingSummary,
    /// The raw benchmark results.
    results_raw: Vec<TimingSummary>,
}

impl BenchmarkResults {
    /// Generate summarized benchmark results from raw run data.
    fn generate(ty: BenchmarkType, connections: usize, raw: Vec<TimingSummary>) -> Self {
        let mut download_ttfb_secs = raw.iter().map(|s| s.download_ttfb_sec).collect::<Vec<_>>();
        float_ord::sort(&mut download_ttfb_secs);
        let mut download_rate_megabits = raw
            .iter()
            .map(|s| s.download_rate_megabit)
            .collect::<Vec<_>>();
        float_ord::sort(&mut download_rate_megabits);
        let mut upload_ttfb_secs = raw.iter().map(|s| s.upload_ttfb_sec).collect::<Vec<_>>();
        float_ord::sort(&mut upload_ttfb_secs);
        let mut upload_rate_megabits = raw
            .iter()
            .map(|s| s.upload_rate_megabit)
            .collect::<Vec<_>>();
        float_ord::sort(&mut upload_rate_megabits);
        let samples = raw.len();
        BenchmarkResults {
            ty,
            samples,
            connections,
            results_mean: TimingSummary {
                download_ttfb_sec: download_ttfb_secs.iter().sum::<f64>() / samples as f64,
                download_rate_megabit: download_rate_megabits.iter().sum::<f64>() / samples as f64,
                upload_ttfb_sec: upload_ttfb_secs.iter().sum::<f64>() / samples as f64,
                upload_rate_megabit: upload_rate_megabits.iter().sum::<f64>() / samples as f64,
            },
            results_median: TimingSummary {
                download_ttfb_sec: download_ttfb_secs[samples / 2],
                download_rate_megabit: download_rate_megabits[samples / 2],
                upload_ttfb_sec: upload_ttfb_secs[samples / 2],
                upload_rate_megabit: upload_rate_megabits[samples / 2],
            },
            results_best: TimingSummary {
                download_ttfb_sec: download_ttfb_secs[0],
                download_rate_megabit: download_rate_megabits[samples - 1],
                upload_ttfb_sec: upload_ttfb_secs[0],
                upload_rate_megabit: upload_rate_megabits[samples - 1],
            },
            results_worst: TimingSummary {
                download_ttfb_sec: download_ttfb_secs[samples - 1],
                download_rate_megabit: download_rate_megabits[0],
                upload_ttfb_sec: upload_ttfb_secs[samples - 1],
                upload_rate_megabit: upload_rate_megabits[0],
            },
            results_raw: raw,
        }
    }
}

/// A summary of all benchmarks conducted throughout the invocation of `arti-bench`.
///
/// Designed to be stored as an artifact and compared against other later runs.
#[derive(Clone, Serialize, Debug)]
struct BenchmarkSummary {
    /// The version of `arti-bench` used to generate the benchmark results.
    crate_version: String,
    /// All benchmark results conducted, indexed by benchmark type.
    results: HashMap<BenchmarkType, BenchmarkResults>,
}

impl<R: Runtime> Benchmark<R> {
    /// Run a type of benchmark (`ty`), performing `self.samples` benchmark runs, and using
    /// `self.concurrent` concurrent connections.
    ///
    /// Uses `stream_generator`, a stream that generates futures that themselves generate streams,
    /// in order to obtain the required number of streams to run the test over.
    fn run<F, G, S, E>(&mut self, ty: BenchmarkType, stream_generator: F) -> Result<()>
    where
        F: Stream<Item = G> + Unpin,
        G: Future<Output = Result<S, E>>,
        S: AsyncRead + AsyncWrite + Unpin,
        E: std::error::Error + Send + Sync + 'static,
    {
        let mut results = vec![];
        // NOTE(eta): This could make more streams than we need. We assume this is okay.
        let mut stream_generator = stream_generator
            .buffered(self.concurrent)
            .take(self.samples * self.concurrent);
        for n in 0..self.samples {
            let mut streams = vec![];
            for _ in 0..self.concurrent {
                let stream =
                    self.runtime
                        .block_on(stream_generator.next())
                        .ok_or_else(|| {
                            anyhow!(
                                "internal error: stream generator couldn't supply enough streams"
                            )
                        })??; // one ? for the error above, next ? for G's output
                streams.push(stream);
            }
            let futures = streams
                .into_iter()
                .map(|stream| {
                    let up = Arc::clone(&self.upload_payload);
                    let dp = Arc::clone(&self.download_payload);
                    Box::pin(async move { client(stream, up, dp).await })
                })
                .collect::<futures::stream::FuturesUnordered<_>>()
                .collect::<Vec<_>>();
            info!(
                "Benchmarking {:?} with {} connections, run {}/{}...",
                ty,
                self.concurrent,
                n + 1,
                self.samples
            );
            let stats = self
                .runtime
                .block_on(futures)
                .into_iter()
                .map(|x| x.and_then(|x| TimingSummary::generate(&x)))
                .collect::<Result<Vec<_>>>()?;
            results.extend(stats);
        }
        let results = BenchmarkResults::generate(ty, self.concurrent, results);
        self.results.insert(ty, results);
        Ok(())
    }

    /// Benchmark without Arti on loopback.
    fn without_arti(&mut self) -> Result<()> {
        let ca = self.connect_addr;
        self.run(
            BenchmarkType::RawLoopback,
            futures::stream::repeat_with(|| tokio::net::TcpStream::connect(ca)),
        )
    }

    /// Benchmark through a SOCKS5 proxy at address `addr`.
    fn with_proxy(&mut self, addr: &str) -> Result<()> {
        let ca = self.connect_addr;
        self.run(
            BenchmarkType::Socks,
            futures::stream::repeat_with(|| Socks5Stream::connect(addr, ca)),
        )
    }

    /// Benchmark through Arti, using the provided `TorClientConfig`.
    fn with_arti(&mut self, tcc: TorClientConfig) -> Result<()> {
        info!("Starting Arti...");
        let tor_client = self
            .runtime
            .block_on(TorClient::bootstrap(self.runtime.clone(), tcc))?;

        let addr = TorAddr::dangerously_from(self.connect_addr)?;

        self.run(
            BenchmarkType::Arti,
            futures::stream::repeat_with(|| tor_client.connect(addr.clone())),
        )
    }
}