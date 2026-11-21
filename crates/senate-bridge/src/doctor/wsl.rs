//! What differs in WSL ([`crate::wsl`]): the distro needs systemd and
//! linger for the daemon to keep running, and nothing Senate runs may be
//! a Windows program under `/mnt`.

use std::path::PathBuf;

use senate_core::Locale;

use super::Check;
use crate::service;

/// The distro this runs in, and where its system files are (`/`, or a
/// fake root in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wsl {
    pub distro: String,
    pub user: String,
    pub root: PathBuf,
    /// The directory `doctor` runs in.
    pub cwd: PathBuf,
}

impl Wsl {
    /// This process's distro, if it runs in one.
    pub fn here() -> Option<Self> {
        Some(Self {
            distro: crate::wsl::distro()?,
            user: std::env::var("USER").unwrap_or_default(),
            root: PathBuf::from("/"),
            cwd: std::env::current_dir().unwrap_or_default(),
        })
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}

pub fn checks(wsl: &Wsl, spec: Option<&service::Spec>, l: Locale) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(if wsl.at("/run/systemd/system").is_dir() {
        Check::ok("wsl.systemd", l.pick("systemd 在运行", "systemd is running"))
    } else {
        Check::fail(
            "wsl.systemd",
            l.pick(
                format!(
                    "{} 中 systemd 没有运行：在 /etc/wsl.conf 里设 [boot] systemd=true，再在 Windows 上运行 wsl.exe --shutdown",
                    wsl.distro
                ),
                format!(
                    "systemd is not running in {}: set [boot] systemd=true in /etc/wsl.conf, then run wsl.exe --shutdown on Windows",
                    wsl.distro
                ),
            ),
            None::<String>,
        )
    });
    let linger = wsl.at("/var/lib/systemd/linger").join(&wsl.user);
    checks.push(if linger.exists() {
        Check::ok("wsl.linger", l.pick("已开启 linger", "linger is on"))
    } else {
        Check::warn(
            "wsl.linger",
            l.pick(
                format!("{} 没有开启 linger：退出登录后守护进程会停止", wsl.user),
                format!(
                    "linger is off for {}: the daemon stops when you log out",
                    wsl.user
                ),
            ),
            Some(format!("sudo loginctl enable-linger {}", wsl.user)),
        )
    });
    if let Some(spec) = spec
        && let Ok(text) = std::fs::read_to_string(spec.file())
        && let Some(path) = service::path_in_file(spec.platform, &text)
    {
        let windows: Vec<PathBuf> = std::env::split_paths(&path)
            .filter(|d| crate::wsl::is_windows_path(d))
            .collect();
        checks.push(match windows.first() {
            None => Check::ok(
                "wsl.service-path",
                l.pick(
                    "服务的 PATH 中没有 Windows 目录",
                    "the service's PATH has no Windows directories",
                ),
            ),
            Some(first) => Check::fail(
                "wsl.service-path",
                l.pick(
                    format!(
                        "服务的 PATH 含有 {} 个 Windows 目录（例如 {}）：可能启动 Windows 版的 claude 或 codex",
                        windows.len(),
                        first.display()
                    ),
                    format!(
                        "the service's PATH has {} Windows directories (e.g. {}): it may start a Windows claude or codex",
                        windows.len(),
                        first.display()
                    ),
                ),
                Some(super::SERVICE_INSTALL),
            ),
        });
    }
    if crate::wsl::is_windows_path(&wsl.cwd) {
        checks.push(Check::warn(
            "wsl.repository",
            l.pick(
                format!(
                    "{} 在 Windows 盘上：放到 WSL 的文件系统里（例如 ~/src）更快也更可靠",
                    wsl.cwd.display()
                ),
                format!(
                    "{} is on a Windows drive: on the WSL filesystem (e.g. ~/src) it is faster and locks reliably",
                    wsl.cwd.display()
                ),
            ),
            None::<String>,
        ));
    }
    checks
}

#[cfg(test)]
pub(super) fn fake(root: &std::path::Path) -> Wsl {
    Wsl {
        distro: "Ubuntu".into(),
        user: "alice".into(),
        root: root.to_owned(),
        cwd: root.join("home/alice/src"),
    }
}
