//! A minimal SMTP server for tests: it accepts every message and keeps it.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

pub struct FakeSmtp {
    pub port: u16,
    pub messages: Arc<Mutex<Vec<String>>>,
}

impl FakeSmtp {
    pub async fn start() -> FakeSmtp {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let messages: Arc<Mutex<Vec<String>>> = Arc::default();
        let store = messages.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let store = store.clone();
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let mut lines = BufReader::new(read).lines();
                    write.write_all(b"220 localhost ESMTP fake\r\n").await.ok();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let command = line.to_ascii_uppercase();
                        let reply: &[u8] =
                            if command.starts_with("EHLO") || command.starts_with("HELO") {
                                b"250 localhost\r\n"
                            } else if command.starts_with("DATA") {
                                write.write_all(b"354 send it\r\n").await.ok();
                                let mut message = String::new();
                                while let Ok(Some(line)) = lines.next_line().await {
                                    if line == "." {
                                        break;
                                    }
                                    message.push_str(&line);
                                    message.push('\n');
                                }
                                store.lock().unwrap().push(message);
                                b"250 queued\r\n"
                            } else if command.starts_with("QUIT") {
                                write.write_all(b"221 bye\r\n").await.ok();
                                break;
                            } else {
                                b"250 OK\r\n"
                            };
                        write.write_all(reply).await.ok();
                    }
                });
            }
        });
        FakeSmtp { port, messages }
    }

    pub fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
}
