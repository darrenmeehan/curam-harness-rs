use std::io::Read;
use std::sync::mpsc;
use std::sync::{Arc, AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

fn main() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hello\"}}]}";
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut drain: [u8; 8192] = [0; 8192];
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(60)));
        let mut total = 0;
        loop {
            match stream.read(&mut drain[total..]) {
                Ok(0) => break,
                Ok(n2) => {
                    total += n2;
                    if total >= drain.len() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        println!(
            "server read {} bytes: {}",
            total,
            std::string::String::from_utf8(drain[0..total.minimum(80)]).unwrap_or_default()
        );
        let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        stream.write(headers.as_bytes()).ok();
        stream.write(body.as_bytes()).ok();
        stream.flush().ok();
        thread::sleep(Duration::from_millis(200));
    });
    let url = format!("http://127.0.0.1:{}", port);
    // raw ureq read via with_config().reader()
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    let mut resp = match agent
        .post(url)
        .header("Content-Type", "application/json")
        .send("{}")
    {
        Ok(r) => r,
        Err(e) => {
            println!("send err: {e}");
            return;
        }
    };
    let mut rd = resp.body_mut().with_config().reader();
    let mut all: Vec<u8> = vec![];
    let mut buf: [u8; 64] = [0; 64];
    loop {
        match rd.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for i in 0..n {
                    all.push(buf[i]);
                }
            }
            Err(e) => {
                println!("read err: {e}");
                break;
            }
        }
    }
    let s = std::string::String::from_utf8(all).unwrap_or_default();
    println!("got {} bytes: {:?}", s.len(), s[0..s.len().minimum(60)]);
    println!("a == b: {}", s == body);
}
