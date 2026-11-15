//! Compression and decompression utilities for HTTP bodies.
//!
//! Supports gzip, deflate, and brotli compression.

use bytes::Bytes;
use flate2::read::DeflateDecoder;
use flate2::read::MultiGzDecoder;
use flate2::read::ZlibDecoder;
use serde::Deserialize;
use serde::Serialize;
use std::io::Read;
use std::io::Write;
use tracing::trace;

/// Compression encoding types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// No compression
    Identity,

    /// Gzip compression
    Gzip,

    /// Deflate compression
    Deflate,

    /// Brotli compression
    Brotli,
}

impl std::fmt::Display for Encoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity => write!(f, "identity"),
            Self::Gzip => write!(f, "gzip"),
            Self::Deflate => write!(f, "deflate"),
            Self::Brotli => write!(f, "br"),
        }
    }
}

impl Encoding {
    /// Parse an encoding from a Content-Encoding header value.
    pub fn from_header(value: &str) -> Self {
        let value = value.trim().to_lowercase();
        match value.as_str() {
            "gzip" | "x-gzip" => Self::Gzip,
            "deflate" => Self::Deflate,
            "br" | "brotli" => Self::Brotli,
            _ => Self::Identity,
        }
    }

    /// Check if this encoding requires decompression.
    pub fn is_compressed(self) -> bool {
        !matches!(self, Self::Identity)
    }
}

/// Most bytes a body is decompressed to (128 MB).
///
/// A few kilobytes of gzip can expand to gigabytes. Output past this is
/// dropped, so the decompressed body is a prefix of the real one.
pub const MAX_DECOMPRESSED_SIZE: usize = 128 * 1024 * 1024;

/// Decompress a body based on the Content-Encoding.
///
/// Returns the original body if decompression fails or encoding is
/// identity. Output is capped at [`MAX_DECOMPRESSED_SIZE`].
pub fn decompress(body: &Bytes, encoding: Encoding) -> Bytes {
    decompress_capped(body, encoding, MAX_DECOMPRESSED_SIZE)
}

fn decompress_capped(body: &Bytes, encoding: Encoding, limit: usize) -> Bytes {
    if !encoding.is_compressed() || body.is_empty() {
        return body.clone();
    }

    let decoded = match encoding {
        // A response may hold several gzip members back to back;
        // `GzDecoder` would stop after the first.
        Encoding::Gzip => read_capped(MultiGzDecoder::new(body.as_ref()), limit),
        // HTTP's "deflate" is the zlib format (RFC 9110 §8.4.1.2), but
        // some servers send raw deflate. Try the standard one first.
        Encoding::Deflate => read_capped(ZlibDecoder::new(body.as_ref()), limit)
            .or_else(|_| read_capped(DeflateDecoder::new(body.as_ref()), limit)),
        Encoding::Brotli => read_capped(brotli::Decompressor::new(body.as_ref(), 4096), limit),
        Encoding::Identity => return body.clone(),
    };

    match decoded {
        Ok(decompressed) => {
            trace!(
                %encoding,
                original_size = body.len(),
                decompressed_size = decompressed.len(),
                "decompressed body"
            );
            Bytes::from(decompressed)
        }
        Err(e) => {
            trace!(%encoding, error = %e, "decompression failed, returning original");
            body.clone()
        }
    }
}

/// Read a decoder to the end or to `limit` bytes, whichever comes first.
fn read_capped(decoder: impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let limit = u64::try_from(limit).unwrap_or(u64::MAX);
    let mut decompressed = Vec::new();
    decoder.take(limit).read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

/// Compress a body using the specified encoding.
///
/// Returns the original body if compression fails or encoding is identity.
#[allow(dead_code)] // May be used in future for request modification
pub fn compress(body: &Bytes, encoding: Encoding) -> Bytes {
    if !encoding.is_compressed() || body.is_empty() {
        return body.clone();
    }

    match encoding {
        Encoding::Gzip => compress_gzip(body),
        Encoding::Deflate => compress_deflate(body),
        Encoding::Brotli => compress_brotli(body),
        Encoding::Identity => body.clone(),
    }
}

/// Compress data using gzip.
fn compress_gzip(body: &Bytes) -> Bytes {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());

    if encoder.write_all(body).is_err() {
        return body.clone();
    }

    match encoder.finish() {
        Ok(compressed) => {
            trace!(
                original_size = body.len(),
                compressed_size = compressed.len(),
                "compressed gzip"
            );
            Bytes::from(compressed)
        }
        Err(_) => body.clone(),
    }
}

/// Compress data using deflate.
fn compress_deflate(body: &Bytes) -> Bytes {
    use flate2::Compression;
    use flate2::write::DeflateEncoder;

    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());

    if encoder.write_all(body).is_err() {
        return body.clone();
    }

    match encoder.finish() {
        Ok(compressed) => {
            trace!(
                original_size = body.len(),
                compressed_size = compressed.len(),
                "compressed deflate"
            );
            Bytes::from(compressed)
        }
        Err(_) => body.clone(),
    }
}

/// Compress data using brotli.
fn compress_brotli(body: &Bytes) -> Bytes {
    let mut compressed = Vec::new();

    match brotli::BrotliCompress(&mut body.as_ref(), &mut compressed, &Default::default()) {
        Ok(_) => {
            trace!(
                original_size = body.len(),
                compressed_size = compressed.len(),
                "compressed brotli"
            );
            Bytes::from(compressed)
        }
        Err(_) => body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoding_from_header() {
        assert_eq!(Encoding::from_header("gzip"), Encoding::Gzip);
        assert_eq!(Encoding::from_header("GZIP"), Encoding::Gzip);
        assert_eq!(Encoding::from_header("x-gzip"), Encoding::Gzip);
        assert_eq!(Encoding::from_header("deflate"), Encoding::Deflate);
        assert_eq!(Encoding::from_header("br"), Encoding::Brotli);
        assert_eq!(Encoding::from_header("brotli"), Encoding::Brotli);
        assert_eq!(Encoding::from_header("identity"), Encoding::Identity);
        assert_eq!(Encoding::from_header("unknown"), Encoding::Identity);
    }

    #[test]
    fn test_gzip_roundtrip() {
        let original = Bytes::from("Hello, World! This is a test of gzip compression.");
        let compressed = compress_gzip(&original);
        let decompressed = decompress(&compressed, Encoding::Gzip);
        assert_eq!(original, decompressed);
    }

    #[test]
    fn test_deflate_roundtrip() {
        let original = Bytes::from("Hello, World! This is a test of deflate compression.");
        let compressed = compress_deflate(&original);
        let decompressed = decompress(&compressed, Encoding::Deflate);
        assert_eq!(original, decompressed);
    }

    #[test]
    fn test_brotli_roundtrip() {
        let original = Bytes::from("Hello, World! This is a test of brotli compression.");
        let compressed = compress_brotli(&original);
        let decompressed = decompress(&compressed, Encoding::Brotli);
        assert_eq!(original, decompressed);
    }

    #[test]
    fn test_deflate_accepts_the_zlib_format_http_specifies() {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;

        let original = b"zlib-wrapped, as RFC 9110 defines deflate";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = Bytes::from(encoder.finish().unwrap());

        assert_eq!(decompress(&compressed, Encoding::Deflate), &original[..]);
    }

    #[test]
    fn test_gzip_reads_every_member() {
        let mut both = compress_gzip(&Bytes::from_static(b"first ")).to_vec();
        both.extend_from_slice(&compress_gzip(&Bytes::from_static(b"second")));

        assert_eq!(
            decompress(&Bytes::from(both), Encoding::Gzip),
            &b"first second"[..]
        );
    }

    #[test]
    fn test_decompression_output_is_capped() {
        // Highly compressible input well past the cap.
        let large = Bytes::from(vec![0u8; 1024 * 1024]);
        for encoding in [Encoding::Gzip, Encoding::Deflate, Encoding::Brotli] {
            let compressed = compress(&large, encoding);
            assert!(
                compressed.len() < large.len() / 100,
                "{encoding}: a small input"
            );

            let decompressed = decompress_capped(&compressed, encoding, 4096);
            assert_eq!(decompressed.len(), 4096, "{encoding}");
        }
    }

    #[test]
    fn test_decompress_empty() {
        let empty = Bytes::new();
        assert_eq!(decompress(&empty, Encoding::Gzip), empty);
    }

    #[test]
    fn test_decompress_identity() {
        let body = Bytes::from("unchanged");
        assert_eq!(decompress(&body, Encoding::Identity), body);
    }
}
