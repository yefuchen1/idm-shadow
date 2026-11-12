//! m3u8 播放列表解析。
//!
//! 只处理媒体列表（VOD）和直播列表两种。加密只支持 AES-128，
//! SAMPLE-AES 属于 DRM 范畴，不在支持范围内。

use std::collections::BTreeMap;

use crate::http::RequestContext;

/// 一个分片。
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// 分片的绝对地址。
    pub url: String,
    /// 在播放列表里的序号，从 0 开始。
    pub sequence: u64,
    /// 该分片用的密钥。
    pub key: Option<EncryptionKey>,
    /// 直播列表里该分片的持续时间（秒）。
    pub duration: f64,
}

/// AES-128 密钥。
#[derive(Debug, Clone, PartialEq)]
pub struct EncryptionKey {
    /// key 文件的绝对地址。
    pub uri: String,
    /// 初始向量。缺省时用分片序号当 IV。
    pub iv: Option<Vec<u8>>,
}

/// 播放列表。
#[derive(Debug, Clone, Default)]
pub struct Playlist {
    pub segments: Vec<Segment>,
    /// 直播列表的媒体序号起点。
    pub media_sequence: u64,
    /// 是不是直播（会滚动更新的列表）。
    pub is_live: bool,
    /// 列表总时长（秒）。
    pub total_duration: f64,
    /// 目标时长。
    pub target_duration: f64,
    /// 播放列表里声明的加密方式（不解密的那些）。
    pub unsupported_methods: Vec<String>,
}

impl Playlist {
    /// 分片总数。
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

/// 解析 m3u8 文本。`base_url` 是播放列表自身的地址，用来解析相对路径。
pub fn parse(text: &str, base_url: &str) -> Result<Playlist, String> {
    let mut playlist = Playlist::default();
    let mut current_key: Option<EncryptionKey> = None;
    let mut pending_duration = 0.0f64;
    let mut saw_endlist = false;
    let mut saw_media_sequence = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let dur = rest
                .split(',')
                .next()
                .unwrap_or("0")
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0);
            pending_duration = dur;
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            playlist.media_sequence = rest.trim().parse().unwrap_or(0);
            saw_media_sequence = true;
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            playlist.target_duration = rest.trim().parse().unwrap_or(0.0);
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            let kind = rest.trim().to_ascii_lowercase();
            if kind != "vod" {
                playlist.is_live = true;
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let method = attr(rest, "METHOD").unwrap_or_default();
            if method.eq_ignore_ascii_case("NONE") {
                current_key = None;
            } else if method.eq_ignore_ascii_case("AES-128") {
                current_key = attr(rest, "URI").and_then(|uri| {
                    // 密钥地址也可能是相对的，要对着播放列表的地址解析
                    let resolved = resolve_url(&uri, base_url).unwrap_or(uri);
                    Some(EncryptionKey {
                        uri: resolved,
                        iv: attr(rest, "IV").and_then(|v| {
                            hex_decode(v.trim_start_matches("0x").trim_start_matches("0X"))
                        }),
                    })
                });
            } else {
                // 不支持的加密方式
                current_key = None;
                if !playlist.unsupported_methods.contains(&method) {
                    playlist.unsupported_methods.push(method);
                }
            }
            continue;
        }

        if line == "#EXT-X-ENDLIST" {
            saw_endlist = true;
            continue;
        }

        // 其它 #EXT 标签暂时忽略
        if line.starts_with('#') {
            continue;
        }

        // 非注释行就是分片地址
        let url = resolve_url(line, base_url)
            .ok_or_else(|| format!("无法解析分片地址: {line}"))?;
        let sequence = playlist.media_sequence + playlist.segments.len() as u64;
        playlist.segments.push(Segment {
            url,
            sequence,
            key: current_key.clone(),
            duration: pending_duration,
        });
        playlist.total_duration += pending_duration;
        pending_duration = 0.0;
    }

    if playlist.segments.is_empty() {
        return Err("播放列表里没有分片".to_string());
    }

    // 有 MEDIA-SEQUENCE 且没有 ENDLIST 的，视为直播列表
    if saw_media_sequence && !saw_endlist {
        playlist.is_live = true;
    }

    Ok(playlist)
}

/// 从 `KEY=VALUE,KEY2=VALUE2` 里取值。值可能带引号。
fn attr(input: &str, name: &str) -> Option<String> {
    for pair in split_attrs(input) {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case(name) {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// 按逗号切属性，但要避开引号里的逗号。
fn split_attrs(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(ch);
            }
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// 十六进制解码。
pub fn hex_decode(input: &str) -> Option<Vec<u8>> {
    // 去掉 0x 前缀和空白
    let cleaned: String = input
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.len() % 2 != 0 {
        return None;
    }
    let bytes = cleaned.as_bytes();
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for pair in bytes.chunks(2) {
        let s = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(out)
}

/// 把相对地址解析成绝对地址。
pub fn resolve_url(reference: &str, base_url: &str) -> Option<String> {
    if reference.starts_with("http://") || reference.starts_with("https://") {
        return Some(reference.to_string());
    }
    if reference.starts_with("//") {
        let scheme = base_url.split_once("://").map(|(s, _)| s).unwrap_or("https");
        return Some(format!("{scheme}:{reference}"));
    }

    // 根相对路径：直接挂到主机上
    if reference.starts_with('/') {
        let scheme = base_url.split_once("://").map(|(s, _)| s).unwrap_or("https");
        let host = base_url
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
            .unwrap_or("");
        return Some(format!("{scheme}://{host}{reference}"));
    }

    let base = base_url.split_once("://")?;
    let (scheme, rest) = base;
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, p),
        None => (rest, ""),
    };

    // base 路径的最后一段是文件名，去掉它得到目录（去掉首尾斜杠）
    let dir = match path.rfind('/') {
        Some(i) => path[..i].trim_matches('/'),
        None => "",
    };
    let mut combined = if dir.is_empty() {
        format!("/{reference}")
    } else {
        format!("/{dir}/{reference}")
    };
    while let Some(idx) = combined.find("/./") {
        combined.replace_range(idx..idx + 2, "");
    }
    while let Some(idx) = combined.find("/../") {
        let head = &combined[..idx];
        // tail 已经带着开头的斜杠了
        let tail = combined[idx + 3..].trim_start_matches('/');
        match head.rfind('/') {
            // 去掉上一级目录
            Some(i) => {
                let parent = head[..i].trim_end_matches('/');
                combined = if parent.is_empty() {
                    format!("/{tail}")
                } else {
                    format!("{parent}/{tail}")
                };
            }
            None => combined = format!("/{tail}"),
        }
    }
    Some(format!("{scheme}://{host}{combined}"))
}

/// 从播放列表的响应里取出要带的请求头（有些站点要 Referer）。
pub fn request_context(base_url: &str, referer: Option<&str>) -> RequestContext {
    let mut headers = BTreeMap::new();
    if let Some(r) = referer {
        headers.insert("Referer".to_string(), r.to_string());
    }
    RequestContext {
        url: base_url.to_string(),
        referer: referer.map(|s| s.to_string()),
        headers,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vod_playlist() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:9.009,\nseg0.ts\n#EXTINF:9.009,\nseg1.ts\n#EXT-X-ENDLIST\n";
        let pl = parse(text, "http://a.com/v/index.m3u8").unwrap();
        assert_eq!(pl.len(), 2);
        assert!(!pl.is_live);
        assert_eq!(pl.segments[0].url, "http://a.com/v/seg0.ts");
        assert_eq!(pl.segments[1].sequence, 1);
        assert!((pl.total_duration - 18.018).abs() < 0.001);
    }

    #[test]
    fn parses_live_playlist() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:120\n#EXTINF:4.0,\na.ts\n#EXTINF:4.0,\nb.ts\n";
        let pl = parse(text, "http://a.com/live.m3u8").unwrap();
        assert!(pl.is_live);
        assert_eq!(pl.media_sequence, 120);
        assert_eq!(pl.segments[0].sequence, 120);
        assert_eq!(pl.segments[1].sequence, 121);
    }

    #[test]
    fn parses_aes128_key() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090a0b0c0d0e0f\n#EXTINF:4.0,\nseg.ts\n";
        let pl = parse(text, "http://a.com/v/i.m3u8").unwrap();
        let key = pl.segments[0].key.as_ref().unwrap();
        assert_eq!(key.uri, "http://a.com/v/key.bin");
        assert_eq!(key.iv.as_ref().unwrap().len(), 16);
        assert_eq!(key.iv.as_ref().unwrap()[0], 0);
    }

    #[test]
    fn records_unsupported_encryption() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:4.0,\nseg.ts\n";
        let pl = parse(text, "http://a.com/i.m3u8").unwrap();
        assert_eq!(pl.unsupported_methods, vec!["SAMPLE-AES"]);
        assert!(pl.segments[0].key.is_none());
    }

    #[test]
    fn key_none_resets_encryption() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k1\"\n#EXTINF:4.0,\na.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4.0,\nb.ts\n";
        let pl = parse(text, "http://a.com/i.m3u8").unwrap();
        assert!(pl.segments[0].key.is_some());
        assert!(pl.segments[1].key.is_none());
    }

    #[test]
    fn resolves_relative_urls() {
        assert_eq!(resolve_url("seg.ts", "http://a.com/v/i.m3u8").unwrap(), "http://a.com/v/seg.ts");
        assert_eq!(resolve_url("../seg.ts", "http://a.com/v/i.m3u8").unwrap(), "http://a.com/seg.ts");
        assert_eq!(resolve_url("/seg.ts", "http://a.com/v/i.m3u8").unwrap(), "http://a.com/seg.ts");
        assert_eq!(resolve_url("http://b.com/x.ts", "http://a.com/i.m3u8").unwrap(), "http://b.com/x.ts");
        assert_eq!(resolve_url("//b.com/x.ts", "http://a.com/i.m3u8").unwrap(), "http://b.com/x.ts");
    }

    #[test]
    fn hex_decoding() {
        assert_eq!(hex_decode("00010203").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(hex_decode("0x0a0b").unwrap(), vec![10, 11]);
        assert!(hex_decode("abc").is_none());
    }

    #[test]
    fn rejects_empty_playlist() {
        assert!(parse("#EXTM3U\n", "http://a.com/i.m3u8").is_err());
    }
}
