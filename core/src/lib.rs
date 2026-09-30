//! Shared pipeline code for HWA Video.
//!
//! This crate exposes the FFmpeg wrapper, the wgpu pipelines, the shader
//! sources, and the export helper. Both `hwa-preview` (standalone winit
//! binary) and `hwa-editor` (Tauri app) depend on it.

pub mod export;
pub mod ffmpeg;
pub mod gpu;
