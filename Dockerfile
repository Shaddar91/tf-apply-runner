#syntax=docker/dockerfile:1
#tf-apply runner image: build the executor, add a pinned Terraform, ship a non-root glibc runtime.

FROM rust:1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY tf-apply ./tf-apply
COPY tf-apply-mcp ./tf-apply-mcp
RUN cargo build --release -p tf-apply

FROM debian:bookworm-slim AS terraform
ARG TERRAFORM_VERSION=1.7.5
ARG TF_ARCH=amd64
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends curl unzip ca-certificates; \
    curl -fsSL "https://releases.hashicorp.com/terraform/${TERRAFORM_VERSION}/terraform_${TERRAFORM_VERSION}_linux_${TF_ARCH}.zip" -o /tmp/tf.zip; \
    unzip /tmp/tf.zip -d /usr/local/bin; \
    rm /tmp/tf.zip; \
    /usr/local/bin/terraform version

FROM debian:bookworm-slim
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates; \
    rm -rf /var/lib/apt/lists/*
COPY --from=terraform /usr/local/bin/terraform /usr/local/bin/terraform
COPY --from=builder /build/target/release/tf-apply /usr/local/bin/tf-apply
#empty.tfrc must exist in-image: runner.rs pins TF_CLI_CONFIG_FILE to it so no host tfrc leaks in.
RUN mkdir -p /etc/tf-apply && : > /etc/tf-apply/empty.tfrc && chmod 0644 /etc/tf-apply/empty.tfrc
EXPOSE 1937
CMD ["tf-apply"]
