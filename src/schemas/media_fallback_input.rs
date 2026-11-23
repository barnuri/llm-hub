use serde::Deserialize;
use utoipa::ToSchema;

/// The media fallback settings the UI saves. An empty model turns that conversion off.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MediaFallbackInput {
    pub enabled: bool,
    /// Drop media a text-only model cannot read and leave a system reminder in its place.
    #[serde(default)]
    pub skip: bool,
    /// The `<profile>/<model>` that captions images.
    #[serde(default)]
    pub vision_model: String,
    /// The `<profile>/<model>` that transcribes audio.
    #[serde(default)]
    pub stt_model: String,
}
