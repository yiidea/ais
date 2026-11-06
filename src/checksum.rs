//! CRC32C（Castagnoli），用于 WAL 帧与段头校验。
//!
//! 设计文档冻结的是 `crc32c`（03 §2.2 / §2.1）。MVP 不引入外部 crate，
//! 这里实现标准的反射多项式 `0x82F63B78` 查表算法。

const POLY: u32 = 0x82F6_3B78;

const fn build_table() -> [[u32; 256]; 1] {
    let mut table = [[0u32; 256]; 1];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[0][i] = crc;
        i += 1;
    }
    table
}

static TABLE: [[u32; 256]; 1] = build_table();

/// 计算 CRC32C。
pub fn crc32c(data: &[u8]) -> u32 {
    let mut c = Crc32c::new();
    c.update(data);
    c.finish()
}

/// 增量计算器（段头与帧体都需要分多段喂入时使用）。
///
/// 内部保存「已取反」的累加值，`finish` 再取反一次，因此
/// `update` 多次与 `crc32c` 一次性计算的结果完全一致。
#[derive(Clone, Copy, Debug)]
pub struct Crc32c {
    crc: u32,
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32c {
    pub fn new() -> Self {
        Crc32c { crc: !0u32 }
    }

    pub fn update(&mut self, data: &[u8]) {
        let table = &TABLE[0];
        let mut crc = self.crc;
        for &b in data {
            crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        self.crc = crc;
    }

    pub fn finish(&self) -> u32 {
        !self.crc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_vector() {
        // RFC 3720 / Castagnoli 标准测试向量。
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
    }

    #[test]
    fn incremental_matches_oneshot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let mut c = Crc32c::new();
        c.update(&data[..10]);
        c.update(&data[10..]);
        assert_eq!(c.finish(), crc32c(data));
    }
}
