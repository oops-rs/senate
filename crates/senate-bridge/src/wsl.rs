//! Windows through WSL 2: Senate runs inside the Linux distro as it does on
//! any Linux machine. Three things differ, and `senate setup` handles them
//! here:
//!
//! - WSL appends the Windows `PATH` (`/mnt/c/…`) to the distro's own, so a
//!   bare `claude` or `codex` can resolve to a Windows program (an npm
//!   wrapper that runs Windows Node, or a `.exe`). The daemon must start
//!   the Linux ones, so setup leaves Windows directories out of the `PATH`
//!   it looks programs up on and writes into the service.
//! - WSL stops the distro once nothing is attached to it, which takes the
//!   daemon with it. Setup cannot keep it running from inside, so it says
//!   what to do on the Windows side.
//! - Repositories under `/mnt/c` are slow, and file locking there is not
//!   the same as on a Linux filesystem: they belong in the distro's own
//!   filesystem.

use std::path::Path;

use senate_core::Locale;

/// The WSL distro this process runs in, if it runs in one: the name WSL
/// gives it (`WSL_DISTRO_NAME`), else `WSL` when only the kernel says so.
pub fn distro() -> Option<String> {
    distro_from(
        std::env::var("WSL_DISTRO_NAME").ok().as_deref(),
        std::fs::read_to_string("/proc/version").ok().as_deref(),
    )
}

fn distro_from(name: Option<&str>, proc_version: Option<&str>) -> Option<String> {
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        return Some(name.to_owned());
    }
    // WSL 2 kernels are "…-microsoft-standard-WSL2", WSL 1 "…-Microsoft".
    proc_version
        .is_some_and(|v| v.to_ascii_lowercase().contains("microsoft"))
        .then(|| "WSL".to_owned())
}

/// Whether `path` is on a Windows drive as WSL mounts it (`/mnt/c/…`).
pub fn is_windows_path(path: &Path) -> bool {
    let mut parts = path.components().map(|c| c.as_os_str().to_string_lossy());
    parts.next().as_deref() == Some("/")
        && parts.next().as_deref() == Some("mnt")
        && parts
            .next()
            .is_some_and(|drive| drive.len() == 1 && drive.chars().all(|c| c.is_ascii_alphabetic()))
}

/// `path` (a `PATH` value) without its Windows directories.
pub fn linux_path(path: &str) -> String {
    let dirs: Vec<_> = std::env::split_paths(path)
        .filter(|dir| !is_windows_path(dir))
        .collect();
    std::env::join_paths(dirs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

/// The `PATH` setup uses: in WSL, without its Windows directories.
pub fn setup_path(path: Option<String>) -> Option<String> {
    match (path, distro()) {
        (Some(path), Some(_)) => Some(linux_path(&path)),
        (path, _) => path,
    }
}

/// What `senate setup` cannot do from inside the distro, said once.
pub fn notes(distro: &str, l: Locale) -> String {
    l.pick(
        format!(
            "\n在 WSL（{distro}）中运行。还需要你自己完成：\n\
             - 把仓库放在 WSL 的文件系统里（例如 ~/src），不要放在 /mnt/c 下。\n\
             - 在 WSL 里安装 Linux 版的 Claude Code 和 Codex；/mnt/c 下的 Windows 版本不会被使用。\n\
             - 守护进程：/etc/wsl.conf 里设 [boot] systemd=true，运行 `sudo loginctl enable-linger $USER`，\n\
             \x20 再运行 `senate setup service install --yes`。\n\
             - 最后一个 WSL 终端关闭后，WSL 会停止发行版，守护进程也随之停止。要让它一直运行，\n\
             \x20 在 Windows 上建一个登录时运行 `wsl.exe -d {distro} --exec sleep infinity` 的计划任务\n\
             \x20 （见 README 的 “Windows (WSL)” 一节）。开着 Docker Desktop 的 WSL 集成时，发行版本来就一直运行。\n"
        ),
        format!(
            "\nRunning in WSL ({distro}). Steps left for you:\n\
             - Keep repositories on the WSL filesystem (e.g. ~/src), not under /mnt/c.\n\
             - Install the Linux Claude Code and Codex inside WSL; Windows ones under /mnt/c are not used.\n\
             - The daemon: set [boot] systemd=true in /etc/wsl.conf, run `sudo loginctl enable-linger $USER`,\n\
             \x20 then `senate setup service install --yes`.\n\
             - WSL stops the distro, and the daemon with it, after its last terminal closes. To keep it\n\
             \x20 running, add a Windows scheduled task that runs `wsl.exe -d {distro} --exec sleep infinity`\n\
             \x20 at logon (see \"Windows (WSL)\" in the README). With Docker Desktop's WSL integration on,\n\
             \x20 the distro stays up anyway.\n"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_distro_comes_from_its_name_or_the_kernel() {
        assert_eq!(distro_from(Some("Ubuntu"), None).as_deref(), Some("Ubuntu"));
        let wsl2 = "Linux version 5.15.153.1-microsoft-standard-WSL2 (root@x) #1 SMP";
        assert_eq!(distro_from(Some(""), Some(wsl2)).as_deref(), Some("WSL"));
        assert_eq!(
            distro_from(None, Some("Linux version 6.8.0-45-generic")),
            None
        );
        assert_eq!(distro_from(None, None), None);
    }

    #[test]
    fn windows_drives_are_mnt_and_one_letter() {
        assert!(is_windows_path(Path::new(
            "/mnt/c/Users/me/AppData/Roaming/npm/claude"
        )));
        assert!(is_windows_path(Path::new("/mnt/D")));
        assert!(!is_windows_path(Path::new("/mnt/wsl/docker-desktop")));
        assert!(!is_windows_path(Path::new("/mnt")));
        assert!(!is_windows_path(Path::new("/home/me/.local/bin")));
        assert!(!is_windows_path(Path::new("mnt/c/x")));
    }

    #[test]
    fn the_linux_path_keeps_order_and_drops_windows_directories() {
        let path = "/home/me/.local/bin:/usr/bin:/mnt/c/WINDOWS/system32:\
                    /mnt/c/Program Files/nodejs/:/usr/lib/wsl/lib:/snap/bin";
        assert_eq!(
            linux_path(path),
            "/home/me/.local/bin:/usr/bin:/usr/lib/wsl/lib:/snap/bin"
        );
    }
}
