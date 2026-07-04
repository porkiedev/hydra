use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;
use poll_promise::Promise;
use tokio::net::TcpStream;
use tracing::{debug, error, info, trace, warn};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::RwLock;

/// A rough estimate of how long it takes for mercury to send a PENDING response to a CQFRAME request
const CQ_DURATION_MILLIS: u128 = 2500;

/// A handle to interface with the mercury modem
///
/// This is also an async abstraction so I can call functions directly from the GUI without blocking
#[derive(Debug)]
pub struct Modem {
    state: Arc<RwLock<State>>,
}
impl Default for Modem {
    fn default() -> Self {
        Self {
            state: Default::default(),
        }
    }
}
impl Modem {
    /// Whether we're connected to mercury or not
    pub fn is_mercury_connected(&self) -> bool { self.state.blocking_read().tcp_connection.is_some() }
    /// The station we're connected to, if any
    pub fn get_connected_station(&self) -> Option<OpenConnection> { self.state.blocking_read().connection.clone() }
    /// Whether we're on cooldown from calling CQ
    pub fn is_cq_on_cooldown(&self) -> bool { Instant::now().duration_since(self.state.blocking_read().last_cq).as_millis() < CQ_DURATION_MILLIS }
    /// Whether PTT is enabled according to mercury
    pub fn get_ptt_state(&self) -> bool { self.state.blocking_read().ptt }
    /// Returns the number of bytes remaining in the TX buffer
    pub fn get_tx_buffer_len(&self) -> usize {
        self.state.blocking_read().buffer_len
    }
    /// Returns the latest measured SNR
    pub fn get_snr(&self) -> f32 { self.state.blocking_read().snr }
    /// Returns the current mode
    pub fn get_mode(&self) -> String { self.state.blocking_read().bitrate.0.clone() }
    /// Returns the current bitrate relative to the current mode
    pub fn get_bitrate(&self) -> usize { self.state.blocking_read().bitrate.1 }
    /// Attempt to connect to mercury
    ///
    /// - `destination` is the ip:port of mercury
    /// - `mycall` the callsign of this station
    /// - `listen` whether to listen to incoming ARQ connections
    pub fn connect_mercury(
        &mut self,
        destination: String,
        mycall: String,
        listen: bool,
        bandwidth: Bandwidth
    ) {
        let s = self.state.clone();
        let p: Promise<Result<()>> = Promise::spawn_async(async move {

            info!("Connecting to mercury");
            let mut stream = TcpStream::connect(destination).await?;
            stream.set_nodelay(true)?; // Don't delay
            info!("Connected to mercury successfully");

            // Split the stream into a read half and a write half
            let (mut read, mut write) = stream.into_split();

            // Set callsign
            write.write_all(format!("MYCALL {mycall}\r").as_bytes()).await?;
            // Set listening state
            let l = match listen {
                true => "ON",
                false => "OFF"
            };
            write.write_all(format!("LISTEN {l}\r").as_bytes()).await?;
            // Set bandwidth
            write.write_all(format!("{}\r", bandwidth.as_bw()).as_bytes()).await?;

            // Save write half of the stream for later use
            s.write().await.tcp_connection = Some(write);

            // Spawn the arq reader
            Promise::spawn_async(Self::mercury_control_listener(s, read));

            Ok(())
        });
    }
    /// Disconnect from mercury
    pub fn disconnect_mercury(&mut self) {
        info!("Disconnecting from mercury...");
        let s = self.state.clone();
        Promise::spawn_async(async move {
            s.write().await.tcp_connection = None;
            s.write().await.connection = None;
        });
    }
    /// Send disconnect request to remote station
    pub fn disconnect(&mut self) {
        info!("Sending disconnect request to remote station");
        self.write_to_control("DISCONNECT".into());
    }
    /// Send ARQ connect request to remote station
    /// - `source` is the initiating callsign, i.e. the current station
    /// - `destination` is the destination callsign that you're trying to connect to
    pub fn connect(&mut self, source: &str, destination: &str) {
        info!("Attempting connection: {source} -> {destination}");
        self.write_to_control(format!("CONNECT {source} {destination}"));
    }
    /// Abort connection with remote station
    pub fn abort(&mut self) {
        self.write_to_control("ABORT".into())
    }
    /// Send a CQ frame
    pub fn send_cq(&mut self, source: &str, bandwidth: Bandwidth) {
        // Only send CQ if it's been a while since the last one
        if Instant::now().duration_since(self.state.blocking_read().last_cq).as_millis() > CQ_DURATION_MILLIS {
            self.write_to_control(format!("CQFRAME {source} {}", bandwidth.as_bw_stripped()));
            self.state.blocking_write().last_cq = Instant::now();
        }
    }
    /// Set the maximum channel bandwidth
    pub fn set_bandwidth(&mut self, bandwidth: Bandwidth) {
        self.write_to_control(bandwidth.as_bw().into());
    }
    /// Set the listen state
    pub fn set_listen(&mut self, listen: bool) {
        let l = match listen {
            true => "ON",
            false => "OFF"
        };
        self.write_to_control(format!("LISTEN {l}"));
    }
    /// Set the public state
    pub fn set_public(&mut self, public: bool) {
        let p = match public {
            true => "ON",
            false => "OFF"
        };
        self.write_to_control(format!("PUBLIC {p}"));
    }

    // Utility Functions //

    /// Sends the provided command to the mercury control port
    fn write_to_control(&self, mut msg: String) {
        let s = self.state.clone();
        Promise::spawn_async(async move {
            let mut s = s.write().await;
            let Some(stream) = s.tcp_connection.as_mut() else { return };
            // Mercury denotes the end of a command with \r so we append that to the end of every command
            msg.push('\r');
            // Write the bytes
            match stream.write_all(msg.as_bytes()).await {
                Ok(_) => trace!("Sent control message to mercury: {msg}"),
                Err(e) => error!("Failed to send control message `{msg}`: {e}")
            };
        });
    }
    /// The listen loop that reads and handles incoming messages from the mercury control port
    async fn mercury_control_listener(s: Arc<RwLock<State>>, read: OwnedReadHalf) {
        // Split into messages by mercury's delimiter (\r)
        let mut messages = BufReader::new(read).split(b'\r');
        // Read new message
        while let Ok(Some(msg)) = messages.next_segment().await {

            // Convert message bytes into a string
            let msg = String::from_utf8_lossy(&msg);

            // Split message into its various parts
            let mut parts = msg.split(' ');
            // Get the prefix/message type
            let Some(prefix) = parts.next() else { continue };

            // let (prefix, remaining) = msg.split_once(' ').unwrap_or((&msg, ""));
            // Filter by message prefix/type
            match prefix {
                "OK" => trace!("Received OK from modem"),
                "WRONG" => warn!("Received WRONG from modem, indicating a malformed command or some other failure"),
                "IAMALIVE" => trace!("Received keep-alive from modem"),
                "BUFFER" => {
                    // Keep track of the buffer size
                    let Some(n_part) = parts.next() else { continue };
                    let n: usize = n_part.trim().parse().unwrap_or(0);
                    s.write().await.buffer_len = n;
                    trace!("There are {n} bytes remaining in the ARQ TX buffer");
                },
                "SN" => {
                    // Keep track of the SNR
                    let Some(snr_part) = parts.next() else { continue };
                    let snr: f32 = snr_part.trim().parse().unwrap_or(0.0);
                    s.write().await.snr = snr;
                    trace!("Measured SNR update received: {snr:.1}");
                },
                "BITRATE" => {
                    // Keep track of the mode/level and current throughput in bits/s
                    let Some(level_part) = parts.next() else { continue };
                    let Some(bps_part) = parts.next() else { continue };
                    let bps: usize = bps_part.trim().parse().unwrap_or(0);
                    trace!("Received mode and bitrate update: {level_part} ({bps}bps)");
                    s.write().await.bitrate = (level_part.into(), bps);
                },
                "PTT" => {
                    // Keep track of the PTT state
                    let Some(ptt_part) = parts.next() else { continue };
                    let ptt = match ptt_part {
                        "ON" => true,
                        "OFF" => false,
                        _ => {
                            warn!("Received unknown PTT state from mercury: {ptt_part}");
                            continue;
                        }
                    };
                    s.write().await.ptt = ptt;
                    trace!("Received PTT state: {ptt}");
                },
                "CONNECTED" => {
                    // Keep track of the station we're connected to
                    let Some(source_part) = parts.next() else { continue };
                    let Some(destination_part) = parts.next() else { continue };
                    let Some(bandwidth_part) = parts.next() else { continue };
                    let Ok(bandwidth) = serde_plain::from_str::<Bandwidth>(bandwidth_part) else {
                        warn!("Received unknown bandwidth value from a CONNECTED packet: {bandwidth_part}");
                        continue;
                    };
                    s.write().await.connection = Some(OpenConnection {
                        source_call: source_part.into(),
                        destination_call: destination_part.into(),
                        bandwidth,
                    });
                    info!("Connection established: {source_part} -> {destination_part} (BW: {bandwidth_part})");
                },
                "DISCONNECTED" => {
                    // Keep track of when we get disconnected from the target station (whether intentionally or by timeout)
                    s.write().await.connection = None;
                    info!("Disconnected from remote station")
                },
                "PENDING" => {
                    // The PENDING event is only sent by mercury when a CQ frame is being sent, or when there's an incoming connection
                    // Thus, if a PENDING event is received and a CQFRAME has not been sent relatively recently, we assume that it's an incoming connection request
                    if Instant::now().duration_since(s.read().await.last_cq).as_millis() > CQ_DURATION_MILLIS {
                        info!("Incoming connection request...");
                    }
                },
                "CANCELPENDING" => {},
                "CQFRAME" => {
                    // Keep track of stations calling CQ
                    let Some(source_part) = parts.next() else { continue };
                    let Some(bandwidth_part) = parts.next() else { continue };
                    let Ok(bandwidth) = serde_plain::from_str::<Bandwidth>(bandwidth_part) else {
                        warn!("Received unknown bandwidth value from a CQFRAME packet: {bandwidth_part}");
                        continue;
                    };
                    trace!("Received CQFRAME from {source_part} (BW: {bandwidth_part}");
                },
                _ => warn!("Received unknown message from modem: `{msg}`")
            }
        }

        info!("Disconnected from mercury. If this wasn't intentional, mercury may have crashed!");
        // Close write half of the connection just to be sure.
        s.write().await.tcp_connection = None;
    }
}



#[derive(Debug)]
struct State {
    /// An active write-only connection to the mercury ARQ/control port
    tcp_connection: Option<OwnedWriteHalf>,
    /// Current number of pending bytes in the ARQ transmit buffer
    buffer_len: usize,
    /// The latest measured SNR
    snr: f32,
    /// The current mode and bitrate `(mode_name, bps)`
    bitrate: (String, usize),
    /// Whether PTT is engaged
    ptt: bool,
    /// When we called CQ
    last_cq: Instant,
    /// The station we're connected to, if any
    connection: Option<OpenConnection>
}
impl Default for State {
    fn default() -> State {
        Self {
            tcp_connection: None,
            buffer_len: 0,
            snr: 0.0,
            bitrate: ("".to_string(), 0),
            ptt: false,
            last_cq: Instant::now(),
            connection: None,
        }
    }
}

/// Represents an open connection with a target station
#[derive(Debug, Clone)]
pub struct OpenConnection {
    /// The callsign of the station that initiated the connection
    pub source_call: String,
    /// The callsign of the destination station
    pub destination_call: String,
    /// The negotiated bandwidth between both stations
    pub bandwidth: Bandwidth
}

/// Supported bandwidth modes by mercury
#[derive(Debug, EnumIter, PartialEq, Copy, Clone, Serialize, Deserialize)]
pub enum Bandwidth {
    #[serde(alias = "500")]
    BW500,
    #[serde(alias = "2300")]
    BW2300,
    #[serde(alias = "2750")]
    BW2750
}
impl Bandwidth {
    /// Returns the bandwidth without the BW prefix, i.e. `BW500` returns `500`
    pub fn as_bw_stripped (&self) -> &'static str {
        match self {
            Bandwidth::BW500 => "500",
            Bandwidth::BW2300 => "2300",
            Bandwidth::BW2750 => "2750"
        }
    }
    /// Returns the bandwidth with the BW prefix, i.e. `BW500` returns `BW500`
    pub fn as_bw(&self) -> &'static str {
        match self {
            Bandwidth::BW500 => "BW500",
            Bandwidth::BW2300 => "BW2300",
            Bandwidth::BW2750 => "BW2750"
        }
    }
}
