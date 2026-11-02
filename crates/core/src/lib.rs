//! Platform-free parts of tape: the MP4 muxer, codec parsing, the replay ring, audio timing,
//! settings and the host/engine protocol. Nothing here touches Windows, so all of it is tested
//! on any machine with `cargo test -p tape-core`.

pub mod audio;
pub mod cursor;
pub mod game;
pub mod ipc;
pub mod mp4;
pub mod naming;
pub mod packet;
pub mod record;
pub mod ring;
pub mod settings;
pub mod share;
pub mod sink;
