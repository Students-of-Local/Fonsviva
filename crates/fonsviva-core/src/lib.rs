//! Fonsviva core: hardware detection, catalog and fit, model store, engines
//! (llama.cpp, Ollama) and measurements.

pub mod catalog;
pub mod download;
pub mod engine;
pub mod gguf;
pub mod hardware;
pub mod paths;
pub mod stats;
pub mod store;
pub mod util;
