#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![deny(rust_2018_idioms)]

#[tokio::main]
async fn main() {
    llm_hub::run().await;
}
