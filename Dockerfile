FROM aigccontainer.azurecr.cn/rust:1.85.1 as builder
WORKDIR /app
COPY Cargo.toml ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo build --release

FROM aigccontainer.azurecr.cn/debian:12-slim
# 更新源
RUN sed -i 's/deb.debian.org/ftp.cn.debian.org/g' /etc/apt/sources.list.d/debian.sources
# 更新包列表
RUN apt-get update && \
    apt-get upgrade -y && \
    apt-get clean -y && \
    apt-get autoremove -y && \
    apt-get autoclean -y

# 安装 libssl
RUN apt-get update -y && apt-get install libssl3 -y && apt-get install ca-certificates -y

COPY --from=builder /app/target/release/dify-sso-adapter /app/
COPY config/mapping.yaml /app/config/
EXPOSE 8080
CMD ["/app/http-mapping-reproxy"]