/// The client protocol a buffered reply is re-emitted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyProtocol {
    /// `OpenAI` `chat.completion` chunks, ended by `data: [DONE]`.
    OpenAiChat,
    /// Anthropic Messages events, from `message_start` to `message_stop`.
    AnthropicMessage,
}
