FROM cgr.dev/chainguard/rust:latest-dev AS build
ARG PACKAGE=versions
WORKDIR /app

# Copy Cargo.toml and src folder to the working directory
COPY Cargo.toml ./
COPY src ./src

USER root
RUN apk update && apk add libssl3 openssl-dev libcrypto3
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release && cp /app/target/release/${PACKAGE} ./${PACKAGE}


FROM cgr.dev/chainguard/glibc-dynamic
COPY --from=build /usr/lib/libssl.so.3 /usr/lib/libssl.so.3
COPY --from=build /usr/lib/libcrypto.so.3 /usr/lib/libcrypto.so.3 
COPY --from=build --chown=nonroot:nonroot /app/${PACKAGE} /usr/local/bin/${PACKAGE}
#USER nonroot
USER root
CMD ["/usr/local/bin/${PACKAGE}"]
