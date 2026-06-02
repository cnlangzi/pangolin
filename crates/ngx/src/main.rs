// Pangolin gateway (ngx) — entry point placeholder.
// Real implementation per README.md design:
//   - 5 tables: sites / domains / tun / tokens / certs (all TEXT primary keys)
//   - 3 in-memory indexes: domainIndex / tunIndex / tokenIndex
//   - pingora ProxyHttp trait
//   - 3 backend types: direct (http/https) / tunnel (WS) / file:///

fn main() {
    eprintln!("ngx: not yet implemented — see README.md design");
    std::process::exit(1);
}
