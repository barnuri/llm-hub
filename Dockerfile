FROM rust:1.97 AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY ui ./ui
RUN cargo build --release

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /build/target/release/llm-hub /llm-hub
EXPOSE 8410
ENV LLM_HUB_BIND=0.0.0.0
ENTRYPOINT ["/llm-hub"]
