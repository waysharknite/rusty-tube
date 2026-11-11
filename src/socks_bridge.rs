//! Локальный HTTP-прокси поверх SOCKS5 для загрузки участков видео.
//!
//! Участок (`--download-sections`) yt-dlp качает через ffmpeg, а ffmpeg понимает только
//! HTTP-прокси: с `socks5h://` yt-dlp предупреждает «does not support SOCKS proxies»,
//! и ffmpeg не может подключиться или идёт в сеть мимо VPN. Мост слушает 127.0.0.1,
//! принимает `CONNECT host:port` и открывает тот же туннель через SOCKS5-прокси.
//! TLS остаётся сквозным: мост только перекладывает байты.

use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

/// Сколько ждать ответа SOCKS5-прокси при установке туннеля.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Предел заголовка запроса: CONNECT — это пара коротких строк.
const MAX_HEAD: usize = 16 * 1024;

/// Работающий мост; останавливается при удалении.
pub struct Bridge {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Bridge {
    /// Запускает мост к SOCKS5-прокси на `127.0.0.1:socks_port`.
    pub fn start(socks_port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        thread::spawn(move || {
            for conn in listener.incoming() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(client) = conn {
                    thread::spawn(move || {
                        let _ = serve(client, socks_port);
                    });
                }
            }
        });
        Ok(Self { port, stop })
    }

    /// Значение для `yt-dlp --proxy`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Будим `accept`, чтобы поток увидел флаг и закрыл порт.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
    }
}

fn serve(client: TcpStream, socks_port: u16) -> io::Result<()> {
    client.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut reader = BufReader::new(client);
    let head = read_head(&mut reader)?;
    let mut client = reader.get_ref().try_clone()?;

    let Some((host, port)) = parse_connect(&head) else {
        client.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    };
    let mut upstream = match socks_connect(socks_port, &host, port) {
        Ok(stream) => stream,
        Err(e) => {
            client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")?;
            return Err(e);
        }
    };
    client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    client.set_read_timeout(None)?;
    // Клиент мог прислать данные вслед за заголовком — они уже в буфере.
    upstream.write_all(reader.buffer())?;

    let mut to_upstream = upstream.try_clone()?;
    let mut from_client = client.try_clone()?;
    let uplink = thread::spawn(move || {
        let _ = io::copy(&mut from_client, &mut to_upstream);
        let _ = to_upstream.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = uplink.join();
    Ok(())
}

/// Читает заголовок HTTP-запроса до пустой строки.
fn read_head(reader: &mut impl BufRead) -> io::Result<String> {
    let mut head = String::new();
    loop {
        let read = reader.read_line(&mut head)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if head.len() > MAX_HEAD {
            return Err(io::Error::other("слишком длинный заголовок"));
        }
        if head.ends_with("\r\n\r\n") || head.ends_with("\n\n") {
            return Ok(head);
        }
    }
}

/// `CONNECT host:port HTTP/1.1` → (host, port). IPv6 — в квадратных скобках.
fn parse_connect(head: &str) -> Option<(String, u16)> {
    let mut words = head.lines().next()?.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let (host, port) = words.next()?.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = port.parse().ok()?;
    (!host.is_empty()).then(|| (host.to_string(), port))
}

/// Открывает туннель к `host:port` через SOCKS5 без авторизации. Имя хоста передаётся
/// прокси как есть, чтобы DNS тоже резолвился через него (как у `socks5h`).
fn socks_connect(socks_port: u16, host: &str, port: u16) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, socks_port))?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.write_all(&[5, 1, 0])?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply)?;
    if reply != [5, 0] {
        return Err(io::Error::other("SOCKS5-прокси требует авторизацию"));
    }

    let mut request = vec![5, 1, 0];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            let name = host.as_bytes();
            let len = u8::try_from(name.len())
                .map_err(|_| io::Error::other("слишком длинное имя хоста"))?;
            request.push(3);
            request.push(len);
            request.extend_from_slice(name);
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request)?;

    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply)?;
    if reply[1] != 0 {
        return Err(io::Error::other(format!(
            "SOCKS5-прокси отказал в подключении (код {})",
            reply[1]
        )));
    }
    // Адрес, который прокси выделил под соединение, нам не нужен.
    let addr_len = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len)?;
            usize::from(len[0])
        }
        _ => return Err(io::Error::other("неверный ответ SOCKS5-прокси")),
    };
    let mut skip = vec![0u8; addr_len + 2];
    stream.read_exact(&mut skip)?;
    stream.set_read_timeout(None)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_line() {
        assert_eq!(
            parse_connect("CONNECT rr1.googlevideo.com:443 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("rr1.googlevideo.com".into(), 443))
        );
        assert_eq!(
            parse_connect("CONNECT [::1]:8443 HTTP/1.1\r\n\r\n"),
            Some(("::1".into(), 8443))
        );
        assert_eq!(parse_connect("GET http://x/ HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_connect("CONNECT x HTTP/1.1\r\n\r\n"), None);
    }

    /// Эхо-сервер за поддельным SOCKS5-прокси: данные проходят через мост туда и обратно.
    #[test]
    fn tunnels_through_socks() {
        let echo = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut s, _) = echo.accept().unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).unwrap();
            s.write_all(&buf).unwrap();
        });

        let socks = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let socks_port = socks.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut s, _) = socks.accept().unwrap();
            let mut greeting = [0u8; 3];
            s.read_exact(&mut greeting).unwrap();
            s.write_all(&[5, 0]).unwrap();
            let mut head = [0u8; 5];
            s.read_exact(&mut head).unwrap();
            assert_eq!(head[3], 3, "имя хоста должно уйти прокси, а не IP");
            let mut rest = vec![0u8; usize::from(head[4]) + 2];
            s.read_exact(&mut rest).unwrap();
            let name = std::str::from_utf8(&rest[..rest.len() - 2]).unwrap();
            assert_eq!(name, "localhost");
            let port = u16::from_be_bytes([rest[rest.len() - 2], rest[rest.len() - 1]]);
            let mut target = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            s.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).unwrap();
            let mut down = s.try_clone().unwrap();
            let mut target_r = target.try_clone().unwrap();
            thread::spawn(move || io::copy(&mut target_r, &mut down));
            let _ = io::copy(&mut s, &mut target);
        });

        let bridge = Bridge::start(socks_port).unwrap();
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, bridge.port)).unwrap();
        client
            .write_all(format!("CONNECT localhost:{echo_port} HTTP/1.1\r\n\r\n").as_bytes())
            .unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let head = read_head(&mut reader).unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        client.write_all(b"ping").unwrap();
        let mut answer = [0u8; 4];
        reader.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"ping");
    }
}
