// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/http.c + lib/include/chiaki/http.h (chiaki-ng).
//
// Bewusst kein HTTP-Client-Framework: Regist & Co. brauchen low-level
// Kontrolle über den eigenen Socket + StopPipe (wie im C-Original). Portiert
// sind exakt die Funktionen, die http.h anbietet:
//
// - chiaki_http_header_parse   -> header_parse()
// - chiaki_http_response_parse -> response_parse()
// - chiaki_recv_http_header    -> recv_http_header()
//
// `chiaki_send_recv_http_header_psn()` (HTTP-Header über ChiakiRudp) liegt
// bewusst NICHT in chiaki-core: Rudp gehört laut Workspace-Aufteilung nach
// chiaki-remote und wird dort zusammen mit dem PSN-Regist-Pfad portiert.
//
// Es gibt in diesem chiaki-ng-Stand bewusst KEIN ChiakiHttpRequest und kein
// chiaki_http_download — Requests werden von den Aufrufern (regist.c, ctrl.c)
// selbst als Strings formatiert und über den Socket geschickt.

use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::error::{ChiakiError, ChiakiResult};
use super::stoppipe::StopPipe;

/// Port von `ChiakiHttpHeader` (C: einfach verkettete Liste; Rust: Vektor,
/// Reihenfolge = Reihenfolge im Dokument).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHeader {
    pub key: String,
    pub value: String,
}

/// Port von `ChiakiHttpResponse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub code: i32,
    pub headers: Vec<HttpHeader>,
}

/// Port von `chiaki_http_header_parse()`.
///
/// Das C-Original terminiert Key/Value im Puffer mit '\0' und baut eine
/// prepend-verkettete Liste (Iteration liefert damit die *umgekehrte*
/// Dokumentreihenfolge; bei doppelten Keys gewinnt dort effektiv die erste
/// Okkurrenz im Dokument). Hier werden die Spans direkt gesammelt — Aufrufer
/// mit 1:1-Semantik nehmen den ersten Treffer pro Key.
/// Ein NUL-Byte beendet das Parsing wie im C (`if(!c) break`); ein unvoll-
/// ständiger Header am Ende wird still verworfen (wie im C).
pub fn header_parse(buf: &[u8]) -> ChiakiResult<Vec<HttpHeader>> {
    let mut headers = Vec::new();
    let mut key_start = 0usize;
    let mut key_end = 0usize; // Position des ':'
    let mut value_start: Option<usize> = None;

    let mut i = 0usize;
    while i < buf.len() {
        let c = buf[i];
        if c == 0 {
            // C: if(!c) break;
            break;
        }

        match value_start {
            None => {
                if c == b':' {
                    if key_start == i {
                        // leerer Key
                        return Err(ChiakiError::InvalidData);
                    }
                    key_end = i;
                    i += 1;
                    if i == buf.len() {
                        return Err(ChiakiError::InvalidData);
                    }
                    if buf[i] == b' ' {
                        i += 1;
                        if i == buf.len() {
                            return Err(ChiakiError::InvalidData);
                        }
                    }
                    value_start = Some(i);
                } else if c == b'\r' || c == b'\n' {
                    if key_start + 1 < i {
                        // Zeile ohne ':' (länger als 1 Zeichen)
                        return Err(ChiakiError::InvalidData);
                    }
                    // Leerzeile/einzelnes Zeichen ohne ':' überspringen
                    key_start = i + 1;
                }
            }
            Some(vs) => {
                if c == b'\r' || c == b'\n' {
                    if vs == i {
                        // leerer Value
                        return Err(ChiakiError::InvalidData);
                    }
                    headers.push(HttpHeader {
                        key: String::from_utf8_lossy(&buf[key_start..key_end]).into_owned(),
                        value: String::from_utf8_lossy(&buf[vs..i]).into_owned(),
                    });
                    key_start = i + 1;
                    value_start = None;
                }
            }
        }
        i += 1;
    }
    Ok(headers)
}

/// Port von `chiaki_http_response_parse()`.
///
/// Erwartet `"HTTP/1.1 <code> ...\n"` gefolgt von Header-Zeilen.
pub fn response_parse(buf: &[u8]) -> ChiakiResult<HttpResponse> {
    const HTTP_VERSION: &[u8] = b"HTTP/1.1 ";
    let http_version_size = HTTP_VERSION.len();

    if buf.len() < http_version_size {
        return Err(ChiakiError::InvalidData);
    }
    if &buf[..http_version_size] != HTTP_VERSION {
        return Err(ChiakiError::InvalidData);
    }

    let rest = &buf[http_version_size..];
    let line_end = rest
        .iter()
        .position(|&c| c == b'\n')
        .ok_or(ChiakiError::InvalidData)?;
    let line_length = line_end + 1;
    if rest.len() <= line_length {
        return Err(ChiakiError::InvalidData);
    }

    let code = strtol_i32(&rest[..line_length]);
    if code == 0 {
        return Err(ChiakiError::InvalidData);
    }

    header_parse(&rest[line_length..]).map(|headers| HttpResponse { code, headers })
}

/// Poll-Intervall, in dem `recv_http_header` zusätzlich das Stop-Signal prüft.
/// (Rust-std kennt kein select() über Socket+Event; das C-Original wartet in
/// `chiaki_stop_pipe_select_single` exakt bis zum Deadline-Rest — hier wird
/// der Socket blockierend mit kurzem SO_RCVTIMEO gepollt, Reaktionszeit auf
/// stop() <= `RECV_POLL`.)
const RECV_POLL: Duration = Duration::from_millis(50);

/// Port von `chiaki_recv_http_header()`.
///
/// Liest einen HTTP-Header (`\r\n\r\n`-begrenzt) vom Socket. Der Empfänger-
/// Puffer darf größer als der Header sein: Bytes, die über das Header-Ende
/// hinaus ankommen (Beginn des Bodys), bleiben im Puffer bei
/// `buf[header_size..received_size]` liegen — exakt wie im C.
///
/// Gibt `(header_size, received_size)` zurück.
///
/// * `stop_pipe` — optional (wie im C); bricht mit `ChiakiError::Canceled` ab.
/// * `timeout_ms` — nur relevant, wenn `stop_pipe` gesetzt ist (C-Doku);
///   `u64::MAX` = ohne Deadline.
pub fn recv_http_header(
    sock: &mut TcpStream,
    buf: &mut [u8],
    stop_pipe: Option<&StopPipe>,
    timeout_ms: u64,
) -> ChiakiResult<(usize, usize)> {
    // 0 = ""
    // 1 = "\r"
    // 2 = "\r\n"
    // 3 = "\r\n\r"
    // 4 = "\r\n\r\n" (final)
    let mut nl_state = 0usize;
    const TRANSITIONS_R: [usize; 4] = [1, 1, 3, 1];
    const TRANSITIONS_N: [usize; 4] = [0, 2, 0, 4];

    let deadline = if timeout_ms == u64::MAX {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms))
    };

    let mut filled = 0usize;
    loop {
        // C: chiaki_stop_pipe_select_single(stop_pipe, sock, false, wait_timeout_ms)
        let wait_ms = match deadline {
            None => u64::MAX,
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    return Err(ChiakiError::Timeout);
                }
                (d - now).as_millis() as u64
            }
        };
        if let Some(sp) = stop_pipe {
            sp.check()?;
        }

        let poll = if stop_pipe.is_some() {
            RECV_POLL.min(Duration::from_millis(wait_ms.max(1)))
        } else {
            Duration::from_millis(wait_ms.max(1))
        };
        sock.set_read_timeout(Some(poll))
            .map_err(|_| ChiakiError::Network)?;

        let n = match sock.read(&mut buf[filled..]) {
            Ok(0) => return Err(ChiakiError::Disconnected),
            Ok(n) => n,
            // C: WSAEWOULDBLOCK -> continue (Deadline läuft oben weiter)
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue
            }
            Err(_) => return Err(ChiakiError::Network),
        };

        for i in filled..filled + n {
            match buf[i] {
                b'\r' => nl_state = TRANSITIONS_R[nl_state],
                b'\n' => nl_state = TRANSITIONS_N[nl_state],
                _ => nl_state = 0,
            }
            if nl_state == 4 {
                let header_size = i + 1;
                let received_size = filled + n;
                return Ok((header_size, received_size));
            }
        }
        filled += n;
    }
}

/// `strtol(buf, NULL, 10)`-Semantik (Whitespace überspringen, optionales
/// Vorzeichen, Dezimalziffern, saturierend) für den Status-Code.
fn strtol_i32(buf: &[u8]) -> i32 {
    let mut i = 0usize;
    while i < buf.len() && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut sign = 1i64;
    if i < buf.len() && (buf[i] == b'+' || buf[i] == b'-') {
        if buf[i] == b'-' {
            sign = -1;
        }
        i += 1;
    }
    let mut val = 0i64;
    while i < buf.len() && buf[i].is_ascii_digit() {
        val = val.saturating_mul(10).saturating_add((buf[i] - b'0') as i64);
        i += 1;
    }
    (sign * val).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, TcpListener};
    use std::sync::Arc;
    use std::thread;

    const RESPONSE_CRLF: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-type: text/html, text, plain\r\nUltimate Ability: Gamer\r\n\r\n";
    const RESPONSE_LF: &[u8] =
        b"HTTP/1.1 200 Ok\nContent-type: text/html, text, plain\nUltimate Ability:Gamer\n";

    /// Port von test/http.c "/response_parse" (Parameter crlf/lf).
    ///
    /// Das C testet zusätzlich die (umgekehrte) Listenreihenfolge der
    /// prepend-Kette; in Rust liegt der Vektor in Dokumentreihenfolge —
    /// inhaltlich werden dieselben Header geprüft.
    fn assert_response_parse(buf: &[u8]) {
        let parsed = response_parse(buf).expect("response_parse");
        assert_eq!(parsed.code, 200);

        let headers = &parsed.headers;
        assert_eq!(headers.len(), 2);

        // Reihenfolge wie im Dokument; das C-Original iteriert die Liste
        // rückwärts und sieht "Ultimate Ability" zuerst.
        assert_eq!(headers[0].key, "Content-type");
        assert_eq!(headers[0].value, "text/html, text, plain");
        assert_eq!(headers[1].key, "Ultimate Ability");
        assert_eq!(headers[1].value, "Gamer");
    }

    #[test]
    fn response_parse_crlf() {
        assert_response_parse(RESPONSE_CRLF);
    }

    #[test]
    fn response_parse_lf() {
        assert_response_parse(RESPONSE_LF);
    }

    #[test]
    fn response_parse_rejects_bad_input() {
        // kein HTTP/1.1-Prefix
        assert_eq!(
            response_parse(b"HTTP/1.0 200 OK\r\n\r\n").unwrap_err(),
            ChiakiError::InvalidData
        );
        // zu kurz
        assert_eq!(response_parse(b"HTT").unwrap_err(), ChiakiError::InvalidData);
        // keine Status-Zeile
        assert_eq!(
            response_parse(b"HTTP/1.1 \r\nX: y\r\n").unwrap_err(),
            ChiakiError::InvalidData
        );
        // Code 0
        assert_eq!(
            response_parse(b"HTTP/1.1 000 x\nA: b\n").unwrap_err(),
            ChiakiError::InvalidData
        );
        // Status-Zeile, aber kein Byte danach
        assert_eq!(
            response_parse(b"HTTP/1.1 200 OK\n").unwrap_err(),
            ChiakiError::InvalidData
        );
    }

    #[test]
    fn header_parse_errors() {
        // leerer Key
        assert_eq!(header_parse(b": value\n").unwrap_err(), ChiakiError::InvalidData);
        // Zeile ohne ':' (mehr als 1 Zeichen)
        assert_eq!(
            header_parse(b"garbage\r\nKey: v\n").unwrap_err(),
            ChiakiError::InvalidData
        );
        // ':' am Pufferende
        assert_eq!(header_parse(b"Key:").unwrap_err(), ChiakiError::InvalidData);
        // ": " am Pufferende (Space konsumiert, danach Ende)
        assert_eq!(header_parse(b"Key: ").unwrap_err(), ChiakiError::InvalidData);

        // C-Quirks (1:1 portiert): das Zeichen direkt nach ':' (bzw. nach
        // ": ") wird nie geprüft — der "leerer Value"-Fehler des C-Codes ist
        // damit unerreichbar. "Key:\n" liefert daher keinen Fehler, sondern
        // gar keinen Header (der Value-Start wird konsumiert, Puffer endet);
        // "Key:\r\n" liefert den Value "\r".
        assert_eq!(header_parse(b"Key:\n").unwrap(), Vec::<HttpHeader>::new());
        assert_eq!(header_parse(b"Key: \n").unwrap(), Vec::<HttpHeader>::new());
        let h = header_parse(b"Key:\r\n").unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].value, "\r");
    }

    #[test]
    fn header_parse_edge_cases_from_c_flow() {
        // 1-Zeichen-Zeile ohne ':' wird übersprungen (C: key_ptr + 1 < buf)
        let h = header_parse(b"x\nKey: v\n").unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].key, "Key");
        assert_eq!(h[0].value, "v");

        // Nur EIN Space direkt nach ':' wird übersprungen (wie im C)
        let h = header_parse(b"Key:  double space\n").unwrap();
        assert_eq!(h[0].value, " double space");

        // NUL beendet das Parsing
        let buf = b"Key: v\n\x00Key2: v2\n".to_vec();
        let h = header_parse(&buf).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].key, "Key");

        // Mehrzeiliger Standardfall
        let buf = b"A: 1\nB: 2\r\nC: 3\r\n\r\n".to_vec();
        let h = header_parse(&buf).unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h[1].key, "B");
        assert_eq!(h[1].value, "2");

        // Unvollständiger letzter Header wird verworfen (wie im C)
        let h = header_parse(b"A: 1\nB: no-newline").unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].key, "A");
    }

    /// Startet einen TCP-"Server", der `chunks` nacheinander (mit Pausen)
    /// schreibt; liefert die Client-Seite zurück.
    fn serve_chunks(chunks: Vec<Vec<u8>>) -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            use std::io::Write;
            for (i, c) in chunks.iter().enumerate() {
                if i > 0 {
                    thread::sleep(Duration::from_millis(30));
                }
                let _ = sock.write_all(c);
            }
            // Socket offen halten, bis der Test fertig gelesen hat
            thread::sleep(Duration::from_millis(1000));
        });
        TcpStream::connect(addr).unwrap()
    }

    #[test]
    fn recv_http_header_across_chunks() {
        let mut sock = serve_chunks(vec![
            b"HTTP/1.1 200 OK\r\n".to_vec(),
            b"Content-Length: 5\r\n".to_vec(),
            // Header-Ende und Body-Anfang im selben Chunk
            b"\r\nHELLO".to_vec(),
        ]);

        let mut buf = [0u8; 256];
        let (header_size, received_size) =
            recv_http_header(&mut sock, &mut buf, None, u64::MAX).expect("recv header");
        assert_eq!(
            &buf[..header_size],
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"
        );
        // Body-Bytes bleiben wie im C im Puffer hinter dem Header liegen
        assert_eq!(received_size, header_size + 5);
        assert_eq!(&buf[header_size..received_size], b"HELLO");
    }

    #[test]
    fn recv_http_header_ends_exactly_at_header() {
        // Body kommt erst später: received_size == header_size
        let mut sock = serve_chunks(vec![
            b"HTTP/1.1 200 OK\r\nX: y\r\n".to_vec(),
            b"\r\n".to_vec(),
            b"BODY".to_vec(),
        ]);
        let mut buf = [0u8; 256];
        let (header_size, received_size) =
            recv_http_header(&mut sock, &mut buf, None, u64::MAX).expect("recv header");
        assert_eq!(&buf[..header_size], b"HTTP/1.1 200 OK\r\nX: y\r\n\r\n");
        assert_eq!(received_size, header_size);
    }

    #[test]
    fn recv_http_header_disconnect_without_header_end() {
        let mut sock = serve_chunks(vec![b"HTTP/1.1 200 OK\r\n".to_vec()]);
        let mut buf = [0u8; 64];
        let err = recv_http_header(&mut sock, &mut buf, None, u64::MAX).expect_err("disconnected");
        assert_eq!(err, ChiakiError::Disconnected);
    }

    #[test]
    fn recv_http_header_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (_sock, _) = listener.accept().unwrap();
            // sendet nichts
            thread::sleep(Duration::from_millis(400));
        });
        let mut sock = TcpStream::connect(addr).unwrap();
        let mut buf = [0u8; 64];
        let t = Instant::now();
        let err = recv_http_header(&mut sock, &mut buf, Some(&StopPipe::new()), 100)
            .expect_err("timeout expected");
        assert_eq!(err, ChiakiError::Timeout);
        assert!(t.elapsed() >= Duration::from_millis(90));
    }

    #[test]
    fn recv_http_header_canceled_via_stop_pipe() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (_sock, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(500));
        });
        let mut sock = TcpStream::connect(addr).unwrap();
        let stop = Arc::new(StopPipe::new());
        let stop2 = Arc::clone(&stop);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            stop2.stop();
        });
        let mut buf = [0u8; 64];
        let err =
            recv_http_header(&mut sock, &mut buf, Some(&stop), u64::MAX).expect_err("canceled");
        assert_eq!(err, ChiakiError::Canceled);
    }

    #[test]
    fn strtol_i32_semantics() {
        assert_eq!(strtol_i32(b"200 OK\r\n"), 200);
        assert_eq!(strtol_i32(b"  42\n"), 42);
        assert_eq!(strtol_i32(b"-7 rest"), -7);
        assert_eq!(strtol_i32(b"+9"), 9);
        assert_eq!(strtol_i32(b"abc"), 0);
        assert_eq!(strtol_i32(b""), 0);
        assert_eq!(strtol_i32(b"999999999999"), i32::MAX);
    }
}
