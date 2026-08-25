use crate::ResultExt;
use anyhow::{Result, bail};
use async_fs as fs;
use futures_lite::StreamExt;
use std::path::{Path, PathBuf};

/// Removes all files and directories matching the given predicate
pub async fn remove_matching<F>(dir: &Path, predicate: F)
where
    F: Fn(&Path) -> bool,
{
    if let Some(mut entries) = fs::read_dir(dir).await.log_err() {
        while let Some(entry) = entries.next().await {
            if let Some(entry) = entry.log_err() {
                let entry_path = entry.path();
                if predicate(entry_path.as_path())
                    && let Ok(metadata) = fs::metadata(&entry_path).await
                {
                    if metadata.is_file() {
                        fs::remove_file(&entry_path).await.log_err();
                    } else {
                        fs::remove_dir_all(&entry_path).await.log_err();
                    }
                }
            }
        }
    }
}

pub async fn collect_matching<F>(dir: &Path, predicate: F) -> Vec<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    let mut matching = vec![];

    if let Some(mut entries) = fs::read_dir(dir).await.log_err() {
        while let Some(entry) = entries.next().await {
            if let Some(entry) = entry.log_err()
                && predicate(entry.path().as_path())
            {
                matching.push(entry.path());
            }
        }
    }

    matching
}

pub async fn find_file_name_in_dir<F>(dir: &Path, predicate: F) -> Option<PathBuf>
where
    F: Fn(&str) -> bool,
{
    if let Some(mut entries) = fs::read_dir(dir).await.log_err() {
        while let Some(entry) = entries.next().await {
            if let Some(entry) = entry.log_err() {
                let entry_path = entry.path();

                if let Some(file_name) = entry_path
                    .file_name()
                    .map(|file_name| file_name.to_string_lossy())
                    && predicate(&file_name)
                {
                    return Some(entry_path);
                }
            }
        }
    }

    None
}

pub async fn move_folder_files_to_folder<P: AsRef<Path>>(
    source_path: P,
    target_path: P,
) -> Result<()> {
    if !target_path.as_ref().is_dir() {
        bail!("Folder not found or is not a directory");
    }

    let mut entries = fs::read_dir(source_path.as_ref()).await?;
    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let old_path = entry.path();
        let new_path = target_path.as_ref().join(entry.file_name());

        fs::rename(&old_path, &new_path).await?;
    }

    fs::remove_dir(source_path).await?;

    Ok(())
}

#[cfg(all(unix, not(target_env = "ohos")))]
/// Set the permissions for the given path so that the file becomes executable.
/// This is a noop for non-unix platforms.
pub async fn make_file_executable(path: &Path) -> std::io::Result<()> {
    fs::set_permissions(
        path,
        <fs::Permissions as fs::unix::PermissionsExt>::from_mode(0o755),
    )
    .await
}

#[cfg(target_env = "ohos")]
/// Set the permissions for the given path so that the file becomes executable.
/// On OHOS the file lives on the VM (commands run there), so the chmod is
/// executed on the VM through the cmd-agent.
pub async fn make_file_executable(path: &Path) -> std::io::Result<()> {
    let output = crate::command::new_command("chmod")
        .arg("+x")
        .arg(path)
        .output()
        .await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "chmod +x failed on the VM: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        ))
    }
}

#[cfg(not(unix))]
#[allow(clippy::unused_async)]
/// Set the permissions for the given path so that the file becomes executable.
/// This is a noop for non-unix platforms.
pub async fn make_file_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
