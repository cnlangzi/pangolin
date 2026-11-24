//! The upstream header rewrite does not scale with the number of
//! headers (issue #253).
//!
//! The rewrite used to materialise every request header into an owned
//! `Vec<(String, String)>`, run the policy over it and write the
//! result back — an O(n²) scan and O(n) allocations per request. It
//! now rewrites pingora's own header map in place, so the work it
//! does does not depend on how many headers the client sent.
//!
//! The test lives in its own integration-test binary because it
//! installs a counting global allocator: the allocator is process
//! wide, and a binary of its own keeps it away from every other
//! test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pangolin_core::types::HostMode;
use pangolin_core::{ProxyCtx, Scheme};
use pingora::http::RequestHeader;

use ngx::proxy::rewrite_upstream_headers;

/// Counts the allocations of the measured section.
///
/// The counter is **thread local** and off by default, so the rest of
/// the binary — and every other thread — is unaffected.
struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count_one() {
    let _ = COUNTING.try_with(|counting| {
        if counting.get() {
            ALLOCATIONS.with(|allocs| allocs.set(allocs.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_one();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_one();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_one();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The allocations of `f`, and nothing else.
fn allocations_of(f: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|allocs| allocs.set(0));
    COUNTING.with(|counting| counting.set(true));
    f();
    COUNTING.with(|counting| counting.set(false));
    ALLOCATIONS.with(|allocs| allocs.get())
}

/// A request header with `count` filler headers on top of the ones
/// every request carries.
fn header_with(count: usize) -> RequestHeader {
    let mut header = RequestHeader::build("GET", b"/", None).unwrap();
    header.insert_header("Host", "public.example.com").unwrap();
    header.insert_header("Cookie", "session=abc").unwrap();
    for i in 0..count {
        header
            .insert_header(format!("X-Filler-{i}"), "value")
            .unwrap();
    }
    header
}

/// The proxy context the rewrite reads.
fn policy_ctx() -> ProxyCtx {
    ProxyCtx {
        original_host: "public.example.com".to_string(),
        original_scheme: Scheme::Http,
        host_mode: HostMode::Backend,
        host_custom: None,
        peer_ip: Some("203.0.113.9".to_string()),
    }
}

/// The rewrite does not allocate per header.
///
/// A request with eight headers and a request with five hundred are
/// rewritten with the same number of allocations: the rewrite touches
/// the `Host` header and the `X-Forwarded-*` family, and nothing
/// else. The rewrite used to copy every header into an owned vector
/// first, so the allocations grew with the header count (issue #253).
#[test]
fn the_rewrite_does_not_allocate_per_header() {
    let ctx = policy_ctx();

    let mut few = header_with(8);
    let few_allocations = allocations_of(|| {
        rewrite_upstream_headers(&mut few, &ctx, Some("backend.internal:8080"));
    });

    let mut many = header_with(500);
    let many_allocations = allocations_of(|| {
        rewrite_upstream_headers(&mut many, &ctx, Some("backend.internal:8080"));
    });

    assert!(
        many_allocations <= few_allocations + 8,
        "500 headers took {many_allocations} allocations, 8 headers took \
         {few_allocations} — the rewrite still scales with the header count"
    );
}

/// The rewrite still does its job on a request with many headers.
#[test]
fn the_rewrite_applies_the_policy_on_a_request_with_many_headers() {
    let mut header = header_with(500);
    rewrite_upstream_headers(&mut header, &policy_ctx(), Some("backend.internal:8080"));

    assert_eq!(
        header.headers.get("host").unwrap(),
        "backend.internal:8080",
        "the Host header is the backend's authority"
    );
    assert_eq!(
        header.headers.get("x-forwarded-for").unwrap(),
        "203.0.113.9",
        "the hop path is the peer address"
    );
    assert_eq!(
        header.headers.get("cookie").unwrap(),
        "session=abc",
        "the client's own header is untouched"
    );
    assert_eq!(
        header.headers.get_all("x-filler-499").iter().count(),
        1,
        "every filler header survives"
    );
}
