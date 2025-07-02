# http-mapping-reproxy

本项目是一个通用的 HTTP 协议字段映射程序，旨在解决不同系统间 HTTP 请求（包括 Query、Header、JSON Body 和 Form Body）字段不兼容的问题，实现灵活的数据转换和适配。

## 项目结构

- `.cargo/`: Cargo 配置文件。
- `.gitignore`: Git 忽略文件。
- `.gitmodules`: Git 子模块配置。
- `Cargo.toml`: Rust 项目清单文件。
- `Dockerfile`: 用于容器化的 Dockerfile。
- `config/`: 配置文件，包括映射文件。
- `kubernetes/`: Kubernetes 部署配置。
- `src/`: 源代码。
  - `config.rs`: 配置解析和处理。
  - `main.rs`: 主应用程序入口点。

## 入门指南

### 先决条件

- Rust（推荐最新稳定版本）
- Docker（可选，用于容器化部署）

### 安装

1. **克隆仓库：**

   ```bash
   git clone <repository_url>
   cd http-mapping-reproxy
   ```

2. **构建项目：**

   ```bash
   cargo build --release
   ```

### 配置

1. **环境变量：**

   请参考 `.env.example` 文件配置您的环境变量，例如数据库连接、API 密钥等。

2. **映射文件：**

   `config/` 目录包含映射文件（例如 `mapping.yaml`、`mapping_sse.yaml`），这些文件定义了 HTTP 请求中 Query、Header、JSON Body 和 Form Body 之间字段的转换规则。请根据您的具体需求审查和调整这些文件。

### 使用方法

运行服务：

```bash
cargo run --release
```

对于容器化部署，构建并运行 Docker 镜像：

```bash
docker build -t http-mapping-reproxy .
docker run -p 8080:8080 http-mapping-reproxy
```

## 部署

`kubernetes/` 目录包含 Kubernetes 部署配置示例。您可以调整这些文件以将服务部署到您的 Kubernetes 集群。

## 贡献

欢迎贡献！如有任何改进或错误修复，请提交问题或拉取请求。

## 许可证

本项目采用 MIT 许可证 - 有关详细信息，请参阅 LICENSE 文件。