/// One entry of a profile's model catalog: the id to send upstream for an advertised model.
///
/// Copilot advertises Claude ids with dots (`claude-opus-4.5`) while the hub advertises
/// dashes (`claude-opus-4-5`), so the advertised id alone is not enough to call upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamModel {
    /// The model id exactly as the upstream lists it.
    pub upstream_id: String,
    /// True when the upstream serves this model on Anthropic `/v1/messages` natively.
    pub native_messages: bool,
}
