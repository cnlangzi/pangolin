//! DEFLATE compression for permessage-deflate WebSocket extension.
//!
//! Raw DEFLATE (no zlib header/trailer) is used by permessage-deflate.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};

/// Compress data to raw DEFLATE (no zlib header/trailer).
/// Returns data that permessage-deflate expects.
pub fn deflate_encode(data: &[u8]) -> Vec<u8> {
    let mut comp = Compress::new(Compression::fast(), true); // true = raw deflate
    let mut out = Vec::new();
    comp.compress(data, &mut out, FlushCompress::Finish)
        .unwrap();
    // For very small inputs, flate2 may produce 0 bytes; in that case use raw data
    if out.is_empty() && !data.is_empty() {
        out.extend_from_slice(data);
    }
    out
}

/// Decode raw DEFLATE to original data.
pub fn deflate_decode(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut decomp = Decompress::new(true); // true = raw deflate
    let mut out = Vec::new();
    let status = decomp.decompress(data, &mut out, FlushDecompress::Finish)?;
    if status == flate2::Status::StreamEnd {
        Ok(out)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "incomplete deflate stream",
        ))
    }
}
