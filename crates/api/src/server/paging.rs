//! The paging of the list endpoints.
//!
//! A list endpoint answers with a **JSON array** — that shape does not
//! change when the caller asks for a page of it. The paging is two
//! **optional** query parameters, `limit` and `offset`, and it is
//! applied to the **view** — the `Vec` that is about to be serialized
//! — so the entries of a page are exactly the entries the answer
//! carries.
//!
//! The parameters are part of the request target, and the canonical
//! string signs the target **as it was sent**, query string included
//! (see [`crate::protocol::canonical`]), so a paged read travels down
//! the outbound channel without a change of the protocol.

use super::Ctx;
use crate::protocol::error::ApiError;

/// The paging parameters of one request.
///
/// Both parameters are optional: without them the page is the whole
/// list. A **missing** parameter is not an error — only a parameter
/// that is there and is not a valid integer is.
#[derive(Debug, Clone, Copy)]
pub struct Page {
    /// The most entries this page carries. `None` means "all of them".
    limit: Option<usize>,
    /// The entries this page skips. `None` means "none of them".
    offset: Option<usize>,
}

impl Page {
    /// Read and validate the `limit` / `offset` parameters.
    ///
    /// `limit` must be a positive integer and `offset` a non-negative
    /// one. A parameter that is not is a `422` naming the offending
    /// field — the same shape the `date` parameter of the raw log
    /// endpoints answers with.
    pub fn from_ctx(ctx: &Ctx<'_>) -> Result<Self, ApiError> {
        let limit = match ctx.query_param("limit") {
            Some(raw) => Some(parse_limit(&raw)?),
            None => None,
        };
        let offset = match ctx.query_param("offset") {
            Some(raw) => Some(parse_offset(&raw)?),
            None => None,
        };
        Ok(Self { limit, offset })
    }

    /// The slice of `items` this page is.
    ///
    /// An `offset` past the end of the list is an **empty** page: the
    /// caller has read to the end, which is not an error (see
    /// `an_offset_past_the_end_is_an_empty_page`).
    pub fn slice<'a, T>(&self, items: &'a [T]) -> &'a [T] {
        let offset = self.offset.unwrap_or(0).min(items.len());
        let rest = &items[offset..];
        match self.limit {
            Some(limit) => &rest[..limit.min(rest.len())],
            None => rest,
        }
    }
}

/// Parse the `limit` — a positive decimal integer.
///
/// A `limit` names how many entries the caller wants, so it has to be
/// at least one; a caller that wants nothing says so with an
/// `offset` past the end of the list.
fn parse_limit(raw: &str) -> Result<usize, ApiError> {
    match raw.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(ApiError::unprocessable(
            "limit_invalid",
            "the 'limit' parameter must be a positive integer",
            Some("limit"),
        )),
    }
}

/// Parse the `offset` — a non-negative decimal integer.
///
/// The value is parsed as an unsigned integer, so a negative offset
/// is rejected here.
fn parse_offset(raw: &str) -> Result<usize, ApiError> {
    match raw.trim().parse::<usize>() {
        Ok(n) => Ok(n),
        _ => Err(ApiError::unprocessable(
            "offset_invalid",
            "the 'offset' parameter must be a non-negative integer",
            Some("offset"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page with the given parameters.
    fn page(limit: Option<usize>, offset: Option<usize>) -> Page {
        Page { limit, offset }
    }

    #[test]
    fn a_page_without_parameters_is_the_whole_list() {
        let items = [0, 1, 2, 3, 4];
        assert_eq!(page(None, None).slice(&items), &items[..]);
    }

    #[test]
    fn a_limit_alone_takes_the_first_entries() {
        let items = [0, 1, 2, 3, 4];
        assert_eq!(page(Some(2), None).slice(&items), &items[..2]);
    }

    #[test]
    fn an_offset_alone_skips_to_the_end() {
        let items = [0, 1, 2, 3, 4];
        assert_eq!(page(None, Some(3)).slice(&items), &items[3..]);
    }

    #[test]
    fn a_limit_and_an_offset_take_one_segment() {
        let items = [0, 1, 2, 3, 4];
        assert_eq!(page(Some(2), Some(3)).slice(&items), &items[3..5]);
    }

    #[test]
    fn an_offset_past_the_end_is_an_empty_page() {
        let items = [0, 1, 2, 3, 4];
        assert!(page(None, Some(100_000)).slice(&items).is_empty());
        assert!(page(Some(2), Some(100_000)).slice(&items).is_empty());
    }

    #[test]
    fn a_limit_past_the_end_is_the_rest_of_the_list() {
        let items = [0, 1, 2, 3, 4];
        assert_eq!(page(Some(100_000), None).slice(&items), &items[..]);
    }

    #[test]
    fn a_limit_may_not_be_zero() {
        let err = parse_limit("0").expect_err("a limit of zero is invalid");
        assert_eq!(err.status, 422);
        assert_eq!(err.code, "limit_invalid");
        assert_eq!(err.field.as_deref(), Some("limit"));
    }

    #[test]
    fn a_limit_may_not_be_a_word_a_fraction_or_a_negative() {
        for raw in ["abc", "1.5", "-1", "", "1 2"] {
            let err = parse_limit(raw).expect_err("the limit is invalid");
            assert_eq!(err.code, "limit_invalid", "{raw}");
            assert_eq!(err.field.as_deref(), Some("limit"), "{raw}");
        }
    }

    #[test]
    fn a_limit_may_not_be_bigger_than_the_platform_holds() {
        let err = parse_limit("99999999999999999999999999").expect_err("the limit is invalid");
        assert_eq!(err.code, "limit_invalid");
    }

    #[test]
    fn an_offset_may_not_be_negative() {
        let err = parse_offset("-1").expect_err("the offset is invalid");
        assert_eq!(err.status, 422);
        assert_eq!(err.code, "offset_invalid");
        assert_eq!(err.field.as_deref(), Some("offset"));
    }

    #[test]
    fn an_offset_of_zero_is_the_start_of_the_list() {
        assert_eq!(parse_offset("0").expect("the offset is valid"), 0);
    }

    #[test]
    fn a_parameter_may_carry_whitespace_and_leading_zeros() {
        assert_eq!(parse_limit("  007 ").expect("the limit is valid"), 7);
        assert_eq!(parse_offset("\t042\n").expect("the offset is valid"), 42);
    }
}
