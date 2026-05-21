#![allow(dead_code, unused_imports)]
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub dify_url: String,
    pub sso_url: Option<String>,
    pub config_path: String,
    pub use_mode: UseMode,
    pub dify_host: Option<String>,
    pub self_host: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UseMode {
    Proxy,
    Normal,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PathConfig {
    pub request: RequestMapConfig,
    pub response: ResponseMapConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RequestMapConfig {
    pub target_service: ServiceType,
    pub method_mapping: Option<MethodMapping>,
    pub body_conversion: Option<BodyConversion>,
    pub mix_mappings: Vec<MixMapping>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ResponseMapConfig {
    pub method_mapping: Option<MethodMapping>,
    pub body_conversion: Option<BodyConversion>,
    pub mix_mappings: Vec<MixMapping>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    Dify,
    SSO,
    Redirect(Option<String>),
    SSE(String),
    // 直接响应：不发送真实请求，直接进入 response 流程
    DirectResponse,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MethodMapping {
    GetToPost,
    PostToGet,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MixMapping {
    #[serde(default)]
    pub source: Option<MixSource>,
    #[serde(default)]
    pub target: Option<MixTarget>,
    pub action: MixAction,
    #[serde(default)]
    pub transformations: Option<Vec<Transformation>>,
    #[serde(default)]
    pub cache_key_field: Option<String>,
    #[serde(default)]
    pub cache_expires_in: Option<u64>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Transformation {
    Base64Decode,
    Base64Encode,
    Split { separator: String, index: usize },
    Replace { from: String, to: String },
    Format { format: String },
    Append { value: String },
    Extract { regex: String },
    If,
    Merge,
    Lowercase,
    Uppercase,
    HttpRequest { url: String, method: Option<String>, body: Option<String>, headers: Option<std::collections::HashMap<String, String>>, query_params: Option<std::collections::HashMap<String, String>>, response_field: Option<String> },
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MixAction {
    Move,
    Copy,
    DeleteSrc,
    AddTarget(String),
    // 缓存操作（配合 MixMapping.cache_key_field 和 cache_expires_in 使用）
    CacheSet,
    CacheGet,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MixSource {
    Header(String),
    BodyField(String),
    Query(String),
    // 特殊 source：表示所有 query 参数，用于 httpquery 转换
    AllQueries,
    // 从原始 request 中获取数据（用于 response mix mappings）
    ReqQuery(String),
    ReqHeader(String),
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MixTarget {
    Header(String),
    BodyField(String),
    Query(String),
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BodyConversion {
    FormToJson,
    JsonToForm,
}