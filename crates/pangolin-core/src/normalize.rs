//! Host normalization at the request entry point.

/// Strip the port (if any) and lowercase the host.
///
/// Examples:
///   `Foo.Example.COM:8443` → `foo.example.com`
///   `foo.example.com`      → `foo.example.com`
///   `foo.example.com:443`  → `foo.example.com`
pub fn normalize_host(host: &str) -> String {
    // splitn with limit 2 handles "host:port" without allocating.
    let without_port = host.split(':').next().unwrap_or(host);
    without_port.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase() {
        assert_eq!(normalize_host("Foo.Example.COM"), "foo.example.com");
        assert_eq!(normalize_host("APP.EXAMPLE.COM"), "app.example.com");
    }

    #[test]
    fn strip_port() {
        assert_eq!(normalize_host("foo.example.com:8443"), "foo.example.com");
        assert_eq!(normalize_host("foo.example.com:443"), "foo.example.com");
    }

    #[test]
    fn both_lowercase_and_strip_port() {
        assert_eq!(normalize_host("Foo.Example.COM:8443"), "foo.example.com");
    }

    #[test]
    fn no_port_no_change() {
        assert_eq!(normalize_host("foo.example.com"), "foo.example.com");
    }

    #[test]
    fn empty_input() {
        assert_eq!(normalize_host(""), "");
    }
}
