//! MSE/PE（协议加密）握手。
//!
//! 为什么自己写，而不用 `bittorrent-rs` 的 `mse` feature：
//!
//! 库里的实现不是标准 MSE。它用了别的素数（Oakley group 1，而不是 MSE 规范
//! 用的那个），密钥派生是 `SHA1(secret || "initiator")`（规范是
//! `SHA1("keyA" || S || SKEY)`），握手也没有 VC / crypto_provide 协商，
//! 库自己的文档里写着「没有明文回退」。
//!
//! 后果很直接：开启它之后，我们发出去的字节跟所有真实客户端对不上，
//! **每一个**出站连接都会被对端断开。所以那个开关必须保持关着，
//! 这里的实现才是能用的那个。
//!
//! 参考实现：libtorrent 的 `src/pe_crypto.cpp` 和
//! `src/bt_peer_connection.cpp`，下面的字节布局按那两个文件对齐。
//!
//! 目前只做**入站**（服务端）一侧：
//! 对端是发起方，它决定要不要用加密；我们负责接住。
//! 出站那边保持明文——出站加密需要对端支持才能用，
//! 而我们的连接对象是别人给的 peer 列表，没法预知对端支不支持，
//! 贸然改成加密会把本来能连上的连接全弄断。

use std::io::{Read, Write};

use crypto_bigint::modular::runtime_mod::{DynResidue, DynResidueParams};
use crypto_bigint::{Encoding, U768};
use sha1::Digest;

/// MSE 规范用的 768 位 DH 素数。
///
/// 注意这跟 RFC 2409 的 Oakley group 1 素数**不一样**：
/// 末尾是 `...A36210000000000090563`，不是 `...A3620FFFFFFFFFFFFFFFF`。
/// 用错素数，密钥就永远对不上。
///
/// 这串值跟 libtorrent `src/pe_crypto.cpp` 里的 `dh_prime()` 一致。
const MSE_PRIME: U768 = U768::from_be_hex(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74\
     020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437\
     4FE1356D6D51C245E485B576625E7EC6F44C42E9A63A36210000000000090563",
);

/// DH 公钥长度（字节）。
const DH_KEY_LEN: usize = 96;

/// 明文 BT 握手长度（字节）。
const HANDSHAKE_LEN: usize = 68;

/// 标准 BT 握手的前 20 字节。
const PROTOCOL_IDENTIFIER: &[u8; 20] = b"\x13BitTorrent protocol";

/// PadA / PadB 的最大长度。
const MAX_PAD_LEN: usize = 512;

/// BitTorrent 握手。
#[derive(Debug, Clone)]
pub struct Handshake {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub reserved: [u8; 8],
}

impl Handshake {
    /// 解析 68 字节的明文握手。
    pub fn parse(raw: &[u8]) -> Option<Self> {
        if raw.len() < HANDSHAKE_LEN {
            return None;
        }
        if raw[0] != 19 || &raw[1..20] != b"BitTorrent protocol" {
            return None;
        }
        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&raw[28..48]);
        let mut peer_id = [0u8; 20];
        peer_id.copy_from_slice(&raw[48..68]);
        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&raw[20..28]);
        Some(Self {
            info_hash,
            peer_id,
            reserved,
        })
    }

    /// 生成回给对方的握手。
    ///
    /// reserved 位我们自己定，置上 DHT（0x01）、扩展协议（0x10），
    /// 以及 fast 扩展（0x04）。
    ///
    /// **不沿用对方的 reserved 位**：对方可能置了 v2（0x10 in reserved[7]）
    /// 之类的标志，我们并没有实现那些协议，回过去就等于骗对方。
    /// libtorrent 也是自己写一份固定的 reserved。
    ///
    /// **不置 PE 支持位（0x40）**：这个位在 libtorrent 里不是「我支持加密」，
    /// 而是 PEX 扩展里 peer 标志的一个约定，各家实现并不统一。
    /// 我们是入站方，由对端决定要不要发起加密握手，不需要靠这个位宣告。
    pub fn build_reply(info_hash: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HANDSHAKE_LEN);
        out.push(19);
        out.extend_from_slice(b"BitTorrent protocol");
        let mut res = [0u8; 8];
        res[5] |= 0x10; // 扩展协议
        res[7] |= 0x01; // DHT
        res[7] |= 0x04; // fast 扩展
        out.extend_from_slice(&res);
        out.extend_from_slice(info_hash);
        out.extend_from_slice(b"-IDMS-0001-000000000");
        out
    }
}

/// RC4 流密码。
///
/// MSE 要求初始化后先丢弃 1024 字节密钥流，
/// 这样握手头部就没法被直接猜出来。
pub struct Rc4 {
    state: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    pub fn new(key: &[u8]) -> Self {
        let mut state = [0u8; 256];
        for (i, b) in state.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut j: u8 = 0;
        for i in 0..256 {
            j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
            state.swap(i, j as usize);
        }
        Self { state, i: 0, j: 0 }
    }

    /// 丢弃前 1024 字节密钥流（MSE 的要求）。
    pub fn discard_1024(&mut self) {
        let mut buf = [0u8; 1024];
        self.apply(&mut buf);
    }

    /// 就地加解密（RC4 是对称的，同一个操作既加密也解密）。
    pub fn apply(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.state[self.i as usize]);
            self.state.swap(self.i as usize, self.j as usize);
            let t = self.state[self.i as usize].wrapping_add(self.state[self.j as usize]);
            *byte ^= self.state[t as usize];
        }
    }
}

/// DH 密钥交换。
pub struct DhKeyExchange {
    private_key: U768,
    /// 96 字节的本地公钥。
    pub public_key: [u8; DH_KEY_LEN],
}

impl DhKeyExchange {
    pub fn new() -> Self {
        use rand::RngCore;
        let mut rng = rand::thread_rng();
        let mut private_bytes = [0u8; DH_KEY_LEN];
        let mut private_key;
        loop {
            rng.fill_bytes(&mut private_bytes);
            private_key = U768::from_be_slice(&private_bytes);
            // 私钥不能是 0 或者 1，否则公钥会退化。
            if private_key >= U768::from(2u8) {
                break;
            }
        }
        Self::from_private_key(private_key)
    }

    /// 用给定的私钥构造。
    ///
    /// 单独拆出来是为了测试——测试里要用一个**固定的**私钥，
    /// 这样客户端侧才能预先算出同一个共享密钥，
    /// 从而把双方的字节流拼起来做端到端验证。
    fn from_private_key(private_key: U768) -> Self {

        let params = DynResidueParams::new(&MSE_PRIME);
        let g = DynResidue::new(&U768::from(2u8), params);
        let public = g.pow(&private_key).retrieve();
        let mut public_key = [0u8; DH_KEY_LEN];
        public_key.copy_from_slice(&public.to_be_bytes());

        Self {
            private_key,
            public_key,
        }
    }

    /// 固定私钥的构造（测试用）。
    #[cfg(test)]
    fn with_fixed_private_key(seed: u8) -> Self {
        let mut private_bytes = [0u8; DH_KEY_LEN];
        // 填成固定值，但保证 >= 2。
        private_bytes[DH_KEY_LEN - 1] = seed.max(2);
        Self::from_private_key(U768::from_be_slice(&private_bytes))
    }

    /// 算出共享密钥。
    ///
    /// 对端公钥必须在 `[2, p-2]` 区间里，
    /// 否则共享密钥会被固定成一小撮已知值，握手就等于没加密。
    /// libtorrent 的判定是 `key < 2 || key >= p - 1`，这里跟它一致。
    pub fn compute_secret(&self, remote_public: &[u8; DH_KEY_LEN]) -> Option<[u8; DH_KEY_LEN]> {
        let remote = U768::from_be_slice(remote_public);
        if remote < U768::from(2u8) {
            return None;
        }
        // p - 2 是合法上界，p - 1 及以上都拒绝。
        //
        // `Uint` 不实现 `Sub`（怕溢出），所以用包装减法算。
        let p_minus_2 = MSE_PRIME.wrapping_sub(&U768::from(2u8));
        if remote > p_minus_2 {
            return None;
        }
        let params = DynResidueParams::new(&MSE_PRIME);
        let remote_res = DynResidue::new(&remote, params);
        let secret = remote_res.pow(&self.private_key).retrieve();
        let mut out = [0u8; DH_KEY_LEN];
        out.copy_from_slice(&secret.to_be_bytes());
        Some(out)
    }
}

/// 一次 PE 握手协商出来的加密状态。
pub struct PeStream {
    /// 加密用（发给对端的方向）。
    pub enc: Rc4,
    /// 解密用（从对端收的方向）。
    pub dec: Rc4,
}

/// 算 SHA-1。
fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut hasher = sha1::Sha1::new();
    for p in parts {
        hasher.update(p);
    }
    let out = hasher.finalize();
    let mut result = [0u8; 20];
    result.copy_from_slice(&out);
    result
}

/// 服务端（入站）方向的密钥。
///
/// 规范里：
/// - 发起方加密用 `hash("keyA", S, SKEY)`，解密用 `hash("keyB", S, SKEY)`
/// - 接收方反过来
///
/// 这跟 libtorrent 的 `init_pe_rc4_handler(secret, info_hash, outgoing)` 一致：
/// `outgoing == false`（也就是入站服务端）时，本地密钥是 keyB，远程密钥是 keyA。
fn server_keys(secret: &[u8; DH_KEY_LEN], info_hash: &[u8; 20]) -> PeStream {
    let mut enc = Rc4::new(&sha1(&[b"keyB", secret, info_hash]));
    let mut dec = Rc4::new(&sha1(&[b"keyA", secret, info_hash]));
    enc.discard_1024();
    dec.discard_1024();
    PeStream { enc, dec }
}

/// 两个 20 字节哈希按位异或。
fn xor20(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// 随机 Pad 长度（0..=512）。
fn random_pad_len() -> usize {
    use rand::Rng;
    rand::thread_rng().gen_range(0..=MAX_PAD_LEN)
}

/// 随机字节。
fn random_bytes(len: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut buf = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

/// 判断一段字节是不是明文 BT 握手的头。
pub fn is_plain_protocol_identifier(raw: &[u8]) -> bool {
    raw.len() >= 20 && &raw[..20] == PROTOCOL_IDENTIFIER
}

/// 带缓冲的读取器。
///
/// 为什么需要它：PE 握手里有一步是「在字节流里搜索同步哈希」。
/// 搜索的时候不能一次性读一大块——读进来的、位于同步点**之后**的字节
/// 属于握手的下一阶段，必须留给下一步读，不能被吃掉。
/// 所以这里保留一个缓冲区，支持「把读回来的字节再塞回去」。
pub struct Buffered<R> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
}

impl<R: Read> Buffered<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// 把还没读出去的字节挪到最前面（丢掉已经读掉的）。
    fn compact(&mut self) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }

    /// 缓冲区里还没读出去的部分。
    pub fn buffered(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// 底层 reader。
    pub fn inner(&self) -> &R {
        &self.inner
    }

    /// 读满 `n` 个字节。
    pub fn take(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        while self.buffered().len() < n {
            let mut tmp = [0u8; 4096];
            let got = self.inner.read(&mut tmp)?;
            if got == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "PE 握手提前结束",
                ));
            }
            self.compact();
            self.buf.extend_from_slice(&tmp[..got]);
        }
        let out = self.buf[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Ok(out)
    }

    /// 把 `n` 个字节读进 `out`。
    ///
    /// 用已有的缓冲区做中转，避免再分配一块。
    pub fn take_exact(&mut self, out: &mut [u8]) -> std::io::Result<()> {
        let n = out.len();
        let data = self.take(n)?;
        out.copy_from_slice(&data);
        Ok(())
    }

    /// 丢弃 `n` 个字节。
    pub fn skip(&mut self, n: usize) -> std::io::Result<()> {
        self.take(n).map(|_| ())
    }

    /// 在流里搜索一段字节，返回它相对当前读位置的偏移。
    ///
    /// 最多看 `max` 字节（规范说 PadA 最多 512 字节，
    /// 加上同步哈希本身 20 字节，所以实际上限给到 532）。
    /// **找到之后，匹配段之后的字节会留在缓冲区里**，交给下一步读。
    pub fn find(&mut self, needle: &[u8], max: usize) -> std::io::Result<Option<usize>> {
        loop {
            let hay = self.buffered();
            if let Some(pos) = find_subslice(hay, needle) {
                return Ok(Some(pos));
            }
            // 看过的字节数已经到上限，放弃。
            if hay.len() >= max {
                return Ok(None);
            }
            let mut tmp = [0u8; 512];
            let got = self.inner.read(&mut tmp)?;
            if got == 0 {
                return Ok(None);
            }
            self.compact();
            self.buf.extend_from_slice(&tmp[..got]);
        }
    }

    /// 把字节塞回缓冲区最前面。
    ///
    /// 用在「先 peek 一下是不是明文握手，对不上就退回从头读」这种场合。
    pub fn prepend(&mut self, data: &[u8]) {
        self.compact();
        let mut new = Vec::with_capacity(data.len() + self.buf.len());
        new.extend_from_slice(data);
        new.extend_from_slice(&self.buf);
        self.buf = new;
        self.pos = 0;
    }
}

/// 找子串位置。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 服务端：接住一次 PE 加密握手。
///
/// 流程（对着 libtorrent 的 `on_receive_impl` 状态机写）：
/// 1. 读 96 字节 DH 公钥
/// 2. 回自己的 DH 公钥 + PadB
/// 3. 在接下来的字节里找 `hash("req1", S)`（最多找 512 字节）
/// 4. 读 20 字节 skeyhash（明文），校验它
/// 5. 读 VC(8) + crypto_provide(4) + len_pad(2)（加密）
/// 6. 读 pad + len_ia(2)（加密），再读 IA（加密的 BT 握手）
/// 7. 回 VC(8) + crypto_select(4) + len_pad(2) + pad（加密）
///
/// 注意两件事：
///
/// - 第 3 步之后**不需要**再发 40 字节的明文回包——
///   那是发起方（出站）才会做的事，服务端这边只回第 7 步那包加密的东西。
/// - 第 7 步之后**必须**再回一份 RC4 加密的 BT 握手。
///   libtorrent 在 `read_info_hash` 状态里调 `write_handshake()`，
///   而这时候收发两个方向都已经切到 RC4 了，所以那份握手是**加密**的。
///   漏掉这一步，对端会一直等我们的握手，
///   表现为「加密入站全部连上但一个字节都传不动」。
pub fn server_handshake<R: Read, W: Write>(
    reader: &mut Buffered<R>,
    writer: &mut W,
    info_hash: &[u8; 20],
) -> std::io::Result<Option<(Handshake, PeStream)>> {
    server_handshake_with(reader, writer, info_hash, DhKeyExchange::new())
}

/// 服务端握手，但 DH 密钥对由调用方指定。
///
/// 单独拆一个入口是为了测试：测试里要用**固定的**服务端私钥，
/// 这样客户端侧才能预先算出同一个共享密钥，
/// 把双方的字节流拼起来做端到端验证。
pub fn server_handshake_with<R: Read, W: Write>(
    reader: &mut Buffered<R>,
    writer: &mut W,
    info_hash: &[u8; 20],
    dh: DhKeyExchange,
) -> std::io::Result<Option<(Handshake, PeStream)>> {
    // 1. DH 公钥
    let remote_pub_bytes = reader.take(DH_KEY_LEN)?;
    let mut remote_pub = [0u8; DH_KEY_LEN];
    remote_pub.copy_from_slice(&remote_pub_bytes);

    let Some(secret) = dh.compute_secret(&remote_pub) else {
        return Ok(None);
    };
    let mut keys = server_keys(&secret, info_hash);

    // 2. 回 DH 公钥 + PadB
    let pad_b_len = random_pad_len();
    let mut out = Vec::with_capacity(DH_KEY_LEN + pad_b_len);
    out.extend_from_slice(&dh.public_key);
    out.extend_from_slice(&random_bytes(pad_b_len));
    writer.write_all(&out)?;
    writer.flush()?;

    // 3. 找 synchash。前面的是 PadA，消耗掉就行。
    let sync_hash = sha1(&[b"req1", &secret]);
    let Some(offset) = reader.find(&sync_hash, MAX_PAD_LEN + 20)? else {
        return Ok(None);
    };
    // 跳过 PadA **和** 同步哈希本身。
    //
    // 这里必须按 `offset + len` 跳，不能只跳 `len`：
    // `find` 返回的是同步哈希在**流里**的绝对偏移，
    // 而 `skip` 是从当前读指针往后数的。
    // 只跳 20 字节的话，跳过的是 PadA 的头 20 字节，
    // 同步哈希还留在缓冲区里——接下来读到的 "skeyhash"
    // 其实是同步哈希，校验必然失败。
    reader.skip(offset + sync_hash.len())?;

    // 4. skeyhash（明文 20 字节）
    let skeyhash_bytes = reader.take(20)?;
    let mut skeyhash = [0u8; 20];
    skeyhash.copy_from_slice(&skeyhash_bytes);

    // 校验 skeyhash：hash("req2", SKEY) xor hash("req3", S)
    let expect = xor20(&sha1(&[b"req2", info_hash]), &sha1(&[b"req3", &secret]));
    if skeyhash != expect {
        // 不是给我们的种子。
        return Ok(None);
    }

    // 5. 加密区：VC(8) + crypto_provide(4) + len_pad(2)
    let mut enc_head = reader.take(14)?;
    keys.dec.apply(&mut enc_head);
    let vc = &enc_head[0..8];
    if vc.iter().any(|b| *b != 0) {
        // 验证常量不对，说明对方算出来的密钥跟我们不一样。
        return Ok(None);
    }
    let crypto_provide =
        u32::from_be_bytes([enc_head[8], enc_head[9], enc_head[10], enc_head[11]]);
    let len_pad = u16::from_be_bytes([enc_head[12], enc_head[13]]) as usize;
    if len_pad > MAX_PAD_LEN {
        return Ok(None);
    }

    // 对端提供的加密方式里必须包含 RC4，否则我们没法继续。
    // bit 0 = 明文，bit 1 = RC4。
    if crypto_provide & 0x02 == 0 {
        return Ok(None);
    }

    // 回我们的选择：RC4。
    //
    // 顺序照 libtorrent：它在读到 pad/IA 之前就把这包发出去。
    // 对端是发起方，它早把整条握手发出去了，不存在互相等待。
    let mut reply = Vec::new();
    reply.extend_from_slice(&[0u8; 8]); // VC
    reply.extend_from_slice(&2u32.to_be_bytes()); // crypto_select = RC4
    let reply_pad = random_pad_len();
    reply.extend_from_slice(&(reply_pad as u16).to_be_bytes());
    reply.extend_from_slice(&random_bytes(reply_pad));
    keys.enc.apply(&mut reply);
    writer.write_all(&reply)?;
    writer.flush()?;

    // 6. pad + len_ia
    let mut tail = reader.take(len_pad + 2)?;
    keys.dec.apply(&mut tail);
    let len_ia = u16::from_be_bytes([tail[len_pad], tail[len_pad + 1]]) as usize;
    if len_ia > HANDSHAKE_LEN {
        return Ok(None);
    }

    // IA：加密的 BT 握手
    let mut ia = reader.take(len_ia)?;
    keys.dec.apply(&mut ia);

    // IA 后面如果还有握手（len_ia < 68），补读。
    //
    // 注意补读的这段**也是 RC4 加密的**——RC4 是流密码，
    // 加密流不会因为「这一段单独发」而断开。
    let handshake = if len_ia == HANDSHAKE_LEN {
        Handshake::parse(&ia)
    } else {
        let mut raw = vec![0u8; HANDSHAKE_LEN];
        raw[..len_ia].copy_from_slice(&ia);
        let mut rest = reader.take(HANDSHAKE_LEN - len_ia)?;
        keys.dec.apply(&mut rest);
        raw[len_ia..].copy_from_slice(&rest);
        Handshake::parse(&raw)
    };

    let Some(handshake) = handshake else {
        return Ok(None);
    };

    // 8. 回我们的 BT 握手（RC4 加密）。
    //
    // 这是 libtorrent 的 `write_handshake()`：入站方在读完对方的
    // 68 字节握手之后，要把**自己**的握手也发过去，
    // 否则对方连我们的 peer id 都拿不到。
    //
    // 注意这里是加密的：此刻收发两个方向都已经切到 RC4，
    // libtorrent 那边也是 `switch_send_crypto(m_rc4)` 之后才写的。
    let mut our_hs = Handshake::build_reply(info_hash);
    keys.enc.apply(&mut our_hs);
    writer.write_all(&our_hs)?;
    writer.flush()?;

    Ok(Some((handshake, keys)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6229 的 RC4 测试向量。
    ///
    /// MSE 的 RC4 要先丢 1024 字节，所以向量给的是偏移 1024 处的密钥流。
    /// 用它验证我们的 RC4 实现跟标准一致——这是整个 PE 能不能用的前提。
    #[test]
    fn rc4_matches_rfc6229_vectors() {
        struct Vector {
            key: &'static str,
            at_1024: &'static str,
        }
        let vectors = [
            Vector {
                key: "0102030405",
                at_1024: "30abbcc7c20b01609f23ee2d5f6bb7df",
            },
            Vector {
                key: "0102030405060708090a0b0c0d0e0f10",
                at_1024: "bdf0324e6083dcc6d3cedd3ca8c53c16",
            },
            Vector {
                key: "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
                at_1024: "7fec5bfd9f9b89ce6548309092d7e958",
            },
        ];

        for v in vectors {
            let key = hex_decode(v.key);
            let mut rc4 = Rc4::new(&key);
            // 丢弃前 1024 字节后，取 16 字节密钥流
            let mut buf = vec![0u8; 1024 + 16];
            rc4.apply(&mut buf);
            let got = hex_encode(&buf[1024..]);
            assert_eq!(got, v.at_1024, "RC4 向量不匹配: key={}", v.key);
        }
    }

    #[test]
    fn dh_双方算出同一个共享密钥() {
        let a = DhKeyExchange::new();
        let b = DhKeyExchange::new();
        let sa = a.compute_secret(&b.public_key).unwrap();
        let sb = b.compute_secret(&a.public_key).unwrap();
        assert_eq!(sa, sb, "共享密钥应当一致");
    }

    #[test]
    fn dh_拒绝退化公钥() {
        let dh = DhKeyExchange::new();
        // 公钥必须是 96 字节的大端整数。
        // 注意 `[1u8; 96]` 是 0x0101…01，不是数字 1——
        // 想表示 1 得写成「95 个 0 + 一个 1」。
        assert!(dh.compute_secret(&[0u8; 96]).is_none());
        let mut one = [0u8; 96];
        one[95] = 1;
        assert!(dh.compute_secret(&one).is_none());
        // p 和 p-1 都非法（退化公钥会让共享密钥固定成已知值）。
        let mut p = [0u8; 96];
        p.copy_from_slice(&MSE_PRIME.to_be_bytes());
        assert!(dh.compute_secret(&p).is_none());
        let mut p_minus_1 = p;
        p_minus_1[95] -= 1;
        assert!(dh.compute_secret(&p_minus_1).is_none());
        // p-2 是合法的上界。
        let mut p_minus_2 = p;
        p_minus_2[95] -= 2;
        assert!(dh.compute_secret(&p_minus_2).is_some());
    }

    #[test]
    fn 握手能解析() {
        let info_hash = [7u8; 20];
        let reply = Handshake::build_reply(&info_hash);
        let parsed = Handshake::parse(&reply).expect("应当能解析自己发的握手");
        assert_eq!(parsed.info_hash, info_hash);
        assert_eq!(&parsed.peer_id[..], b"-IDMS-0001-000000000");
        assert_ne!(parsed.reserved[5] & 0x10, 0, "应当宣告支持扩展协议");
        assert_ne!(parsed.reserved[7] & 0x01, 0, "应当宣告支持 DHT");
    }

    #[test]
    fn 坏握手解析失败() {
        assert!(Handshake::parse(&[0u8; 68]).is_none());
        assert!(Handshake::parse(&[0u8; 10]).is_none());
    }

    #[test]
    fn 子串查找正确() {
        assert_eq!(find_subslice(b"abcdef", b"cd"), Some(2));
        assert_eq!(find_subslice(b"abcdef", b"gh"), None);
        assert_eq!(find_subslice(b"ab", b"abc"), None);
    }

    /// 缓冲区：搜索到的位置之后的字节必须留着给下一步读。
    ///
    /// 这是 PE 握手最容易出错的地方——旧实现一次性读 512 字节，
    /// 把同步点之后的握手字节一起吃掉了，后面全乱。
    #[test]
    fn buffered_搜索后保留后续字节() {
        let data = b"AAAAsyncTAIL".to_vec();
        let mut reader = Buffered::new(std::io::Cursor::new(data));
        let offset = reader.find(b"sync", 64).unwrap();
        assert_eq!(offset, Some(4));
        // 跳到同步哈希之后，后面的字节必须原样留着。
        reader.skip(4 + 4).unwrap();
        assert_eq!(reader.take(4).unwrap(), b"TAIL");
    }

    #[test]
    fn buffered_超限就放弃() {
        let data = vec![0u8; 100];
        let mut reader = Buffered::new(std::io::Cursor::new(data));
        assert!(reader.find(b"nope", 32).unwrap().is_none());
    }

    /// 端到端：一个规范客户端跟我们的服务端握手，两边各自算密钥，
    /// 再互解对方发来的字节。
    #[test]
    fn pe_握手能接住规范客户端() {
        let info_hash = [0x42u8; 20];

        // 客户端（发起方）侧的密钥。
        let client_dh = DhKeyExchange::new();
        // 服务端用**固定**的私钥，客户端才能预先算出同一个共享密钥。
        // 服务端如果自己随机生成密钥，客户端这边就无从知道 secret，
        // 双方算出来的 RC4 密钥对不上，测试必然失败。
        let server_dh = DhKeyExchange::with_fixed_private_key(7);
        let secret = client_dh
            .compute_secret(&server_dh.public_key)
            .expect("客户端应当能算出共享密钥");

        // 发起方：加密 keyA，解密 keyB。
        //
        // 规范里发起方加密用 keyA、解密用 keyB；
        // 接收方（我们）反过来。
        let mut client_enc = Rc4::new(&sha1(&[b"keyA", &secret, &info_hash]));
        let mut client_dec = Rc4::new(&sha1(&[b"keyB", &secret, &info_hash]));
        client_enc.discard_1024();
        client_dec.discard_1024();

        // 客户端发 DH 公钥 + PadA + sync_hash + skeyhash + (加密的 VC/provide/len_pad/pad/len_ia/IA)
        let pad_a = random_bytes(64);
        let mut client_stream = Vec::new();
        client_stream.extend_from_slice(&client_dh.public_key);
        client_stream.extend_from_slice(&pad_a);
        client_stream.extend_from_slice(&sha1(&[b"req1", &secret]));
        let skeyhash = xor20(&sha1(&[b"req2", &info_hash]), &sha1(&[b"req3", &secret]));
        client_stream.extend_from_slice(&skeyhash);

        let ia = Handshake::build_reply(&info_hash);
        let pad_len = 32u16;
        let mut enc_part = Vec::new();
        enc_part.extend_from_slice(&[0u8; 8]); // VC
        enc_part.extend_from_slice(&2u32.to_be_bytes()); // crypto_provide: RC4
        enc_part.extend_from_slice(&pad_len.to_be_bytes());
        enc_part.extend_from_slice(&random_bytes(pad_len as usize));
        enc_part.extend_from_slice(&(ia.len() as u16).to_be_bytes());
        enc_part.extend_from_slice(&ia);
        client_enc.apply(&mut enc_part);
        client_stream.extend_from_slice(&enc_part);

        // 服务端跑握手。
        let mut server_out: Vec<u8> = Vec::new();
        let mut reader = Buffered::new(std::io::Cursor::new(client_stream));
        let result = server_handshake_with(&mut reader, &mut server_out, &info_hash, server_dh)
            .expect("握手过程不该有 IO 错误");
        let Some((handshake, _server_keys)) = result else {
            panic!("应当走加密握手");
        };
        assert_eq!(handshake.info_hash, info_hash);

        // 服务端回包：DH 公钥 + PadB + (加密的 VC/select/len_pad/pad) + (加密的 BT 握手)
        // 前 96 字节是服务端的 DH 公钥，后面全是加密的。
        let raw = server_out[DH_KEY_LEN..].to_vec();
        // 客户端怎么跳过 PadB？
        // 规范里是拿 RC4 密钥流的前 8 字节去**原始字节**里搜——
        // 因为服务端加密的 VC 就是 8 个零，加密之后正好等于密钥流本身。
        // libtorrent 的 `read_pe_syncvc` 就是这么干的。
        let mut probe = Rc4::new(&sha1(&[b"keyB", &secret, &info_hash]));
        probe.discard_1024();
        let mut vc_marker = [0u8; 8];
        probe.apply(&mut vc_marker);
        let offset = find_subslice(&raw, &vc_marker)
            .expect("应当能在原始字节里找到加密后的验证常量");
        let mut client_view = raw[offset..].to_vec();

        // 客户端的解密方向用 keyB，服务端的加密方向也用 keyB，
        // 所以整段回包都能用客户端的解密密钥解开。
        client_dec.apply(&mut client_view);

        // crypto_select 回包：VC(8 零) + crypto_select(4) + len_pad(2) + pad
        assert_eq!(&client_view[0..8], &[0u8; 8], "验证常量应当是全零");
        assert_eq!(
            u32::from_be_bytes([
                client_view[8],
                client_view[9],
                client_view[10],
                client_view[11],
            ]),
            2,
            "crypto_select 应当是 RC4"
        );
        let reply_pad = u16::from_be_bytes([client_view[12], client_view[13]]) as usize;

        // 客户端用自己的解密密钥解服务端的回包，应当跟服务端解出来的一致。
        // 第 8 步：服务端还要回一份**加密的** BT 握手。
        // 客户端要能把它解出来。
        let hs_bytes = client_view[14 + reply_pad..].to_vec();
        assert_eq!(hs_bytes.len(), HANDSHAKE_LEN, "应当回一份 68 字节的握手");
        let server_hs = Handshake::parse(&hs_bytes).expect("应当能解析出服务端的握手");
        assert_eq!(server_hs.info_hash, info_hash);
        assert_eq!(&server_hs.peer_id[..], b"-IDMS-0001-000000000");
    }

    /// hex 辅助（测试用）。
    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn hex_encode(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
