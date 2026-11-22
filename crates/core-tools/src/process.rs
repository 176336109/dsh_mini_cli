//! 子进程清理：工具超时被丢弃时，把整个进程组一起带走。
//!
//! 只杀直接子进程不够：`sh -c 'sleep 100 &'` 的孙进程会活下来，继续占着管道和
//! CPU。这里用"进程组跟着 guard 一起死"的办法兜住 —— 调用方先让子进程自成一个
//! 进程组（`Command::process_group(0)`），再把 pid 交给 [`ProcessGroupGuard`]。
//!
//! 注意语义边界：guard 只在**被丢弃**（超时 / 取消）时动手。正常等待结束后调用
//! [`ProcessGroupGuard::disarm`]，这样 `bash` 里故意启动的后台服务不会被误杀。

/// 平台无关的句柄。Windows 上没有进程组等价物（需要 Job Object），退化为无操作。
pub type ProcessGroupGuard = platform::Guard;

/// 让子进程自成一个进程组，且组 id 等于它的 pid。
///
/// 必须在 spawn 之前调用。Windows 上无等价语义，直接返回。
#[cfg(unix)]
pub fn isolate_process_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}

#[cfg(not(unix))]
pub fn isolate_process_group(_cmd: &mut tokio::process::Command) {}

/// 为刚 spawn 的子进程装上"丢弃即杀组"的保险。
pub fn guard_process_group(pid: Option<u32>) -> ProcessGroupGuard {
    platform::Guard::new(pid)
}

#[cfg(unix)]
mod platform {
    use std::fmt;

    /// Drop 时对整个进程组发 SIGKILL。
    pub struct Guard {
        pid: i32,
        armed: bool,
    }

    impl Guard {
        pub fn new(pid: Option<u32>) -> Self {
            Self {
                pid: pid.map(|pid| pid as i32).unwrap_or(0),
                armed: true,
            }
        }

        /// 正常收尾后解除武装，避免误杀用户故意留在后台的进程。
        pub fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl fmt::Debug for Guard {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ProcessGroupGuard")
                .field("pid", &self.pid)
                .field("armed", &self.armed)
                .finish()
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            // pid <= 1 说明拿不到有效 pid：宁可不杀，也不能误杀 1 号进程或当前组。
            if !self.armed || self.pid <= 1 {
                return;
            }
            // SAFETY: kill(2) 只读 pid 参数；负值表示"进程组"。失败（ESRCH 等）无害，
            // 这里也不关心返回值 —— Drop 里不能 panic。
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(not(unix))]
mod platform {
    use std::fmt;

    /// 非 unix 平台：直接子进程由 `kill_on_drop` 负责，这里不做额外动作。
    pub struct Guard {
        _private: (),
    }

    impl Guard {
        pub fn new(_pid: Option<u32>) -> Self {
            Self { _private: () }
        }

        pub fn disarm(&mut self) {}
    }

    impl fmt::Debug for Guard {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ProcessGroupGuard").finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_without_pid_is_a_no_op() {
        let mut guard = guard_process_group(None);
        guard.disarm();
    }

    #[test]
    fn disarm_prevents_the_kill() {
        // pid 0 不是有效进程组，而且解除了武装：这里只验证不 panic、不误杀。
        let mut guard = guard_process_group(Some(0));
        guard.disarm();
        // 作用域结束时自然 Drop。
    }
}
