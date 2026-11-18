//! The `llm-hub` binary.
#![forbid(unsafe_code)]

#[tokio::main]
async fn main() {
    llm_hub::run().await;
}
