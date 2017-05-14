use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use errors::Result;

pub fn compress(src: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::Default);
    encoder.write_all(src)?;
    Ok(encoder.finish()?)
}

pub fn uncompress<T: Read>(src: T) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(src)?;
    let mut buffer: Vec<u8> = Vec::new();
    decoder.read_to_end(&mut buffer)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uncompress() {
        use std::io::Cursor;
        // The vector should uncompress to "test"
        let msg: Vec<u8> = vec![31, 139, 8, 0, 192, 248, 79, 85, 2, 255, 43, 73, 45, 46, 1, 0, 12,
                                126, 127, 216, 4, 0, 0, 0];
        let uncomp_msg = String::from_utf8(uncompress(Cursor::new(msg)).unwrap()).unwrap();
        assert_eq!(&uncomp_msg[..], "test");
    }

    #[test]
    #[should_panic]
    fn test_uncompress_panic() {
        use std::io::Cursor;
        let msg: Vec<u8> = vec![12, 42, 84, 104, 105, 115, 32, 105, 115, 32, 116, 101, 115, 116];
        let uncomp_msg = String::from_utf8(uncompress(Cursor::new(msg)).unwrap()).unwrap();
        assert_eq!(&uncomp_msg[..], "This is test");
    }
}
