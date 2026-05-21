# SSO Adapter 实现总结

## 当前分支
`feature/sm2-sign`

## 已完成功能

### 1. 通用 JSON 缓存模块 (`src/cache.rs`)
- **CachedResponse**: 缓存条目结构，存储任意 JSON 数据 (`serde_json::Value`)，包含缓存时间和过期时间
- **JsonCache**: 基于 `DashMap` 的线程安全内存缓存
  - `set()`: 设置缓存，自动覆盖同名 key
  - `get()`: 获取缓存，过期自动删除
  - `get_and_clear()`: 获取并清除缓存（原子操作，用于一次性消费场景）
  - `clear()`: 手动清除指定 key
  - `len()`: 获取当前缓存大小
- **SharedCache**: `Arc<JsonCache>` 全局共享类型

### 2. 配置扩展 (`src/config.rs`)
- **ServiceType::DirectResponse**: 直接响应服务类型，跳过真实 HTTP 请求
- **MixSource::ReqQuery**: 从原始请求 query 参数获取值
- **MixSource::ReqHeader**: 从原始请求 header 获取值
- **MixAction::CacheSet**: 响应时设置缓存
  - `cache_key_field`: 从响应体哪个字段提取缓存 key
  - `cache_expires_in`: 过期时间（秒），默认 3600
- **MixAction::CacheGet**: 请求时获取缓存
  - `cache_key_field`: 从请求哪个字段提取缓存 key
- **Transformation::HttpRequest**: HTTP 请求转换
  - 支持在配置中发起内部 HTTP 请求
  - `url`, `method`, `headers`, `body`, `query_params` 参数
  - `response_field`: 从响应中提取的字段路径

### 3. 主流程集成 (`src/main.rs`)
- 全局缓存实例: `static USER_CACHE: Lazy<SharedCache>`
- **DirectResponse 服务类型处理**:
  - 跳过真实 HTTP 请求，直接使用配置构建响应
- **CacheSet 响应处理**:
  - 从响应体提取 `cache_key_field` 对应的值作为缓存 key
  - 将整个响应体 JSON 存入缓存
- **CacheGet 请求处理**:
  - 从请求参数提取 `cache_key_field` 对应的值作为缓存 key
  - 尝试从缓存获取数据，命中则直接返回
- **ReqQuery/ReqHeader 获取原始请求数据**:
  - `MixSource::ReqQuery(key)`: 从原始请求 query 获取值
  - `MixSource::ReqHeader(key)`: 从原始请求 header 获取值
- **环境变量解析**:
  - `substitute_env_vars()` 函数替换 `${VAR}` 占位符
  - 在 YAML 配置加载前完成替换

### 4. 瑞众项目配置 (`projs/config/mapping_ruizhong_oauth.yaml`)

| 路径 | 功能 | 说明 |
|------|------|------|
| `/pclogin` | 重定向 | Dify 配置入口，跳转到瑞众 portal 登录页 |
| `/oauth/api/v1/auth/sign/sso_token/code` | code 换 token | 瑞众实际接口路径，需要签名 |
| `/oauth/api/v1/auth/sign/checkSsoToken` | 获取用户信息 | 瑞众实际接口路径，从缓存读取 |
| `/api/enterprise/sso/oauth2/callback` | Dify 回调 | 透传 state 到 cookie |

**瑞众 SSO 流程**:
1. Dify 请求 `/pclogin` → 重定向到瑞众 portal 获取 code
2. Dify 请求 `/oauth/api/v1/auth/sign/sso_token/code` → 代理到瑞众 SSO，用 code 换 token
   - 所有 query 参数排序拼接后发起 HTTP 请求获取签名
   - 瑞众返回用户信息（包含 sso_token）
   - 将用户数据以 `sso_token` 为 key 缓存
3. Dify 请求 `/oauth/api/v1/auth/sign/checkSsoToken` → 从缓存获取用户数据并返回
   - 使用 `sso_token` 作为 key 查缓存
   - 缓存一次性消费，读取后自动清除

### 5. 依赖更新 (`Cargo.toml`)
- `dashmap = "5.5"`: 线程安全 HashMap
- `once_cell = "1.19"`: 全局静态变量延迟初始化

## 待完成功能

### SM2 签名
- 预留了 sm2、sha2、hex 依赖，后续需要时启用

## 文件变更清单

| 文件 | 状态 | 说明 |
|------|------|------|
| `src/cache.rs` | 新增 | 通用 JSON 缓存模块 |
| `src/config.rs` | 修改 | 添加 DirectResponse、ReqQuery/ReqHeader、CacheSet/CacheGet Action |
| `src/main.rs` | 修改 | 集成缓存读写逻辑、环境变量解析 |
| `Cargo.toml` | 修改 | 添加 dashmap、once_cell 依赖 |
| `projs/config/mapping_ruizhong_oauth.yaml` | 新增 | 瑞众 SSO 映射配置 |
