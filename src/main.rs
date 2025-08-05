use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use clap::Parser;
/// Program to forward serial port over TCP
#[derive(Parser, Debug)]
#[command(version, about, long_about)]
struct Args {
    #[arg(short, long, default_value = "/dev/ttyUSB0")]
    /// Serial port to forward
    serialport: String,
    #[arg(
        short,
        long,
        default_value = "38400",
        value_parser = clap::value_parser!(u32).range(1..=1000000)
    )]
    /// Baud rate for the serial port
    baud: u32,
    #[arg(
        short,
        long,
        default_value = "10001",
        value_parser = clap::value_parser!(u16).range(1..=65535)
    )]
    /// Network port to listen on to send commands
    writeport: u16,
    #[arg(
        short,
        long,
        default_value = "10002",
        value_parser = clap::value_parser!(u16).range(1..=65535)
    )]
    /// Network port to listen on to receive data
    readport: u16,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    // Initialize the logger
    env_logger::init();
    // Parse command line arguments
    let args = Args::parse();
    log::info!("Arguments: {args:#?}");
    let serial = PathBuf::from(&args.serialport);
    if !serial.exists() {
        log::error!("[COM] Fatal error: {} does not exist.", &args.serialport);
        return;
    }
    // Synchronizer
    let running = Arc::new(AtomicBool::new(true));
    // Data source and sink for serial data
    let (ser_to_net, _) = tokio::sync::broadcast::channel(100);
    let (net_to_ser, ser_from_net) = tokio::sync::mpsc::channel(100);
    // Start the TCP reader server
    let netsink_task = {
        let running = running.clone();
        let ser_to_net = ser_to_net.clone();
        let readport = args.readport;
        log::info!("[NET] Starting TCP server on port {readport}");
        tokio::spawn(async move {
            tcp_server(readport, running, Some(ser_to_net), None).await;
        })
    };
    // Start the TCP writer server
    let netsrc_task = {
        let running = running.clone();
        let net_to_ser = net_to_ser.clone();
        let writeport = args.writeport;
        log::info!("[NET] Starting TCP server on port {writeport}");
        tokio::spawn(async move {
            tcp_server(writeport, running, None, Some(net_to_ser)).await;
        })
    };
    // Start the serial thread
    let serial_task = tokio::task::spawn_blocking({
        let running = running.clone();
        let path = args.serialport.clone();
        let baud = args.baud;
        move || {
            serial_thread(path, baud, running, ser_to_net, ser_from_net);
        }
    });
    // Handle Ctrl+C to stop the server gracefully
    let _ctrlchdl = tokio::spawn({
        let running = running.clone();
        async move {
            tokio::signal::ctrl_c().await.unwrap();
            running.store(false, Ordering::SeqCst);
        }
    });
    // Wait for the tasks to finish
    while running.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // Abort the listener
    log::info!("[COM] Stopping tasks...");
    netsink_task.abort();
    netsrc_task.abort();
    let (netsink_res, netsrc_res, ser_res) = tokio::join!(netsink_task, netsrc_task, serial_task);
    if let Err(e) = netsink_res {
        log::error!("[COM] TCP reader task failed: {e}");
    } else {
        log::info!("[COM] TCP reader task completed successfully");
    }
    if let Err(e) = netsrc_res {
        log::error!("[COM] TCP writer task failed: {e}");
    } else {
        log::info!("[COM] TCP writer task completed successfully");
    }
    if let Err(e) = ser_res {
        log::error!("[COM] Serial thread task failed: {e}");
    } else {
        log::info!("[COM] Serial thread task completed successfully");
    }
    log::info!("[COM] All tasks completed, exiting.");
}

pub fn serial_thread(
    path: String,
    baud: u32,
    running: Arc<AtomicBool>,
    data_sink: tokio::sync::broadcast::Sender<Arc<Vec<u8>>>,
    data_source: tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    log::info!("[SER] Serial thread started.");
    let mut data_source = data_source;
    'root: while running.load(std::sync::atomic::Ordering::Relaxed) {
        let ser = serialport::new(&path, baud)
            .stop_bits(serialport::StopBits::One)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_millis(10));
        let mut ser = match serialport::TTYPort::open(&ser) {
            Ok(ser) => {
                log::info!("[SER] Serial port opened: {path}");
                ser
            }
            Err(e) => {
                log::error!("[SER] Failed to open serial port: {e}");
                std::thread::sleep(Duration::from_secs(1));
                continue 'root;
            }
        };
        let mut buf = Vec::with_capacity(1024);
        while running.load(Ordering::Relaxed) {
            // send things from serial to network
            data_source.try_recv().map_or_else(
                |e| {
                    if e == tokio::sync::mpsc::error::TryRecvError::Empty {
                        // No data to read, see if there is anything to receive
                    } else {
                        log::error!("[SER] Error receiving data: {e}");
                    }
                },
                |data| {
                    let cmd = String::from_utf8_lossy(&data).into_owned();
                    log::info!(
                        "[SER] Received command from network for PICTURE: {}",
                        cmd.trim()
                    );
                    // let mut data = data;
                    // data.push(10); // Append newline character
                    if let Err(e) = ser.write_all(&data) {
                        log::error!("[SER] Failed to write data to serial port: {e}");
                    } else {
                        log::info!(
                            "[SER] Successfully sent {} bytes to serial port",
                            data.len()
                        );
                    }
                },
            );
            // read things from serial and send to network
            let mut tmp = [0; 512];
            match ser.read(&mut tmp[..]) {
                Ok(0) => {
                    if !buf.is_empty() {
                        log::info!(
                            "[SER] Sending {} bytes to network: {}",
                            buf.len(),
                            String::from_utf8_lossy(&buf)
                        );
                        if let Err(e) = data_sink.send(Arc::new(buf.clone())) {
                            log::error!("[SER] Failed to send data: {e}");
                        }
                        buf.clear();
                    }
                }
                Ok(n) => {
                    log::info!(
                        "[SER] Read {} bytes from serial port: {}",
                        n,
                        String::from_utf8_lossy(&buf)
                    );
                    buf.extend_from_slice(&tmp[..n]);
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::TimedOut {
                        log::error!("[SER] Error reading from serial port: {e}");
                    } else if !buf.is_empty() {
                        log::info!(
                            "[SER] Sending {} bytes to network: {}",
                            buf.len(),
                            String::from_utf8_lossy(&buf)
                        );
                        if let Err(e) = data_sink.send(Arc::new(buf.clone())) {
                            log::error!("[SER] Failed to send data: {e}");
                        }
                        buf.clear();
                    }
                }
            }
        }
        log::info!("[SER] Stopping serial reader thread");
    }
}

async fn tcp_server(
    port: u16,
    running: Arc<AtomicBool>,
    ser_to_net: Option<tokio::sync::broadcast::Sender<Arc<Vec<u8>>>>,
    net_to_ser: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
) {
    let mode = match (ser_to_net.is_some(), net_to_ser.is_some()) {
        (true, true) => "both",
        (true, false) => "write",
        (false, true) => "read",
        (false, false) => "none",
    };

    log::info!("[NET] Starting TCP {mode} server on port {port}");
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("[NET] Failed to bind TCP listener");
    log::info!("[NET] TCP {mode} server listening on port {port}");
    while running.load(Ordering::Relaxed) {
        match listener.accept().await {
            Ok((socket, addr)) => {
                log::info!("[NET] Accepted connection from {addr}");
                let ser_to_net = ser_to_net.clone();
                let net_to_ser = net_to_ser.clone();
                let running = running.clone();
                tokio::spawn(async move {
                    handle_client(socket, addr, running, ser_to_net, net_to_ser).await;
                });
            }
            Err(e) => {
                log::error!("[NET] Failed to accept connection on {mode} server: {e}");
            }
        }
    }
    log::info!("[NET] TCP {mode} server stopped");
}

async fn handle_client(
    socket: tokio::net::TcpStream,
    addr: std::net::SocketAddr,
    running: Arc<AtomicBool>,
    ser_to_net: Option<tokio::sync::broadcast::Sender<Arc<Vec<u8>>>>,
    net_to_ser: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
) {
    log::info!("[NET] Client: Connected to {addr}");
    let ser_to_net = ser_to_net.map(|s| s.subscribe());

    let (mut reader, mut writer) = socket.into_split();
    // Task to read from serial and send to TCP
    let ser_to_net_task = if let Some(mut ser_to_net) = ser_to_net {
        let running = running.clone();
        Some(tokio::spawn(async move {
            while running.load(Ordering::Relaxed) {
                match ser_to_net.recv().await {
                    Ok(data) => {
                        let mut written = 0;
                        while written < data.len() {
                            match writer.write(&data[written..]).await {
                                Ok(n) => {
                                    written += n;
                                    log::info!(
                                        "[NET] Sent {n} bytes to {addr}: {}",
                                        String::from_utf8_lossy(&data[written - n..])
                                    );
                                }
                                Err(e) => {
                                    log::error!("[NET] Failed to write to {addr}: {e}");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("[NET] {addr}: Error receiving data from serial: {e}");
                        break;
                    }
                }
            }
        }))
    } else {
        None
    };

    // Main loop to read from TCP
    'root: while running.load(Ordering::Relaxed) {
        // let mut tbuf = [0; 1024];
        while running.load(Ordering::Relaxed) {
            let mut buf = [0; 512]; // Buffer size for reading from TCP
            match reader.read(&mut buf).await {
                Ok(0) => {
                    // Connection closed
                    log::info!("[NET] {addr} disconnected");
                    break 'root;
                }
                Ok(n) => {
                    if let Some(net_to_ser) = net_to_ser.as_ref() {
                        log::info!("[NET] Read {n} bytes from {addr}");
                        if let Err(e) = net_to_ser.send(Vec::from(&buf[..n])).await {
                            log::error!("[NET] {addr}: Failed to send data to serial: {e}");
                            break;
                        }
                    }
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(1)).await; // Avoid busy waiting
                    break;
                }
            }
        }
    }

    // Cleanup tasks
    if let Some(ser_to_net_task) = ser_to_net_task {
        ser_to_net_task.abort();
    }
    log::info!("[NET] Client connection closed");
}
