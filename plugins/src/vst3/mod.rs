//! The VST3 host.

#[allow(unsafe_code, dead_code)]
mod ffi;
#[allow(unsafe_code, dead_code)]
pub(crate) mod shim;

/// Why a VST3 operation failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Vst3Error {
    #[error("cannot load VST3 plugin {0}: {1}")]
    Load(String, String),
    #[error("no VST3 plugin `{0}`")]
    NotFound(String),
    #[error("cannot create VST3 plugin `{0}`: {1}")]
    Instantiate(String, String),
    #[error("VST3 processing: {0}")]
    Process(String),
    #[error("VST3 plugins are not supported on this platform")]
    Unsupported,
}
