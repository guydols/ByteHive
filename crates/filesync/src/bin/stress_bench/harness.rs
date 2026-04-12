use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn parse_level(line: &str) -> &'static str {
    match line.split_whitespace().next().unwrap_or("") {
        "ERROR" => "ERROR",
        "WARN" => "WARN",
        "INFO" => "INFO",
        "DEBUG" => "DEBUG",
        "TRACE" => "TRACE",
        _ => "INFO",
    }
}

pub struct BenchServer {
    child: Child,
    pid: u32,
    port: u16,
    dhat_path: Option<PathBuf>,
    log_thread: Option<std::thread::JoinHandle<Vec<super::types::LogLine>>>,
    stdout_log_thread: Option<std::thread::JoinHandle<Vec<super::types::LogLine>>>,
}

impl BenchServer {
    /// Start the server subprocess, optionally wrapped with dhat.
    ///
    /// When `dhat_path` is `Some(path)`, the server is launched as:
    /// ```
    /// valgrind --tool=dhat --dhat-out-file=<path> <exe> __server <dir> <port-file>
    /// ```
    pub fn start(
        dir: PathBuf,
        dhat_path: Option<PathBuf>,
        bench_start: std::time::Instant,
        log_level: &str,
        data_store: std::sync::Arc<super::data_store::DataStore>,
    ) -> Self {
        fs::create_dir_all(&dir).expect("create server dir");

        let port_file = dir.join(".bench_port");

        // Remove stale port file from a previous run
        if port_file.exists() {
            fs::remove_file(&port_file).expect("remove stale port file");
        }

        let exe = std::env::current_exe().expect("current_exe");
        let dir_str = dir.to_str().expect("dir path must be valid UTF-8");
        let port_file_str = port_file
            .to_str()
            .expect("port file path must be valid UTF-8");

        let rust_log = format!("bytehive_filesync={log_level}");

        let mut child = if let Some(ref path) = dhat_path {
            let path_str = path.to_str().expect("dhat output path must be valid UTF-8");
            let exe_str = exe.to_str().expect("exe path must be valid UTF-8");
            eprintln!("[harness] starting server with valgrind/dhat (output: {path_str})");
            let dhat_out_arg = format!("--dhat-out-file={path_str}");
            Command::new("valgrind")
                .args([
                    "--tool=dhat",
                    dhat_out_arg.as_str(),
                    exe_str,
                    "__server",
                    dir_str,
                    port_file_str,
                ])
                .env("RUST_LOG", &rust_log)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn server subprocess with valgrind/dhat")
        } else {
            Command::new(&exe)
                .args(["__server", dir_str, port_file_str])
                .env("RUST_LOG", &rust_log)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn server subprocess")
        };

        let pid = child.id();

        // Spawn log reader thread for server stderr.
        let stderr_handle = child.stderr.take().expect("server stderr should be piped");
        let stdout_handle = child.stdout.take().expect("server stdout should be piped");
        let source_tag = "server";
        let log_thread = {
            let start = bench_start;
            let tag = source_tag;
            let ds = data_store.clone();
            std::thread::Builder::new()
                .name(format!("{tag}-log-reader").into())
                .spawn(move || {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(stderr_handle);
                    let mut lines: Vec<super::types::LogLine> = Vec::new();
                    for raw in reader.lines() {
                        match raw {
                            Ok(line) => {
                                let elapsed = start.elapsed().as_secs_f64();
                                eprintln!("[{tag}] {line}");
                                let level = parse_level(&line).to_string();
                                let log_line = super::types::LogLine {
                                    elapsed_secs: elapsed,
                                    source: tag.to_string(),
                                    level,
                                    message: line,
                                };
                                // Write to data store immediately so crash-partial runs still have logs
                                if tag == "server" {
                                    ds.append(&super::data_store::DataRecord::ServerLog {
                                        line: log_line.clone(),
                                    });
                                } else {
                                    ds.append(&super::data_store::DataRecord::ClientLog {
                                        line: log_line.clone(),
                                    });
                                }
                                lines.push(log_line);
                            }
                            Err(_) => break,
                        }
                    }
                    lines
                })
                .expect(&format!("spawn {source_tag}-log-reader"))
        };
        let ds_stdout = data_store;
        let stdout_log_thread = {
            let start = bench_start;
            let tag = source_tag;
            let stdout_tag = format!("{tag}_stdout");
            std::thread::Builder::new()
                .name(format!("{tag}-stdout-log-reader").into())
                .spawn(move || {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(stdout_handle);
                    let mut lines: Vec<super::types::LogLine> = Vec::new();
                    for raw in reader.lines() {
                        match raw {
                            Ok(line) => {
                                let elapsed = start.elapsed().as_secs_f64();
                                eprintln!("[{tag}/stdout] {line}");
                                let level = parse_level(&line).to_string();
                                let log_line = super::types::LogLine {
                                    elapsed_secs: elapsed,
                                    source: stdout_tag.clone(),
                                    level,
                                    message: line,
                                };
                                if tag == "server" {
                                    ds_stdout.append(&super::data_store::DataRecord::ServerLog {
                                        line: log_line.clone(),
                                    });
                                } else {
                                    ds_stdout.append(&super::data_store::DataRecord::ClientLog {
                                        line: log_line.clone(),
                                    });
                                }
                                lines.push(log_line);
                            }
                            Err(_) => break,
                        }
                    }
                    lines
                })
                .expect(&format!("spawn {source_tag}-stdout-log-reader"))
        };

        // Wait for the port file to appear and contain a valid port number.
        let port = {
            let start = Instant::now();
            let timeout = Duration::from_secs(15);
            loop {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("server port file did not appear within {timeout:?}");
                }
                if port_file.exists() {
                    if let Ok(contents) = fs::read_to_string(&port_file) {
                        let trimmed = contents.trim();
                        if let Ok(p) = trimmed.parse::<u16>() {
                            break p;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
        };

        // Wait for the server to accept TCP connections.
        {
            let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let start = Instant::now();
            let timeout = Duration::from_secs(10);
            loop {
                if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                    break;
                }
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("server did not accept connections within {timeout:?}");
                }
                thread::sleep(Duration::from_millis(50));
            }
        }

        BenchServer {
            child,
            pid,
            port,
            dhat_path,
            log_thread: Some(log_thread),
            stdout_log_thread: Some(stdout_log_thread),
        }
    }

    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Returns the dhat output path that was passed to `start()`, if dhat was enabled.
    pub fn dhat_path(&self) -> Option<&Path> {
        self.dhat_path.as_deref()
    }

    pub fn shutdown_and_collect_logs(&mut self) -> Vec<super::types::LogLine> {
        let _ = self.child.stdin.take(); // EOF → subprocess shuts down
        let start = Instant::now();
        let timeout = Duration::from_secs(30);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => break,
            }
            if start.elapsed() >= timeout {
                eprintln!("[harness] subprocess did not exit gracefully, killing");
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let mut logs = if let Some(handle) = self.log_thread.take() {
            handle.join().unwrap_or_default()
        } else {
            vec![]
        };
        if let Some(handle) = self.stdout_log_thread.take() {
            logs.extend(handle.join().unwrap_or_default());
        }
        logs
    }
}

pub struct BenchClient {
    child: Child,
    pid: u32,
    dhat_path: Option<PathBuf>,
    log_thread: Option<std::thread::JoinHandle<Vec<super::types::LogLine>>>,
    stdout_log_thread: Option<std::thread::JoinHandle<Vec<super::types::LogLine>>>,
}

impl BenchClient {
    /// Start the client subprocess, optionally wrapped with dhat.
    ///
    /// When `dhat_path` is `Some(path)`, the client is launched as:
    /// ```
    /// valgrind --tool=dhat --dhat-out-file=<path> <exe> __client <dir> <server-addr>
    /// ```
    pub fn start(
        dir: PathBuf,
        server_addr: String,
        dhat_path: Option<PathBuf>,
        bench_start: std::time::Instant,
        log_level: &str,
        data_store: std::sync::Arc<super::data_store::DataStore>,
    ) -> Self {
        fs::create_dir_all(&dir).expect("create client dir");

        let exe = std::env::current_exe().expect("current_exe");
        let dir_str = dir.to_str().expect("dir path must be valid UTF-8");

        let rust_log = format!("bytehive_filesync={log_level}");

        let mut child = if let Some(ref path) = dhat_path {
            let path_str = path.to_str().expect("dhat output path must be valid UTF-8");
            let exe_str = exe.to_str().expect("exe path must be valid UTF-8");
            eprintln!("[harness] starting client with valgrind/dhat (output: {path_str})");
            let dhat_out_arg = format!("--dhat-out-file={path_str}");
            Command::new("valgrind")
                .args([
                    "--tool=dhat",
                    dhat_out_arg.as_str(),
                    exe_str,
                    "__client",
                    dir_str,
                    &server_addr,
                ])
                .env("RUST_LOG", &rust_log)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn client subprocess with valgrind/dhat")
        } else {
            Command::new(&exe)
                .args(["__client", dir_str, &server_addr])
                .env("RUST_LOG", &rust_log)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn client subprocess")
        };

        let pid = child.id();

        // Spawn log reader thread for client stderr.
        let stderr_handle = child.stderr.take().expect("client stderr should be piped");
        let stdout_handle = child.stdout.take().expect("client stdout should be piped");
        let source_tag = "client";
        let log_thread = {
            let start = bench_start;
            let tag = source_tag;
            let ds = data_store.clone();
            std::thread::Builder::new()
                .name(format!("{tag}-log-reader").into())
                .spawn(move || {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(stderr_handle);
                    let mut lines: Vec<super::types::LogLine> = Vec::new();
                    for raw in reader.lines() {
                        match raw {
                            Ok(line) => {
                                let elapsed = start.elapsed().as_secs_f64();
                                eprintln!("[{tag}] {line}");
                                let level = parse_level(&line).to_string();
                                let log_line = super::types::LogLine {
                                    elapsed_secs: elapsed,
                                    source: tag.to_string(),
                                    level,
                                    message: line,
                                };
                                // Write to data store immediately so crash-partial runs still have logs
                                if tag == "server" {
                                    ds.append(&super::data_store::DataRecord::ServerLog {
                                        line: log_line.clone(),
                                    });
                                } else {
                                    ds.append(&super::data_store::DataRecord::ClientLog {
                                        line: log_line.clone(),
                                    });
                                }
                                lines.push(log_line);
                            }
                            Err(_) => break,
                        }
                    }
                    lines
                })
                .expect(&format!("spawn {source_tag}-log-reader"))
        };
        let ds_stdout = data_store;
        let stdout_log_thread = {
            let start = bench_start;
            let tag = source_tag;
            let stdout_tag = format!("{tag}_stdout");
            std::thread::Builder::new()
                .name(format!("{tag}-stdout-log-reader").into())
                .spawn(move || {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(stdout_handle);
                    let mut lines: Vec<super::types::LogLine> = Vec::new();
                    for raw in reader.lines() {
                        match raw {
                            Ok(line) => {
                                let elapsed = start.elapsed().as_secs_f64();
                                eprintln!("[{tag}/stdout] {line}");
                                let level = parse_level(&line).to_string();
                                let log_line = super::types::LogLine {
                                    elapsed_secs: elapsed,
                                    source: stdout_tag.clone(),
                                    level,
                                    message: line,
                                };
                                if tag == "server" {
                                    ds_stdout.append(&super::data_store::DataRecord::ServerLog {
                                        line: log_line.clone(),
                                    });
                                } else {
                                    ds_stdout.append(&super::data_store::DataRecord::ClientLog {
                                        line: log_line.clone(),
                                    });
                                }
                                lines.push(log_line);
                            }
                            Err(_) => break,
                        }
                    }
                    lines
                })
                .expect(&format!("spawn {source_tag}-stdout-log-reader"))
        };

        BenchClient {
            child,
            pid,
            dhat_path,
            log_thread: Some(log_thread),
            stdout_log_thread: Some(stdout_log_thread),
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Returns the dhat output path that was passed to `start()`, if dhat was enabled.
    pub fn dhat_path(&self) -> Option<&Path> {
        self.dhat_path.as_deref()
    }

    pub fn shutdown_and_collect_logs(&mut self) -> Vec<super::types::LogLine> {
        let _ = self.child.stdin.take(); // EOF → subprocess shuts down
        let start = Instant::now();
        let timeout = Duration::from_secs(30);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => break,
            }
            if start.elapsed() >= timeout {
                eprintln!("[harness] subprocess did not exit gracefully, killing");
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let mut logs = if let Some(handle) = self.log_thread.take() {
            handle.join().unwrap_or_default()
        } else {
            vec![]
        };
        if let Some(handle) = self.stdout_log_thread.take() {
            logs.extend(handle.join().unwrap_or_default());
        }
        logs
    }
}

/// Write a marker file to the server directory and wait for it to appear
/// in the client directory.  This confirms the sync pipeline is working.
pub fn wait_for_connection(server_dir: &Path, client_dir: &Path, timeout: Duration) -> bool {
    let marker = ".bench_connection_marker";
    let marker_content = b"filesync-bench-connection-test";
    fs::write(server_dir.join(marker), marker_content).expect("write marker");

    let start = Instant::now();
    loop {
        if client_dir.join(marker).exists() {
            // Clean up the marker
            let _ = fs::remove_file(server_dir.join(marker));
            return true;
        }
        if start.elapsed() >= timeout {
            eprintln!(
                "[harness] Connection marker did not appear in client dir within {timeout:?}"
            );
            return false;
        }
        thread::sleep(Duration::from_millis(200));
    }
}
