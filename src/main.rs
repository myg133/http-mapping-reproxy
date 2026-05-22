#![allow(dead_code, unused_imports)]
mod cache;

use axum::{
    body::{Bytes, Body},
    http::{header, Method, StatusCode, Uri},
    response::sse::{Event, KeepAlive, Sse},
    response::IntoResponse,
    response::Response,
    routing::any,
    Router,
};
use base64::prelude::*;
use cache::{create_shared_cache, SharedCache};
use config::{Transformation, UseMode};
use futures_util::StreamExt;
use reqwest::{Client,ClientBuilder};
use hyper::{header::HeaderValue, HeaderMap};
use once_cell::sync::Lazy;
use serde_json::{Map, Value};
use core::convert::Into;
use std::{
    collections::HashMap, convert::Infallible, fmt::format, net::SocketAddr, str::{self, FromStr}
};
use tokio::task::yield_now;
use tracing::{event, Level};
use url::form_urlencoded;

mod config;
use crate::config::{
    AppConfig, BodyConversion, MethodMapping, MixAction, MixSource, MixTarget, PathConfig,
    ServiceType,
};
use ::config::{Config, Environment};
use async_stream;
use regex::Regex;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};

// 全局用户信息缓存
static USER_CACHE: Lazy<SharedCache> = Lazy::new(create_shared_cache);

/// 替换配置内容中的 `${VAR}` 环境变量占位符
fn substitute_env_vars(content: &str) -> String {
    let re = Regex::new(r"\$\{([^}]+)\}").unwrap();
    re.replace_all(content, |caps: &regex::Captures| {
        let var_name = &caps[1];
        std::env::var(var_name).unwrap_or_else(|_| caps[0].to_string())
    }).to_string()
}

async fn load_config() -> anyhow::Result<(AppConfig, HashMap<String, PathConfig>)> {
    let _ = dotenv::dotenv().ok(); // 预加载 .env

    event!(Level::INFO, "Loading config");

    let app_config: AppConfig = Config::builder()
        .add_source(Environment::with_prefix("SSO_ADAPTER"))
        .build()
        .map_err(|e| event!(Level::ERROR, "Failed to build config: {}", e))
        .unwrap()
        .try_deserialize()
        .map_err(|e| event!(Level::ERROR, "Failed to deserialize config: {}", e))
        .unwrap();

    event!(Level::DEBUG, "Loaded config app_config: {:?}", app_config);

    match app_config.use_mode {
        UseMode::Normal => {
            // 处理普通模式下的配置加载逻辑
            app_config
                .sso_url
                .as_ref()
                .expect("SSO URL must be provided in Normal mode"); // 确保 SSO URL 存在
        }
        UseMode::Proxy => {
            // 处理其他模式下的配置加载逻辑
            app_config
                .dify_host
                .as_ref()
                .expect("Dify Host must be provided in Proxy mode"); // 确保 Dify Host 存在
        }
    } // 根据 use_mode 加载不同的配置逻辑

    let config_content = std::fs::read_to_string(&app_config.config_path)?;
    let config_content = substitute_env_vars(&config_content);
    let path_configs: HashMap<String, PathConfig> = serde_yaml::from_str(&config_content)
        .map_err(|e| {
            event!(Level::ERROR, "Failed to parse config file: {}", e);
        })
        .unwrap();

    event!(
        Level::DEBUG,
        "Loaded config path_configs: {:?}",
        path_configs
    );

    Ok((app_config, path_configs))
}

fn json_to_flat_map(value: &Value, prefix: &str, result: &mut HashMap<String, Value>) {
    match value {
        Value::Object(obj) => {
            for (key, val) in obj {
                let new_prefix = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", prefix, key)
                };
                json_to_flat_map(val, &new_prefix, result);
            }
        }
        Value::Array(arr) => {
            for (index, item) in arr.iter().enumerate() {
                let new_prefix = format!("{}[{}]", prefix, index);
                json_to_flat_map(item, &new_prefix, result);
            }
        }
        primitive => {
            result.insert(prefix.to_string(), primitive.clone());
        }
    }
}

/// 解析键路径，支持数组语法
/// 示例输入："aa[0].bb[1].cc" → ["aa", "0", "bb", "1", "cc"]
fn parse_key_path(key: &str) -> Vec<&str> {
    let re = Regex::new(r"\[(\d+)\]|(?<index>\d+)|(?<word>\w+)").unwrap();
    let mut parts = Vec::new();

    for cap in re.captures_iter(key) {
        if let Some(num) = cap.get(1).or(cap.name("index")) {
            parts.push(num.as_str());
        } else if let Some(word) = cap.name("word") {
            parts.push(word.as_str());
        }
    }

    parts
}

// map转json
fn flat_map_to_json(map: &HashMap<String, Value>) -> Value {
    let mut root = Value::Object(Map::new());

    for (key, value) in map {
        let parts = parse_key_path(key);
        insert_recursive(&mut root, &parts, value.clone());
    }

    root
}
// 递归处理
fn insert_recursive(current: &mut Value, parts: &[&str], value: Value) {
    let (first, rest) = match parts.split_first() {
        Some(p) => p,
        None => return,
    };
    // 判断当前是否是数组索引
    let is_array_index = first.parse::<usize>().is_ok();

    if is_array_index {
        // 处理数组路径
        let index = first.parse().unwrap();
        if !current.is_array() {
            *current = Value::Array(Vec::new());
        }
        let arr = current.as_array_mut().unwrap();

        // 扩展数组到所需长度
        while arr.len() <= index {
            arr.push(Value::Null);
        }
        if rest.is_empty() {
            // 叶节点：直接插入值
            arr[index] = value;
        }
        // 初始化元素为对象（如果当前位置是Null）
        else{
            if arr[index] == Value::Null {
                arr[index] = Value::Object(Map::new());
            }
            insert_recursive(&mut arr[index], rest, value);
        }
    } else if rest.is_empty() {
        // 叶节点：直接插入值
        if current.is_object() {
            current
                .as_object_mut()
                .unwrap()
                .insert(first.to_string(), value);
        } else {
            let mut map = Map::new();
            map.insert(first.to_string(), value);
            *current = Value::Object(map);
        }
    } else {
        // 确保当前是对象
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }
        // 中间节点：递归处理
        let map = current.as_object_mut().unwrap();
        // 获取或创建子节点
        let entry = map
            .entry(first.to_string())
            .or_insert(Value::Object(Map::new()));

        insert_recursive(entry, rest, value);
    }
}

/// 递归处理数组标识
fn post_process_arrays(map: &mut Map<String, Value>) {
    if let Some(Value::Array(arr)) = map.remove("_array") {
        // 将当前对象替换为数组
        *map = Map::new();
        for (i, mut elem) in arr.into_iter().enumerate() {
            if let Value::Object(elem_map) = &mut elem {
                // 递归处理数组元素
                post_process_arrays(elem_map);
            }
            map.insert(i.to_string(), elem);
        }
    } else {
        // 常规递归处理
        for (_, v) in map.iter_mut() {
            if let Value::Object(child) = v {
                post_process_arrays(child);
            }
        }
    }
}

pub fn query_to_map(query: &str) -> HashMap<String, String> {
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}
// 保留所有值的版本（返回 Vec<String>）
pub fn query_to_multimap(query: &str) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();

    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        map.entry(key.into_owned())
            .or_insert_with(Vec::new)
            .push(value.into_owned());
    }

    map
}
// HashMap<String, String> -> 查询字符串
pub fn map_to_query(map: &HashMap<String, String>) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(map.iter())
        .finish()
}
// HashMap<String, Vec<String>> -> 查询字符串
pub fn multimap_to_query(multimap: &HashMap<String, Vec<String>>) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            multimap
                .iter()
                .flat_map(|(k, vs)| vs.iter().map(move |v| (k.as_str(), v.as_str()))),
        )
        .finish()
}
// 将 query_map 按 key 排序并拼接为 key=value&key2=value2 格式
fn query_map_to_sorted_string(query_map: &HashMap<String, Vec<String>>) -> String {
    let mut keys: Vec<&String> = query_map.keys().collect();
    keys.sort();
    
    let mut parts = Vec::new();
    for key in keys {
        if let Some(values) = query_map.get(key) {
            for value in values {
                parts.push(format!("{}={}", key, value));
            }
        }
    }
    parts.join("&")
}

// json body 多级转换
fn merge_subfields(
    map: &HashMap<String, Value>,
    parent_key: &str,
    pairs: &mut HashMap<String, Value>,
) {
    // 处理父键自身的值（单层结构）
    if let Some(value) = map.get(parent_key) {
        pairs.insert(parent_key.to_string(), value.clone());
    }

    if !pairs.is_empty() {
        return;
    }

    // 处理嵌套子字段（多层结构）
    let prefix = format!("{}.", parent_key);
    for (k, v) in map {
        if let Some(sub_key) = k.strip_prefix(&prefix) {
            pairs.insert(sub_key.to_string(), v.clone());
        }
    }
}
// json body 多级转换为字符串
fn json_body_to_string(
    map: &HashMap<String, Value>,
    format: &str, // 格式模板，如 "{key}={value}"
) -> String {
    let mut pairs = Vec::new();

    if map.len() == 1 {
        return map.values().take(1).nth(0).unwrap().as_str().unwrap().to_string();
    }
    for (k, v) in map {
        let value_str = match v {
            Value::String(s) => s.as_str().to_string(),
            _ => v.to_string(),
        };
        let formatted = format
            .replace("{key}", k)
            .replace("{value}", &value_str);
        pairs.push(formatted);
    }
    // 按字母顺序排序保证一致性
    pairs.sort();
    pairs.join("; ")
}
// 获取 method
fn get_method(config: &Option<PathConfig>, method: &Method) -> Method {
    match config {
        Some(config) => match config.request.method_mapping {
            Some(MethodMapping::GetToPost) => Method::POST,
            Some(MethodMapping::PostToGet) => Method::GET,
            None => method.clone(),
        },
        None => method.clone(),
    }
}

fn map_to_json_body(
    res_json_map: &HashMap<String, Value>,
) -> Result<(Option<mime::Mime>, Vec<u8>), (StatusCode, String)> {
    let json_body = serde_json::to_vec(&flat_map_to_json(res_json_map)).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("JSON conversion error: {}", e),
        )
    })?;
    Ok((Some(mime::APPLICATION_JSON), json_body))
}

fn map_to_form_body(
    res_json_map: &HashMap<String, Value>,
) -> Result<(Option<mime::Mime>, Vec<u8>), (StatusCode, String)> {
    let form_str = serde_urlencoded::to_string(res_json_map).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Form conversion error: {}", e),
        )
    })?;
    Ok((
        Some(mime::APPLICATION_WWW_FORM_URLENCODED),
        form_str.into_bytes(),
    ))
}

// 处理转换
async fn apply_transformations(
        transformations: &[Transformation],
        value: &str,
        dst_value: Option<&str>,
        query_map: Option<&std::collections::HashMap<String, Vec<String>>>
    ) -> Option<String> {
    let mut result = value.to_string();

    // 占位符替换的辅助函数
    let replace_placeholders = |s: &str, current_result: &str| -> String {
        let mut res = s.to_string();
        event!(Level::DEBUG, "[replace_placeholders] input: {}", res);
        // 替换 {input}
        res = res.replace("{input}", current_result);
        event!(Level::DEBUG, "[replace_placeholders] after {{input}}: {}", res);
        // 替换 {query:key}
        if let Some(qm) = query_map {
            event!(Level::DEBUG, "[replace_placeholders] query_map keys: {:?}", qm.keys().collect::<Vec<_>>());
            for (key, vals) in qm.iter() {
                if let Some(v) = vals.first() {
                    let pattern = format!("{{query:{}}}", key);
                    event!(Level::DEBUG, "[replace_placeholders] trying to replace: {} -> {}", pattern, v);
                    res = res.replace(&pattern, v);
                }
            }
        }
        event!(Level::DEBUG, "[replace_placeholders] after query: {}", res);
        // 替换 ${VAR} 环境变量
        for key_val in std::env::vars() {
            res = res.replace(&format!("${{{}}}", key_val.0), &key_val.1);
        }
        event!(Level::DEBUG, "[replace_placeholders] final: {}", res);
        res
    };

    for transform in transformations {
        match transform {
            Transformation::Base64Decode => {
                result = base64::prelude::BASE64_STANDARD
                    .decode(&result)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .unwrap_or_default();
            }
            Transformation::Base64Encode => {
                result = base64::prelude::BASE64_STANDARD
                    .encode(&result);
            }
            Transformation::Split { separator, index } => {
                result = result
                    .split(separator)
                    .nth(*index)
                    .unwrap_or_default()
                    .to_string();
            }
            Transformation::Replace { from, to } => {
                result = result.replace(from, to);
            }
            Transformation::Format { format } => {
                result = format!("{}{}", format, result);
            }
            Transformation::Append { value } => {
                result.push_str(value);
            }
            Transformation::Merge => {
                if let Some(d_value) = dst_value {
                    result.push_str(d_value);
                }
            }
            Transformation::If => {
                if let Some(d_val) = dst_value {
                    if !d_val.is_empty() {
                        result = d_val.to_string();
                    }
                }
            }
            // Test
            Transformation::Extract { regex } => {
                let re = Regex::new(regex).unwrap();
                result = re.find(&result)
                .map(|mat| mat.as_str().to_string())
                .unwrap_or_else(|| result);
            }
            Transformation::Lowercase => {
                result = result.to_lowercase();
            }
            Transformation::Uppercase => {
                result = result.to_uppercase();
            }
            Transformation::HttpRequest { url, method, body, headers, query_params, response_field } => {
                event!(Level::DEBUG, ">>> HttpRequest transformation start");
                let client = ClientBuilder::new()
                    .danger_accept_invalid_certs(true)
                    .build()
                    .unwrap_or_else(|_| Client::new());
                let http_method = method.as_deref().unwrap_or("POST");
                event!(Level::DEBUG, "    method: {}", http_method);
                
                // 处理 URL 占位符替换
                let processed_url = replace_placeholders(url, &result);
                event!(Level::DEBUG, "    url: {} -> {}", url, processed_url);
                
                let mut request_builder = match http_method.to_uppercase().as_str() {
                    "GET" => client.get(processed_url),
                    "POST" => client.post(processed_url),
                    "PUT" => client.put(processed_url),
                    "DELETE" => client.delete(processed_url),
                    _ => {
                        event!(Level::ERROR, "Unsupported HTTP method: {}", http_method);
                        return None;
                    }
                };
                
                // 添加 headers
                if let Some(header_map) = headers {
                    for (key, val) in header_map {
                        let processed_val = replace_placeholders(val, &result);
                        event!(Level::DEBUG, "    header: {} -> {}", key, processed_val);
                        if let Ok(header_val) = reqwest::header::HeaderValue::from_str(&processed_val) {
                            request_builder = request_builder.header(key, header_val);
                        }
                    }
                }
                
                // 添加 query params
                if let Some(query_map) = query_params {
                    for (key, val) in query_map {
                        let processed_val = replace_placeholders(val, &result);
                        event!(Level::DEBUG, "    query: {} -> {}", key, processed_val);
                        request_builder = request_builder.query(&[(key, processed_val)]);
                    }
                }
                
                // 如果有 body，设置请求体
                if let Some(body_content) = body {
                    let processed_body = replace_placeholders(body_content, &result);
                    event!(Level::DEBUG, "    body: {} -> {}", body_content, processed_body);
                    request_builder = request_builder.body(processed_body);
                } else {
                    event!(Level::DEBUG, "    body (no config): {}", result);
                    request_builder = request_builder.body(result.clone());
                }
                
                // 构建请求并打印完整 URL
                if let Some(cloned) = request_builder.try_clone() {
                    match cloned.build() {
                        Ok(req) => {
                            event!(Level::DEBUG, "<<< HttpRequest complete URL: {}", req.url());
                        }
                        Err(e) => {
                            event!(Level::WARN, "<<< HttpRequest failed to build: {:?}", e);
                        }
                    }
                } else {
                    event!(Level::WARN, "<<< HttpRequest failed to clone builder for URL logging");
                }
                
                // 发送请求并获取响应
                event!(Level::DEBUG, "<<< HttpRequest sending...");
                match request_builder.send().await {
                    Ok(response) => {
                        event!(Level::DEBUG, "    response status: {}", response.status());
                        match response.text().await {
                            Ok(text) => {
                                event!(Level::DEBUG, "    response body: {}", text);
                                
                                // 如果配置了 response_field，则从 JSON 中提取该字段
                                if let Some(field) = response_field {
                                    event!(Level::DEBUG, "    Extracting field: {}", field);
                                    match serde_json::from_str::<serde_json::Value>(&text) {
                                        Ok(json) => {
                                            if let Some(value) = json.get(field) {
                                                if let Some(str_value) = value.as_str() {
                                                    result = str_value.to_string();
                                                    event!(Level::DEBUG, "    Extracted value: {}", result);
                                                } else {
                                                    // 如果不是字符串，直接转为字符串
                                                    result = value.to_string();
                                                    event!(Level::DEBUG, "    Extracted non-string value: {}", result);
                                                }
                                            } else {
                                                event!(Level::WARN, "    Field '{}' not found in response", field);
                                                result = text;
                                            }
                                        }
                                        Err(e) => {
                                            event!(Level::WARN, "    Failed to parse response as JSON: {:?}", e);
                                            result = text;
                                        }
                                    }
                                } else {
                                    result = text;
                                }
                            }
                            Err(e) => {
                                event!(Level::ERROR, "Failed to read HTTP response: {:?}", e);
                                return None;
                            }
                        }
                    }
                    Err(e) => {
                        event!(Level::ERROR, "HTTP request failed: {:?}", e);
                        return None;
                    }
                }
            }
        }

        if result.is_empty() {
            return None;
        }
    }

    Some(result)
}

// 重构，获取headervalue
fn get_header_val(
    headers_map: &mut hyper::HeaderMap,
    action: &config::MixAction,
    src: &String,
) -> Option<HeaderValue> {
    let value = match &action {
        MixAction::Move => headers_map.remove(src.as_str()),
        MixAction::Copy => headers_map.get(src.as_str()).cloned(),
        MixAction::AddTarget(value) => Some(value.parse().unwrap()),
        MixAction::DeleteSrc => {
            headers_map.remove(src.as_str());
            None
        }
        // CacheSet/CacheGet 不在 header 级别处理
        MixAction::CacheSet | MixAction::CacheGet | MixAction::CacheHeaderSet(_) => None,
    };
    value
}

// 重构，获取querymap value
fn get_querymap_val(
    map: &mut HashMap<String, Vec<String>>,
    action: &config::MixAction,
    src: &String,
) -> Option<Vec<String>> {
    let value = match &action {
        MixAction::Move => map.remove(src.as_str()),
        MixAction::Copy => map.get(src.as_str()).cloned(),
        MixAction::AddTarget(value) => Some(vec![value.clone().parse().unwrap()]),
        MixAction::DeleteSrc => {
            map.remove(src.as_str());
            None
        }
        // CacheSet/CacheGet 不在 query 级别处理
        MixAction::CacheSet | MixAction::CacheGet | MixAction::CacheHeaderSet(_) => None,
    };
    value
}

// 重构，获取bodymap value
fn get_bodymap_val(
    map: &mut HashMap<String, Value>,
    action: &config::MixAction,
    src: &String,
) -> Option<Value> {
    let value = match &action {
        MixAction::Move => map.remove(src.as_str()),
        MixAction::Copy => map.get(src.as_str()).cloned(),
        MixAction::AddTarget(value) => Some(value.clone().parse().unwrap()),
        MixAction::DeleteSrc => {
            map.remove(src.as_str());
            None
        }
        // CacheSet/CacheGet 不在 body 级别处理
        MixAction::CacheSet | MixAction::CacheGet | MixAction::CacheHeaderSet(_) => None,
    };
    value
}

async fn proxy_handler(
    //request: axum::extract::Request,
    uri: Uri,
    method: Method,
    headers: header::HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    // 打印当前缓存状态（调试用）
    USER_CACHE.debug_print();

    // if method == Method::CONNECT {
    //     return handle_https_tunnel(uri, *addr).await;
    // }

    // event!(Level::DEBUG, "origin info {:?}",request);

    // let uri: Uri = request.uri().clone();
    // let method: Method = request.method().clone();
    // let headers: header::HeaderMap = request.headers().clone();
    // let body: Bytes = axum::body::to_bytes(request.into_body(),usize::MAX).await.unwrap();

    let (app_config, path_configs) = load_config().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Config load failed: {}", e),
        )
    })?;
    // 使用模式
    let use_mode = app_config.use_mode.clone();
    event!(Level::INFO, "Use mode: {:?}", use_mode);

    // 记录请求基本信息
    event!(Level::INFO, "Received {} request to {}", method, uri);
    event!(
        Level::DEBUG,
        "Request headers: {:?} | Body size: {} bytes",
        headers
            .iter()
            .map(|(n, v)| format!("{}={}", n, v.to_str().unwrap()))
            .collect::<Vec<_>>(),
        body.len()
    );

    let mut path = uri.path();
    let query = uri.query();

    event!(Level::DEBUG, "Path: {:?}", path);
    event!(Level::DEBUG, "Query: {:?}", query);

    // 模式
    let (config, base_url) = match use_mode {
        // 代理模式，如果 config 为空， 则执行代理模式
        UseMode::Proxy => {
            let config = path_configs.get(path).clone();
            match config {
                // 命中配置
                Some(config) => (
                    Some(config.clone()),
                    match config.request.target_service.clone() {
                        ServiceType::Dify => &app_config.dify_url,
                        ServiceType::Redirect(url) => {
                            if let Some(u) = url {
                                path = "";
                                &u.clone()
                            }
                            else {
                                &app_config
                                .sso_url
                                .ok_or((
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    format!("SSO URL not configured"),
                                ))?
                                .clone()
                            }
                        }
                        ServiceType::SSO | ServiceType::SSE(_) | ServiceType::DirectResponse => &uri
                            .host()
                            .ok_or((StatusCode::BAD_REQUEST, format!("Host header missing")))?
                            .to_string()
                            .clone(), // 使用原始请求的host
                    },
                ),
                // 未命中配置，判断是否为入栈请求，入栈请求则转发到 dify_url，否则转发到原始请求的host
                None => {
                    if uri.host().is_some()
                        && app_config.dify_host.is_some()
                        && app_config
                            .dify_host
                            .unwrap()
                            .eq(&uri.host().unwrap().to_string())
                    {
                        // 入栈
                        (None, &app_config.dify_url)
                    } else {
                        // 出站
                        (
                            None,
                            &format!(
                                "{}://{}",
                                uri.scheme_str().unwrap_or("https"),
                                uri.host().unwrap().to_string()
                            ),
                        )
                    }
                }
            }
            // 返回结果
        }
        // 正常模式， config 不能为空，否则返回404
        UseMode::Normal => {
            let config = path_configs
                .get(path)
                .ok_or((
                    StatusCode::NOT_FOUND,
                    format!("Path {} not configured", path),
                ))?
                .clone();
            // 返回结果
            (
                Some(config.clone()),
                match config.request.target_service {
                    ServiceType::Dify => &app_config.dify_url,
                    ServiceType::Redirect(url) => {
                        if let Some(u) = url {
                            path = "";
                            &u.clone()
                        }
                        else {
                            &app_config
                            .sso_url
                            .ok_or((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("SSO URL not configured"),
                            ))?
                            .clone()
                        }
                    }
                    ServiceType::SSO | ServiceType::SSE(_) | ServiceType::DirectResponse => &app_config
                        .sso_url
                        .ok_or((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("SSO URL not configured"),
                        ))?
                        .clone(),
                },
            )
        }
    };

    // method转换
    let target_method = get_method(&config, &method);

    // query - 必须提前构建，因为 CacheGet 需要用到
    let mut query_map = match query {
        Some(query) => query_to_multimap(&query),
        None => HashMap::new(),
    };
    // body
    let mut json_map = HashMap::new();
    // headers
    let mut headers_map = HeaderMap::new();
    
    // 保存原始 request 的 headers 和 query，供 response mix mappings 使用
    let req_headers = headers.clone();
    let req_query = query.map(|q| q.to_string()).unwrap_or_default();
    // 解析原始 query 供 response mix mappings 使用
    let req_query_map = if !req_query.is_empty() {
        query_to_multimap(&req_query)
    } else {
        HashMap::new()
    };

    let content_type = match method {
        Method::POST | Method::PUT => headers.get(header::CONTENT_TYPE).cloned().ok_or((
            StatusCode::BAD_REQUEST,
            format!("Missing Header: content-type"),
        ))?,
        _ => HeaderValue::from_str("").unwrap(),
    };

    // 打印原始 body 内容
    event!(Level::DEBUG, "Raw request body: {}", String::from_utf8_lossy(&body));
    
    // 根据 content-type 解析 body 数据
    if content_type == mime::APPLICATION_JSON.essence_str() {
        // json
        let json_data: Value = serde_json::from_slice(&body)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("JSON parse error: {}", e)))?;
        json_to_flat_map(&json_data, "", &mut json_map);
    } else if content_type == mime::APPLICATION_WWW_FORM_URLENCODED.essence_str() {
        // form
        let form_data = serde_urlencoded::from_bytes::<HashMap<String, Value>>(&body)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("Form parse error: {}", e)))?;
        json_map = form_data.clone();
    }
    
    // 打印解析后的 body map
    event!(Level::DEBUG, "Parsed json_map: {:?}", json_map);

    event!(Level::DEBUG, "matched config: {:?}", &config);

    // 未匹配的，添加源header到新header
    headers_map.extend(headers.clone());

    // 处理request.mix_mappings
    if let Some(conf) = &config {
        event!(Level::DEBUG, ">>> Starting mix mappings, query_map keys: {:?}", query_map.keys().collect::<Vec<_>>());
        for (idx, mapping) in conf.request.mix_mappings.iter().enumerate() {
            let m = mapping.clone();
            let s = m.source.clone();
            let t = m.target.clone();
            let trans_s = m.transformations.clone();
            event!(Level::DEBUG, ">>> mix mapping {}", idx);

            // 处理 CacheHeaderSet（它不需要 source 和 target）
            match &m.action {
                MixAction::CacheHeaderSet(header_name) => {
                    if let Some(key_field) = &m.cache_key_field {
                        event!(Level::DEBUG, ">>> Request CacheHeaderSet header: {}, key_field: {}", header_name, key_field);
                        // 从 query_map 中获取 key_field 对应的值作为缓存 key
                        if let Some(cache_key) = query_map.get(key_field).and_then(|v| v.first()) {
                            if let Some(value) = headers.get(header_name.as_str()) {
                                let value_str = value.to_str().unwrap_or_default().to_string();
                                event!(Level::DEBUG, ">>> CacheHeaderSet cache_key: {}, value: {}", cache_key, value_str);
                                let expires = m.cache_expires_in.unwrap_or(3600);
                                USER_CACHE.set(cache_key.clone(), Value::String(value_str), expires);
                                event!(Level::INFO, "Header {} cached with key {}", header_name, cache_key);
                            } else {
                                event!(Level::WARN, ">>> CacheHeaderSet header {} not found in request", header_name);
                            }
                        } else {
                            event!(Level::WARN, ">>> CacheHeaderSet key_field {} not found in query", key_field);
                        }
                    }
                    continue;
                }
                _ => {}
            }

            if s.is_none() || t.is_none() {
                continue;
            }
            let s = s.unwrap();
            let t = t.unwrap();
            match (&s, t) {
                // ReqQuery 和 ReqHeader 只在 response mix mappings 中使用
                (MixSource::ReqQuery(_) | MixSource::ReqHeader(_), _) => {
                    event!(Level::WARN, "ReqQuery/ReqHeader only supported in response mix_mappings, ignoring");
                }
                // Header to Header
                (MixSource::Header(src), MixTarget::Header(dst)) => {
                    if let Some(mut value) = get_header_val(&mut headers_map, &m.action, src) {
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_header_val(&mut headers_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.to_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value.to_str().unwrap(), dst_val.as_deref(), Some(&query_map)).await
                            {
                                value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        headers_map.insert(obj.as_str(), value.clone());
                    }
                }
                // Header to Body
                (MixSource::Header(src), MixTarget::BodyField(dst)) => {
                    if let Some(mut value) = get_header_val(&mut headers_map, &m.action, src) {
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_bodymap_val(&mut json_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.as_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans,&value.to_str().unwrap(), dst_val.as_deref(), Some(&query_map)).await
                            {
                                value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        json_map.insert(
                            obj.to_string(),
                            Value::String(value.clone().to_str().unwrap().to_string()),
                        );
                    }
                }
                // Header to Query
                (MixSource::Header(src), MixTarget::Query(dst)) => {
                    event!(Level::DEBUG, ">>> Header to Query: {} -> {}", src, dst);
                    let value_opt = get_header_val(&mut headers_map, &m.action, src);
                    event!(Level::DEBUG, ">>> get_header_val returned: {:?}", value_opt);
                    if let Some(mut value) = value_opt {
                        event!(Level::DEBUG, ">>> value before trans: {}", value.to_str().unwrap_or_default());
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_querymap_val(&mut query_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.join(",")));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value.to_str().unwrap(), dst_val.as_deref(), Some(&query_map)).await
                            {
                                value = transformed.parse().unwrap();
                                event!(Level::DEBUG, ">>> value after trans: {}", value.to_str().unwrap_or_default());
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        event!(Level::DEBUG, ">>> inserting into query_map: {} -> {}", obj.to_string(), value.to_str().unwrap_or_default());
                        query_map.insert(
                            obj.to_string(),
                            vec![value.clone().to_str().unwrap().to_string()],
                        );
                    }
                }
                // CacheHeader to Query (request 阶段支持)
                (MixSource::CacheHeader, MixTarget::Query(dst)) => {
                    event!(Level::DEBUG, ">>> CacheHeader to Query");
                    if let Some(key_field) = &m.cache_key_field {
                        // 从 query_map 中获取 key_field 对应的值作为缓存 key
                        if let Some(cache_key) = query_map.get(key_field).and_then(|v| v.first()) {
                            if let Some(cached) = USER_CACHE.get(cache_key) {
                                if let Some(value_str) = cached.as_str() {
                                    event!(Level::DEBUG, ">>> CacheHeader found: {} -> {}", cache_key, value_str);
                                    let obj = Box::leak(Box::new(dst));
                                    query_map.insert(obj.to_string(), vec![value_str.to_string()]);
                                }
                            } else {
                                event!(Level::WARN, ">>> CacheHeader key {} not found in cache", cache_key);
                            }
                        } else {
                            event!(Level::WARN, ">>> CacheHeader key_field {} not found in query", key_field);
                        }
                    }
                }
                // CacheHeader to Header (request 阶段支持)
                (MixSource::CacheHeader, MixTarget::Header(dst)) => {
                    event!(Level::DEBUG, ">>> CacheHeader to Header");
                    if let Some(key_field) = &m.cache_key_field {
                        // 从 query_map 中获取 key_field 对应的值作为缓存 key
                        if let Some(cache_key) = query_map.get(key_field).and_then(|v| v.first()) {
                            if let Some(cached) = USER_CACHE.get(cache_key) {
                                if let Some(value_str) = cached.as_str() {
                                    event!(Level::DEBUG, ">>> CacheHeader found: {} -> {}", cache_key, value_str);
                                    let header_value: HeaderValue = value_str.parse().unwrap();
                                    let obj = Box::leak(Box::new(dst));
                                    headers_map.insert(obj.as_str(), header_value);
                                }
                            }
                        }
                    }
                }
                // CacheHeader to BodyField (request 阶段支持)
                (MixSource::CacheHeader, MixTarget::BodyField(dst)) => {
                    event!(Level::DEBUG, ">>> CacheHeader to BodyField");
                    if let Some(key_field) = &m.cache_key_field {
                        // 从 query_map 中获取 key_field 对应的值作为缓存 key
                        if let Some(cache_key) = query_map.get(key_field).and_then(|v| v.first()) {
                            if let Some(cached) = USER_CACHE.get(cache_key) {
                                if let Some(value_str) = cached.as_str() {
                                    event!(Level::DEBUG, ">>> CacheHeader found: {} -> {}", cache_key, value_str);
                                    let obj = Box::leak(Box::new(dst));
                                    json_map.insert(obj.to_string(), Value::String(value_str.to_string()));
                                }
                            }
                        }
                    }
                }
                // Quert to Query
                (MixSource::Query(src), MixTarget::Query(dst)) => {
                    event!(Level::DEBUG, ">>> Query to Query: {} -> {}", src, dst);
                    let value_opt = match &m.action {
                        MixAction::AddTarget(value) => {
                            // AddTarget 直接使用指定的值，不依赖 source
                            event!(Level::DEBUG, ">>> AddTarget with value: {}", value);
                            Some(vec![value.clone()])
                        },
                        _ => get_querymap_val(&mut query_map, &m.action, src)
                    };
                    event!(Level::DEBUG, ">>> value_opt resolved to: {:?}", value_opt);
                    if let Some(mut value) = value_opt {
                        event!(Level::DEBUG, ">>> value before trans: {}", value.join(","));
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_querymap_val(&mut query_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.join(",")));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value.join(",").as_str(),dst_val.as_deref(), Some(&query_map)).await
                            {
                                let v:String = transformed.parse().unwrap();
                                value = vec!(v.split(",").collect());
                                event!(Level::DEBUG, ">>> value after trans: {}", value.join(","));
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        event!(Level::DEBUG, ">>> inserting into query_map: {} -> {}", obj.to_string(), value.join(","));
                        query_map.insert(obj.to_string(), value);
                    }
                }
                // Query to Header
                (MixSource::Query(src), MixTarget::Header(dst)) => {
                    if let Some(mut value) = get_querymap_val(&mut query_map, &m.action, src){
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_header_val(&mut headers_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.to_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value.join(",").as_str(), dst_val.as_deref(), Some(&query_map)).await
                            {
                                let v:String = transformed.parse().unwrap();
                                value = vec!(v.split(",").collect());
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        headers_map.insert(
                            obj.as_str(),
                            HeaderValue::from_str(value.join(",").as_str()).unwrap(),
                        );
                    }
                }
                // AllQueries to Header - 将所有 query 参数排序拼接
                (MixSource::AllQueries, MixTarget::Header(dst)) => {
                    let query_str = query_map_to_sorted_string(&query_map);
                    let mut value = query_str.clone();
                    if let Some(trans) = trans_s.clone() {
                        let dst_val: Option<String> = get_header_val(&mut headers_map, &MixAction::Copy, &dst)
                            .map_or(None, |v| Some(v.to_str().unwrap().to_string()));
                        if let Some(transformed) =
                        apply_transformations(&trans, &query_str, dst_val.as_deref(), Some(&query_map)).await
                        {
                            value = transformed;
                        }
                    }
                    let obj = Box::leak(Box::new(dst));
                    headers_map.insert(
                        obj.as_str(),
                        HeaderValue::from_str(value.as_str()).unwrap(),
                    );
                }
                // Query to Body
                (MixSource::Query(src), MixTarget::BodyField(dst)) => {
                    if let Some(mut value) = get_querymap_val(&mut query_map, &m.action, src){
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_bodymap_val(&mut json_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.as_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value.join(",").as_str(),dst_val.as_deref(), Some(&query_map)).await
                            {
                                let v:String = transformed.parse().unwrap();
                                value = vec!(v.split(",").collect());
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        json_map.insert(obj.to_string(), Value::String(value.join(",")));
                    }
                }
                // Body to Body
                // TODO Handle transformations
                (MixSource::BodyField(src), MixTarget::BodyField(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&json_map, &src, &mut res_json);
                    match &m.action {
                        MixAction::Move => {
                            for (k, v) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                json_map.remove(src_key.as_str()); // delete source
                                json_map.remove(k.as_str()); // delete source
                                json_map.insert(k.clone().replace(src, dst.as_str()), v.clone());
                            }
                        }
                        MixAction::Copy => {
                            for (k, v) in res_json.iter() {
                                json_map.insert(k.clone().replace(src, dst.as_str()), v.clone());
                            }
                        }
                        MixAction::AddTarget(v) => {
                            json_map.insert(src.clone(), Value::String(v.clone()));
                        }
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                json_map.remove(k.as_str()); // delete source
                            }
                        }
                        // CacheSet/CacheGet 不在 request body->body 处理
                        MixAction::CacheSet | MixAction::CacheGet | MixAction::CacheHeaderSet(_) => {}
                    };
                }
                // Body to Query
                // TODO Handle transformations
                (MixSource::BodyField(src), MixTarget::Query(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&json_map, &src, &mut res_json);
                    let value = match &m.action {
                        MixAction::Move => {
                            for (k, _) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                json_map.remove(src_key.as_str()); // delete source
                                json_map.remove(k.as_str());
                            }
                            Some(json_body_to_string(&res_json, "{key}={value}"))
                        }
                        MixAction::Copy => Some(json_body_to_string(&res_json, "{key}={value}")),
                        MixAction::AddTarget(v) => Some(v.clone()), // Add a static value to the query
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                json_map.remove(k.as_str());
                            }
                            None
                        }
                        // CacheSet/CacheGet 不在 request body->query 处理
                        MixAction::CacheSet | MixAction::CacheGet | MixAction::CacheHeaderSet(_) => None,
                    };
                    if let Some(value) = value {
                        let obj = Box::leak(Box::new(dst));
                        query_map.insert(obj.to_string(), vec![value]);
                    }
                }
                // AllQueries to Query/Body - 不支持
                (MixSource::AllQueries, MixTarget::Query(_)) |
                (MixSource::AllQueries, MixTarget::BodyField(_)) => {
                    event!(Level::WARN, "AllQueries source only supports Header target");
                }
                // Body to Header
                // TODO Handle transformations
                (MixSource::BodyField(src), MixTarget::Header(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&json_map, &src, &mut res_json);
                    let value = match &m.action {
                        MixAction::Move => {
                            for (k, _) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                json_map.remove(src_key.as_str()); // delete source
                                json_map.remove(k.as_str());
                            }
                            Some(json_body_to_string(&res_json, "{key}={value}"))
                        }
                        MixAction::Copy => Some(json_body_to_string(&res_json, "{key}={value}")),
                        MixAction::AddTarget(v) => Some(v.clone()), // Add a static value to the query
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                json_map.remove(k.as_str());
                            }
                            None
                        }
                        // CacheSet/CacheGet 不在 request body->header 处理
                        MixAction::CacheSet | MixAction::CacheGet | MixAction::CacheHeaderSet(_) => None,
                    };
                    if let Some(value) = value {
                        let obj = Box::leak(Box::new(dst));
                        headers_map
                            .insert(obj.as_str(), HeaderValue::from_str(value.as_str()).unwrap());
                    }
                }
            }
            event!(Level::DEBUG, "<<< mix mapping {}, query_map keys: {:?}", idx, query_map.keys().collect::<Vec<_>>());
        }
    }

    event!(Level::DEBUG, "All mix mappings done, query_map keys: {:?}", query_map.keys().collect::<Vec<_>>());
    event!(Level::DEBUG, "final body : {:?}", json_map);

    // 目标地址处理 + query参数
    let target_url = if query_map.len() > 0 {
        format!("{}{}?{}", base_url, path, multimap_to_query(&query_map))
    } else {
        format!("{}{}", base_url, path)
    };
    event!(Level::DEBUG, "Target URL: {}", target_url);

    let c = config.clone();

    // redirect处理
    let req_red = match c.clone().unwrap().request.target_service.clone() {
        ServiceType::Redirect(_) => {
            // 处理重定向服务的请求
            let mut h = header::HeaderMap::new();
            h.insert(
                header::LOCATION,
                HeaderValue::from_str(target_url.as_str()).unwrap(),
            );
            let b = Vec::<u8>::new();
            // 返回重定向响应
            Some((StatusCode::FOUND, h, axum::body::Bytes::from(b)))
        }
        _ => None,
    };

    if req_red.is_some() {
        return Ok(req_red.unwrap().into_response());
    }

    // 如果是 GET 请求，清空 json_map 来避免发送 body
    let mut final_json_map = json_map.clone();
    if target_method == Method::GET {
        final_json_map.clear();
    }

    // 如果是 GET 请求，强制使用空 body
    let (def_content_type, def_body) = if target_method == Method::GET {
        (None, Vec::new())
    } else {
        (
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.parse().unwrap()),
            body.to_vec(),
        )
    };
    let def_json_body = (def_content_type, def_body);

    // 生成真实请求body
    let (content_type, converted_body) = match &config {
        Some(config) => match config.request.body_conversion {
            Some(BodyConversion::FormToJson) => map_to_json_body(&final_json_map)?,
            Some(BodyConversion::JsonToForm) => {
                let form_str = serde_urlencoded::to_string(&final_json_map).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Form conversion error: {}", e),
                    )
                })?;
                (
                    Some(mime::APPLICATION_WWW_FORM_URLENCODED),
                    form_str.into_bytes(),
                )
            }
            None => {
                if !final_json_map.is_empty() {
                    map_to_json_body(&final_json_map)?
                } else {
                    def_json_body
                }
            }
        },
        None => {
            if !final_json_map.is_empty() {
                map_to_json_body(&final_json_map)?
            } else {
                def_json_body
            }
        }
    };

    let mut _b = String::new();
    _b = String::from_utf8(converted_body.clone()).unwrap();
    event!(Level::DEBUG, "Request Body: {:?}", &_b);

    // 转换body类型
    if content_type.is_some() {
        // 处理Body转换的header
        headers_map.remove(header::CONTENT_TYPE);
        headers_map.insert(
            header::CONTENT_TYPE,
            content_type.unwrap().to_string().parse().unwrap(),
        );
    }

    // 如果是 GET 请求，移除 Content-Type 和 Content-Length
    if target_method == Method::GET {
        headers_map.remove(header::CONTENT_TYPE);
        headers_map.remove(header::CONTENT_LENGTH);
    } else {
        // 非 GET 请求，才设置 Content-Length
        headers_map.insert(
            header::CONTENT_LENGTH,
            converted_body.len().to_string().parse().unwrap(),
        );
    }

    // 请求模式需要修改 host头
    if use_mode == UseMode::Normal {
        let to_host = Uri::from_str(base_url).unwrap().host().unwrap().to_string();
        // 处理 host header
        headers_map.remove(header::HOST);
        headers_map.insert(header::HOST, to_host.parse().unwrap()); // 设置目标host
    }

    if headers_map.contains_key(header::TRANSFER_ENCODING) {
        let transfer_encoding = headers_map.get(header::TRANSFER_ENCODING);
        if transfer_encoding.is_some()
            && transfer_encoding
                .unwrap()
                .to_str()
                .unwrap()
                .contains("chunked")
        {
            headers_map.remove(header::CONTENT_LENGTH);
        }
    }

    // 移除压缩编码头
    if headers_map.contains_key(header::ACCEPT_ENCODING){
        headers_map.remove(header::ACCEPT_ENCODING);
    }
    event!(Level::INFO, "Sending {} request to {}", &target_method, &target_url);
    event!(
        Level::DEBUG,
        "Request headers: {:?} | Body size: {}",
        &headers_map,
        &converted_body.len()
    );

    // sse 处理 所有前置处理完成后
    let is_sse_req = match c.as_ref().unwrap().request.target_service.clone() {
        ServiceType::SSE(source) => {
            // 解析配置字符串（例如 "bodyfield-stream"）
            let (src_type, src_value) = source.split_once('-').expect("Invalid SSE source format");
            match src_type.to_lowercase().as_str() {
                "bodyfield" => json_map.get(src_value).and_then(|v| v.as_bool()).unwrap(),
                "header" => headers_map
                    .get(src_value)
                    .and_then(|hv| hv.to_str().ok())
                    .and_then(|s| s.parse().ok()).unwrap(),
                "query" => query_map
                    .get(src_value)
                    .map(|values| Value::String(values.join(",")))
                    .and_then(|v| v.as_bool()).unwrap(),
                _ => false,
            }
        }
        _ => false,
    };

    event!(Level::DEBUG, "is_sse_req: {:?}", is_sse_req);

    // 检查是否是 DirectResponse 类型请求（跳过真实请求，直接进入 response 流程）
    let is_direct_response = match c.as_ref().unwrap().request.target_service.clone() {
        ServiceType::DirectResponse => true,
        _ => false,
    };
    
    event!(Level::DEBUG, "is_direct_response: {:?}", is_direct_response);

    // 尝试从缓存获取数据（如果配置了 CacheGet）
    let cache_config = if is_direct_response {
        // 从 response mix_mappings 中找 CacheGet action 的配置
        let mut cache_key: Option<String> = None;
        if let Some(conf) = &c {
            for m in &conf.response.mix_mappings {
                if let MixAction::CacheGet = &m.action {
                    if let Some(key_field) = &m.cache_key_field {
                        event!(Level::DEBUG, ">>> CacheGet looking for key_field: {}", key_field);
                        // 尝试从 query_map 中获取 key（经过 request mix mappings 处理后的）
                        cache_key = query_map.get(key_field)
                            .and_then(|v| v.first()).map(|s| s.clone());
                        if cache_key.is_none() {
                            // 尝试从 header 中获取 key
                            cache_key = headers.get(key_field.as_str())
                                .and_then(|v| v.to_str().ok())
                                .map(|s| s.to_owned());
                        }
                        event!(Level::DEBUG, ">>> CacheGet found key: {:?}", cache_key);
                    }
                    break;
                }
            }
        }

        if let Some(key) = cache_key {
            if let Some(cached_body) = USER_CACHE.get_and_clear(&key) {
                event!(Level::INFO, "Cache hit for key: {}...", &key[..8.min(key.len())]);
                Some(cached_body)
            } else {
                event!(Level::WARN, "Cache miss for key: {}...", &key[..8.min(key.len())]);
                None
            }
        } else {
            event!(Level::WARN, ">>> CacheGet: no key found");
            None
        }
    } else {
        None
    };

    // 如果是 SSE 请求或 DirectResponse，不发送真实请求
    let mut response: Option<reqwest::Response> = if is_sse_req || is_direct_response {
        None
    } else {
        let client = ClientBuilder::new()
            .danger_accept_invalid_hostnames(true)
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .build();

        let request_builder = match target_method {
            Method::GET => client.unwrap().get(&target_url.clone()),
            Method::POST => {
                // 如果 final_json_map 为空，不发送 body
                if final_json_map.is_empty() {
                    client.unwrap().post(&target_url.clone())
                } else {
                    client.unwrap().post(&target_url.clone()).body(converted_body)
                }
            }
            _ => unreachable!(),
        };

        Some(request_builder
            .headers(headers_map)
            .send()
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Forward request failed: {}", e),
                )
            })?)
    };

    // redirect - cache_hit 时跳过 redirect 处理
    if let Some(ref resp) = response {
        if resp.status().is_redirection() {
            let mut red_headers_map = header::HeaderMap::new();
            let location_header = resp.headers();
            red_headers_map.extend(location_header.clone());
            let b = Vec::<u8>::new();
            event!(Level::DEBUG, "Redirect Header: {:?}", red_headers_map);
            return Ok((
                resp.status(),
                red_headers_map,
                axum::body::Bytes::from(b)
            ).into_response());
        }
    }

    // headers
    let mut res_headers_map = header::HeaderMap::new();

    // response mapping processing
    let res_header = if let Some(ref resp) = response {
        resp.headers().clone()
    } else {
        header::HeaderMap::new()
    };

    // 未匹配的，添加response header到新header
    res_headers_map.extend(res_header.clone());

    event!(Level::DEBUG, "Response Headers: {:?}", res_headers_map);

    let res_status = if let Some(ref resp) = response {
        resp.status()
    } else {
        StatusCode::OK
    };

    event!(Level::DEBUG, "Response res_status: {:?}", res_status);

    // sse 忽略 response 配置
    if is_sse_req {
        res_headers_map.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );

        event!(Level::DEBUG, "SSE request, ignore response config");

        // 需要获取 response 的所有权
        let stream_response = response.take();
        let stream = async_stream::stream! {
            if let Some(resp) = stream_response {
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                            Ok(c) => c,
                            Err(e) => {
                                let error_msg = format!("Error: {}", e);
                                yield Ok::<Vec<u8>, std::io::Error>(error_msg.into_bytes());
                                continue;
                            }
                        };
                    event!(Level::DEBUG, "SSE chunk: {:?}", chunk);

                    yield Ok(
                        chunk.to_vec()
                    );
                };
            }
        };

        return Ok((
            res_status,
            res_headers_map,
            Body::from_stream(stream),
        )
            .into_response());
    }

    // 没有配置response mix_mappings，直接返回response
    if config.clone().is_some() {
        let config = config.as_ref().unwrap();
        if config.response.mix_mappings.is_empty() {
            event!(
                Level::DEBUG,
                "No need process mix, Return response directly"
            );
            let body_bytes = if cache_config.is_some() {
                cache_config.clone().unwrap().to_string().into_bytes()
            } else if let Some(resp) = response.take() {
                resp.bytes().await.unwrap().to_vec()
            } else {
                Vec::new()
            };
            return Ok((
                res_status,
                res_headers_map,
                axum::body::Bytes::from(body_bytes),
            )
                .into_response());
        }
    }

    let res_body = if cache_config.is_some() {
        cache_config.clone().unwrap().to_string().into_bytes()
    } else if let Some(resp) = response.take() {
        resp.bytes().await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Body read failed: {}", e),
            )
        })?.to_vec()
    } else {
        Vec::new()
    };

    event!(Level::INFO, "Received response {} from {}", res_status, target_url);
    event!(
        Level::DEBUG,
        "Response headers: {:?} | Body size: {} bytes",
        res_header
            .iter()
            .map(|(n, v)| format!("{}={}", n, v.to_str().unwrap()))
            .collect::<Vec<_>>(),
        res_body.len()
    );

    let mut _rb = String::new();
    _rb = String::from_utf8(res_body.clone()).unwrap();
    event!(Level::DEBUG, "Response origin Body: {:?}", &_rb);

    // body
    let mut res_json_map = HashMap::new();
    let mut res_json_data: Option<Value> = None; // 保存原始 JSON 用于缓存

    // 如果是 cache_hit，直接用缓存数据初始化 res_json_map
    if cache_config.is_some() {
        let cached_body = cache_config.clone().unwrap();
        res_json_data = Some(cached_body.clone());
        json_to_flat_map(&cached_body, "", &mut res_json_map);
    } else {
        let res_content_type = res_headers_map.get(header::CONTENT_TYPE).cloned().ok_or((
            StatusCode::BAD_REQUEST,
            format!("Missing Header: content-type"),
        ))?;

        // 根据 response content-type 解析 res_body 数据
        if res_content_type
            .to_str()
            .unwrap()
            .starts_with(mime::APPLICATION_JSON.essence_str()) ||
            res_content_type
                .to_str()
                .unwrap()
                .starts_with(mime::TEXT_PLAIN.essence_str())
        {
            // json
            let json_data: Value = serde_json::from_slice(&res_body)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("JSON parse error: {}", e)))?;
            res_json_data = Some(json_data.clone());
            json_to_flat_map(&json_data, "", &mut res_json_map);
        } else if res_content_type
            .to_str()
            .unwrap()
            .starts_with(mime::APPLICATION_WWW_FORM_URLENCODED.essence_str())
        {
            // form
            let form_data = serde_urlencoded::from_bytes::<HashMap<String, Value>>(&res_body)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("Form parse error: {}", e)))?;
            res_json_map = form_data.clone();
            // form 数据也包装成 JSON 保存
            res_json_data = Some(serde_json::to_value(&form_data).unwrap_or(Value::Null));
        }
    }

    // 处理response.mix_mappings
    if let Some(conf) = &config {
        for mapping in &conf.response.mix_mappings {
            let m = mapping.clone();
            let s = m.source.clone();
            let t = m.target.clone();
            let trans_s = m.transformations.clone();
            
            // 处理 CacheSet 和 CacheHeaderSet（它们不需要 source 和 target）
            match &m.action {
                MixAction::CacheSet => {
                    // 从 res_json_map 中提取缓存 key，缓存整个响应 JSON
                    if let Some(key_field) = &m.cache_key_field {
                        event!(Level::DEBUG, ">>> CacheSet looking for key_field: {}", key_field);
                        event!(Level::DEBUG, ">>> res_json_map keys: {:?}", res_json_map.keys().collect::<Vec<_>>());
                        if let Some(key) = res_json_map.get(key_field) {
                            let key_str = key.as_str().unwrap_or_default().to_string();
                            event!(Level::DEBUG, ">>> CacheSet found key: {}", key_str);
                            let expires = m.cache_expires_in.unwrap_or(3600);
                            if let Some(body) = res_json_data.clone() {
                                event!(Level::DEBUG, ">>> CacheSet caching body: {:?}", body);
                                USER_CACHE.set(key_str, body, expires);
                                event!(Level::INFO, "Response cached successfully");
                            }
                        } else {
                            event!(Level::WARN, ">>> CacheSet key not found in res_json_map");
                        }
                    }
                    continue;
                }
                MixAction::CacheHeaderSet(header_name) => {
                    // 从原始请求 headers 中提取指定 header 并缓存
                    if let Some(key_field) = &m.cache_key_field {
                        event!(Level::DEBUG, ">>> CacheHeaderSet header: {}, key_field: {}", header_name, key_field);
                        // 从 req_headers 获取要缓存的 header 值
                        if let Some(value) = req_headers.get(header_name) {
                            let value_str = value.to_str().unwrap_or_default().to_string();
                            event!(Level::DEBUG, ">>> CacheHeaderSet value: {}", value_str);
                            let expires = m.cache_expires_in.unwrap_or(3600);
                            // 使用 key_field 作为缓存 key，缓存 header 值
                            USER_CACHE.set(key_field.clone(), Value::String(value_str), expires);
                            event!(Level::INFO, "Header {} cached with key {}", header_name, key_field);
                        } else {
                            event!(Level::WARN, ">>> CacheHeaderSet header {} not found in request", header_name);
                        }
                    }
                    continue;
                }
                _ => {}
            }
            
            if s.is_none() || t.is_none() {
                continue;
            }
            let s = s.unwrap();
            let t = t.unwrap();
            match (&s, t) {
                // 从原始 request query 中获取数据
                (MixSource::ReqQuery(src), MixTarget::Header(dst)) => {
                    // 先从 req_query 解析获取
                    let req_query_map = query_to_multimap(&req_query);
                    if let Some(value) = req_query_map.get(src) {
                        let value_str = value.join(",");
                        let mut header_value = HeaderValue::from_str(&value_str).unwrap();
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_header_val(&mut res_headers_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.to_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value_str, dst_val.as_deref(), Some(&query_map)).await
                            {
                                header_value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        res_headers_map.insert(obj.as_str(), header_value);
                    }
                }
                (MixSource::ReqQuery(src), MixTarget::BodyField(dst)) => {
                    let req_query_map = query_to_multimap(&req_query);
                    if let Some(value) = req_query_map.get(src) {
                        let value_str = value.join(",");
                        let mut final_value = value_str.clone();
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_bodymap_val(&mut res_json_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.as_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value_str, dst_val.as_deref(), Some(&query_map)).await
                            {
                                final_value = transformed;
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        res_json_map.insert(obj.to_string(), Value::String(final_value));
                    }
                }
                (MixSource::ReqQuery(src), MixTarget::Query(dst)) => {
                    let req_query_map = query_to_multimap(&req_query);
                    if let Some(value) = req_query_map.get(src) {
                        let value_str = value.join(",");
                        let mut final_value = value.clone();
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_querymap_val(&mut query_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.join(",")));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value_str, dst_val.as_deref(), Some(&query_map)).await
                            {
                                final_value = vec![transformed];
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        query_map.insert(obj.to_string(), final_value);
                    }
                }
                // 从原始 request header 中获取数据
                (MixSource::ReqHeader(src), MixTarget::Header(dst)) => {
                    if let Some(value) = req_headers.get(src) {
                        let value_str = value.to_str().unwrap_or_default().to_string();
                        let mut header_value = value.clone();
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_header_val(&mut res_headers_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.to_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value_str, dst_val.as_deref(), Some(&query_map)).await
                            {
                                header_value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        res_headers_map.insert(obj.as_str(), header_value);
                    }
                }
                (MixSource::ReqHeader(src), MixTarget::BodyField(dst)) => {
                    if let Some(value) = req_headers.get(src) {
                        let value_str = value.to_str().unwrap_or_default().to_string();
                        let mut final_value = value_str.clone();
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_bodymap_val(&mut res_json_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.as_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value_str, dst_val.as_deref(), Some(&query_map)).await
                            {
                                final_value = transformed;
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        res_json_map.insert(obj.to_string(), Value::String(final_value));
                    }
                }
                (MixSource::ReqHeader(src), MixTarget::Query(dst)) => {
                    if let Some(value) = req_headers.get(src) {
                        let value_str = value.to_str().unwrap_or_default().to_string();
                        let mut final_value = vec![value_str.clone()];
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_querymap_val(&mut query_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.join(",")));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value_str, dst_val.as_deref(), Some(&query_map)).await
                            {
                                final_value = vec![transformed];
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        query_map.insert(obj.to_string(), final_value);
                    }
                }
                // 从缓存中获取 header（用于跨请求共享数据）
                (MixSource::CacheHeader, MixTarget::Header(dst)) => {
                    // 从 req_query_map 中获取 key_field 对应的值作为缓存 key
                    if let Some(key_field) = &m.cache_key_field {
                        if let Some(cache_key) = req_query_map.get(key_field).and_then(|v| v.first()) {
                            if let Some(cached) = USER_CACHE.get(cache_key) {
                                if let Some(value_str) = cached.as_str() {
                                    event!(Level::DEBUG, ">>> CacheHeader {} found: {}", cache_key, value_str);
                                    let header_value: HeaderValue = value_str.parse().unwrap();
                                    let obj = Box::leak(Box::new(dst));
                                    res_headers_map.insert(obj.as_str(), header_value);
                                }
                            } else {
                                event!(Level::WARN, ">>> CacheHeader key {} not found in cache", cache_key);
                            }
                        } else {
                            event!(Level::WARN, ">>> CacheHeader key_field {} not found in req_query", key_field);
                        }
                    }
                }
                (MixSource::CacheHeader, MixTarget::BodyField(dst)) => {
                    if let Some(key_field) = &m.cache_key_field {
                        if let Some(cache_key) = req_query_map.get(key_field).and_then(|v| v.first()) {
                            if let Some(cached) = USER_CACHE.get(cache_key) {
                                if let Some(value_str) = cached.as_str() {
                                    event!(Level::DEBUG, ">>> CacheHeader {} found: {}", cache_key, value_str);
                                    let obj = Box::leak(Box::new(dst));
                                    res_json_map.insert(obj.to_string(), Value::String(value_str.to_string()));
                                }
                            }
                        }
                    }
                }
                (MixSource::CacheHeader, MixTarget::Query(dst)) => {
                    if let Some(key_field) = &m.cache_key_field {
                        if let Some(cache_key) = req_query_map.get(key_field).and_then(|v| v.first()) {
                            if let Some(cached) = USER_CACHE.get(cache_key) {
                                if let Some(value_str) = cached.as_str() {
                                    event!(Level::DEBUG, ">>> CacheHeader {} found: {}", cache_key, value_str);
                                    let obj = Box::leak(Box::new(dst));
                                    query_map.insert(obj.to_string(), vec![value_str.to_string()]);
                                }
                            }
                        }
                    }
                }
                // Header to Header
                (MixSource::Header(src), MixTarget::Header(dst)) => {
                    if let Some(mut value) = get_header_val(&mut res_headers_map, &m.action, src) {
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_header_val(&mut res_headers_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.to_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value.to_str().unwrap(),dst_val.as_deref(), Some(&query_map)).await
                            {
                                value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        res_headers_map.insert(obj.as_str(), value.clone());
                    }
                }
                // Header to Body
                (MixSource::Header(src), MixTarget::BodyField(dst)) => {
                    if let Some(mut value) = get_header_val(&mut res_headers_map, &m.action, src){
                        if let Some(trans) = trans_s.clone() {
                            let dst_val: Option<String> = get_bodymap_val(&mut res_json_map, &MixAction::Copy, &dst)
                                .map_or(None, |v| Some(v.as_str().unwrap().to_string()));
                            if let Some(transformed) =
                            apply_transformations(&trans, &value.to_str().unwrap(), dst_val.as_deref(), Some(&query_map)).await
                            {
                                value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        res_json_map.insert(
                            obj.to_string(),
                            Value::String(value.clone().to_str().unwrap().to_string()),
                        );
                    }
                }
                // Body to Body
                (MixSource::BodyField(src), MixTarget::BodyField(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&res_json_map, &src, &mut res_json);
                    match &m.action {
                        MixAction::Move => {
                            for (k, v) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                res_json_map.remove(src_key.as_str()); // delete source
                                res_json_map
                                    .insert(k.clone().replace(src, dst.as_str()), v.clone());
                            }
                        }
                        MixAction::Copy => {
                            for (k, v) in res_json.iter() {
                                res_json_map
                                    .insert(k.clone().replace(src, dst.as_str()), v.clone());
                            }
                        }
                        MixAction::AddTarget(v) => {
                            res_json_map.insert(src.clone(), Value::String(v.clone()));
                        }
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                res_json_map.remove(k.as_str()); // delete source
                            }
                        }
                        // CacheSet/CacheGet 不在此处处理
                        MixAction::CacheSet | MixAction::CacheGet | MixAction::CacheHeaderSet(_) => {}
                    };
                }
                // Body to Header
                (MixSource::BodyField(src), MixTarget::Header(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&res_json_map, &src, &mut res_json);
                    let value = match &m.action {
                        MixAction::Move => {
                            for (k, _) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                res_json_map.remove(src_key.as_str()); // delete source
                                res_json_map.remove(k.as_str());
                            }
                            Some(json_body_to_string(&res_json, "{key}={value}"))
                        }
                        MixAction::Copy => Some(json_body_to_string(&res_json, "{key}={value}")),
                        MixAction::AddTarget(v) => Some(v.clone()), // Add a static value to the query
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                json_map.remove(k.as_str());
                            }
                            None
                        }
                        // CacheSet/CacheGet 不在此处处理
                        MixAction::CacheSet | MixAction::CacheGet | MixAction::CacheHeaderSet(_) => None,
                    };
                    if let Some(value) = value {
                        let obj = Box::leak(Box::new(dst));
                        res_headers_map
                            .insert(obj.as_str(), HeaderValue::from_str(value.as_str()).unwrap());
                    }
                }
                // 无其他配置
                _ => {
                    // CacheGet 在请求阶段通过特殊处理，这里不需要做操作
                    // CacheSet 已经在前面处理了
                    if matches!(&m.action, MixAction::CacheGet) {
                        // CacheGet 在请求阶段处理，response 阶段不需要做任何操作
                    }
                }
            }
        }
    }

    let def_res_json_body = (
        res_header
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.parse().unwrap()),
        res_body.to_vec(),
    );

    let (res_content_type, res_converted_body) = match &config {
        Some(config) => match config.response.body_conversion {
            Some(BodyConversion::FormToJson) => map_to_json_body(&res_json_map)?,
            Some(BodyConversion::JsonToForm) => map_to_form_body(&res_json_map)?,
            None => {
                if !res_json_map.is_empty() {
                    map_to_json_body(&res_json_map)?
                } else {
                    def_res_json_body
                }
            }
        },
        None => {
            if !res_json_map.is_empty() {
                // 没有配置 body 转换，但是有其他地方有移动进来的数据，需要转换为 JSON
                map_to_json_body(&res_json_map)?
            } else {
                // 原始数据。没有做修改
                def_res_json_body
            }
        }
    };

    if res_content_type.is_some() {
        // 处理Body转换的header
        res_headers_map.remove(header::CONTENT_TYPE);
        res_headers_map.insert(
            header::CONTENT_TYPE,
            res_content_type.unwrap().to_string().parse().unwrap(),
        );
    }

    // 代理模式不需要处理host
    if use_mode == UseMode::Normal {
        let from_host = app_config.self_host.clone();
        // 处理 host header
        res_headers_map.remove(header::HOST);
        res_headers_map.insert(header::HOST, from_host.parse().unwrap()); // 设置目标host
    }

    if res_headers_map.contains_key(header::TRANSFER_ENCODING) {
        let transfer_encoding = res_headers_map.get(header::TRANSFER_ENCODING);
        if transfer_encoding.is_some()
            && transfer_encoding
                .unwrap()
                .to_str()
                .unwrap()
                .contains("chunked")
        {
            res_headers_map.remove(header::CONTENT_LENGTH);

            event!(Level::DEBUG, "Response status: {:?}", res_status);
            event!(Level::DEBUG, "Response headers: {:?}", res_headers_map);
            let mut _rb = String::new();
            _rb = String::from_utf8(res_converted_body.clone()).unwrap();
            event!(Level::DEBUG, "Response Body: {:?}", &_rb);

            return Ok((
                res_status,
                res_headers_map,
                axum::body::Bytes::from(res_converted_body),
            )
                .into_response());
        }
    }
    let status = res_status;
    let body = res_converted_body.clone();
    // 处理Body转换后的header
    res_headers_map.remove(header::CONTENT_LENGTH);
    res_headers_map.insert(
        header::CONTENT_LENGTH,
        body.len().to_string().parse().unwrap(),
    );

    let headers = res_headers_map;

    event!(Level::INFO, "Response {} to {}", status, uri);
    event!(Level::DEBUG, "Response status: {:?}", status);
    event!(Level::DEBUG, "Response headers: {:?}", headers);
    let mut _rb = String::new();
    _rb = String::from_utf8(res_converted_body.clone()).unwrap();
    event!(Level::DEBUG, "Response Body: {:?}", &_rb);
    event!(Level::DEBUG, "Response body size: {} bytes", body.len());

    Ok((status, headers, axum::body::Bytes::from(body)).into_response())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let app = Router::new().fallback(any(proxy_handler));
    event!(Level::INFO, "Starting sso_adapter server on port 8080");
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener,app.into_make_service())
        .await
        .unwrap();
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_array() {
        let mut res_json_map = HashMap::<String,Value>::new();
        // let john = json!({"entryUUID":"8a6287d4-7c09-ff8f-017c-0bc12adf4c90","sub":"IYANG10","role":["id=VALIDATE,id=FRANCHISE,id=appRole,ou=role,OU=Repository,o=decathlon","id=Global,id=Shoppertrak,id=appRole,ou=role,OU=Repository,o=decathlon","id=UserIT,id=Confluence,id=appRole,ou=role,OU=Repository,o=decathlon","id=COM2U_ContRev,id=COM2U,id=appRole,ou=role,OU=Repository,o=decathlon","id=StrongAuth,id=PingId,id=appRole,ou=role,ou=repository,o=decathlon","id=ACCESS,id=IPAC,id=appRole,ou=role,ou=repository,o=decathlon","id=DECASTORE_national_user,id=DECASTORE,id=appRole,ou=role,OU=Repository,o=decathlon","id=biItTools,id=OBIEE,id=appRole,ou=role,OU=Repository,o=decathlon","id=LECTURE,id=CATALOGUEAGENCEMENT,id=appRole,ou=role,ou=repository,o=decathlon","id=Admin,id=TATTOO,id=appRole,ou=role,OU=Repository,o=decathlon","id=WebApp_chiffres_secu3,id=Webapp_chiffres,id=appRole,ou=role,OU=Repository,o=decathlon","id=WebApp_chiffres_secu1,id=Webapp_chiffres,id=appRole,ou=role,OU=Repository,o=decathlon","id=CRCStoreUser,id=CRC,id=appRole,ou=role,OU=Repository,o=decathlon","id=WebApp_chiffres_secu2,id=Webapp_chiffres,id=appRole,ou=role,OU=Repository,o=decathlon","id=SportLeader,id=MyGame,id=appRole,ou=role,OU=Repository,o=decathlon","id=READER,id=POSDATA,id=appRole,ou=role,OU=Repository,o=decathlon","id=Standard,id=eTCO,id=appRole,ou=role,OU=Repository,o=decathlon","id=access,id=pds,id=appRole,ou=role,ou=repository,o=decathlon","id=User,id=Bird-Office,id=appRole,ou=role,OU=Repository,o=decathlon","id=Read,id=OptiPCB,id=appRole,ou=role,OU=Repository,o=decathlon","id=ACCESS,id=CZ_DECASPACE,id=appRole,ou=role,ou=repository,o=decathlon","id=HON,id=CZ_DECASPACE,id=appRole,ou=role,ou=repository,o=decathlon","id=SFA,id=CZ_DECASPACE,id=appRole,ou=role,ou=repository,o=decathlon","id=ADMIN,id=CZ_DECASPACE,id=appRole,ou=role,ou=repository,o=decathlon","id=ZANTHUS_PROFILE,id=ZANTHUS,id=appRole,ou=role,OU=Repository,o=decathlon","id=IT_ACCESS_DTC,id=DTC_V2,id=appRole,ou=role,OU=Repository,o=decathlon","id=access,id=sptTool,id=appRole,ou=role,ou=repository,o=decathlon","id=ROLE_SUPPORT,id=DKTRENT,id=appRole,ou=role,OU=Repository,o=decathlon","id=QC_VIEWER_ACCESS,id=QUERY_CATALOG_PORTAL,id=appRole,ou=role,OU=Repository,o=decathlon","id=SERVICES,id=WSO,id=appRole,ou=role,OU=Repository,o=decathlon","id=RCOA,id=PSV,id=appRole,ou=role,OU=Repository,o=decathlon","id=PRODUCT_DATA_INTERNATIONAL_WRITER,id=SPID,id=appRole,ou=role,OU=Repository,o=decathlon","id=PRODUCT_DATA_LOCALIZED_WRITER,id=SPID,id=appRole,ou=role,OU=Repository,o=decathlon","id=GITHUB,id=ACCESS,id=appRole,ou=role,OU=Repository,o=decathlon","id=DEFAULT,id=SPORTYCOINS,id=appRole,ou=role,OU=Repository,o=decathlon","id=VIEWER,id=CAT,id=appRole,ou=role,OU=Repository,o=decathlon"],"c":"CN","mail":"irene.yang@decathlon.com","displayName":"YANG Irene","givenName":"Irene","sex":"2","mobile":"+8617312678351","cn":"YANG Irene","sitetype":"HQ","title":"Data Engineer","objectclass":["top","person","organizationalPerson","inetOrgPerson","ocExtendedperson"],"uuid":"8a6287d4-7c09-ff8f-017c-0bc12adf4c90","allsites":"CNHQCHLB","uid":"IYANG10","site":"CNHQCHLB","federation_idp":"d1","hrid":"6101715","familyName":"YANG","sitename":"CHINA LAB","sn":"YANG","costcenter":"005010585017C105","jobname":"DATA.ENG"});
        let john = json!({"role":["aad", {"name":"aa", "sex":"n", "dd": ["ad"]}],"t1":{"ar": 123}});
        json_to_flat_map(&john, "",&mut res_json_map);
        for ele in res_json_map.clone() {
            print!("{}\n",ele.0)
        }
        let v = flat_map_to_json(&res_json_map);
        println!("{:?}", v.clone());
        println!("\n{}", v.to_string());
        assert_eq!(1,1);
    }
}