//! Landlock 实现：把子进程的可写范围钉死在白名单目录里。
//!
//! 设计要点（都是踩过坑才会知道的细节）：
//!
//! 1. **只在父进程里构造 ruleset**。`landlock_create_ruleset` / `add_rule` 会分配
//!    内存，而 `pre_exec` 是在 `fork` 之后、多线程进程里执行的，那里只允许
//!    async-signal-safe 调用（malloc 可能死锁）。所以规则在父进程建好，子进程里
//!    只调两个裸系统调用：`prctl(NO_NEW_PRIVS)` + `landlock_restrict_self`。
//! 2. **只接管写类权限**。读权限不接管 —— 否则 `sh`、`rustc`、动态库加载会全面
//!    失败。效果是"读随便读，写只能写白名单"。
//! 3. **ABI 决定可用的权限位**。往老内核传它不认识的位会直接 EINVAL，所以
//!    `REFER`(v2) / `TRUNCATE`(v3) 按查询到的版本有条件地加。
//! 4. **`/dev/null` 与 TMPDIR 必须额外放行**，否则 `> /dev/null`、`git`、`rustc`
//!    会因为临时目录不可写而报一堆莫名其妙的错。
//!
//! 边界：Landlock 只管文件系统（ABI v4 起还管 TCP 端口），不等于容器；进程仍可
//! 联网、仍可 fork。它解决的是"模型手滑写坏系统文件"，不是"恶意代码提权"。

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub use supported::LandlockSandbox;

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub use unsupported::LandlockSandbox;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod supported {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};

    use tokio::process::Command;

    use crate::sandbox::{Sandbox, SandboxError};

    /// `landlock_create_ruleset` 的 `flags`：只查询 ABI 版本。
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    /// 规则类型：把权限授予某棵目录树。
    const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

    // ---- 文件系统权限位（uapi/linux/landlock.h） ---------------------------------
    const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
    const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
    const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
    const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
    const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
    const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
    const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
    const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
    const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
    const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
    /// ABI v2 起支持（跨目录 rename/link）。
    const ACCESS_FS_REFER: u64 = 1 << 13;
    /// ABI v3 起支持（`truncate(2)`、`O_TRUNC`）。
    const ACCESS_FS_TRUNCATE: u64 = 1 << 14;

    /// 只传 `handled_access_fs` 这 8 字节：内核要求 size 落在
    /// `[8, sizeof(内核结构)]` 区间且多余位为 0，传最小尺寸最兼容。
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

    #[repr(C)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// 内核报告的 Landlock ABI 版本；不支持时返回 `None`。
    fn abi_version() -> Option<u32> {
        // SAFETY: attr 传 NULL/size 0 是查询版本的既定用法，不读用户内存。
        let ret = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        (ret >= 1).then_some(ret as u32)
    }

    /// 写类权限全集，按 ABI 逐级放开。
    fn write_rights(abi: u32) -> u64 {
        let mut rights = ACCESS_FS_WRITE_FILE
            | ACCESS_FS_REMOVE_DIR
            | ACCESS_FS_REMOVE_FILE
            | ACCESS_FS_MAKE_CHAR
            | ACCESS_FS_MAKE_DIR
            | ACCESS_FS_MAKE_REG
            | ACCESS_FS_MAKE_SOCK
            | ACCESS_FS_MAKE_FIFO
            | ACCESS_FS_MAKE_BLOCK
            | ACCESS_FS_MAKE_SYM;
        if abi >= 2 {
            rights |= ACCESS_FS_REFER;
        }
        if abi >= 3 {
            rights |= ACCESS_FS_TRUNCATE;
        }
        rights
    }

    /// 单个文件（如 `/dev/null`）可用的写权限。
    fn file_write_rights(abi: u32) -> u64 {
        let mut rights = ACCESS_FS_WRITE_FILE;
        if abi >= 3 {
            rights |= ACCESS_FS_TRUNCATE;
        }
        rights
    }

    fn create_ruleset(handled: u64) -> io::Result<OwnedFd> {
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        // SAFETY: attr 是栈上按 C 布局初始化的结构，长度如实传入。
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 刚刚由内核返回的、我们独占的文件描述符。
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    fn add_rule(ruleset: &OwnedFd, path: &Path, allowed: u64) -> io::Result<()> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径含 NUL 字节"))?;
        // SAFETY: 只读地打开目录自身（O_PATH 不触发读权限判定）。
        let dir = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if dir < 0 {
            return Err(io::Error::last_os_error());
        }

        let attr = PathBeneathAttr {
            allowed_access: allowed,
            parent_fd: dir,
        };
        // SAFETY: attr 布局与内核结构一致（u64 + s32，尾部补齐），dir 是本进程持有的有效 fd。
        let ret = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                LANDLOCK_RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        let failure = (ret < 0).then(io::Error::last_os_error);
        // SAFETY: dir 由本函数独占，未被别处接管。
        unsafe { libc::close(dir) };
        match failure {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    #[derive(Debug)]
    pub struct LandlockSandbox {
        ruleset: OwnedFd,
        abi: u32,
        writable: Vec<PathBuf>,
    }

    impl LandlockSandbox {
        /// 以 `root` 为唯一可写根建造规则集。不支持 Landlock 的内核直接报错
        /// （故障关闭：调用方要么显式选 `none`，要么就别跑）。
        pub fn new(root: &Path) -> Result<Self, SandboxError> {
            let abi = abi_version().ok_or_else(|| {
                SandboxError::Unavailable(
                    "内核不支持 Landlock（需要 Linux 5.13+，且未在启动参数里禁用）".into(),
                )
            })?;
            let handled = write_rights(abi);
            let ruleset = create_ruleset(handled).map_err(|err| {
                SandboxError::Unavailable(format!("创建 Landlock ruleset 失败：{err}"))
            })?;

            add_rule(&ruleset, root, handled).map_err(|err| {
                SandboxError::Unavailable(format!("规则集拒绝工作目录 {}：{err}", root.display()))
            })?;
            let mut writable = vec![root.to_path_buf()];

            // 临时目录：构建工具链要写，不给会变成一堆难以定位的失败。
            let tmp = std::env::temp_dir();
            if tmp != root && add_rule(&ruleset, &tmp, handled).is_ok() {
                writable.push(tmp);
            }

            // `> /dev/null` 是最常见的重定向，必须放行。
            let dev_null = Path::new("/dev/null");
            let _ = add_rule(&ruleset, dev_null, file_write_rights(abi));

            Ok(Self {
                ruleset,
                abi,
                writable,
            })
        }

        pub fn abi(&self) -> u32 {
            self.abi
        }

        pub fn writable_roots(&self) -> &[PathBuf] {
            &self.writable
        }
    }

    impl Sandbox for LandlockSandbox {
        fn name(&self) -> &'static str {
            "landlock"
        }

        fn describe(&self) -> String {
            let roots: Vec<String> = self
                .writable
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            format!("landlock(abi=v{}, 可写={})", self.abi, roots.join(":"))
        }

        fn wrap(&self, cmd: &mut Command) -> Result<(), SandboxError> {
            let fd = self.ruleset.as_raw_fd();
            // SAFETY: 闭包在 fork 后的子进程里运行，只做两个不分配内存的系统调用：
            // prctl(PR_SET_NO_NEW_PRIVS) 是 landlock_restrict_self 的前置条件。
            // 失败时返回 io::Error，spawn 会直接失败 —— 正是我们要的故障关闭。
            unsafe {
                cmd.as_std_mut().pre_exec(move || {
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn abi_probe_is_stable() {
            // 老内核返回 None 也是合法结果，这里只要求不 panic。
            if let Some(abi) = abi_version() {
                assert!(abi >= 1);
            }
        }

        /// 需要内核支持 Landlock 才有意义；不支持时直接跳过。
        #[tokio::test]
        async fn writes_outside_the_root_are_denied() {
            if abi_version().is_none() {
                eprintln!("跳过：本机内核不支持 Landlock");
                return;
            }

            let root = std::env::temp_dir().join(format!("dsh-ll-root-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();

            // 越界目标必须落在 /tmp 之外 —— 临时目录本身是放行的。
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/root"));
            if home.starts_with(std::env::temp_dir()) {
                eprintln!("跳过：HOME 就在临时目录下，无法构造越界样本");
                return;
            }
            let outside = home.join(format!("dsh-ll-out-{}", std::process::id()));
            let _ = std::fs::remove_file(&outside);

            let sandbox = LandlockSandbox::new(&root).unwrap();

            // 根内写入应当成功。
            let mut inside_cmd = Command::new("sh");
            inside_cmd
                .args(["-c", "echo ok > inside.txt"])
                .current_dir(&root);
            sandbox.wrap(&mut inside_cmd).unwrap();
            assert!(
                inside_cmd
                    .output()
                    .await
                    .expect("spawn inside")
                    .status
                    .success(),
                "根内写入被误拒"
            );

            // 根外写入应当失败。
            let mut outside_cmd = Command::new("sh");
            outside_cmd
                .args(["-c", &format!("echo bad > {}", outside.display())])
                .current_dir(&root);
            sandbox.wrap(&mut outside_cmd).unwrap();
            let outside_status = outside_cmd.output().await.expect("spawn outside").status;
            assert!(!outside_status.success(), "根外写入没有被拦住");
            assert!(!outside.exists(), "越界文件竟然真的被写出来了");

            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_file(&outside);
        }
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod unsupported {
    use std::path::{Path, PathBuf};

    use tokio::process::Command;

    use crate::sandbox::{Sandbox, SandboxError};

    /// 非 Linux / 非 64 位架构上的占位实现：构造必然失败。
    #[derive(Debug)]
    pub struct LandlockSandbox {
        _private: (),
    }

    impl LandlockSandbox {
        pub fn new(_root: &Path) -> Result<Self, SandboxError> {
            Err(SandboxError::Unavailable(format!(
                "Landlock 只在 Linux x86_64/aarch64 上可用（当前 {} / {}）",
                std::env::consts::OS,
                std::env::consts::ARCH
            )))
        }

        pub fn abi(&self) -> u32 {
            0
        }

        pub fn writable_roots(&self) -> &[PathBuf] {
            &[]
        }
    }

    impl Sandbox for LandlockSandbox {
        fn name(&self) -> &'static str {
            "landlock"
        }

        fn wrap(&self, _cmd: &mut Command) -> Result<(), SandboxError> {
            Err(SandboxError::Unavailable("当前平台没有 Landlock".into()))
        }
    }
}
