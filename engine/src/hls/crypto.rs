//! AES-128 解密。
//!
//! HLS 的 AES-128 用的是 CBC 模式，IV 优先取 #EXT-X-KEY 里的 IV，
//! 没给的话用分片的媒体序号当作 IV（大端 16 字节）。

use aes::cipher::{BlockDecryptMut, KeyIvInit};
use anyhow::{anyhow, Result};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// 用 key 和 iv 解密一段数据。
pub fn decrypt_aes128(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 16 {
        return Err(anyhow!(
            "AES-128 密钥必须是 16 字节，实际 {} 字节",
            key.len()
        ));
    }
    if iv.len() != 16 {
        return Err(anyhow!("AES-128 IV 必须是 16 字节，实际 {} 字节", iv.len()));
    }

    let mut buffer = data.to_vec();
    let decryptor = Aes128CbcDec::new_from_slices(key, iv)
        .map_err(|_| anyhow!("AES-128 初始化失败"))?;

    // CBC 模式会留下末尾的 PKCS#7 填充，去掉
    let decrypted = decryptor
        .decrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut buffer)
        .map(|d| d.to_vec())
        .map_err(|_| anyhow!("AES-128 解密失败（密钥或 IV 不对？）"))?;

    Ok(decrypted)
}

/// 没有 IV 时，用分片序号生成一个。
pub fn iv_from_sequence(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockEncryptMut, KeyIvInit};

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    #[test]
    fn roundtrip_encrypt_decrypt() {
        let key = [7u8; 16];
        let iv = [3u8; 16];
        let plain = b"hello hls segment, this is a test payload!";

        // cipher 0.4 的 padding 需要额外空间容纳填充块
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let buf_len = plain.len();
        let enc = Aes128CbcEnc::new_from_slices(&key, &iv).unwrap();
        let ciphered = enc
            .encrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut buf, buf_len)
            .unwrap()
            .to_vec();

        let out = decrypt_aes128(&ciphered, &key, &iv).unwrap();
        assert_eq!(out, plain);
    }

    #[test]
    fn rejects_wrong_key_length() {
        assert!(decrypt_aes128(b"data", &[0u8; 8], &[0u8; 16]).is_err());
    }

    #[test]
    fn rejects_wrong_iv_length() {
        assert!(decrypt_aes128(b"data", &[0u8; 16], &[0u8; 4]).is_err());
    }

    #[test]
    fn iv_from_sequence_is_big_endian_in_tail() {
        let iv = iv_from_sequence(0x0102);
        assert_eq!(iv[14], 0x01);
        assert_eq!(iv[15], 0x02);
        assert_eq!(iv[0], 0);
    }
}
