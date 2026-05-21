# http-mapping-reproxy

HTTP 协议字段映射代理服务，支持请求/响应字段转换、缓存、签名等功能。

## 功能特性

- **字段映射**: Query、Header、JSON Body、Form Body 之间的灵活转换
- **响应混合**: 支持从多个数据源组合响应内容
- **缓存支持**: 内置 JSON 缓存，可配置缓存 key 和过期时间
- **环境变量**: 配置文件支持 `${VAR}` 格式的环境变量注入
- **DirectResponse**: 无需后端服务，直接返回配置数据

## 项目结构

```
src/
  main.rs      # 主程序入口
  config.rs    # 配置解析
  cache.rs     # 缓存模块
config/        # 映射配置文件
projs/config/ # 项目专用配置
```

## 快速开始

### 配置

1. 复制 `.env.example` 为 `.env` 并配置环境变量
2. 修改映射配置文件（如 `projs/config/mapping_ruizhong_oauth.yaml`）

### 运行

```bash
cargo run --release
```

## 配置说明

### 服务类型 (ServiceType)

| 类型 | 说明 |
|------|------|
| `sso` | 代理到 SSO 服务 |
| `redirect` | 重定向到指定 URL |
| `sse` | SSE 流式响应 |
| `directresponse` | 直接返回配置数据 |

### 数据源 (MixSource)

| 类型 | 说明 |
|------|------|
| `!query <key>` | 从请求 query 参数获取 |
| `!header <key>` | 从请求 header 获取 |
| `!bodyfield <path>` | 从请求 body JSON 字段获取 |
| `!reqquery <key>` | 从原始请求 query 获取（用于响应混合） |
| `!reqheader <key>` | 从原始请求 header 获取（用于响应混合） |

### 操作 (MixAction)

| 操作 | 说明 |
|------|------|
| `move` | 移动字段 |
| `copy` | 复制字段 |
| `deletesrc` | 删除源字段 |
| `addtarget <value>` | 添加目标字段 |
| `cacheset` | 将响应数据存入缓存 |
| `cacheget` | 从缓存获取数据 |

### 转换 (Transformation)

- `split`: 分割字符串
- `replace`: 替换内容
- `base64decode`: Base64 解码
- `lowercase` / `uppercase`: 大小写转换
- `format`: 格式化字符串
- `httpquery`: HTTP 请求转换
- `httpRequest`: 内部 HTTP 请求

## 环境变量

配置文件中支持 `${VAR_NAME}` 格式的环境变量：

```yaml
target_service: !redirect ${SSO_ADAPTER_SSO_URL}/some/path
```

`.env` 文件会在配置加载前自动读取。

## 许可证

MIT
