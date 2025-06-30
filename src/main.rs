use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use clap::Parser;
/// Program to forward serial port over TCP
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long, required = true)]
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
    /// Network port to listen on
    #[arg(
        short,
        long,
        default_value = "8080",
        value_parser = clap::value_parser!(u16).range(1..=65535)
    )]
    port: u16,
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
    // Start the TCP server
    let server_task = {
        let running = running.clone();
        let ser_to_net = ser_to_net.clone();
        let net_to_ser = net_to_ser.clone();
        let port = args.port;
        log::info!("[NET] Starting TCP server on port {}", port);
        tokio::spawn(async move {
            tcp_server(port, running, ser_to_net, net_to_ser).await;
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
    let (net_res, ser_res) = tokio::join!(server_task, serial_task);
    if let Err(e) = net_res {
        log::error!("[COM] TCP server task failed: {}", e);
    } else {
        log::info!("[COM] TCP server task completed successfully");
    }
    if let Err(e) = ser_res {
        log::error!("[COM] Serial thread task failed: {}", e);
    } else {
        log::info!("[COM] Serial thread task completed successfully");
    }
    log::info!("[COM] All tasks completed, exiting.");
}

pub fn serial_thread(
    path: String,
    baud: u32,
    running: Arc<AtomicBool>,
    data_sink: tokio::sync::broadcast::Sender<Vec<u8>>,
    data_source: tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    log::info!("[SER] Serial thread started.");
    let mut data_source = data_source;
    'root: while running.load(std::sync::atomic::Ordering::Relaxed) {
        let ser = serialport::new(&path, baud).timeout(Duration::from_secs(1));
        let mut ser = match serialport::TTYPort::open(&ser) {
            Ok(ser) => {
                log::info!("[SER] Serial port opened: {}", path);
                ser
            }
            Err(e) => {
                log::error!("[SER] Failed to open serial port: {}", e);
                std::thread::sleep(Duration::from_secs(1));
                continue 'root;
            }
        };
        let sig = Arc::new(AtomicBool::new(true));
        let reader = ser
            .try_clone_native()
            .expect("Failed to clone serial port for reading");
        let reader_hdl = {
            let sig = sig.clone();
            let sink = data_sink.clone();
            std::thread::spawn(move || serial_reader(reader, sig, sink))
        };
        log::info!("[SER] Serial reader thread started");
        while running.load(Ordering::Relaxed) {
            data_source.try_recv().map_or_else(
                |e| {
                    if e == tokio::sync::mpsc::error::TryRecvError::Empty {
                        // No data to read, continue to the next iteration
                        std::thread::sleep(Duration::from_millis(1));
                    } else {
                        log::error!("[SER] Error receiving data: {}", e);
                    }
                },
                |data| {
                    log::info!("[SER] Received {} bytes from data source", data.len());
                    if let Err(e) = ser.write_all(&data) {
                        log::error!("[SER] Failed to write data to serial port: {}", e);
                    }
                },
            );
        }
        log::info!("[SER] Stopping serial reader thread");
        sig.store(false, Ordering::Relaxed);
        if let Err(e) = reader_hdl.join() {
            log::error!("[SER] Failed to join serial reader thread: {:?}", e);
        } else {
            log::info!("[SER] Serial reader thread joined successfully");
        }
    }
}

fn serial_reader(
    ser: serialport::TTYPort,
    running: Arc<AtomicBool>,
    data_sink: tokio::sync::broadcast::Sender<Vec<u8>>,
) {
    log::info!("[COM] Serial reader thread started");
    let mut ser = ser;
    while running.load(Ordering::Relaxed) {
        let mut buf = Vec::new();
        match ser.read_to_end(&mut buf) {
            Ok(0) => {
                // No data read, continue to the next iteration
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(n) => {
                log::info!("[COM] Read {} bytes from serial port", n);
                if let Err(e) = data_sink.send(buf) {
                    log::error!("[COM] Failed to send data: {}", e);
                }
            }
            Err(e) => {
                log::error!("[COM] Error reading from serial port: {}", e);
                std::thread::sleep(Duration::from_secs(1)); // Wait before retrying
            }
        }
    }
    log::info!("[COM] Serial reader thread exiting");
}

async fn tcp_server(
    port: u16,
    running: Arc<AtomicBool>,
    ser_to_net: tokio::sync::broadcast::Sender<Vec<u8>>,
    net_to_ser: tokio::sync::mpsc::Sender<Vec<u8>>,
) {
    log::info!("[NET] Starting TCP server on port {}", port);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("[NET] Failed to bind TCP listener");
    log::info!("[NET] TCP server listening on port {}", port);
    while running.load(Ordering::Relaxed) {
        match listener.accept().await {
            Ok((socket, addr)) => {
                log::info!("[NET] Accepted connection from {}", addr);
                let ser_to_net = ser_to_net.clone();
                let net_to_ser = net_to_ser.clone();
                let running = running.clone();
                tokio::spawn(async move {
                    handle_client(socket, running, ser_to_net, net_to_ser).await;
                });
            }
            Err(e) => {
                log::error!("[NET] Failed to accept connection: {}", e);
            }
        }
    }
    log::info!("[NET] TCP server stopped");
}

async fn handle_client(
    socket: tokio::net::TcpStream,
    running: Arc<AtomicBool>,
    ser_to_net: tokio::sync::broadcast::Sender<Vec<u8>>,
    net_to_ser: tokio::sync::mpsc::Sender<Vec<u8>>,
) {
    log::info!("[NET] Handling new client connection");
    let mut ser_to_net = ser_to_net.subscribe();
    let (reader, writer) = socket.into_split();

    // Task to read from serial and send to TCP
    let ser_to_net_task = {
        let running = running.clone();
        tokio::spawn(async move {
            while running.load(Ordering::Relaxed) {
                match ser_to_net.recv().await {
                    Ok(data) => {
                        let mut written = 0;
                        while written < data.len() {
                            match writer.try_write(&data[written..]) {
                                Ok(n) => {
                                    written += n;
                                    log::info!("[NET] Sent {} bytes to TCP", n);
                                }
                                Err(e) => {
                                    log::error!("[NET] Failed to write to TCP socket: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("[NET] Error receiving data from serial: {}", e);
                        break;
                    }
                }
            }
        })
    };

    // Main loop to read from TCP
    'root: while running.load(Ordering::Relaxed) {
        let mut buf = Vec::with_capacity(1024);
        let mut tbuf = [0; 1024];
        while running.load(Ordering::Relaxed) {
            match reader.try_read(&mut tbuf) {
                Ok(0) => {
                    // Connection closed
                    log::info!("[NET] Client disconnected");
                    break 'root;
                }
                Ok(n) => {
                    buf.extend_from_slice(&tbuf[..n]);
                    log::info!("[NET] Read {} bytes from TCP", n);
                }
                Err(e) => {
                    log::error!("[NET] Error reading from TCP socket: {}", e);
                    break;
                }
            }
        }
        if !buf.is_empty() {
            log::info!("[NET] Sending {} bytes to serial", buf.len());
            if let Err(e) = net_to_ser.send(buf).await {
                log::error!("[NET] Failed to send data to serial: {}", e);
                break;
            }
        }
    }

    // Cleanup tasks
    ser_to_net_task.abort();
    log::info!("[NET] Client connection closed");
}
