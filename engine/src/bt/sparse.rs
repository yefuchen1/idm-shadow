//! 稀疏文件占位。
//!
//! ## 为什么需要它
//!
//! 磁力链接的"选文件后下载"要求先知道种子里有哪些文件，
//! 用户勾选之后才开始下。这时候文件得先在磁盘上占位——
//! 因为 `bittorrent-rs` 的写入是按"全局偏移 → 定位到具体文件"来的，
//! 文件不存在的话写块会直接失败。
//!
//! 但库的预分配（`FileAllocationManager::allocate_torrent_structure`）
//! 是这么做的：
//!
//! ```text
//! let handle = File::create(path)?;
//! handle.set_len(file.length)?;
//! ```
//!
//! `File::create` + `set_len` 在 NTFS 上**真的分配磁盘块**。
//! 实测：造一个 4GB 的文件，`set_len` 之后磁盘可用空间少了 4GB，
//! `fsutil sparse queryflag` 说它不是稀疏文件。
//!
//! 也就是说，用户只是点了个磁力链接、还没决定下哪些文件，
//! 硬盘就被整个种子的体积占满了。一个 50GB 的种子的磁力链接就能做到。
//!
//! ## 怎么解决
//!
//! 自己造**稀疏文件**：逻辑长度是整个文件，实际占用只有写过的那些块。
//!
//! Windows 上要用 `DeviceIoControl(FSCTL_SET_SPARSE)`，
//! 光在 `CreateFileW` 里传 `FILE_ATTRIBUTE_SPARSE_FILE` 是**不够的**——
//! 实测那样做出来还是真占盘。必须显式发一次 `FSCTL_SET_SPARSE`。
//!
//! 顺序很关键：先 `CreateFileW`（普通属性）→ `DeviceIoControl(FSCTL_SET_SPARSE)`
//! → `SetFilePointerEx` 到末尾 → `SetEndOfFile` 定长度。
//! 顺序反了就不稀疏。

use std::path::Path;

use anyhow::{Context, Result};

/// 创建（或复用）一个指定长度的稀疏文件。
///
/// 文件已经存在且长度够大时什么也不做——那说明是断点续传，
/// 里面已经有下好的数据了，不能覆盖。
///
/// 非 Windows 平台上退化成普通文件创建（占真磁盘）。
/// 那是可接受的降级：功能正确，只是占盘多一些。
pub fn create_sparse_file(path: &Path, length: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }

    // 已经存在而且够长：断点续传，别碰。
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() >= length {
            return Ok(());
        }
    }

    #[cfg(windows)]
    {
        imp::create_sparse(path, length)
    }
    #[cfg(not(windows))]
    {
        create_sparse_fallback(path, length)
    }
}

/// 文件在磁盘上实际占了多少字节。
///
/// 稀疏文件返回的是**实际占用**（不是逻辑长度），
/// 拿它可以验证"文件建出来了但没占盘"。
pub fn allocated_size(path: &Path) -> Result<u64> {
    #[cfg(windows)]
    {
        imp::allocated_size(path)
    }
    #[cfg(not(windows))]
    {
        Ok(std::fs::metadata(path)?.len())
    }
}

/// 文件是不是稀疏文件（查的是真实的属性位）。
pub fn is_sparse(path: &Path) -> Result<bool> {
    #[cfg(windows)]
    {
        imp::is_sparse(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(not(windows))]
fn create_sparse_fallback(path: &Path, length: u64) -> Result<()> {
    let handle = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("创建文件失败: {}", path.display()))?;
    handle
        .set_len(length)
        .with_context(|| format!("设置文件长度失败: {}", path.display()))?;
    Ok(())
}

// ── Windows 实现 ────────────────────────────────────────────────────────────

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::anyhow;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetCompressedFileSizeW, SetEndOfFile, SetFilePointerEx, FILE_BEGIN,
        FILE_ATTRIBUTE_SPARSE_FILE, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, OPEN_ALWAYS,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    /// 文件句柄的 RAII 包装。句柄漏了就是句柄泄漏。
    struct Handle(HANDLE);

    impl Handle {
        fn new(h: HANDLE) -> Result<Self> {
            if h == INVALID_HANDLE_VALUE || h.is_null() {
                return Err(anyhow!(
                    "打开文件失败: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self(h))
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// 路径转成宽字符（Windows 的 API 要 UTF-16）。
    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// 打开文件。读写权限都要——`FSCTL_SET_SPARSE` 需要写权限。
    fn open(path: &Path) -> Result<Handle> {
        let name = wide(path);
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | FILE_WRITE_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_ALWAYS,
                0,
                std::ptr::null_mut(),
            )
        };
        Handle::new(handle).with_context(|| format!("打开文件失败: {}", path.display()))
    }

    pub(super) fn create_sparse(path: &Path, length: u64) -> Result<()> {
        let handle = open(path)?;

        // 关键一步：显式发 FSCTL_SET_SPARSE。
        //
        // 光在 CreateFileW 里传 FILE_ATTRIBUTE_SPARSE_FILE 不管用——
        // 实测那样建出来的文件照样真占盘。必须走一次 DeviceIoControl。
        let ok = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(anyhow!(
                "设置稀疏属性失败: {}",
                std::io::Error::last_os_error()
            ));
        }

        // 定长度。先把指针移到末尾，再 SetEndOfFile——
        // 这样文件长度是 length，但中间是洞（hole），不占盘。
        let mut new_pos: i64 = length as i64;
        let ok = unsafe { SetFilePointerEx(handle.0, length as i64, &mut new_pos, FILE_BEGIN) };
        if ok == 0 {
            return Err(anyhow!(
                "移动文件指针失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        let ok = unsafe { SetEndOfFile(handle.0) };
        if ok == 0 {
            return Err(anyhow!(
                "设置文件长度失败: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(())
    }

    /// 文件实际占用的磁盘字节数。
    pub(super) fn allocated_size(path: &Path) -> Result<u64> {
        let name = wide(path);
        let mut high: u32 = 0;
        let low = unsafe { GetCompressedFileSizeW(name.as_ptr(), &mut high) };
        if low == u32::MAX && high == u32::MAX {
            return Err(anyhow!(
                "读取文件占用大小失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(((high as u64) << 32) | low as u64)
    }

    /// 文件有没有稀疏属性位。
    pub(super) fn is_sparse(path: &Path) -> Result<bool> {
        // std 在 Windows 上直接把文件属性位露出来了，
        // 不用再绕 GetFileAttributesExW。
        use std::os::windows::fs::MetadataExt;
        let meta = std::fs::metadata(path)?;
        Ok(meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "idm-shadow-sparse-{}-{}",
            tag,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("payload.bin")
    }

    #[test]
    fn sparse_file_has_full_length_but_no_disk_cost() {
        let path = temp_path("len");
        let _ = std::fs::remove_file(&path);

        // 1GB。普通建法会真的吃掉 1GB 磁盘。
        let length = 1024 * 1024 * 1024u64;
        create_sparse_file(&path, length).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), length, "逻辑长度应当是整个文件的长度");

        // 实际占用必须远小于逻辑长度。
        let allocated = allocated_size(&path).unwrap();
        assert!(
            allocated < length / 100,
            "实际占用应当接近 0，实际 {} 字节（逻辑 {}）",
            allocated,
            length
        );

        assert!(is_sparse(&path).unwrap(), "应当被识别为稀疏文件");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn writing_into_a_sparse_file_stays_sparse() {
        let path = temp_path("write");
        let _ = std::fs::remove_file(&path);

        create_sparse_file(&path, 64 * 1024 * 1024u64).unwrap();

        // 只写头 4KB，其余留洞。
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&[7u8; 4096]).unwrap();
        }

        let allocated = allocated_size(&path).unwrap();
        assert!(
            allocated < 1024 * 1024,
            "只写了 4KB，实际占用不该超过 1MB，实际 {} 字节",
            allocated
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn existing_longer_file_is_left_alone() {
        let path = temp_path("keep");
        let _ = std::fs::remove_file(&path);

        create_sparse_file(&path, 1024 * 1024).unwrap();
        // 写点东西进去，模拟断点续传。
        //
        // 注意不能用 `std::fs::write`——那会把文件**截断**成 11 字节，
        // 而断点续传的真实情况是文件还是满长度、只是前面有数据。
        // 这里只覆盖开头，文件长度保持不变。
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(b"already here").unwrap();
        }

        // 再调一次，不该把已有数据清掉。
        create_sparse_file(&path, 1024 * 1024).unwrap();
        let content = std::fs::read(&path).unwrap();
        assert_eq!(&content[..12], b"already here", "已有数据不该被清掉");
        assert_eq!(
            content.len(),
            1024 * 1024,
            "文件长度应当还是整个文件的长度"
        );

        let _ = std::fs::remove_file(&path);
    }
}
