//! HTTP 引擎。

pub mod engine;
pub mod segmenter;

pub use engine::{DownloadConfig, HttpEngine, TaskProgress};
pub use segmenter::{Segmenter, WorkerId, Work};

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    LAST_MODIFIED, RANGE, REFERER, USER_AGENT,
};
use reqwest::{Client, RequestBuilder, StatusCode};

use crate::task::{filename_from_content_disposition, Task};

/// 探测结果。
#[derive(Debug, Clone, Default)]
pub struct ResourceMeta {
    /// 总大小。服务器不给就是 None。
    pub total_size: Option<u64>,
    /// 是否支持 Range 请求（决定能不能分段、能不能续传）。
    pub supports_range: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub suggested_filename: Option<String>,
    pub content_type: Option<String>,
    /// 响应头里原始的 Content-Disposition。
    ///
    /// 留着原始值（而不是解析后的文件名）是为了让上层能自己决定
    /// 怎么用它——比如"只有当解析不出名字时才用它"。
    pub content_disposition: Option<String>,
    /// 跟随重定向后的最终地址。
    pub final_url: String,
}

impl ResourceMeta {
    /// 判断是不是 HLS 播放列表。
    pub fn looks_like_hls(&self) -> bool {
        let url = self.final_url.to_ascii_lowercase();
        if url.contains(".m3u8") {
            return true;
        }
        match self.content_type.as_deref() {
            Some(ct) => {
                let ct = ct.to_ascii_lowercase();
                ct.contains("mpegurl") || ct.contains("m3u8")
            }
            None => false,
        }
    }
}

/// 发请求时需要的上下文：URL 与各种头部。
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub url: String,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub cookies: Option<String>,
    pub headers: BTreeMap<String, String>,
}

impl RequestContext {
    pub fn from_task(task: &Task) -> Self {
        Self {
            url: task.url.clone(),
            referer: task.referer.clone(),
            user_agent: task.user_agent.clone(),
            cookies: task.cookies.clone(),
            headers: task.headers.clone(),
        }
    }

    /// 把上下文里的头部挂到请求上。
    pub fn apply(&self, rb: RequestBuilder) -> RequestBuilder {
        let mut rb = rb;
        if let Some(ua) = &self.user_agent {
            rb = rb.header(USER_AGENT, ua);
        }
        if let Some(referer) = &self.referer {
            rb = rb.header(REFERER, referer);
        }
        if let Some(cookies) = &self.cookies {
            rb = rb.header(reqwest::header::COOKIE, cookies);
        }
        for (k, v) in &self.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        rb
    }
}

/// 创建带连接池的 HTTP 客户端。
pub fn build_client(user_agent: Option<&str>) -> Result<Client> {
    Client::builder()
        .user_agent(user_agent.unwrap_or("Mozilla/5.0 (Windows NT 10.0; Win64; x64)"))
        .connect_timeout(std::time::Duration::from_secs(20))
        .pool_max_idle_per_host(32)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .context("创建 HTTP 客户端失败")
}
/// 探测资源：大小、是否支持 Range、ETag、文件名。
pub async fn probe(client: &Client, ctx: &RequestContext) -> Result<ResourceMeta> {
    probe_with_timeout(client, ctx, Duration::from_secs(20)).await
}

/// 探测资源，并给整个请求加个时间上限。
///
/// 建任务的时候要靠探测结果定文件名，那一步是**同步挡在用户界面前面**的：
/// 探测慢一点，扩展那边就一直等着，用户的点击像是没反应。
/// 所以定名那条路用短超时（几秒），超时就退回按 URL 猜名字。
pub async fn probe_with_timeout(
    client: &Client,
    ctx: &RequestContext,
    timeout: Duration,
) -> Result<ResourceMeta> {
    let rb = ctx.apply(client.get(&ctx.url)).header(RANGE, "bytes=0-0");
    let resp = tokio::time::timeout(timeout, rb.send())
        .await
        .map_err(|_| anyhow!("探测超时: {}", ctx.url))?
        .with_context(|| format!("探测失败: {}", ctx.url))?;

    let status = resp.status();
    let headers = resp.headers().clone();
    let final_url = resp.url().to_string();

    let partial = status == StatusCode::PARTIAL_CONTENT;
    let total_size = if partial {
        parse_content_range_total(&headers)
    } else {
        headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
    };

    let supports_range = partial
        || headers
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().contains("bytes"))
            .unwrap_or(false);

    let suggested_filename = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(filename_from_content_disposition)
        .or_else(|| crate::task::filename_from_url(&final_url));

    let content_disposition = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());

    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());

    let etag = headers
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());
    let last_modified = headers
        .get(LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());

    Ok(ResourceMeta {
        total_size,
        supports_range,
        etag,
        last_modified,
        suggested_filename,
        content_type,
        content_disposition,
        final_url,
    })
}

/// 从 `Content-Range: bytes 0-0/12345` 里解析总大小。
pub fn parse_content_range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let total = value.rsplit('/').next()?.trim();
    if total == "*" {
        return None;
    }
    total.parse::<u64>().ok()
}

/// 这些状态码重试也没用，直接判失败。
pub fn is_permanent_client_error(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::GONE | StatusCode::BAD_REQUEST
    )
}

/// 把 HTTP 状态码映射成任务级错误。
pub fn status_to_error(status: StatusCode) -> anyhow::Error {
    let code = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::SERVICE_UNAVAILABLE => "unavailable",
        _ => "http_error",
    };
    anyhow!("{} ({})", code, status.as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn parses_content_range() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-0/12345"));
        assert_eq!(parse_content_range_total(&headers), Some(12345));

        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-0/*"));
        assert_eq!(parse_content_range_total(&headers), None);
    }

    #[test]
    fn permanent_errors_listed() {
        assert!(is_permanent_client_error(StatusCode::NOT_FOUND));
        assert!(!is_permanent_client_error(
            StatusCode::INTERNAL_SERVER_ERROR
        ));
    }
}
