use anyhow::Context as _;
use std::path::Path;
use std::path::PathBuf;
use tokio::process::Command;

/// Page where users can sign up to be notified when an official Linux build of
/// Codex Desktop becomes available from OpenAI.
const CODEX_LINUX_SIGNUP_URL: &str = "https://openai.com/form/codex-app/";

/// In-tree desktop wrapper built from this repository. Distributed as a
/// standalone binary called `codex-desktop` (or as a `Codex.AppImage` self-bundle).
const IN_TREE_DESKTOP_BINARY: &str = "codex-desktop";

pub async fn run_linux_app_open_or_install(
    workspace: PathBuf,
    download_url_override: Option<String>,
) -> anyhow::Result<()> {
    if let Some(launcher) = find_existing_codex_desktop_launcher() {
        eprintln!(
            "Opening Codex Desktop at {launcher}...",
            launcher = launcher.display()
        );
        return launch_codex_desktop(&launcher, &workspace).await;
    }

    if let Some(url) = download_url_override {
        eprintln!("Codex Desktop not found; downloading {url}...");
        let installed = download_and_install_appimage(&url)
            .await
            .context("failed to download/install Codex Desktop AppImage")?;
        eprintln!(
            "Launching Codex Desktop from {installed}...",
            installed = installed.display()
        );
        return launch_codex_desktop(&installed, &workspace).await;
    }

    print_linux_install_guidance(&workspace);
    Ok(())
}

fn print_linux_install_guidance(workspace: &Path) {
    eprintln!();
    eprintln!("Codex Desktop is not installed.");
    eprintln!();
    eprintln!("OpenAI does not currently publish an official Linux build of Codex");
    eprintln!("Desktop. To avoid relying on third-party redistributions, this");
    eprintln!("repository ships an in-tree desktop wrapper you build yourself.");
    eprintln!();
    eprintln!("On Ubuntu 24.04+ install it via:");
    eprintln!("    git clone <this repo> && cd codex-rs");
    eprintln!("    cargo build --release -p codex-desktop");
    eprintln!("    install -Dm755 target/release/codex-desktop \\");
    eprintln!("        ~/.local/bin/codex-desktop");
    eprintln!();
    eprintln!("Then re-run `codex app` and it will detect the binary in $PATH.");
    eprintln!();
    eprintln!("Alternatively, if you have a self-built AppImage you trust:");
    eprintln!("    codex app --download-url https://example.com/Codex.AppImage");
    eprintln!();
    eprintln!(
        "To be notified when an official Linux build is released, sign up at:"
    );
    eprintln!("    {url}", url = CODEX_LINUX_SIGNUP_URL);
    eprintln!();
    eprintln!(
        "Workspace requested: {workspace}",
        workspace = workspace.display()
    );
    eprintln!("(In the meantime, `codex` (the TUI) is fully supported on Linux.)");
    eprintln!();
}

/// Discover an installed Codex Desktop launcher on Linux.
///
/// Priority is the in-tree `codex-desktop` binary, then well-known install
/// paths, then user-side AppImage drops. We deliberately do not look in
/// directories that would be populated by third-party redistributions.
fn find_existing_codex_desktop_launcher() -> Option<PathBuf> {
    candidate_codex_desktop_launchers()
        .into_iter()
        .find(|candidate| is_executable_file(candidate))
}

fn candidate_codex_desktop_launchers() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();

    if let Some(from_path) = which(IN_TREE_DESKTOP_BINARY) {
        paths.push(from_path);
    }

    paths.push(PathBuf::from("/usr/local/bin/codex-desktop"));
    paths.push(PathBuf::from("/usr/bin/codex-desktop"));

    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        paths.push(home.join(".local").join("bin").join("codex-desktop"));
        paths.push(home.join(".cargo").join("bin").join("codex-desktop"));
        for dir in appimage_search_dirs(&home) {
            for name in ["Codex.AppImage", "codex.AppImage", "Codex-Desktop.AppImage"] {
                paths.push(dir.join(name));
            }
        }
    }

    paths
}

fn appimage_search_dirs(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("Applications"),
        home.join(".local").join("bin"),
        home.join(".local").join("share").join("applications"),
    ]
}

fn which(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path_var) {
        let candidate = entry.join(program);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    metadata.permissions().mode() & 0o111 != 0
}

async fn launch_codex_desktop(launcher: &Path, workspace: &Path) -> anyhow::Result<()> {
    eprintln!(
        "Opening workspace {workspace}...",
        workspace = workspace.display()
    );
    let status = Command::new(launcher)
        .arg(workspace)
        .status()
        .await
        .with_context(|| {
            format!(
                "failed to spawn {launcher}",
                launcher = launcher.display()
            )
        })?;

    if status.success() {
        return Ok(());
    }

    anyhow::bail!(
        "`{launcher} {workspace}` exited with {status}",
        launcher = launcher.display(),
        workspace = workspace.display()
    );
}

async fn download_and_install_appimage(url: &str) -> anyhow::Result<PathBuf> {
    if !looks_like_appimage_url(url) {
        anyhow::bail!(
            "--download-url must point to a .AppImage you trust; got {url}.\n\
             For .deb/.rpm packages, install them with your distro's package manager and re-run `codex app`."
        );
    }

    let dest_dir = appimage_install_dir().context("failed to determine AppImage install dir")?;
    std::fs::create_dir_all(&dest_dir).with_context(|| {
        format!(
            "failed to create install dir {dest_dir}",
            dest_dir = dest_dir.display()
        )
    })?;

    let dest = dest_dir.join("Codex.AppImage");
    eprintln!(
        "Downloading Codex Desktop AppImage to {dest}...",
        dest = dest.display()
    );
    download_with_curl(url, &dest).await?;
    set_executable(&dest)?;
    Ok(dest)
}

fn looks_like_appimage_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let trimmed = lower.split_once('?').map(|(left, _)| left).unwrap_or(&lower);
    trimmed.ends_with(".appimage")
}

fn appimage_install_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

async fn download_with_curl(url: &str, dest: &Path) -> anyhow::Result<()> {
    let status = Command::new("curl")
        .arg("-fL")
        .arg("--retry")
        .arg("3")
        .arg("--retry-delay")
        .arg("1")
        .arg("-o")
        .arg(dest)
        .arg(url)
        .status()
        .await
        .context("failed to invoke `curl`; install curl with `sudo apt install curl`")?;

    if status.success() {
        return Ok(());
    }
    anyhow::bail!("curl download failed with {status}");
}

fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {path}", path = path.display()))?
        .permissions();
    let mode = perms.mode();
    perms.set_mode(mode | 0o755);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("failed to chmod +x {path}", path = path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::looks_like_appimage_url;
    use pretty_assertions::assert_eq;

    #[test]
    fn appimage_url_detection_accepts_plain_url() {
        assert!(looks_like_appimage_url(
            "https://example.com/codex/Codex.AppImage"
        ));
    }

    #[test]
    fn appimage_url_detection_is_case_insensitive() {
        assert!(looks_like_appimage_url(
            "https://example.com/Codex.appimage"
        ));
    }

    #[test]
    fn appimage_url_detection_strips_query_string() {
        assert!(looks_like_appimage_url(
            "https://example.com/Codex.AppImage?token=abc"
        ));
    }

    #[test]
    fn appimage_url_detection_rejects_other_extensions() {
        assert_eq!(
            looks_like_appimage_url("https://example.com/codex/Codex.dmg"),
            false
        );
        assert_eq!(
            looks_like_appimage_url("https://example.com/codex/Codex.deb"),
            false
        );
    }
}
