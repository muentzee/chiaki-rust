// tkproto: prost-generierte Typen für takion.proto (1:1 aus chiaki-ng).
// C-Referenz: F:\projekte\chiaki-rust-remaster\lib\set/@nanopb/takion.proto +
// lib/src/pb_utils.h (nanopb-Callback-Helper). build.rs kompiliert die .proto
// nach OUT_DIR; die generierte Datei ist tkproto.rs.
//
// # pb_utils.h-Portierungsnotiz
//
// nanopb arbeitet bei Strings/Bytes/Repeated-Listen mit Callback-Feldern
// (`pb_callback_t` mit `funcs.encode`/`funcs.decode`); prost erzeugt direkte
// Rust-Typen. Die C-Helper aus lib/src/pb_utils.h entfallen daher bewusst:
//
// - `chiaki_pb_encode_string` (string-Feld aus char*): entfällt — prost
//   generiert `String` und encodiert selbst (Nutzung z. B.
//   streamconnection.c:1066 `msg.big_payload.session_key.funcs.encode`).
// - `chiaki_pb_encode_buf` / `chiaki_pb_encode_zero_encrypted_key` (bytes-Feld
//   aus Buf): entfällt — prost generiert `Vec<u8>`. Das Zero-Füllen des
//   encrypted_key (streamconnection.c:986/1069) geschieht künftig vor dem
//   Encode auf dem Vec, nicht mehr im Encoder-Callback.
// - `chiaki_pb_decode_buf` (bytes in festen Puffer, mit max_size-Limit):
//   entfällt — prost generiert `Vec<u8>`, die Allokation übernimmt prost.
//   Das max_size-Limit fehlt, ist aber protokollseitig irrelevant, da die
//   Takion-Paketgröße ohnehin durch die MTU begrenzt ist.
// - `chiaki_pb_decode_buf_alloc` (bytes in malloc-Puffer): entfällt —
//   prost generiert `Vec<u8>` (streamconnection.c:871
//   `resolution.video_header`).
// - `chiaki_pb_encode_list` + `List`-Struct (bis zu 4 int32 als *ungepacktes*
//   repeated varint, senkusha.c:788 `supported_takion_versions`): entfällt —
//   prost generiert `Vec<u32>` und schreibt für proto2-repeated ebenfalls
//   ungepackt; prost decodiert zusätzlich auch gepacktes Encoding. Wire-
//   identisch zum nanopb-Format (siehe Test `nanopb_compat_takion_protocol_request`).
//
// # proto2-required und prost
//
// prost *encodiert* required-Felder immer (auch bei Default-Wert 0), verhält
// sich also wire-kompatibel zu nanopb (nanopb schreibt required ebenfalls
// immer). Beim *Dekodieren* erzwingt prost fehlende required-Felder jedoch
// nicht (nanopb liefert dort einen Decode-Fehler). Für Kompatibilität mit dem
// C-Verhalten erzwingt `decode_takion_message` das Top-Level-required-Feld 1
// (`type`) per Wire-Scan.
//
// Die Feldnummern/Wiretypes kommen unverändert aus takion.proto und sind damit
// mit dem nanopb-Client identisch; die Tests unten bauen das nanopb-Byte-
// Layout von Hand und verifizieren Byte-Gleichheit mit der prost-Ausgabe.

#[allow(clippy::all)]
#[allow(missing_docs)]
pub mod tkproto {
    include!(concat!(env!("OUT_DIR"), "/tkproto.rs"));
}

pub use tkproto::*;

/// Liest ein base-128-Varint vom Pufferanfang und gibt (Wert, gelesene Bytes)
/// zurück.
fn read_varint(buf: &[u8]) -> Result<(u64, usize), prost::DecodeError> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if shift >= 64 {
            return Err(prost::DecodeError::new("invalid varint: too long"));
        }
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok((value, i + 1));
        }
        shift += 7;
    }
    Err(prost::DecodeError::new("invalid varint: truncated"))
}

/// Wire-Format-Scan: prüft, ob ein (varint-codiertes) Feld mit der Nummer
/// `field_no` in den rohen protobuf-Bytes vorkommt. Erkennt truncation und
/// ungültige Wire-Types als Fehler. proto2-Groups (Wire-Type 3/4) kommen im
/// chiaki-Protokoll nicht vor und werden als Fehler behandelt.
fn wire_has_varint_field(data: &[u8], field_no: u32) -> Result<bool, prost::DecodeError> {
    let mut buf = data;
    while !buf.is_empty() {
        let (key, n) = read_varint(buf)?;
        buf = &buf[n..];
        let tag = (key >> 3) as u32;
        let wire_type = (key & 0x7) as u8;
        if tag == 0 {
            return Err(prost::DecodeError::new("invalid protobuf tag 0"));
        }
        if tag == field_no {
            // Presence zählt unabhängig vom Wire-Type des konkreten Feldes.
            return Ok(true);
        }
        match wire_type {
            0 => {
                let (_, n) = read_varint(buf)?;
                buf = &buf[n..];
            }
            1 => {
                buf = buf
                    .get(8..)
                    .ok_or_else(|| prost::DecodeError::new("buffer underflow"))?;
            }
            2 => {
                let (len, n) = read_varint(buf)?;
                buf = &buf[n..];
                let len = usize::try_from(len)
                    .map_err(|_| prost::DecodeError::new("varint length overflow"))?;
                if buf.len() < len {
                    return Err(prost::DecodeError::new("buffer underflow"));
                }
                buf = &buf[len..];
            }
            5 => {
                buf = buf
                    .get(4..)
                    .ok_or_else(|| prost::DecodeError::new("buffer underflow"))?;
            }
            3 | 4 => return Err(prost::DecodeError::new("groups not supported")),
            _ => return Err(prost::DecodeError::new("invalid wire type")),
        }
    }
    Ok(false)
}

/// Dekodiert eine TakionMessage und erzwingt — wie nanopb im C-Client — das
/// `required`-Feld 1 (`type`). prost allein würde eine fehlende `type`
/// stillschweigend als BIG(0) deuten.
pub fn decode_takion_message(data: &[u8]) -> Result<TakionMessage, prost::DecodeError> {
    use prost::Message as _;
    if !wire_has_varint_field(data, 1)? {
        return Err(prost::DecodeError::new(
            "tkproto.TakionMessage: missing required field 1 (type)",
        ));
    }
    TakionMessage::decode(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    /// Hängt ein varint-codiertes Feld (tag, wire type 0) an einen Vektor.
    fn push_varint_field(out: &mut Vec<u8>, field_no: u32, value: u64) {
        push_varint(out, u64::from(field_no) << 3);
        push_varint(out, value);
    }

    /// Hängt ein length-delimited Feld (tag, wire type 2) an einen Vektor.
    fn push_len_delimited(out: &mut Vec<u8>, field_no: u32, payload: &[u8]) {
        push_varint(out, (u64::from(field_no) << 3) | 2);
        push_varint(out, payload.len() as u64);
        out.extend_from_slice(payload);
    }

    fn push_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }

    fn sample_big_payload() -> BigPayload {
        BigPayload {
            client_version: 9,
            session_key: "session-key-test".to_owned(),
            launch_spec: "launch-spec-test".to_owned(),
            encrypted_key: vec![0x11, 0x22, 0x33, 0x44],
            ecdh_pub_key: Some(vec![0xaa; 8]),
            ecdh_sig: Some(vec![0xbb; 4]),
        }
    }

    fn sample_takion_message() -> TakionMessage {
        TakionMessage {
            r#type: takion_message::PayloadType::Big.into(),
            big_payload: Some(sample_big_payload()),
            ..Default::default()
        }
    }

    /// Rekonstruiert die prost-Tags aus takion_message::PayloadType — die
    /// Nummern müssen denen der .proto (= nanopb-Deskriptor) entsprechen.
    #[test]
    fn payload_type_discriminants_match_proto() {
        let cases = [
            (takion_message::PayloadType::Big, 0u32),
            (takion_message::PayloadType::Bang, 1),
            (takion_message::PayloadType::Info, 2),
            (takion_message::PayloadType::Heartbeat, 3),
            (takion_message::PayloadType::Packetloss, 4),
            (takion_message::PayloadType::Corruptframe, 5),
            (takion_message::PayloadType::Cursor, 6),
            (takion_message::PayloadType::Timer, 7),
            (takion_message::PayloadType::Disconnect, 8),
            (takion_message::PayloadType::Log, 9),
            (takion_message::PayloadType::Headerrequest, 10),
            (takion_message::PayloadType::Debug, 11),
            (takion_message::PayloadType::Senkusha, 12),
            (takion_message::PayloadType::Streaminfo, 13),
            (takion_message::PayloadType::Streaminfoack, 14),
            (takion_message::PayloadType::Xmbcommand, 15),
            (takion_message::PayloadType::Connectionquality, 16),
            (takion_message::PayloadType::Clientmetric, 17),
            (takion_message::PayloadType::Playtimeleft, 18),
            (takion_message::PayloadType::Servermessage, 19),
            (takion_message::PayloadType::Fpschange, 20),
            (takion_message::PayloadType::Controllerconnection, 21),
            (takion_message::PayloadType::Clientinfo, 22),
            (takion_message::PayloadType::Videocapture, 23),
            (takion_message::PayloadType::Audiocapture, 24),
            (takion_message::PayloadType::Idrrequest, 25),
            (takion_message::PayloadType::Gktrace, 26),
            (takion_message::PayloadType::Periodictimestamp, 27),
            (takion_message::PayloadType::Serversettings, 28),
            (takion_message::PayloadType::Directmessage, 29),
            (takion_message::PayloadType::Micconnection, 30),
            (takion_message::PayloadType::Takionprotocolrequest, 31),
            (takion_message::PayloadType::Takionprotocolrequestack, 32),
        ];
        for (variant, expected) in cases {
            assert_eq!(i32::from(variant), expected as i32);
        }
    }

    /// Encode/Decode-Roundtrip eines voll befüllten TakionMessage
    /// (type=BIG, big_payload mit allen Feldern, inkl. optionals).
    #[test]
    fn roundtrip_full_big_payload() {
        let msg = sample_takion_message();
        let bytes = msg.encode_to_vec();
        let decoded = TakionMessage::decode(bytes.as_slice()).expect("decode");
        assert_eq!(decoded, msg);

        // Alle Felder einzeln gegenprüfen.
        let big = decoded.big_payload.as_ref().expect("big_payload");
        assert_eq!(big.client_version, 9);
        assert_eq!(big.session_key, "session-key-test");
        assert_eq!(big.launch_spec, "launch-spec-test");
        assert_eq!(big.encrypted_key, vec![0x11, 0x22, 0x33, 0x44]);
        assert_eq!(big.ecdh_pub_key.as_deref(), Some([0xaa; 8].as_slice()));
        assert_eq!(big.ecdh_sig.as_deref(), Some([0xbb; 4].as_slice()));
        assert_eq!(decoded.r#type, i32::from(takion_message::PayloadType::Big));
        assert_eq!(
            takion_message::PayloadType::try_from(decoded.r#type),
            Ok(takion_message::PayloadType::Big)
        );
    }

    /// Roundtrip eines SenkushaPayloads mit verschachtelter Message und
    /// optionalen uint32-Feldern (deckt die restlichen protobuf-Formen ab).
    #[test]
    fn roundtrip_nested_senkusha_payload() {
        let msg = TakionMessage {
            r#type: takion_message::PayloadType::Senkusha.into(),
            senkusha_payload: Some(SenkushaPayload {
                command: senkusha_payload::Command::MtuCommand.into(),
                mtu_command: Some(SenkushaMtuCommand {
                    id: 42,
                    mtu_req: 1500,
                    mtu_sent: Some(1400),
                    num: Some(10),
                    send_delay: Some(200),
                    delta: Some(7),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = msg.encode_to_vec();
        let decoded = TakionMessage::decode(bytes.as_slice()).expect("decode");
        assert_eq!(decoded, msg);
    }

    /// Unbekannte Felder (höhere Tag-Nummern, verschiedene Wire-Types) müssen
    /// beim Dekodieren ignoriert werden — für Forward-Kompatibilität mit
    /// neueren Servern (skip_field im prost, wie in nanopb).
    #[test]
    fn unknown_fields_are_ignored() {
        let msg = sample_takion_message();
        let mut bytes = msg.encode_to_vec();

        // Unbekanntes varint-Feld 1007 = 42: key varint (1007<<3)|0 = 8056
        // = [0xf8, 0x3e], value 0x2a.
        bytes.extend_from_slice(&[0xf8, 0x3e, 0x2a]);
        // Unbekanntes length-delimited Feld 55 mit 2 Bytes:
        // key (55<<3)|2 = 442 = [0xba, 0x03], len 2, [0x01, 0x02].
        bytes.extend_from_slice(&[0xba, 0x03, 0x02, 0x01, 0x02]);
        // Unbekanntes 64-bit-Feld 1010: key (1010<<3)|1 = 8081 = [0xf1, 0x3e],
        // 8 feste Bytes.
        bytes.extend_from_slice(&[0xf1, 0x3e, 0, 0, 0, 0, 0, 0, 0, 0]);

        let decoded = TakionMessage::decode(bytes.as_slice()).expect("decode with unknown fields");
        assert_eq!(decoded, msg);
    }

    /// nanopb-kompatibles Byte-Layout von Hand gebaut (wie der C-Client via
    /// pb_encode_serialized_data schreibt): TakionMessage{type=BIG,
    /// big_payload=...}. prost muss das identisch dekodieren und prosts
    /// eigene Ausgabe muss bytegleich sein — das ist der eigentliche
    /// Wire-Kompatibilitäts-Nachweis (gleiche Feldnummern, gleiche
    /// Wire-Types, required wird immer geschrieben).
    #[test]
    fn nanopb_compat_manual_big_payload_bytes() {
        // BigPayload innerhalb (Feldnummern laut .proto):
        //  1 client_version varint  = 0x08 0x09
        //  2 session_key string     = 0x12 <len> ...
        //  3 launch_spec string     = 0x1a <len> ...
        //  4 encrypted_key bytes    = 0x22 <len> ...
        //  5 ecdh_pub_key bytes     = 0x2a <len> ... (optional, gesetzt)
        //  6 ecdh_sig bytes         = 0x32 <len> ... (optional, gesetzt)
        let mut inner = Vec::new();
        push_varint_field(&mut inner, 1, 9);
        push_len_delimited(&mut inner, 2, b"session-key-test");
        push_len_delimited(&mut inner, 3, b"launch-spec-test");
        push_len_delimited(&mut inner, 4, &[0x11, 0x22, 0x33, 0x44]);
        push_len_delimited(&mut inner, 5, &[0xaa; 8]);
        push_len_delimited(&mut inner, 6, &[0xbb; 4]);

        // TakionMessage außen: 1 type varint BIG=0, 2 big_payload embedded.
        let mut manual = Vec::new();
        push_varint_field(&mut manual, 1, 0); // BIG = 0 — nanopb schreibt required immer
        push_len_delimited(&mut manual, 2, &inner);

        // Dekodieren der Handgebauten Bytes.
        let decoded = decode_takion_message(&manual).expect("decode manual nanopb bytes");
        assert_eq!(decoded.r#type, i32::from(takion_message::PayloadType::Big));
        let expected = sample_takion_message();
        assert_eq!(decoded, expected);

        // Und umgekehrt: prost-Encode muss byteidentisch zum nanopb-Layout sein.
        let encoded = expected.encode_to_vec();
        assert_eq!(
            encoded, manual,
            "prost-Encoding weicht vom nanopb-Byte-Layout ab"
        );

        // Strict-Decode akzeptiert die gültigen Bytes selbstverständlich.
        assert!(decode_takion_message(&encoded).is_ok());
    }

    /// nanopb-Compat für `chiaki_pb_encode_list` (senkusha.c:788): repeated
    /// uint32 wird von nanopb *ungepackt* (jedes Element mit eigenem Tag 1
    /// wire type 0) geschrieben — prost decodiert das und encodiert
    /// ebenfalls ungepackt.
    #[test]
    fn nanopb_compat_takion_protocol_request() {
        // TakionMessage: 1 type = TAKIONPROTOCOLREQUEST(31), 31 payload.
        // Payload: wiederholtes 1 varint: Werte 4 und 5, jeweils 0x08 <v>.
        let mut inner = Vec::new();
        push_varint_field(&mut inner, 1, 4);
        push_varint_field(&mut inner, 1, 5);

        let mut manual = Vec::new();
        push_varint_field(&mut manual, 1, 31);
        push_len_delimited(&mut manual, 31, &inner);

        let decoded = decode_takion_message(&manual).expect("decode manual bytes");
        assert_eq!(
            decoded.r#type,
            i32::from(takion_message::PayloadType::Takionprotocolrequest)
        );
        let req = decoded.takion_protocol_request.as_ref().expect("payload");
        assert_eq!(req.supported_takion_versions, vec![4, 5]);

        // prost-Encode: bytegleich (ungepackt, wie nanopb).
        let msg = TakionMessage {
            r#type: takion_message::PayloadType::Takionprotocolrequest.into(),
            takion_protocol_request: Some(TakionProtocolRequestPayload {
                supported_takion_versions: vec![4, 5],
            }),
            ..Default::default()
        };
        assert_eq!(msg.encode_to_vec(), manual);
    }

    /// Verletzung eines required-Feldes → Fehler. prost selbst erzwingt
    /// required nicht (der Test dokumentiert das explizit), daher lehnt
    /// `decode_takion_message` die Bytes ab wie nanopb ("missing required
    /// field").
    #[test]
    fn missing_required_type_is_error() {
        // Nur Feld 2 (BigPayload mit client_version=1), kein Feld 1 (type):
        // [0x12, 0x02, 0x08, 0x01].
        let bytes = [0x12u8, 0x02, 0x08, 0x01];

        // prost roh: akzeptiert fehlendes required (bekannte Lücke).
        assert!(TakionMessage::decode(&bytes[..]).is_ok());

        // Strict-Wrapper: Fehler wie bei nanopb.
        let err = decode_takion_message(&bytes).expect_err("required violation must fail");
        assert!(err.to_string().contains("required field 1"));

        // Auch komplett leere Bytes werden abgelehnt.
        assert!(decode_takion_message(&[]).is_err());

        // Valid: mit Feld 1 funktioniert derselbe Rest.
        let mut ok = Vec::new();
        push_varint_field(&mut ok, 1, 0);
        ok.extend_from_slice(&bytes);
        let decoded = decode_takion_message(&ok).expect("valid message");
        assert_eq!(decoded.r#type, i32::from(takion_message::PayloadType::Big));
        assert_eq!(
            decoded.big_payload.as_ref().expect("payload").client_version,
            1
        );
    }

    /// Der Wire-Scan des Strict-Decoders erkennt truncation/unsinnige Bytes
    /// als Fehler statt als fehlendes Feld.
    #[test]
    fn wire_scan_rejects_truncated_input() {
        // Tag 2, length 200 — aber keine 200 Bytes vorhanden.
        assert!(wire_has_varint_field(&[0x12, 0xc8], 1).is_err());
        // Truncated varint im Wert des *gesuchten* Felds: der Scan meldet
        // Presence (Tag gefunden), der anschließende prost-Decode schlägt
        // aber an derselben Stelle fehl — Fehler nicht beseitigt.
        assert!(wire_has_varint_field(&[0x08, 0x80], 1).is_ok());
        assert!(decode_takion_message(&[0x08, 0x80]).is_err());
        // Tag 0 ist ungültig im protobuf-Format.
        assert!(wire_has_varint_field(&[0x00], 1).is_err());
    }
}
