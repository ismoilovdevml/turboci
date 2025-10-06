use anyhow::Result;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::{Read, Write};

/// Compress data using gzip
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Decompress gzip data
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

/// Check if data is worth compressing (compression ratio > 10%)
pub fn should_compress(data: &[u8]) -> bool {
    if data.len() < 1024 {
        // Don't compress small data (< 1KB)
        return false;
    }

    // Quick check: if data is already compressed (starts with gzip magic bytes)
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        return false;
    }

    true
}

/// Compress data if beneficial, returns (compressed_data, is_compressed)
pub fn compress_if_beneficial(data: &[u8]) -> Result<(Vec<u8>, bool)> {
    if !should_compress(data) {
        return Ok((data.to_vec(), false));
    }

    let compressed = compress(data)?;

    // Only use compression if it reduces size by at least 10%
    if compressed.len() < (data.len() * 9 / 10) {
        Ok((compressed, true))
    } else {
        Ok((data.to_vec(), false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_decompress() {
        let data = b"Hello, World! ".repeat(100);
        let compressed = compress(&data).unwrap();
        let decompressed = decompress(&compressed).unwrap();

        assert_eq!(data.to_vec(), decompressed);
        assert!(compressed.len() < data.len());
    }

    #[test]
    fn test_should_compress() {
        // Small data should not be compressed
        assert!(!should_compress(b"small"));

        // Large data should be compressed
        assert!(should_compress(&vec![0u8; 2048]));

        // Already compressed data should not be re-compressed
        let compressed = compress(b"test data").unwrap();
        assert!(!should_compress(&compressed));
    }

    #[test]
    fn test_compress_if_beneficial() {
        // Highly compressible data
        let data = b"aaaaaaaaaa".repeat(1000);
        let (result, is_compressed) = compress_if_beneficial(&data).unwrap();
        assert!(is_compressed);
        assert!(result.len() < data.len());

        // Random data (not very compressible)
        let random_data = (0..2048).map(|i| (i % 256) as u8).collect::<Vec<_>>();
        let (result, is_compressed) = compress_if_beneficial(&random_data).unwrap();
        // Even random data might compress a bit, so just check result is valid
        if is_compressed {
            assert!(result.len() < random_data.len());
        } else {
            assert_eq!(result.len(), random_data.len());
        }
    }
}
