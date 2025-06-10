FROM rust:1.85.1 as builder
WORKDIR /app
COPY Cargo.toml ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo build --release

FROM ubuntu:questing-20250514
# 更新源
# RUN sed -i 's/deb.debian.org/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list.d/debian.sources
# 更新包列表
RUN apt-get update && \
    apt-get upgrade -y && \
    apt-get clean -y && \
    apt-get autoremove -y && \
    apt-get autoclean -y

# 安装 libssl
RUN apt-get install libssl3 -y && apt-get install ca-certificates -y

COPY --from=builder /app/target/release/http-mapping-reproxy /app/
COPY config/mapping.yaml /app/config/
EXPOSE 8080
CMD ["/app/http-mapping-reproxy"]