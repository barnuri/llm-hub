use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn argument_value(arguments: &[String], name: &str) -> Option<String> {
    let index = arguments.iter().position(|argument| argument == name)?;
    arguments.get(index + 1).cloned()
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next()?.to_owned();
    let path = request_parts.next()?.to_owned();
    let header_end = text.find("\r\n\r\n").unwrap_or(text.len());
    let body = text.get(header_end + 4..).unwrap_or_default().to_owned();
    Some((method, path, body))
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: Option<&str>, body: &[u8]) {
    let mut response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    if let Some(content_type) = content_type {
        let _ = write!(response, "Content-Type: {content_type}\r\n");
    }
    response.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body);
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let port: u16 = argument_value(&arguments, "--port")
        .or_else(|| arguments.first().cloned())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            eprintln!("test-local-server: port is required");
            std::process::exit(2)
        });
    let model = argument_value(&arguments, "-m");

    if let Some(model) = model.as_deref() {
        println!("fake server start");
        if model.contains("broken") {
            eprintln!("cannot load the model");
            std::process::exit(3);
        }
        if let Ok(spawn_log) = std::env::var("SPAWN_LOG") {
            let model_name = std::path::Path::new(model)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(model);
            let line = format!("{} {model_name} {port}\n", std::process::id());
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(PathBuf::from(spawn_log))
                .and_then(|mut file| file.write_all(line.as_bytes()));
        }
        eprintln!("fake llama stderr");
    }

    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|err| {
        eprintln!("test-local-server: cannot bind 127.0.0.1:{port}: {err}");
        std::process::exit(2)
    });
    for stream in listener.incoming().flatten() {
        let model = model.clone();
        thread::spawn(move || handle(stream, model.as_deref()));
    }
}

fn handle(mut stream: TcpStream, model: Option<&str>) {
    let Some((method, path, body_text)) = read_request(&mut stream) else {
        return;
    };

    if method == "GET" {
        if path == "/health" || path == "/healthz" {
            let ready = model.is_none_or(|model| !model.contains("unready"));
            write_response(
                &mut stream,
                if ready {
                    "200 OK"
                } else {
                    "503 Service Unavailable"
                },
                None,
                b"ok",
            );
        } else {
            write_response(&mut stream, "404 Not Found", None, b"");
        }
        return;
    }

    if method != "POST" {
        write_response(&mut stream, "405 Method Not Allowed", None, b"");
        return;
    }

    if path == "/v1/chat/completions" {
        if model.is_some_and(|model| model.contains("slow")) {
            thread::sleep(Duration::from_secs(3));
        }
        if body_text.contains("\"stream\":true") {
            let stream_body = b"data: {\"choices\":[{\"delta\":{\"content\":\"local\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\" reply\"}}]}\n\ndata: [DONE]\n\n";
            write_response(
                &mut stream,
                "200 OK",
                Some("text/event-stream"),
                stream_body,
            );
        } else if model.is_some() {
            let body = br#"{"choices":[{"message":{"content":"local reply"}}]}"#;
            write_response(&mut stream, "200 OK", Some("application/json"), body);
        } else {
            let body = br#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]}"#;
            write_response(&mut stream, "200 OK", Some("application/json"), body);
        }
    } else {
        write_response(&mut stream, "404 Not Found", None, b"");
    }
}
