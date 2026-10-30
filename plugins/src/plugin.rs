//! Types the engine and the plugin formats share.

use serde::{Deserialize, Serialize};

/// What a plugin is for, from its format's category strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Instrument,
    Effect,
    Other,
}

/// The host's transport at the start of a block, for tempo-synced plugins. Sent to the plugin
/// every block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transport {
    pub playing: bool,
    pub recording: bool,
    pub looping: bool,
    /// Quarter notes per minute.
    pub tempo: f64,
    pub time_sig_numerator: u32,
    pub time_sig_denominator: u32,
    /// Timeline position in samples. Jumps when the transport loops or is moved.
    pub project_samples: i64,
    /// Samples since the engine started; never jumps and keeps counting while stopped.
    pub continuous_samples: i64,
    /// Timeline position in quarter notes.
    pub project_beats: f64,
    /// Start of the current bar, in quarter notes.
    pub bar_start_beats: f64,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
}

impl Default for Transport {
    fn default() -> Self {
        Transport {
            playing: false,
            recording: false,
            looping: false,
            tempo: 120.0,
            time_sig_numerator: 4,
            time_sig_denominator: 4,
            project_samples: 0,
            continuous_samples: 0,
            project_beats: 0.0,
            bar_start_beats: 0.0,
            loop_start_beats: 0.0,
            loop_end_beats: 0.0,
        }
    }
}
