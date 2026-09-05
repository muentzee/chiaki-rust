// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/takion.c + lib/include/chiaki/takion.h (chiaki-ng).
//
// "VERY similar to SCTP, see RFC 4960"
//
// Threading-Modell (1:1 zum C):
// - EIN Recv-Thread (takion_thread_func): macht den INIT/COOKIE-Handshake,
//   initialisiert data_queue (32 bit) + video_queue (16 bit, lazy beim ersten
//   AV-Paket) und verarbeitet alle eingehenden Pakete.
// - EIN Send-Buffer-Thread (Resends, takion_send_buffer_thread_func-Port),
//   gestartet nach erfolgreichem Handshake, gestoppt+gejoint bevor das
//   Disconnect-Event gefeuert wird (wie chiaki_takion_send_buffer_fini im C).
// - Sends von beliebigen Threads über die Mutexe des C:
//   seq_num_local (seq_num_local_mutex), gkcrypt_local + key_pos_local
//   (gkcrypt_local_mutex), tag_remote, send_buffer. GKCrypt selbst ist
//   Arc<GKCrypt> und intern synchronisiert.
//
// Mutex-Ordnung (Deadlock-Freiheit): Es wird nie mehr als eine der Mutexe
// von TakionShared gleichzeitig gehalten; Callbacks werden grundsätzlich
// OHNE gehaltene Locks gefeuert. Die GKCrypt-internen Locks sind Blätter.
//
// Abweichungen zum C (dokumentiert):
// - Der C hält gkcrypt_local_mutex als REKURSIVEN Mutex
//   (takion_send_feedback_packet ruft chiaki_takion_crypt_advance_key_pos
//   darin auf). std::sync::Mutex ist nicht rekursiv; deshalb gibt es
//   *_locked-Interna, die den Guard erwarten.
// - recv-Warten: das C select()et auf StopPipe+Socket; hier blockiert recv
//   mit kurzem Read-Timeout (Poll-Quantum) und prüft die StopPipe pro
//   Iteration (Muster aus stoppipe.rs).
// - send_mic_packet nimmt &[u8] und klont intern (C verschlüsselt den
//   Buffer des Aufrufers in place).
// - chiaki_gkcrypt_encrypt/gmac(NULL) wäre im C UB; hier wird ohne
//   lokalen Crypt der Versand im Klartext fortgesetzt.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::error::{ChiakiError, ChiakiResult};
use crate::gkcrypt::{GKCrypt, GMAC_SIZE, GKCRYPT_BLOCK_SIZE, KeyState};
use crate::random;
use crate::reorderqueue::{DropStrategy, ReorderQueue, SeqNumSize};
use crate::sock;
use crate::stoppipe::StopPipe;
use crate::takionsendbuffer::{TakionSendBuffer, TAKION_SEND_BUFFER_SIZE};
use crate::time::{now_ms, now_us};

// ---------------------------------------------------------------------------
// Konstanten (takion.c)
// ---------------------------------------------------------------------------

const TAKION_A_RWND: u32 = 0x19000;
const TAKION_OUTBOUND_STREAMS: u16 = 0x64;
const TAKION_INBOUND_STREAMS: u16 = 0x64;

const TAKION_REORDER_QUEUE_SIZE_EXP: usize = 4; // => 16 entries
const TAKION_AV_VIDEO_REORDER_QUEUE_SIZE_EXP: usize = 6; // => 64 entries
/// ~1 frame at 60fps
const TAKION_AV_REORDER_TIMEOUT_US_DEFAULT: u32 = 16000;
const TAKION_POSTPONE_PACKETS_SIZE: usize = 32;

const TAKION_MESSAGE_HEADER_SIZE: usize = 0x10;

const TAKION_PACKET_BASE_TYPE_MASK: u8 = 0xf;

const TAKION_EXPECT_TIMEOUT_MS: u64 = 5000;
const MAX_CONNECT_RESEND_TRIES: usize = 3;

const TAKION_COOKIE_SIZE: usize = 0x20;

/// Empfangspuffer-Größe pro recv-Aufruf (C: malloc(1500)).
const TAKION_RECV_BUF_SIZE: usize = 1500;
/// Rust-spezifisch: Poll-Quantum des recv-Wartens (Read-Timeout), damit die
/// StopPipe regelmäßig geprüft wird (C: select auf StopPipe+Socket).
const TAKION_RECV_POLL_INTERVAL_MS: u64 = 16;

pub const V9_AV_HEADER_SIZE_VIDEO: usize = 0x17;
pub const V9_AV_HEADER_SIZE_AUDIO: usize = 0x12;

pub const V12_AV_HEADER_SIZE_VIDEO: usize = 0x17;
pub const V12_AV_HEADER_SIZE_AUDIO: usize = 0x13;

pub const V7_AV_HEADER_SIZE_BASE: usize = 0x12;
pub const V7_AV_HEADER_SIZE_VIDEO_ADD: usize = 0x3;
pub const V7_AV_HEADER_SIZE_NALU_INFO_STRUCTS_ADD: usize = 0x3;

/// CHIAKI_TAKION_CONGESTION_PACKET_SIZE
pub const CONGESTION_PACKET_SIZE: usize = 0xf;

/// CHIAKI_FEEDBACK_STATE_BUF_SIZE_MAX (feedback.h)
const FEEDBACK_STATE_BUF_SIZE_MAX: usize = 0x1c;

// ---------------------------------------------------------------------------
// Paket-Typen
// ---------------------------------------------------------------------------

/// Base type of Takion packets. Lower nibble of the first byte in datagrams.
/// Vollständige C-Tabelle (takion.h) — nicht alle Typen werden empfangsseitig
/// konstruiert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
enum TakionPacketType {
    Control = 0,
    FeedbackHistory = 1,
    Video = 2,
    Audio = 3,
    Handshake = 4,
    Congestion = 5,
    FeedbackState = 6,
    ClientInfo = 8,
}

/// @return The offset of the mac of size CHIAKI_GKCRYPT_GMAC_SIZE inside a
/// packet of type or -1 if unknown.
fn takion_packet_type_mac_offset(type_: u8) -> i32 {
    match type_ {
        t if t == TakionPacketType::Control as u8 => 5,
        t if t == TakionPacketType::Video as u8 || t == TakionPacketType::Audio as u8 => 0xa,
        t if t == TakionPacketType::Congestion as u8 => 7,
        _ => -1,
    }
}

/// @return The offset of the 4-byte key_pos inside a packet of type or -1 if
/// unknown.
fn takion_packet_type_key_pos_offset(type_: u8) -> i32 {
    match type_ {
        t if t == TakionPacketType::Control as u8 => 0x9,
        t if t == TakionPacketType::Video as u8 || t == TakionPacketType::Audio as u8 => 0xe,
        t if t == TakionPacketType::Congestion as u8 => 0xb,
        _ => -1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum TakionChunkType {
    Data = 0,
    Init = 1,
    InitAck = 2,
    DataAck = 3,
    Cookie = 0xa,
    CookieAck = 0xb,
}

/// Port von `ChiakiTakionMessageDataType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TakionMessageDataType {
    Protobuf = 0,
    Rumble = 7,
    PadInfo = 9,
    TriggerEffects = 11,
}

impl TryFrom<u8> for TakionMessageDataType {
    type Error = ChiakiError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(TakionMessageDataType::Protobuf),
            7 => Ok(TakionMessageDataType::Rumble),
            9 => Ok(TakionMessageDataType::PadInfo),
            11 => Ok(TakionMessageDataType::TriggerEffects),
            _ => Err(ChiakiError::InvalidData),
        }
    }
}

/// Port von `ChiakiDisableAudioVideo` (bits: 00/01/10/11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum DisableAudioVideo {
    #[default]
    NoneDisabled = 0,
    AudioDisabled = 1,
    VideoDisabled = 2,
    AudioVideoDisabled = 3,
}

impl DisableAudioVideo {
    pub fn bits(self) -> u8 {
        self as u8
    }

    fn contains(self, flag: u8) -> bool {
        (self.bits() & flag) != 0
    }
}

/// Port von `ChiakiTakionAVPacket`. `data` ist in Rust owned
/// (C: borrowed pointer into the received buffer).
#[derive(Debug, Clone, Default)]
pub struct AVPacket {
    pub packet_index: u16,
    pub frame_index: u16,
    pub uses_nalu_info_structs: bool,
    pub is_video: bool,
    pub is_haptics: bool,
    pub unit_index: u16,
    /// source + units_in_frame_fec
    pub units_in_frame_total: u16,
    pub units_in_frame_fec: u16,
    pub codec: u8,
    pub word_at_0x18: u16,
    pub adaptive_stream_index: u8,
    pub byte_at_0x2c: u8,

    pub key_pos: u64,

    pub data: Vec<u8>,
}

impl AVPacket {
    /// chiaki_takion_av_packet_audio_unit_size
    pub fn audio_unit_size(&self) -> u8 {
        (self.units_in_frame_fec >> 8) as u8
    }

    /// chiaki_takion_av_packet_audio_source_units_count
    pub fn audio_source_units_count(&self) -> u8 {
        (self.units_in_frame_fec & 0xf) as u8
    }

    /// chiaki_takion_av_packet_audio_fec_units_count
    pub fn audio_fec_units_count(&self) -> u8 {
        ((self.units_in_frame_fec >> 4) & 0xf) as u8
    }
}

/// Port von `ChiakiTakionCongestionPacket`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CongestionPacket {
    pub word_0: u16,
    pub received: u16,
    pub lost: u16,
}

/// Port von `ChiakiTakionEvent` (union -> enum).
#[derive(Debug)]
pub enum TakionEvent {
    Connected,
    Disconnect(ChiakiError),
    Data {
        data_type: TakionMessageDataType,
        buf: Vec<u8>,
    },
    DataAck {
        seq_num: u32,
    },
    Av(Box<AVPacket>),
}

/// Port von `ChiakiTakionConnectInfo`.
pub struct TakionConnectInfo {
    /// vorgeprüfte Zieladresse (C: sockaddr)
    pub host: SocketAddr,
    pub ip_dontfrag: bool,
    pub callback: Arc<dyn Fn(TakionEvent) + Send + Sync>,
    pub disable_audio_video: DisableAudioVideo,
    pub enable_crypt: bool,
    pub enable_dualsense: bool,
    pub protocol_version: u8,
    /// How long to wait (µs) for a missing head AV packet before skipping it.
    /// 0 = default (16 ms).
    pub av_reorder_timeout_us: u32,
}

// ---------------------------------------------------------------------------
// Message-Header-Parsing/-Aufbau
// ---------------------------------------------------------------------------

/// Port von `TakionMessage`.
struct TakionMessage<'p> {
    #[allow(dead_code)] // Teil der C-Struktur, wird beim Parsen befüllt
    tag: u32,
    #[allow(dead_code)] // Teil der C-Struktur, wird beim Parsen befüllt
    key_pos: u64,
    chunk_type: u8,
    chunk_flags: u8,
    payload_size: usize,
    payload: Option<&'p [u8]>,
}

/// Port von `takion_parse_message()` — `buf` beginnt NACH dem Basis-Typ-Byte.
fn takion_parse_message<'p>(
    buf: &'p [u8],
    tag_local: u32,
    key_state: &mut KeyState,
) -> ChiakiResult<TakionMessage<'p>> {
    if buf.len() < TAKION_MESSAGE_HEADER_SIZE {
        tracing::error!("Takion message received that is too short");
        return Err(ChiakiError::InvalidData);
    }

    let tag = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let key_pos_low = u32::from_be_bytes([buf[0x8], buf[0x9], buf[0xa], buf[0xb]]);
    let key_pos = key_state.request_pos(key_pos_low, true);
    let chunk_type = buf[0xc];
    let chunk_flags = buf[0xd];
    let payload_size =
        u16::from_be_bytes([buf[0xe], buf[0xf]]) as usize;

    if tag != tag_local {
        tracing::error!("Takion received message tag mismatch");
        return Err(ChiakiError::InvalidData);
    }

    if buf.len() != payload_size + 0xc {
        tracing::error!("Takion received message payload size mismatch");
        return Err(ChiakiError::InvalidData);
    }

    let payload_size = payload_size - 0x4;
    let payload = if payload_size > 0 {
        Some(&buf[0x10..0x10 + payload_size])
    } else {
        None
    };

    Ok(TakionMessage {
        tag,
        key_pos,
        chunk_type,
        chunk_flags,
        payload_size,
        payload,
    })
}

/// Port von `takion_write_message_header()`.
///
/// This includes chunk_type, chunk_flags and payload_size.
/// @param raw_payload_size size of the actual data of the payload excluding
/// type_a, type_b and payload_size
fn takion_write_message_header(
    buf: &mut [u8],
    tag: u32,
    key_pos: u64,
    chunk_type: TakionChunkType,
    chunk_flags: u8,
    payload_data_size: usize,
) {
    buf[0..4].copy_from_slice(&tag.to_be_bytes());
    buf[4..8].fill(0); // GMAC-Platzhalter
    buf[8..12].copy_from_slice(&(key_pos as u32).to_be_bytes());
    buf[0xc] = chunk_type as u8;
    buf[0xd] = chunk_flags;
    buf[0xe..0x10].copy_from_slice(&((payload_data_size + 4) as u16).to_be_bytes());
}

// ---------------------------------------------------------------------------
// AV-Packet-Parsing (v7/v9/v12)
// ---------------------------------------------------------------------------

pub type AvPacketParseFn = fn(&mut AVPacket, &mut KeyState, &[u8]) -> ChiakiResult<()>;

/// Port von `chiaki_takion_v9_av_packet_parse()`.
pub fn v9_av_packet_parse(
    packet: &mut AVPacket,
    key_state: &mut KeyState,
    buf: &[u8],
) -> ChiakiResult<()> {
    av_packet_parse(false, packet, key_state, buf)
}

/// Port von `chiaki_takion_v12_av_packet_parse()`.
pub fn v12_av_packet_parse(
    packet: &mut AVPacket,
    key_state: &mut KeyState,
    buf: &[u8],
) -> ChiakiResult<()> {
    av_packet_parse(true, packet, key_state, buf)
}

/// Gemeinsame Implementierung von v9/v12 (C: statisches av_packet_parse()).
fn av_packet_parse(
    v12: bool,
    packet: &mut AVPacket,
    key_state: &mut KeyState,
    buf: &[u8],
) -> ChiakiResult<()> {
    *packet = AVPacket::default();

    if buf.is_empty() {
        return Err(ChiakiError::BufTooSmall);
    }

    let base_type = buf[0] & TAKION_PACKET_BASE_TYPE_MASK;
    if base_type != TakionPacketType::Video as u8 && base_type != TakionPacketType::Audio as u8 {
        return Err(ChiakiError::InvalidData);
    }

    packet.is_video = base_type == TakionPacketType::Video as u8;
    packet.uses_nalu_info_structs = ((buf[0] >> 4) & 1) != 0;

    let mut av = &buf[1..];
    let av_header_size = if v12 {
        if packet.is_video {
            V12_AV_HEADER_SIZE_VIDEO
        } else {
            V12_AV_HEADER_SIZE_AUDIO
        }
    } else if packet.is_video {
        V9_AV_HEADER_SIZE_VIDEO
    } else {
        V9_AV_HEADER_SIZE_AUDIO
    };
    if av.len() < av_header_size + 1 {
        return Err(ChiakiError::BufTooSmall);
    }

    packet.packet_index = u16::from_be_bytes([av[0], av[1]]);
    packet.frame_index = u16::from_be_bytes([av[2], av[3]]);

    let dword_2 = u32::from_be_bytes([av[4], av[5], av[6], av[7]]);
    if packet.is_video {
        packet.unit_index = ((dword_2 >> 0x15) & 0x7ff) as u16;
        packet.units_in_frame_total = (((dword_2 >> 0xa) & 0x7ff) + 1) as u16;
        packet.units_in_frame_fec = (dword_2 & 0x3ff) as u16;
    } else {
        packet.unit_index = ((dword_2 >> 0x18) & 0xff) as u16;
        packet.units_in_frame_total = (((dword_2 >> 0x10) & 0xff) + 1) as u16;
        packet.units_in_frame_fec = (dword_2 & 0xffff) as u16;
    }

    packet.codec = av[8];
    let key_pos_low = u32::from_be_bytes([av[0xd], av[0xe], av[0xf], av[0x10]]);
    packet.key_pos = key_state.request_pos(key_pos_low, true);

    let unknown_1 = av[0x11];
    let _ = unknown_1;

    av = &av[0x11..];

    if packet.is_video {
        packet.word_at_0x18 = u16::from_be_bytes([av[0], av[1]]);
        packet.adaptive_stream_index = av[2] >> 5;
        av = &av[3..];
    } else {
        av = &av[1..];
        // unknown
    }

    // TODO: parsing for uses_nalu_info_structs (before: packet.byte_at_0x1a)

    if packet.is_video {
        packet.byte_at_0x2c = av[0];
        //av += 2;
    }

    if packet.uses_nalu_info_structs {
        av = &av[3..];
    }

    if v12 && !packet.is_video {
        packet.is_haptics = av[0] == 0x02;
        av = &av[1..];
    }

    packet.data = av.to_vec();

    Ok(())
}

/// Port von `chiaki_takion_v7_av_packet_format_header()`.
///
/// Liefert die Header-Größe (C: header_size_out).
pub fn v7_av_packet_format_header(buf: &mut [u8], packet: &AVPacket) -> ChiakiResult<usize> {
    let mut header_size = V7_AV_HEADER_SIZE_BASE;
    if packet.is_video {
        header_size += V7_AV_HEADER_SIZE_VIDEO_ADD;
    }
    if packet.uses_nalu_info_structs {
        header_size += V7_AV_HEADER_SIZE_NALU_INFO_STRUCTS_ADD;
    }

    if header_size > buf.len() {
        return Err(ChiakiError::BufTooSmall);
    }

    buf[0] = if packet.is_video {
        TakionPacketType::Video as u8
    } else {
        TakionPacketType::Audio as u8
    };
    if packet.uses_nalu_info_structs {
        buf[0] |= 0x10;
    }

    buf[1..3].copy_from_slice(&packet.packet_index.to_be_bytes());
    buf[3..5].copy_from_slice(&packet.frame_index.to_be_bytes());

    let dword_2 = (packet.units_in_frame_fec as u32 & 0x3ff)
        | (((packet.units_in_frame_total.wrapping_sub(1) as u32) & 0x7ff) << 0xa)
        | ((packet.unit_index as u32 & 0xffff) << 0x15);
    buf[5..9].copy_from_slice(&dword_2.to_be_bytes());

    buf[9] = packet.codec; // C: codec & 0xff (u8 -> no-op)

    buf[0xa..0xe].copy_from_slice(&[0; 4]); // unknown

    // C: *(uint32_t*)(buf + 0xe) = (uint32_t)packet->key_pos — NATIVE endian!
    buf[0xe..0x12].copy_from_slice(&(packet.key_pos as u32).to_le_bytes());

    let mut cur = 0x12;
    if packet.is_video {
        buf[cur..cur + 2].copy_from_slice(&packet.word_at_0x18.to_be_bytes());
        buf[cur + 2] = packet.adaptive_stream_index << 5;
        cur += 3;
    }

    if packet.uses_nalu_info_structs {
        buf[cur..cur + 2].copy_from_slice(&[0; 2]); // unknown
        buf[cur + 2] = 0; // unknown
    }

    Ok(header_size)
}

/// Port von `chiaki_takion_v7_av_packet_parse()`.
pub fn v7_av_packet_parse(
    packet: &mut AVPacket,
    _key_state: &mut KeyState, // C liest key_pos direkt (kein KeyState)
    buf: &[u8],
) -> ChiakiResult<()> {
    *packet = AVPacket::default();

    if buf.is_empty() {
        return Err(ChiakiError::BufTooSmall);
    }

    let base_type = buf[0] & TAKION_PACKET_BASE_TYPE_MASK;
    if base_type != TakionPacketType::Video as u8 && base_type != TakionPacketType::Audio as u8 {
        return Err(ChiakiError::InvalidData);
    }

    packet.is_video = base_type == TakionPacketType::Video as u8;
    packet.uses_nalu_info_structs = ((buf[0] >> 4) & 1) != 0;

    let mut header_size = V7_AV_HEADER_SIZE_BASE;
    if packet.is_video {
        header_size += V7_AV_HEADER_SIZE_VIDEO_ADD;
    }
    if packet.uses_nalu_info_structs {
        header_size += V7_AV_HEADER_SIZE_NALU_INFO_STRUCTS_ADD;
    }

    if buf.len() < header_size {
        return Err(ChiakiError::BufTooSmall);
    }

    packet.packet_index = u16::from_be_bytes([buf[1], buf[2]]);
    packet.frame_index = u16::from_be_bytes([buf[3], buf[4]]);

    let dword_2 = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]);
    packet.unit_index = ((dword_2 >> 0x15) & 0x7ff) as u16;
    packet.units_in_frame_total = (((dword_2 >> 0xa) & 0x7ff) + 1) as u16;
    packet.units_in_frame_fec = (dword_2 & 0x3ff) as u16;

    packet.codec = buf[9];
    // unknown buf[0xa..0xe]
    packet.key_pos = u32::from_be_bytes([buf[0xe], buf[0xf], buf[0x10], buf[0x11]]) as u64;

    let mut b = &buf[0x12..];

    if packet.is_video {
        packet.word_at_0x18 = u16::from_be_bytes([b[0], b[1]]);
        packet.adaptive_stream_index = b[2] >> 5;
        b = &b[3..];
    }

    if packet.uses_nalu_info_structs {
        b = &b[3..];
        // unknown
    }

    packet.data = b.to_vec();

    Ok(())
}

// ---------------------------------------------------------------------------
// MAC / Congestion-Format
// ---------------------------------------------------------------------------

/// Port von `chiaki_takion_packet_read_key_pos()` (static).
fn takion_packet_read_key_pos(buf: &[u8], key_state: &mut KeyState) -> ChiakiResult<u64> {
    if buf.is_empty() {
        return Err(ChiakiError::BufTooSmall);
    }

    let base_type = buf[0] & TAKION_PACKET_BASE_TYPE_MASK;
    let key_pos_offset = takion_packet_type_key_pos_offset(base_type);
    if key_pos_offset < 0 {
        return Err(ChiakiError::InvalidData);
    }
    let key_pos_offset = key_pos_offset as usize;

    if buf.len() < key_pos_offset + 4 {
        return Err(ChiakiError::BufTooSmall);
    }

    let key_pos_low = u32::from_be_bytes([
        buf[key_pos_offset],
        buf[key_pos_offset + 1],
        buf[key_pos_offset + 2],
        buf[key_pos_offset + 3],
    ]);
    Ok(key_state.request_pos(key_pos_low, false))
}

/// Port von `chiaki_takion_packet_mac()`.
///
/// Calculate the MAC for the packet depending on the type derived from the
/// first byte in buf. The MAC inside buf is replaced by the computed one
/// (after saving the old value to `mac_old_out`).
///
/// If crypt is None, the MAC is left 0.
pub fn packet_mac(
    crypt: Option<&GKCrypt>,
    buf: &mut [u8],
    key_pos: u64,
    mac_out: Option<&mut [u8; GMAC_SIZE]>,
    mac_old_out: Option<&mut [u8; GMAC_SIZE]>,
) -> ChiakiResult<()> {
    if buf.is_empty() {
        return Err(ChiakiError::BufTooSmall);
    }

    let base_type = buf[0] & TAKION_PACKET_BASE_TYPE_MASK;
    let mac_offset = takion_packet_type_mac_offset(base_type);
    let key_pos_offset = takion_packet_type_key_pos_offset(base_type);
    if mac_offset < 0 || key_pos_offset < 0 {
        return Err(ChiakiError::InvalidData);
    }
    let mac_offset = mac_offset as usize;
    let key_pos_offset = key_pos_offset as usize;

    if buf.len() < mac_offset + GMAC_SIZE || buf.len() < key_pos_offset + 4 {
        return Err(ChiakiError::BufTooSmall);
    }

    if let Some(out) = mac_old_out {
        out.copy_from_slice(&buf[mac_offset..mac_offset + GMAC_SIZE]);
    }

    buf[mac_offset..mac_offset + GMAC_SIZE].fill(0);

    if let Some(crypt) = crypt {
        let mut key_pos_tmp = [0u8; 4];
        let zero_key_pos = base_type == TakionPacketType::Control as u8
            || base_type == TakionPacketType::Congestion as u8;
        if zero_key_pos {
            key_pos_tmp.copy_from_slice(&buf[key_pos_offset..key_pos_offset + 4]);
            buf[key_pos_offset..key_pos_offset + 4].fill(0);
        }
        let mac = crypt.gmac(key_pos, buf)?;
        buf[mac_offset..mac_offset + GMAC_SIZE].copy_from_slice(&mac);
        if zero_key_pos {
            buf[key_pos_offset..key_pos_offset + 4].copy_from_slice(&key_pos_tmp);
        }
    }

    if let Some(out) = mac_out {
        out.copy_from_slice(&buf[mac_offset..mac_offset + GMAC_SIZE]);
    }

    Ok(())
}

/// Port von `chiaki_takion_format_congestion()`.
pub fn format_congestion(buf: &mut [u8; CONGESTION_PACKET_SIZE], packet: &CongestionPacket, key_pos: u64) {
    buf[0] = TakionPacketType::Congestion as u8;
    buf[1..3].copy_from_slice(&packet.word_0.to_be_bytes());
    buf[3..5].copy_from_slice(&packet.received.to_be_bytes());
    buf[5..7].copy_from_slice(&packet.lost.to_be_bytes());
    buf[7..11].copy_from_slice(&[0; 4]);
    buf[11..15].copy_from_slice(&(key_pos as u32).to_be_bytes());
}

// ---------------------------------------------------------------------------
// Takion
// ---------------------------------------------------------------------------

/// Lokaler Crypt-State (gkcrypt_local + key_pos_local hinter gkcrypt_local_mutex).
struct LocalCryptState {
    crypt: Option<Arc<GKCrypt>>,
    key_pos_local: u64,
}

struct TakionShared {
    version: u8,

    // Whether or not audio or video is disabled from further processing
    // beyond basic ack
    disable_audio_video: DisableAudioVideo,

    /// Whether encryption should be used.
    ///
    /// If false, encryption and MACs are disabled completely.
    ///
    /// If true, encryption and MACs will be used depending on whether
    /// gkcrypt_local and gkcrypt_remote are non-null, respectively. However,
    /// if gkcrypt_remote is null, only control data packets are passed to the
    /// callback and all other packets are postponed until gkcrypt_remote is
    /// set, so eventually all MACs will be checked.
    enable_crypt: bool,

    /// C: takion->enable_dualsense
    enable_dualsense: bool,

    /// C: takion->av_reorder_timeout_us (0-Handled im connect: Default 16 ms)
    av_reorder_timeout_us: u32,

    callback: Arc<dyn Fn(TakionEvent) + Send + Sync>,
    sock: UdpSocket,
    /// true wenn der Socket von Takion erstellt wurde (C: close_socket).
    /// In Rust schließt der Drop des geteilten Sockets ihn automatisch,
    /// das Flag bleibt nur zur Dokumentation der C-Semantik erhalten.
    #[allow(dead_code)]
    close_socket: bool,
    stop_pipe: StopPipe,
    tag_local: u32,
    av_packet_parse: AvPacketParseFn,

    // Advertised Receiver Window Credit
    a_rwnd: u32,

    seq_num_local: Mutex<u32>,
    gkcrypt_local: Mutex<LocalCryptState>,
    gkcrypt_remote: Mutex<Option<Arc<GKCrypt>>>,
    tag_remote: Mutex<u32>,

    send_buffer: Mutex<TakionSendBuffer>,
    /// Wakeup für den Send-Buffer-Thread (C: send_buffer.cond)
    send_cond: Condvar,
    resend_should_stop: AtomicBool,
    resend_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Port von `ChiakiTakion`.
pub struct Takion {
    shared: Arc<TakionShared>,
    recv_thread: Option<std::thread::JoinHandle<()>>,
}

impl Takion {
    /// Port von `chiaki_takion_connect()`.
    ///
    /// Baut den Socket (falls nicht mitgegeben), startet den Recv-Thread
    /// (der den INIT/COOKIE-Handshake durchführt und danach `Connected`
    /// feuert) und den Send-Buffer-Thread.
    pub fn connect(info: TakionConnectInfo, sock_: Option<UdpSocket>) -> ChiakiResult<Takion> {
        let version = info.protocol_version;
        let av_packet_parse = match version {
            7 => v7_av_packet_parse as AvPacketParseFn,
            9 => v9_av_packet_parse as AvPacketParseFn,
            12 => v12_av_packet_parse as AvPacketParseFn,
            _ => {
                tracing::error!("Unknown Takion Protocol Version {}", version);
                return Err(ChiakiError::InvalidData);
            }
        };

        let av_reorder_timeout_us = if info.av_reorder_timeout_us != 0 {
            info.av_reorder_timeout_us
        } else {
            TAKION_AV_REORDER_TIMEOUT_US_DEFAULT
        };

        let tag_local = random::random_32(); // 0x4823 im C-Kommentar
        let seq_num_local = tag_local;

        tracing::info!("Takion connecting (version {})", info.protocol_version);

        // ---- Socket ----
        let (sock, close_socket) = match sock_ {
            Some(s) => {
                let rcvbuf_val = TAKION_A_RWND as usize;
                sock::set_recv_buffer_size(&s, rcvbuf_val)?;
                sock::set_dont_fragment(&s, info.ip_dontfrag)?;
                if info.ip_dontfrag {
                    tracing::info!("Takion enabled Don't Fragment Bit");
                } else {
                    tracing::info!("Takion disabled Don't Fragment Bit");
                }

                // Stale Pakete einer PSN-Connection verwerfen
                match takion_read_extra_sock_messages(&s) {
                    Ok(())
                    | Err(ChiakiError::Timeout)
                    | Err(ChiakiError::Canceled) => {}
                    Err(e) => {
                        tracing::error!(
                            "Takion had problem reading extra messages from socket using PSN Connection"
                        );
                        return Err(e);
                    }
                }
                (s, false)
            }
            None => {
                let bind_addr: SocketAddr = if info.host.is_ipv6() {
                    "[::]:0".parse().unwrap()
                } else {
                    "0.0.0.0:0".parse().unwrap()
                };
                let opts = sock::UdpSocketOptions {
                    recv_buffer_size: Some(TAKION_A_RWND as usize),
                    dont_fragment: info.ip_dontfrag,
                    ..Default::default()
                };
                let s = sock::create_udp_socket(bind_addr, &opts)?;
                if info.ip_dontfrag {
                    tracing::info!("Takion enabled Don't Fragment Bit");
                } else {
                    tracing::info!("Takion disabled Don't Fragment Bit");
                }
                s.connect(info.host)
                    .map_err(|e| sock::map_io_error(&e))?;
                (s, true)
            }
        };

        let shared = Arc::new(TakionShared {
            version,
            disable_audio_video: info.disable_audio_video,
            enable_crypt: info.enable_crypt,
            enable_dualsense: info.enable_dualsense,
            av_reorder_timeout_us,
            callback: Arc::clone(&info.callback),
            sock,
            close_socket,
            stop_pipe: StopPipe::new(),
            tag_local,
            av_packet_parse,
            a_rwnd: TAKION_A_RWND,
            seq_num_local: Mutex::new(seq_num_local),
            gkcrypt_local: Mutex::new(LocalCryptState {
                crypt: None,
                key_pos_local: 0,
            }),
            gkcrypt_remote: Mutex::new(None),
            tag_remote: Mutex::new(0),
            send_buffer: Mutex::new(TakionSendBuffer::new(TAKION_SEND_BUFFER_SIZE)?),
            send_cond: Condvar::new(),
            resend_should_stop: AtomicBool::new(false),
            resend_thread: Mutex::new(None),
        });

        let shared_thread = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("Chiaki Takion".to_owned())
            .spawn(move || takion_thread_func(shared_thread))
            .map_err(|_| ChiakiError::Thread)?;

        Ok(Takion {
            shared,
            recv_thread: Some(thread),
        })
    }

    /// Port von `chiaki_takion_close()`: stoppt den Recv-Thread (Join) und
    /// räumt auf. Der Socket wird geschlossen, falls Takion ihn erstellt hat
    /// (Drop); ein mitgegebener Socket bleibt offen.
    pub fn close(&mut self) {
        self.shared.stop_pipe.stop();
        if let Some(t) = self.recv_thread.take() {
            let _ = t.join();
        }
    }

    /// Port von `chiaki_takion_set_crypt()`.
    ///
    /// Nur aus dem Takion-Callback-Thread heraus aufrufen (wie im C
    /// dokumentiert) — die Implementierung ist trotzdem thread-safe.
    pub fn set_crypt(
        &self,
        gkcrypt_local: Option<Arc<GKCrypt>>,
        gkcrypt_remote: Option<Arc<GKCrypt>>,
    ) {
        lock(&self.shared.gkcrypt_local).crypt = gkcrypt_local;
        *lock(&self.shared.gkcrypt_remote) = gkcrypt_remote;
    }

    /// Port von `chiaki_takion_crypt_advance_key_pos()`.
    ///
    /// Get a new key pos and advance by data_size.
    /// Thread-safe while Takion is running. Returns 0 if encryption is disabled.
    pub fn crypt_advance_key_pos(&self, data_size: usize) -> ChiakiResult<u64> {
        let mut g = lock(&self.shared.gkcrypt_local);
        crypt_advance_key_pos_locked(&mut g, data_size)
    }

    /// Port von `chiaki_takion_send_raw()`.
    ///
    /// Send a datagram directly on the socket. Thread-safe while Takion is running.
    pub fn send_raw(&self, buf: &[u8]) -> ChiakiResult<()> {
        self.shared.send_raw(buf)
    }

    /// Port von `chiaki_takion_send()`.
    ///
    /// Calculate the MAC for the packet depending on the type derived from the
    /// first byte in buf, assign MAC inside buf at the respective position and
    /// send the packet. If encryption is disabled, the MAC is left 0.
    pub fn send(&self, buf: &mut [u8], key_pos: u64) -> ChiakiResult<()> {
        self.shared.send(buf, key_pos)
    }

    /// Port von `chiaki_takion_send_message_data()`. Liefert die zugewiesene
    /// Seq-Num (C: out-Param). Thread-safe while Takion is running.
    pub fn send_message_data(&self, chunk_flags: u8, channel: u16, buf: &[u8]) -> ChiakiResult<u32> {
        self.shared.send_message_data_internal(chunk_flags, channel, buf, true)
    }

    /// Port von `chiaki_takion_send_message_data_cont()`.
    pub fn send_message_data_cont(
        &self,
        chunk_flags: u8,
        channel: u16,
        buf: &[u8],
    ) -> ChiakiResult<u32> {
        self.shared.send_message_data_internal(chunk_flags, channel, buf, false)
    }

    /// Port von `chiaki_takion_send_congestion()`.
    pub fn send_congestion(&self, packet: CongestionPacket) -> ChiakiResult<()> {
        let key_pos = self.crypt_advance_key_pos(CONGESTION_PACKET_SIZE)?;
        let mut buf = [0u8; CONGESTION_PACKET_SIZE];
        format_congestion(&mut buf, &packet, key_pos);
        self.send(&mut buf, key_pos)
    }

    /// Port von `chiaki_takion_send_feedback_state()`.
    pub fn send_feedback_state(
        &self,
        seq_num: u16,
        feedback_state: &crate::feedback::FeedbackState,
    ) -> ChiakiResult<()> {
        let mut buf = [0u8; 0xc + FEEDBACK_STATE_BUF_SIZE_MAX];
        buf[0] = TakionPacketType::FeedbackState as u8;
        buf[1..3].copy_from_slice(&seq_num.to_be_bytes());
        buf[3] = 0; // TODO
        // buf[4..8] = 0 (key pos), buf[8..12] = 0 (gmac)
        let buf_sz = if self.shared.version <= 9 {
            crate::feedback::feedback_state_format_v9(&mut buf[0xc..], feedback_state)?;
            0xc + crate::feedback::FEEDBACK_STATE_BUF_SIZE_V9
        } else {
            crate::feedback::feedback_state_format_v12(&mut buf[0xc..], feedback_state)?;
            0xc + crate::feedback::FEEDBACK_STATE_BUF_SIZE_V12
        };
        self.shared.send_feedback_packet(&mut buf[..buf_sz])
    }

    /// Port von `chiaki_takion_send_mic_packet()` (dualsense/ps5-Layout).
    pub fn send_mic_packet(&self, audio_packet: &[u8], ps5: bool) -> ChiakiResult<()> {
        let ps5_packet = ps5 as usize;
        // C rechnet hier mit size_t underflow bei zu kleinen Paketen (UB);
        // in Rust geordnet: InvalidData.
        if audio_packet.len() < 19 + ps5_packet {
            tracing::error!("Takion mic packet too small: {}", audio_packet.len());
            return Err(ChiakiError::InvalidData);
        }
        let payload_size = audio_packet.len() - 19 - ps5_packet;
        let mut buf = audio_packet.to_vec(); // C: in-place -> hier Klon

        let mut g = lock(&self.shared.gkcrypt_local);
        let key_pos = crypt_advance_key_pos_locked(&mut g, payload_size + GKCRYPT_BLOCK_SIZE)?;
        if let Some(crypt) = &g.crypt {
            crypt.encrypt(key_pos + GKCRYPT_BLOCK_SIZE as u64, &mut buf[19 + ps5_packet..])?;
            buf[14..18].copy_from_slice(&(key_pos as u32).to_be_bytes());
            let mac = crypt.gmac(key_pos, &buf)?;
            buf[10..14].copy_from_slice(&mac);
        }
        drop(g);

        self.shared.send_raw(&buf)
    }

    /// Port von `chiaki_takion_send_feedback_history()`.
    pub fn send_feedback_history(&self, seq_num: u16, payload: &[u8]) -> ChiakiResult<()> {
        let mut buf = vec![0u8; 0xc + payload.len()];
        buf[0] = TakionPacketType::FeedbackHistory as u8;
        buf[1..3].copy_from_slice(&seq_num.to_be_bytes());
        buf[3] = 0; // TODO
        // buf[4..8] = 0 (key pos), buf[8..12] = 0 (gmac)
        buf[0xc..].copy_from_slice(payload);
        self.shared.send_feedback_packet(&mut buf)
    }

    /// Takion-Protokollversion (C: takion->version).
    pub fn version(&self) -> u8 {
        self.shared.version
    }

    /// Alias für [`version()`](Self::version) (C: protocol_version im ConnectInfo).
    pub fn protocol_version(&self) -> u8 {
        self.shared.version
    }

    pub fn enable_dualsense(&self) -> bool {
        self.shared.enable_dualsense
    }
}

impl Drop for Takion {
    fn drop(&mut self) {
        self.close();
    }
}

// Hilfsfeld-Zugriff: enable_dualsense liegt (wie im C) direkt im Shared-State.
impl TakionShared {
    fn fire(&self, ev: TakionEvent) {
        (self.callback)(ev);
    }

    /// Port von `chiaki_takion_send_raw()`.
    fn send_raw(&self, buf: &[u8]) -> ChiakiResult<()> {
        self.sock
            .send(buf)
            .map(|_| ())
            .map_err(|e| {
                tracing::error!("Takion failed to send raw: {e}");
                sock::map_io_error(&e)
            })
    }

    /// Port von `chiaki_takion_send()`: MAC berechnen (in buf schreiben) und senden.
    fn send(&self, buf: &mut [u8], key_pos: u64) -> ChiakiResult<()> {
        let crypt = lock(&self.gkcrypt_local).crypt.clone();
        packet_mac(crypt.as_deref(), buf, key_pos, None, None)?;
        self.send_raw(buf)
    }

    /// Port von `takion_send_feedback_packet()`.
    fn send_feedback_packet(&self, buf: &mut [u8]) -> ChiakiResult<()> {
        assert!(buf.len() >= 0xc);

        let payload_size = buf.len() - 0xc;

        let mut g = lock(&self.gkcrypt_local);
        let key_pos = crypt_advance_key_pos_locked(&mut g, payload_size + GKCRYPT_BLOCK_SIZE)?;
        if let Some(crypt) = &g.crypt {
            crypt.encrypt(key_pos + GKCRYPT_BLOCK_SIZE as u64, &mut buf[0xc..])?;
            buf[4..8].copy_from_slice(&(key_pos as u32).to_be_bytes());
            let mac = crypt.gmac(key_pos, buf)?;
            buf[8..12].copy_from_slice(&mac);
        }
        drop(g);

        self.send_raw(buf)
    }

    /// Gemeinsamer Kern von send_message_data / send_message_data_cont.
    ///
    /// @param with_type true: 9-Byte-Präfix inkl. data_type-Byte (0),
    ///                  false: 8-Byte-Präfix ("cont")
    fn send_message_data_internal(
        &self,
        chunk_flags: u8,
        channel: u16,
        buf: &[u8],
        with_type: bool,
    ) -> ChiakiResult<u32> {
        // TODO(C): can we make this more memory-efficient?
        // TODO(C): split packet if necessary?

        let key_pos = {
            let mut g = lock(&self.gkcrypt_local);
            crypt_advance_key_pos_locked(&mut g, buf.len())?
        };

        let prefix = if with_type { 9usize } else { 8 };
        let mut packet = vec![0u8; 1 + TAKION_MESSAGE_HEADER_SIZE + prefix + buf.len()];
        packet[0] = TakionPacketType::Control as u8;

        let tag_remote = *lock(&self.tag_remote);
        takion_write_message_header(
            &mut packet[1..],
            tag_remote,
            key_pos,
            TakionChunkType::Data,
            chunk_flags,
            prefix + buf.len(),
        );

        let seq_num_val = {
            let mut s = lock(&self.seq_num_local);
            let v = *s;
            *s = s.wrapping_add(1);
            v
        };

        let p = 1 + TAKION_MESSAGE_HEADER_SIZE;
        packet[p..p + 4].copy_from_slice(&seq_num_val.to_be_bytes());
        packet[p + 4..p + 6].copy_from_slice(&channel.to_be_bytes());
        packet[p + 6..p + 8].copy_from_slice(&[0; 2]);
        if with_type {
            packet[p + 8] = 0; // data_type (C schreibt hier fix 0)
            packet[p + 9..].copy_from_slice(buf);
        } else {
            packet[p + 8..].copy_from_slice(buf);
        }

        if let Err(e) = self.send(&mut packet, key_pos) {
            // will alter packet_buf with gmac
            tracing::error!("Takion failed to send data packet: {}", e.as_str());
            return Err(e);
        }

        if let Err(e) = lock(&self.send_buffer).add(seq_num_val, packet) {
            tracing::error!("Takion failed to push packet into send buffer: {e}");
        } else {
            // Buffer war evtl. leer -> Send-Buffer-Thread aufwecken
            self.send_cond.notify_all();
        }

        Ok(seq_num_val)
    }

    /// Port von `chiaki_takion_send_message_data_ack()` (static).
    fn send_message_data_ack(&self, seq_num: u32) -> ChiakiResult<()> {
        let mut buf = [0u8; 1 + TAKION_MESSAGE_HEADER_SIZE + 0xc];
        buf[0] = TakionPacketType::Control as u8;

        let key_pos = {
            let mut g = lock(&self.gkcrypt_local);
            crypt_advance_key_pos_locked(&mut g, buf.len())?
        };

        takion_write_message_header(
            &mut buf[1..],
            *lock(&self.tag_remote),
            key_pos,
            TakionChunkType::DataAck,
            0,
            0xc,
        );

        let p = 1 + TAKION_MESSAGE_HEADER_SIZE;
        buf[p..p + 4].copy_from_slice(&seq_num.to_be_bytes());
        buf[p + 4..p + 8].copy_from_slice(&self.a_rwnd.to_be_bytes());
        buf[p + 8..p + 10].copy_from_slice(&[0; 2]);
        buf[p + 10..p + 12].copy_from_slice(&[0; 2]);

        self.send(&mut buf, key_pos)
    }
}

/// Port von `chiaki_takion_crypt_advance_key_pos()` unter dem gehaltenen Lock
/// (C nutzt dort einen rekursiven Mutex).
fn crypt_advance_key_pos_locked(state: &mut LocalCryptState, data_size: usize) -> ChiakiResult<u64> {
    let data_size = data_size + data_size % GKCRYPT_BLOCK_SIZE;
    if state.crypt.is_some() {
        let cur = state.key_pos_local;
        let sum = cur
            .checked_add(data_size as u64)
            .ok_or(ChiakiError::Overflow)?;
        state.key_pos_local = sum;
        Ok(cur)
    } else {
        Ok(0)
    }
}

// ---------------------------------------------------------------------------
// Empfangspfad (recv thread)
// ---------------------------------------------------------------------------

/// Port von `TakionDataPacketEntry`: das komplette empfangene Paket plus
/// Länge des Message-Payloads (Payload liegt fest bei Offset 0x11).
/// (C speichert zusätzlich channel/type_b — beides wird nach dem Push nie
/// wieder gelesen und daher hier nicht dupliciert.)
struct DataPacketEntry {
    packet_buf: Vec<u8>,
    payload_size: usize,
}

/// Port von `TakionAVPacketEntry`.
struct AVPacketEntry {
    packet: AVPacket,
}

/// Recv-Thread-lokaler Zustand (im C Felder von ChiakiTakion, die nur der
/// Recv-Thread anfasst — dadurch ohne Locks).
struct RecvState {
    key_state: KeyState,
    data_queue: ReorderQueue<DataPacketEntry>,
    video_queue: Option<ReorderQueue<AVPacketEntry>>,
    video_queue_head_wait_start_us: i64,
    video_queue_head_wait_seq_num: u64,
    /// Postponed packets solange enable_crypt && gkcrypt_remote == NULL.
    postponed_packets: Vec<Vec<u8>>,
    crypt_available: bool,
    disconnect_reason: ChiakiError,
}

/// Port von `takion_recv()`: StopPipe-geprüfter Empfang mit Timeout.
fn takion_recv(shared: &TakionShared, buf: &mut [u8], timeout_ms: u64) -> ChiakiResult<usize> {
    let deadline = if timeout_ms == u64::MAX {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms))
    };
    loop {
        // C: chiaki_stop_pipe_select_single -> CANCELED bei Stop
        shared.stop_pipe.check()?;
        let quantum = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => Duration::from_millis(TAKION_RECV_POLL_INTERVAL_MS),
        };
        if quantum.is_zero() {
            return Err(ChiakiError::Timeout);
        }
        let quantum = quantum.min(Duration::from_millis(TAKION_RECV_POLL_INTERVAL_MS));
        if let Err(e) = shared.sock.set_read_timeout(Some(quantum)) {
            tracing::error!("Takion failed to set read timeout: {e}");
            return Err(sock::map_io_error(&e));
        }
        match shared.sock.recv(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                tracing::error!("Takion recv failed: {e}");
                return Err(sock::map_io_error(&e));
            }
        }
    }
}

/// Port von `takion_read_extra_sock_messages()`: verdrängte Pakete einer
/// bestehenden PSN-Connection verwerfen (bis 1s, Abbruch nach 200ms Stille).
fn takion_read_extra_sock_messages(sock: &UdpSocket) -> ChiakiResult<()> {
    let expired = 1000u64.saturating_add(now_ms());
    let mut buf = [0u8; 1500];
    loop {
        if now_ms() > expired {
            return Err(ChiakiError::Timeout);
        }
        match sock.recv_from(&mut buf) {
            Ok(_) => continue,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(ChiakiError::Timeout)
            }
            Err(e) => {
                return Err(sock::map_io_error(&e));
            }
        }
    }
}

/// Port von `takion_handle_packet_mac()`.
///
/// Nimmt nur den `KeyState` (statt des ganzen RecvState), damit Aufrufer die
/// Queue-Entries und den Key-State disjoint borrown können (Crypt-Recheck).
fn takion_handle_packet_mac(
    shared: &TakionShared,
    key_state: &mut KeyState,
    base_type: u8,
    buf: &mut [u8],
) -> ChiakiResult<()> {
    let gkcrypt_remote = lock(&shared.gkcrypt_remote).clone();
    let Some(gkcrypt_remote) = gkcrypt_remote else {
        return Ok(()); // remote gmacs are IGNORED
    };

    let key_pos = match takion_packet_read_key_pos(buf, key_state) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("Takion failed to pull key_pos out of received packet");
            return Err(e);
        }
    };

    let mut mac = [0u8; GMAC_SIZE];
    let mut mac_expected = [0u8; GMAC_SIZE];
    if let Err(e) = packet_mac(
        Some(gkcrypt_remote.as_ref()),
        buf,
        key_pos,
        Some(&mut mac_expected),
        Some(&mut mac),
    ) {
        tracing::error!("Takion failed to calculate mac for received packet");
        return Err(e);
    }

    if mac_expected != mac {
        tracing::error!(
            "Takion packet MAC mismatch for packet type {:#x} with key_pos {:#x}",
            base_type,
            key_pos
        );
        return Err(ChiakiError::InvalidMac);
    }

    key_state.commit(key_pos);

    Ok(())
}

/// Port von `takion_postpone_packet()`.
fn takion_postpone_packet(st: &mut RecvState, buf: Vec<u8>) {
    if st.postponed_packets.len() >= TAKION_POSTPONE_PACKETS_SIZE {
        tracing::error!("Should postpone a packet, but there is no space left");
        return;
    }
    tracing::info!("Postpone packet of size {:#x}", buf.len());
    st.postponed_packets.push(buf);
}

/// Port von `takion_handle_packet()`. Übernimmt ownership von buf (C: free).
fn takion_handle_packet(shared: &TakionShared, st: &mut RecvState, buf: Vec<u8>) {
    assert!(!buf.is_empty());
    let mut buf = buf;
    let base_type = buf[0] & TAKION_PACKET_BASE_TYPE_MASK;

    if takion_handle_packet_mac(shared, &mut st.key_state, base_type, &mut buf).is_err() {
        return;
    }

    match base_type {
        t if t == TakionPacketType::Control as u8 => {
            takion_handle_packet_message(shared, st, buf);
        }
        t if t == TakionPacketType::Video as u8 || t == TakionPacketType::Audio as u8 => {
            let postponed_needed = shared.enable_crypt && lock(&shared.gkcrypt_remote).is_none();
            if postponed_needed {
                takion_postpone_packet(st, buf);
            } else {
                takion_handle_packet_av(shared, st, base_type, buf);
            }
        }
        _ => {
            tracing::warn!("Takion packet with unknown type {:#x} received", base_type);
        }
    }
}

/// Port von `takion_handle_packet_message()`.
fn takion_handle_packet_message(shared: &TakionShared, st: &mut RecvState, buf: Vec<u8>) {
    let msg = match takion_parse_message(&buf[1..], shared.tag_local, &mut st.key_state) {
        Ok(m) => m,
        Err(_) => return,
    };

    match msg.chunk_type {
        t if t == TakionChunkType::Data as u8 => {
            // type_b/payload_size herauskopieren, damit die msg-Borrow auf buf
            // endet und buf in den Handler moved werden kann
            // (C: Zeiger in buf, kein Ownership-Übergang).
            let type_b = msg.chunk_flags;
            let payload_size = msg.payload_size;
            takion_handle_packet_message_data(shared, st, buf, type_b, payload_size);
        }
        t if t == TakionChunkType::DataAck as u8 => {
            takion_handle_packet_message_data_ack(shared, &msg);
        }
        _ => {
            tracing::warn!(
                "Takion received message with unknown chunk type = {:#x}",
                msg.chunk_type
            );
        }
    }
}

/// Port von `takion_handle_packet_message_data()`.
///
/// Übernimmt ownership von packet_buf (C: free nach dem Pull aus der Queue).
/// Der Message-Payload liegt fest bei packet_buf + 0x11
/// (1 Typ-Byte + 0x10 Message-Header), daher genügen type_b/payload_size.
fn takion_handle_packet_message_data(
    shared: &TakionShared,
    st: &mut RecvState,
    packet_buf: Vec<u8>,
    type_b: u8,
    payload_size: usize,
) {
    if type_b != 1 {
        tracing::warn!(
            "Takion received data with type_b = {type_b:#x} (was expecting {:#x})",
            1
        );
    }

    if payload_size < 9 {
        tracing::error!("Takion received data with a size less than the header size");
        return;
    }

    let seq_num = u32::from_be_bytes([
        packet_buf[0x11],
        packet_buf[0x12],
        packet_buf[0x13],
        packet_buf[0x14],
    ]);

    let entry = DataPacketEntry {
        packet_buf,
        payload_size,
    };

    st.data_queue.push(seq_num as u64, entry);
    takion_flush_data_queue(shared, st);
}

/// Port von `takion_flush_data_queue()`.
fn takion_flush_data_queue(shared: &TakionShared, st: &mut RecvState) {
    let mut seq_num = 0u64;
    let mut ack = false;
    while let Some((s, entry)) = st.data_queue.pull() {
        seq_num = s;
        ack = true;

        if entry.payload_size < 9 {
            continue;
        }

        let payload = &entry.packet_buf[0x11..0x11 + entry.payload_size];
        let zero_a = u16::from_be_bytes([payload[6], payload[7]]);
        let data_type = payload[8]; // & 0xf

        if zero_a != 0 {
            tracing::warn!("Takion received data with unexpected nonzero {zero_a:#x} at buf+6");
        }

        match TakionMessageDataType::try_from(data_type) {
            Ok(data_type) => {
                shared.fire(TakionEvent::Data {
                    data_type,
                    buf: payload[9..].to_vec(),
                });
            }
            Err(_) => {
                tracing::warn!("Takion received data with unexpected data type {data_type:#x}");
            }
        }
    }

    if ack {
        let _ = shared.send_message_data_ack(seq_num as u32);
    }
}

/// Port von `takion_handle_packet_message_data_ack()`.
fn takion_handle_packet_message_data_ack(shared: &TakionShared, msg: &TakionMessage<'_>) {
    let Some(buf) = msg.payload else {
        tracing::error!(
            "Takion received data ack with size 0 != {:#x}",
            0xc
        );
        return;
    };
    if buf.len() != 0xc {
        tracing::error!("Takion received data ack with size {:x} != {:#x}", buf.len(), 0xc);
        return;
    }

    let cumulative_seq_num = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let a_rwnd = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let gap_ack_blocks_count = u16::from_be_bytes([buf[8], buf[9]]);
    let dup_tsns_count = u16::from_be_bytes([buf[0xa], buf[0xb]]);

    if buf.len() != gap_ack_blocks_count as usize * 4 + 0xc {
        tracing::warn!("Takion received data ack with invalid gap_ack_blocks_count");
        return;
    }

    if dup_tsns_count != 0 {
        tracing::warn!(
            "Takion received data ack with nonzero dup_tsns_count {dup_tsns_count:#x}"
        );
    }

    // C: CHIAKI_LOGV -> VERBOSE-Level -> tracing::trace
    tracing::trace!(
        "Takion received data ack with cumulative_seq_num = {cumulative_seq_num:#x}, a_rwnd = {a_rwnd:#x}, gap_ack_blocks_count = {gap_ack_blocks_count:#x}, dup_tsns_count = {dup_tsns_count:#x}"
    );

    // C: acked_seq_nums-Array der Größe TAKION_SEND_BUFFER_SIZE ("MUST be
    // consistent") — hier dynamisch.
    let acked_seq_nums = lock(&shared.send_buffer).ack_collect(cumulative_seq_num);

    for seq_num in acked_seq_nums {
        shared.fire(TakionEvent::DataAck { seq_num });
    }
}

/// Port von `takion_handle_packet_av()`.
fn takion_handle_packet_av(shared: &TakionShared, st: &mut RecvState, base_type: u8, buf: Vec<u8>) {
    // HHIxIIx
    assert!(base_type == TakionPacketType::Video as u8 || base_type == TakionPacketType::Audio as u8);
    if shared.disable_audio_video.contains(DisableAudioVideo::VideoDisabled.bits())
        && base_type == TakionPacketType::Video as u8
    {
        return;
    }

    let mut packet = AVPacket::default();
    if let Err(e) = (shared.av_packet_parse)(&mut packet, &mut st.key_state, &buf) {
        if e == ChiakiError::BufTooSmall {
            tracing::error!("Takion received AV packet that was too small");
        }
        return;
    }

    if shared.disable_audio_video.contains(DisableAudioVideo::AudioDisabled.bits())
        && base_type == TakionPacketType::Audio as u8
        && !packet.is_haptics
    {
        return;
    }

    let is_video = base_type == TakionPacketType::Video as u8;
    if !is_video {
        shared.fire(TakionEvent::Av(Box::new(packet)));
        return;
    }

    // Video-Queue wird erst beim ersten AV-Paket mit passenden Parametern
    // aufgesetzt (C: video_queue_initialized).
    if st.video_queue.is_none() {
        let queue_begin = if packet.unit_index > 0 {
            packet.packet_index.wrapping_sub(packet.unit_index) as u64
        } else {
            packet.packet_index as u64
        };
        match ReorderQueue::new(
            TAKION_AV_VIDEO_REORDER_QUEUE_SIZE_EXP,
            queue_begin,
            SeqNumSize::Num16,
        ) {
            Ok(mut queue) => {
                queue.set_drop_strategy(DropStrategy::Begin);
                queue.set_drop_cb(Some(Box::new(|seq_num, _entry: AVPacketEntry| {
                    tracing::debug!("Takion dropping AV packet with index {seq_num:#x}");
                })));
                st.video_queue = Some(queue);
                st.video_queue_head_wait_start_us = 0;
                st.video_queue_head_wait_seq_num = queue_begin;
            }
            Err(_) => {
                // Fallback: dispatch immediately without reordering
                shared.fire(TakionEvent::Av(Box::new(packet)));
                return;
            }
        }
    }

    let queue = st.video_queue.as_mut().expect("video queue initialized above");
    queue.push(packet.packet_index as u64, AVPacketEntry { packet });
    takion_av_queue_flush_with_timeout(
        shared,
        queue,
        &mut st.video_queue_head_wait_start_us,
        &mut st.video_queue_head_wait_seq_num,
    );
}

/// Port von `takion_av_queue_flush_with_timeout()`.
///
/// Pull and dispatch all in-order entries from the given AV queue.
/// If the head packet is missing, wait up to av_reorder_timeout_us before
/// skipping it, then retry. This handles WiFi jitter without stalling on
/// lost packets.
fn takion_av_queue_flush_with_timeout(
    shared: &TakionShared,
    queue: &mut ReorderQueue<AVPacketEntry>,
    head_wait_start_us: &mut i64,
    head_wait_seq_num: &mut u64,
) {
    let now = now_us() as i64;
    let mut made_progress = true;

    while made_progress {
        made_progress = false;

        while let Some((_, entry)) = queue.pull() {
            made_progress = true;
            shared.fire(TakionEvent::Av(Box::new(entry.packet)));
        }

        if made_progress {
            *head_wait_start_us = 0;
        }

        if queue.count() == 0 {
            break;
        }

        if *head_wait_start_us != 0 && queue.begin() != *head_wait_seq_num {
            if queue.seq_num_gt(queue.begin(), *head_wait_seq_num) {
                // The missing head advanced within the same loss burst. Keep the
                // original timeout budget but track the new missing sequence.
                *head_wait_seq_num = queue.begin();
            } else {
                // A genuinely new gap appeared; start a fresh timeout window.
                *head_wait_start_us = now;
                *head_wait_seq_num = queue.begin();
                break;
            }
        }

        // Head slot is missing (packet lost or not yet arrived)
        if *head_wait_start_us == 0 {
            *head_wait_start_us = now;
            *head_wait_seq_num = queue.begin();
            break;
        }

        if now - *head_wait_start_us <= shared.av_reorder_timeout_us as i64 {
            break;
        }

        // Timeout exceeded: skip directly to the first buffered packet so startup
        // and burst reordering only pay a single timeout.
        let mut skipped = 0u64;
        while skipped < queue.count() {
            if queue.peek(skipped).is_some() {
                break;
            }
            skipped += 1;
        }
        if skipped >= queue.count() {
            break;
        }

        tracing::debug!(
            "Takion AV reorder timeout: skipping {} missing packet(s) before {:#x}",
            skipped,
            queue.seq_num_add(queue.begin(), skipped)
        );
        queue.skip_head(skipped);
        *head_wait_start_us = 0;
        made_progress = true;
    }
}

/// Port von `takion_av_queues_flush_with_timeout()`: flusht die AV-Queues mit
/// Reorder-Timeout — im Port (wie im chiaki-ng-C) nur die Video-Queue, falls
/// sie initialisiert ist (Audio-Pakete werden sofort dispatcht).
fn takion_av_queues_flush_with_timeout(shared: &TakionShared, st: &mut RecvState) {
    if let Some(queue) = st.video_queue.as_mut() {
        takion_av_queue_flush_with_timeout(
            shared,
            queue,
            &mut st.video_queue_head_wait_start_us,
            &mut st.video_queue_head_wait_seq_num,
        );
    }
}

/// Port von `takion_av_queues_next_timeout_ms()`.
fn takion_av_queues_next_timeout_ms(shared: &TakionShared, head_wait_start_us: i64) -> u64 {
    if head_wait_start_us == 0 {
        return u64::MAX;
    }

    let now = now_us() as i64;
    let remaining_us = shared.av_reorder_timeout_us as i64 - (now - head_wait_start_us);
    if remaining_us <= 0 {
        return 0;
    }
    ((remaining_us + 999) / 1000) as u64
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TakionMessagePayloadInit {
    tag: u32,
    a_rwnd: u32,
    outbound_streams: u16,
    inbound_streams: u16,
    initial_seq_num: u32,
}

#[derive(Default)]
struct TakionMessagePayloadInitAck {
    tag: u32,
    a_rwnd: u32,
    outbound_streams: u16,
    inbound_streams: u16,
    initial_seq_num: u32,
    cookie: [u8; TAKION_COOKIE_SIZE],
}

/// Port von `takion_send_message_init()`.
fn takion_send_message_init(shared: &TakionShared, payload: &TakionMessagePayloadInit) -> ChiakiResult<()> {
    let mut message = [0u8; 1 + TAKION_MESSAGE_HEADER_SIZE + 0x10];
    message[0] = TakionPacketType::Control as u8;
    takion_write_message_header(
        &mut message[1..],
        *lock(&shared.tag_remote),
        0,
        TakionChunkType::Init,
        0,
        0x10,
    );

    let p = 1 + TAKION_MESSAGE_HEADER_SIZE;
    message[p..p + 4].copy_from_slice(&payload.tag.to_be_bytes());
    message[p + 4..p + 8].copy_from_slice(&payload.a_rwnd.to_be_bytes());
    message[p + 8..p + 10].copy_from_slice(&payload.outbound_streams.to_be_bytes());
    message[p + 10..p + 12].copy_from_slice(&payload.inbound_streams.to_be_bytes());
    message[p + 12..p + 16].copy_from_slice(&payload.initial_seq_num.to_be_bytes());

    shared.send_raw(&message)
}

/// Port von `takion_send_message_cookie()`.
fn takion_send_message_cookie(shared: &TakionShared, cookie: &[u8; TAKION_COOKIE_SIZE]) -> ChiakiResult<()> {
    let mut message = [0u8; 1 + TAKION_MESSAGE_HEADER_SIZE + TAKION_COOKIE_SIZE];
    message[0] = TakionPacketType::Control as u8;
    takion_write_message_header(
        &mut message[1..],
        *lock(&shared.tag_remote),
        0,
        TakionChunkType::Cookie,
        0,
        TAKION_COOKIE_SIZE,
    );
    message[1 + TAKION_MESSAGE_HEADER_SIZE..].copy_from_slice(cookie);
    shared.send_raw(&message)
}

/// Port von `takion_recv_message_init_ack()`.
fn takion_recv_message_init_ack(
    shared: &TakionShared,
    st: &mut RecvState,
    payload: &mut TakionMessagePayloadInitAck,
) -> ChiakiResult<()> {
    let mut message = [0u8; 1 + TAKION_MESSAGE_HEADER_SIZE + 0x10 + TAKION_COOKIE_SIZE];
    let received_size = takion_recv(shared, &mut message, TAKION_EXPECT_TIMEOUT_MS)?;

    if received_size < message.len() {
        tracing::error!(
            "Takion received packet of size {} while expecting init ack packet of exactly {}",
            received_size,
            message.len()
        );
        return Err(ChiakiError::InvalidResponse);
    }

    if message[0] != TakionPacketType::Control as u8 {
        tracing::error!(
            "Takion received packet of type {:#x} while expecting init ack message with type {:#x}",
            message[0],
            TakionPacketType::Control as u8
        );
        return Err(ChiakiError::InvalidResponse);
    }

    let msg = takion_parse_message(&message[1..received_size], shared.tag_local, &mut st.key_state)
        .inspect_err(|_| {
            tracing::error!("Failed to parse message while expecting init ack");
        })?;

    if msg.chunk_type != TakionChunkType::InitAck as u8 || msg.chunk_flags != 0x0 {
        tracing::error!(
            "Takion received unexpected message with type ({:#x}, {:#x}) while expecting init ack",
            msg.chunk_type,
            msg.chunk_flags
        );
        return Err(ChiakiError::InvalidResponse);
    }

    // C: assert(msg.payload_size == 0x10 + TAKION_COOKIE_SIZE)
    let Some(pl) = msg.payload else {
        return Err(ChiakiError::InvalidResponse);
    };
    if pl.len() != 0x10 + TAKION_COOKIE_SIZE {
        return Err(ChiakiError::InvalidResponse);
    }

    payload.tag = u32::from_be_bytes([pl[0], pl[1], pl[2], pl[3]]);
    payload.a_rwnd = u32::from_be_bytes([pl[4], pl[5], pl[6], pl[7]]);
    payload.outbound_streams = u16::from_be_bytes([pl[8], pl[9]]);
    payload.inbound_streams = u16::from_be_bytes([pl[0xa], pl[0xb]]);
    payload.initial_seq_num = u32::from_be_bytes([pl[0xc], pl[0xd], pl[0xe], pl[0xf]]);
    payload.cookie.copy_from_slice(&pl[0x10..0x10 + TAKION_COOKIE_SIZE]);

    Ok(())
}

/// Port von `takion_recv_message_cookie_ack()`.
fn takion_recv_message_cookie_ack(shared: &TakionShared, st: &mut RecvState) -> ChiakiResult<()> {
    let mut message = [0u8; 1 + TAKION_MESSAGE_HEADER_SIZE];
    let mut received_size = takion_recv(shared, &mut message, TAKION_EXPECT_TIMEOUT_MS)?;

    if message[0xd] == TakionChunkType::InitAck as u8 {
        tracing::info!("Received second init ack, looking for cookie ack in next message");
        received_size = takion_recv(shared, &mut message, TAKION_EXPECT_TIMEOUT_MS)?;
    }

    if received_size < message.len() {
        tracing::error!(
            "Takion received packet of size {} while expecting cookie ack packet of exactly {}",
            received_size,
            message.len()
        );
        return Err(ChiakiError::InvalidResponse);
    }

    if message[0] != TakionPacketType::Control as u8 {
        tracing::error!(
            "Takion received packet of type {:#x} while expecting cookie ack message with type {:#x}",
            message[0],
            TakionPacketType::Control as u8
        );
        return Err(ChiakiError::InvalidResponse);
    }

    let msg = takion_parse_message(&message[1..received_size], shared.tag_local, &mut st.key_state)
        .inspect_err(|_| {
            tracing::error!("Failed to parse message while expecting cookie ack");
        })?;

    if msg.chunk_type != TakionChunkType::CookieAck as u8 || msg.chunk_flags != 0x0 {
        tracing::error!(
            "Takion received unexpected message with type ({:#x}, {:#x}) while expecting cookie ack",
            msg.chunk_type,
            msg.chunk_flags
        );
        return Err(ChiakiError::InvalidResponse);
    }

    // C: assert(msg.payload_size == 0)

    Ok(())
}

/// Port von `takion_handshake()`. Liefert die initiale Remote-Seq-Nummer.
fn takion_handshake(shared: &TakionShared, st: &mut RecvState) -> ChiakiResult<u32> {
    // INIT ->
    let init_payload = TakionMessagePayloadInit {
        tag: shared.tag_local,
        a_rwnd: TAKION_A_RWND,
        outbound_streams: TAKION_OUTBOUND_STREAMS,
        inbound_streams: TAKION_INBOUND_STREAMS,
        initial_seq_num: *lock(&shared.seq_num_local),
    };

    let mut init_ack_payload = TakionMessagePayloadInitAck::default();
    let mut last_err = ChiakiError::Unknown;
    let mut ok = false;
    for tries in 0..MAX_CONNECT_RESEND_TRIES {
        if tries > 0 {
            tracing::warn!("Takion hasn't received init ack yet, retrying init [attempt {}] ...", tries + 1);
        }
        takion_send_message_init(shared, &init_payload)
            .inspect_err(|_| tracing::error!("Takion failed to send init"))?;

        tracing::info!("Takion sent init");

        // INIT_ACK <-
        match takion_recv_message_init_ack(shared, st, &mut init_ack_payload) {
            Ok(()) => {
                ok = true;
                break;
            }
            Err(e) => last_err = e,
        }
    }
    if !ok {
        tracing::error!("Takion failed to receive init ack");
        return Err(last_err);
    }

    if init_ack_payload.tag == 0 {
        tracing::error!("Takion remote tag in init ack is 0");
        return Err(ChiakiError::InvalidResponse);
    }

    tracing::info!(
        "Takion received init ack with remote tag {:#x}, outbound streams: {:#x}, inbound streams: {:#x}",
        init_ack_payload.tag,
        init_ack_payload.outbound_streams,
        init_ack_payload.inbound_streams
    );

    *lock(&shared.tag_remote) = init_ack_payload.tag;
    // C: *seq_num_remote_initial = takion->tag_remote; //init_ack_payload.initial_seq_num;
    let seq_num_remote_initial = init_ack_payload.tag;

    if init_ack_payload.outbound_streams == 0
        || init_ack_payload.inbound_streams == 0
        || init_ack_payload.outbound_streams > TAKION_INBOUND_STREAMS
        || init_ack_payload.inbound_streams < TAKION_OUTBOUND_STREAMS
    {
        tracing::error!("Takion min/max check failed");
        return Err(ChiakiError::InvalidResponse);
    }

    // COOKIE ->
    let mut ok = false;
    let mut last_err = ChiakiError::Unknown;
    for tries in 0..MAX_CONNECT_RESEND_TRIES {
        if tries > 0 {
            tracing::warn!("Takion hasn't received cookie ack yet, resending cookie [attempt {}] ...", tries + 1);
        }
        takion_send_message_cookie(shared, &init_ack_payload.cookie)
            .inspect_err(|_| tracing::error!("Takion failed to send cookie"))?;

        tracing::info!("Takion sent cookie");

        // COOKIE_ACK <-
        match takion_recv_message_cookie_ack(shared, st) {
            Ok(()) => {
                ok = true;
                break;
            }
            Err(e) => last_err = e,
        }
    }
    if !ok {
        tracing::error!("Takion failed to receive cookie ack");
        return Err(last_err);
    }

    tracing::info!("Takion received cookie ack");

    // done!
    tracing::info!("Takion connected");

    Ok(seq_num_remote_initial)
}

// ---------------------------------------------------------------------------
// Recv-Thread
// ---------------------------------------------------------------------------

/// Port von `takion_thread_func()`.
fn takion_thread_func(shared: Arc<TakionShared>) {
    let mut st = RecvState {
        key_state: KeyState::new(),
        // data_queue folgt nach dem Handshake (C: init in takion_thread_func)
        data_queue: match ReorderQueue::new(
            TAKION_REORDER_QUEUE_SIZE_EXP,
            0,
            SeqNumSize::Num32,
        ) {
            Ok(q) => q,
            Err(_) => {
                shared.fire(TakionEvent::Disconnect(ChiakiError::Memory));
                return;
            }
        },
        video_queue: None,
        video_queue_head_wait_start_us: 0,
        video_queue_head_wait_seq_num: 0,
        postponed_packets: Vec::new(),
        crypt_available: false,
        disconnect_reason: ChiakiError::Canceled,
    };

    let seq_num_remote_initial = match takion_handshake(&shared, &mut st) {
        Ok(v) => v,
        Err(e) => {
            st.disconnect_reason = e;
            takion_thread_cleanup(&shared);
            return;
        }
    };

    st.data_queue = match ReorderQueue::new(
        TAKION_REORDER_QUEUE_SIZE_EXP,
        seq_num_remote_initial as u64,
        SeqNumSize::Num32,
    ) {
        Ok(q) => q,
        Err(_) => {
            takion_thread_cleanup(&shared);
            return;
        }
    };
    st.data_queue.set_drop_cb(Some(Box::new(|seq_num, _entry: DataPacketEntry| {
        tracing::error!("Takion dropping data with seq num {seq_num:#x}");
    })));

    // Send-Buffer-Thread starten (C: chiaki_takion_send_buffer_init im Thread)
    shared.resend_should_stop.store(false, Ordering::SeqCst);
    {
        let shared_resend = Arc::clone(&shared);
        match std::thread::Builder::new()
            .name("Chiaki Takion Send Buffer".to_owned())
            .spawn(move || takion_send_buffer_thread_func(shared_resend))
        {
            Ok(h) => *lock(&shared.resend_thread) = Some(h),
            Err(_) => {
                tracing::error!("Takion failed to start send buffer thread");
                takion_thread_cleanup(&shared);
                shared.fire(TakionEvent::Disconnect(ChiakiError::Thread));
                return;
            }
        }
    }

    shared.fire(TakionEvent::Connected);

    st.crypt_available = lock(&shared.gkcrypt_remote).is_some();

    loop {
        if shared.stop_pipe.check().is_err() {
            break;
        }

        if shared.enable_crypt && !st.crypt_available {
            let remote = lock(&shared.gkcrypt_remote).clone();
            if remote.is_some() {
                st.crypt_available = true;
                tracing::info!(
                    "Crypt has become available. Re-checking MACs of {} packets",
                    st.data_queue.count()
                );
                let mut i = 0u64;
                while i < st.data_queue.count() {
                    let drop_it = match st.data_queue.peek_mut(i) {
                        Some((_, entry)) => {
                            if entry.packet_buf.is_empty() {
                                false
                            } else {
                                let base_type =
                                    entry.packet_buf[0] & TAKION_PACKET_BASE_TYPE_MASK;
                                // Disjointe Feld-Borrows: entry leiht
                                // st.data_queue, der MAC-Check nur st.key_state.
                                takion_handle_packet_mac(
                                    &shared,
                                    &mut st.key_state,
                                    base_type,
                                    &mut entry.packet_buf,
                                )
                                .is_err()
                            }
                        }
                        None => {
                            i += 1;
                            continue;
                        }
                    };
                    if drop_it {
                        tracing::warn!("Found an invalid MAC");
                        st.data_queue.drop_at(i);
                    } else {
                        i += 1;
                    }
                }
            }
        }

        if !st.postponed_packets.is_empty() && lock(&shared.gkcrypt_remote).is_some() {
            // there are some postponed packets that were waiting until crypt
            // is initialized and it is now :-)
            tracing::info!("Takion flushing {} postpone packet(s)", st.postponed_packets.len());
            let packets = std::mem::take(&mut st.postponed_packets);
            for packet in packets {
                takion_handle_packet(&shared, &mut st, packet);
            }
        }

        let recv_timeout_ms = takion_av_queues_next_timeout_ms(&shared, st.video_queue_head_wait_start_us);
        if recv_timeout_ms == 0 {
            takion_av_queues_flush_with_timeout(&shared, &mut st);
            continue;
        }

        let mut buf = [0u8; TAKION_RECV_BUF_SIZE];
        match takion_recv(&shared, &mut buf, recv_timeout_ms) {
            Ok(received_size) => {
                takion_handle_packet(&shared, &mut st, buf[..received_size].to_vec());
            }
            Err(ChiakiError::Timeout) => {
                takion_av_queues_flush_with_timeout(&shared, &mut st);
                continue;
            }
            Err(e) => {
                st.disconnect_reason = e;
                break;
            }
        }
    }

    takion_thread_cleanup(&shared);
    shared.fire(TakionEvent::Disconnect(st.disconnect_reason));
}

/// Stoppt + joint den Send-Buffer-Thread und räumt die Queues ab
/// (C: chiaki_takion_send_buffer_fini + queue_finis im Thread-Ende).
fn takion_thread_cleanup(shared: &TakionShared) {
    shared.resend_should_stop.store(true, Ordering::SeqCst);
    shared.send_cond.notify_all();
    let handle = lock(&shared.resend_thread).take();
    if let Some(h) = handle {
        let _ = h.join();
    }
    // video_queue/data_queue werden per Drop (fini-Port) abgebaut.
}

/// Port von `takion_send_buffer_thread_func()` + `takion_send_buffer_resend()`.
fn takion_send_buffer_thread_func(shared: Arc<TakionShared>) {
    loop {
        {
            let mut sb = lock(&shared.send_buffer);
            // C: mit Timeout warten wenn Pakete da sind, sonst bis zum Push.
            // Das Stop-Flag wird per cond/Timeout-Poll berücksichtigt.
            while !shared.resend_should_stop.load(Ordering::SeqCst) && sb.count() == 0 {
                sb = wait_no_packets(&shared, sb);
            }
            if shared.resend_should_stop.load(Ordering::SeqCst) {
                break;
            }
            if sb.count() > 0 {
                // bounded wait wie chiaki_cond_timedwait_pred(WAKEUP_TIMEOUT_MS)
                sb = wait_with_packets(&shared, sb);
            }
        }

        if shared.resend_should_stop.load(Ordering::SeqCst) {
            break;
        }

        // takion_send_buffer_resend: unter dem Lock bewerten, senden außerhalb
        let now = now_ms();
        let (to_resend, _given_up) = {
            let mut sb = lock(&shared.send_buffer);
            sb.take_expired_resends(now)
        };
        for buf in to_resend {
            // C ignoriert den Fehler von chiaki_takion_send_raw hier ebenfalls
            let _ = shared.send_raw(&buf);
        }
    }
}

fn wait_no_packets<'a>(
    shared: &'a TakionShared,
    guard: MutexGuard<'a, TakionSendBuffer>,
) -> MutexGuard<'a, TakionSendBuffer> {
    let (g, _) = shared
        .send_cond
        .wait_timeout(guard, Duration::from_millis(200))
        .unwrap_or_else(PoisonError::into_inner);
    g
}

fn wait_with_packets<'a>(
    shared: &'a TakionShared,
    guard: MutexGuard<'a, TakionSendBuffer>,
) -> MutexGuard<'a, TakionSendBuffer> {
    let (g, _) = shared
        .send_cond
        .wait_timeout(guard, Duration::from_millis(crate::takionsendbuffer::TAKION_DATA_RESEND_WAKEUP_TIMEOUT_MS))
        .unwrap_or_else(PoisonError::into_inner);
    g
}
