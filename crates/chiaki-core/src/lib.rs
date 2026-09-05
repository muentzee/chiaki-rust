// chiaki-core: PS4/PS5 Remote-Play-Protokoll in Rust.
// 1:1-Port von chiaki-ng lib/src (Referenz: F:\projekte\chiaki-rust-remaster\lib).
#![deny(unsafe_code)]

pub mod error;
pub mod time;
pub mod sock;
pub mod stoppipe;
pub mod base64;
pub mod bitstream;
pub mod random;
pub mod seqnum;
pub mod rpcrypt;
pub mod gkcrypt;
pub mod ecdh;
pub mod fec;
pub mod proto;
pub mod reorderqueue;
pub mod takionsendbuffer;
pub mod takion;
pub mod ctrl;
pub mod launchspec;
pub mod session;
pub mod streamconnection;
pub mod regist;
pub mod discovery;
pub mod discoveryservice;
pub mod senkusha;
pub mod http;
pub mod audioreceiver;
pub mod audiosender;
pub mod videoreceiver;
pub mod congestioncontrol;
pub mod packetstats;
pub mod frameprocessor;
pub mod feedback;
pub mod feedbacksender;
pub mod controller;
pub mod orientation;
pub mod pidecoder;
pub mod video;
pub mod audio;

pub use error::{ChiakiError, ChiakiResult, Target, Codec};
