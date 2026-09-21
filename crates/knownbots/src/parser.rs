//! Official IP-list parsers (google / openai / txt / uptimerobot / ahrefs / amazon).

use std::io::Read;

use anyhow::{Context, Result};
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use serde::Deserialize;

/// Parser names [`parse`] handles without falling through to txt.
pub fn is_known(parser: &str) -> bool {
    matches!(
        parser,
        "google" | "openai" | "uptimerobot" | "ahrefs" | "amazon" | "txt"
    )
}

/// Parse a response body into CIDR prefixes using the named parser.
pub fn parse(parser: &str, body: &[u8]) -> Result<Vec<IpNet>> {
    match parser {
        "google" => parse_google(body),
        "openai" => parse_openai(body),
        "uptimerobot" => parse_uptimerobot(body),
        "ahrefs" => parse_ahrefs(body),
        "amazon" => parse_amazon(body),
        // Default / unknown → line-oriented CIDR or bare IP.
        _ => parse_txt(body),
    }
}

fn parse_google(body: &[u8]) -> Result<Vec<IpNet>> {
    #[derive(Deserialize)]
    struct Resp {
        prefixes: Vec<Pfx>,
    }
    #[derive(Deserialize)]
    struct Pfx {
        #[serde(default, rename = "ipv4Prefix")]
        ipv4: String,
        #[serde(default, rename = "ipv6Prefix")]
        ipv6: String,
    }
    let resp: Resp = serde_json::from_slice(body).context("google json")?;
    let mut out = Vec::new();
    for p in resp.prefixes {
        push_cidr(&mut out, &p.ipv4);
        push_cidr(&mut out, &p.ipv6);
    }
    Ok(out)
}

fn parse_openai(body: &[u8]) -> Result<Vec<IpNet>> {
    #[derive(Deserialize)]
    struct Resp {
        prefixes: Vec<Pfx>,
    }
    #[derive(Deserialize)]
    struct Pfx {
        #[serde(default)]
        prefix: String,
    }
    let resp: Resp = serde_json::from_slice(body).context("openai json")?;
    let mut out = Vec::new();
    for p in resp.prefixes {
        push_cidr(&mut out, &p.prefix);
    }
    Ok(out)
}

fn parse_uptimerobot(body: &[u8]) -> Result<Vec<IpNet>> {
    #[derive(Deserialize)]
    struct Resp {
        prefixes: Vec<Pfx>,
    }
    #[derive(Deserialize)]
    struct Pfx {
        #[serde(default, rename = "ip_prefix")]
        ip: String,
        #[serde(default, rename = "ipv6_prefix")]
        ipv6: String,
    }
    let resp: Resp = serde_json::from_slice(body).context("uptimerobot json")?;
    let mut out = Vec::new();
    for p in resp.prefixes {
        push_cidr(&mut out, &p.ip);
        push_cidr(&mut out, &p.ipv6);
    }
    Ok(out)
}

fn parse_ahrefs(body: &[u8]) -> Result<Vec<IpNet>> {
    #[derive(Deserialize)]
    struct Resp {
        ips: Vec<Ip>,
    }
    #[derive(Deserialize)]
    struct Ip {
        #[serde(default, rename = "ip_address")]
        ip_address: String,
    }
    let resp: Resp = serde_json::from_slice(body).context("ahrefs json")?;
    let mut out = Vec::new();
    for p in resp.ips {
        push_addr_or_cidr(&mut out, &p.ip_address);
    }
    Ok(out)
}

fn parse_amazon(body: &[u8]) -> Result<Vec<IpNet>> {
    // Amazon publishes an HTML page with an embedded JSON blob.
    // Prefer the JSON; fall back to scraping bare IPv4 literals.
    let text = String::from_utf8_lossy(body);
    if let Some(json_slice) = extract_amazon_json(&text) {
        #[derive(Deserialize)]
        struct Resp {
            prefixes: Vec<Pfx>,
        }
        #[derive(Deserialize)]
        struct Pfx {
            #[serde(default, rename = "ipv4Prefix")]
            ipv4: String,
        }
        if let Ok(resp) = serde_json::from_str::<Resp>(json_slice) {
            let mut out = Vec::new();
            for p in resp.prefixes {
                push_addr_or_cidr(&mut out, &p.ipv4);
            }
            if !out.is_empty() {
                return Ok(out);
            }
        }
    }
    // Fallback: any IPv4 literal → /32.
    let mut out = Vec::new();
    let mut start = None;
    for (i, b) in text.bytes().enumerate() {
        let is_ip_char = b.is_ascii_digit() || b == b'.';
        if is_ip_char {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            push_addr_or_cidr(&mut out, &text[s..i]);
        }
    }
    if let Some(s) = start {
        push_addr_or_cidr(&mut out, &text[s..]);
    }
    // Dedup while preserving order.
    out.sort();
    out.dedup();
    Ok(out)
}

fn extract_amazon_json(text: &str) -> Option<&str> {
    // Look for `{"creationTime": "...", "prefixes": [...]}`.
    let start = text.find("\"prefixes\"")?;
    let obj_start = text[..start].rfind('{')?;
    // Naive brace match from obj_start.
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    for (i, &b) in bytes[obj_start..].iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[obj_start..obj_start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_txt(body: &[u8]) -> Result<Vec<IpNet>> {
    let mut out = Vec::new();
    let mut reader = body;
    let mut buf = String::new();
    // line-oriented without requiring a Seek.
    let mut tmp = Vec::new();
    reader.read_to_end(&mut tmp)?;
    buf.push_str(&String::from_utf8_lossy(&tmp));
    for line in buf.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        push_addr_or_cidr(&mut out, line);
    }
    Ok(out)
}

fn push_cidr(out: &mut Vec<IpNet>, s: &str) {
    let s = s.trim();
    if s.is_empty() {
        return;
    }
    if let Ok(net) = s.parse::<IpNet>() {
        out.push(net);
    }
}

fn push_addr_or_cidr(out: &mut Vec<IpNet>, s: &str) {
    let s = s.trim();
    if s.is_empty() {
        return;
    }
    if let Ok(net) = s.parse::<IpNet>() {
        out.push(net);
        return;
    }
    if let Ok(v4) = s.parse::<std::net::Ipv4Addr>() {
        out.push(IpNet::V4(Ipv4Net::new(v4, 32).expect("ipv4 /32")));
        return;
    }
    if let Ok(v6) = s.parse::<std::net::Ipv6Addr>() {
        out.push(IpNet::V6(Ipv6Net::new(v6, 128).expect("ipv6 /128")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn google_parser_v4_v6() {
        let body =
            br#"{"prefixes":[{"ipv4Prefix":"66.249.64.0/19"},{"ipv6Prefix":"2001:4860::/32"}]}"#;
        let nets = parse_google(body).unwrap();
        assert_eq!(nets.len(), 2);
        assert!(nets[0].contains(&"66.249.66.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn openai_parser() {
        let body = br#"{"prefixes":[{"prefix":"132.196.86.0/24"}]}"#;
        let nets = parse_openai(body).unwrap();
        assert_eq!(nets.len(), 1);
    }

    #[test]
    fn txt_parser_bare_ip() {
        let body = b"1.2.3.4\n# comment\n5.6.7.0/24\n";
        let nets = parse_txt(body).unwrap();
        assert_eq!(nets.len(), 2);
        assert!(nets[0].contains(&"1.2.3.4".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn uptimerobot_parser() {
        let body = br#"{"prefixes":[{"ip_prefix":"3.12.251.153/32"},{"ipv6_prefix":"2600::/32"}]}"#;
        let nets = parse_uptimerobot(body).unwrap();
        assert_eq!(nets.len(), 2);
    }
}
