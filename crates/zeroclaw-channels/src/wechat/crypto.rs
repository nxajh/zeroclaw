use aes::Aes128;
use ecb::Encryptor;
use ecb::Decryptor;
use ecb::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use md5::{Md5, Digest};

/// AES-128-ECB encryption with PKCS7 padding.
pub fn encrypt_ecb(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let padded = pkcs7_pad(plaintext, 16);
    let mut encryptor = Encryptor::<Aes128>::new(key.into());
    padded
        .chunks(16)
        .flat_map(|chunk| {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            encryptor.encrypt_block_mut(&mut block.into());
            block.to_vec()
        })
        .collect()
}

/// AES-128-ECB decryption with PKCS7 unpadding.
pub fn decrypt_ecb(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    if ciphertext.len() % 16 != 0 {
        return Err("Ciphertext length is not a multiple of 16".into());
    }
    let mut decryptor = Decryptor::<Aes128>::new(key.into());
    let decrypted: Vec<u8> = ciphertext
        .chunks(16)
        .flat_map(|chunk| {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            decryptor.decrypt_block_mut(&mut block.into());
            block.to_vec()
        })
        .collect();
    pkcs7_unpad(&decrypted)
}

/// Encrypt and return base64-encoded string.
pub fn encrypt_to_b64(plaintext: &[u8], key: &[u8; 16]) -> String {
    BASE64.encode(encrypt_ecb(plaintext, key))
}

/// Decrypt from base64-encoded string.
pub fn decrypt_from_b64(b64: &str, key: &[u8; 16]) -> Result<Vec<u8>, String> {
    let ciphertext = BASE64
        .decode(b64)
        .map_err(|e| format!("base64 decode: {}", e))?;
    decrypt_ecb(&ciphertext, key)
}

/// MD5 hex digest.
pub fn md5_hex(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// PKCS7 padding.
fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let padding = block_size - (data.len() % block_size);
    let mut padded = data.to_vec();
    padded.extend(vec![padding as u8; padding]);
    padded
}

/// PKCS7 unpadding.
fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("Empty data".into());
    }
    let pad_len = *data.last().unwrap() as usize;
    if pad_len == 0 || pad_len > data.len() {
        return Err("Invalid padding".into());
    }
    if data[data.len() - pad_len..].iter().any(|&b| b != pad_len as u8) {
        return Err("Invalid PKCS7 padding".into());
    }
    Ok(data[..data.len() - pad_len].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = b"0123456789abcdef";
        let plaintext = b"hello world test";
        let encrypted = encrypt_ecb(plaintext, key);
        let decrypted = decrypt_ecb(&encrypted, key).unwrap();
        assert_eq!(plaintext.to_vec(), decrypted);
    }

    #[test]
    fn test_b64_roundtrip() {
        let key = b"0123456789abcdef";
        let plaintext = b"hello world";
        let b64 = encrypt_to_b64(plaintext, key);
        let decrypted = decrypt_from_b64(&b64, key).unwrap();
        assert_eq!(plaintext.to_vec(), decrypted);
    }

    #[test]
    fn test_md5() {
        let hash = md5_hex(b"test");
        assert_eq!(hash, "098f6bcd4621d373cade4e832627b4f6");
    }

    #[test]
    fn test_pkcs7_pad_unpad() {
        let data = b"12345";
        let padded = pkcs7_pad(data, 16);
        assert_eq!(padded.len(), 16);
        assert_eq!(&padded[5..], &[11u8; 11]);
        let unpadded = pkcs7_unpad(&padded).unwrap();
        assert_eq!(data.to_vec(), unpadded);
    }
}
