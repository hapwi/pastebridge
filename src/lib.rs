pub mod clipboard;
pub mod config;
pub mod crypto;
pub mod daemon;
pub mod discovery;
pub mod doctor;
pub mod identity;
pub mod macos_identity;
pub mod pairing;
pub mod protocol;
pub mod service;
pub mod sync;
pub mod tls;
pub mod ui;
pub mod update;

pub use config::Config;
pub use identity::Identity;

pub fn init_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
