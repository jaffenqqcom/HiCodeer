use std::path::Path;

#[cfg(target_env = "ohos")]
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use async_zip::base::read;
#[cfg(not(windows))]
use futures::AsyncSeek;
use futures::{AsyncRead, io::BufReader};
#[cfg(target_env = "ohos")]
use futures::{AsyncReadExt, StreamExt};

#[cfg(any(unix, windows))]
fn archive_path_is_normal(filename: &str) -> bool {
    Path::new(filename).components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

#[cfg(windows)]
pub async fn extract_zip<R: AsyncRead + Unpin>(destination: &Path, reader: R) -> Result<()> {
    let mut reader = read::stream::ZipFileReader::new(BufReader::new(reader));

    let destination = &destination
        .canonicalize()
        .unwrap_or_else(|_| destination.to_path_buf());

    while let Some(mut item) = reader.next_with_entry().await? {
        let entry_reader = item.reader_mut();
        let entry = entry_reader.entry();
        let filename = entry
            .filename()
            .as_str()
            .context("reading zip entry file name")?;

        if !archive_path_is_normal(filename) {
            reader = item.skip().await.context("reading next zip entry")?;
            continue;
        }

        let path = destination.join(filename);

        if entry
            .dir()
            .with_context(|| format!("reading zip entry metadata for path {path:?}"))?
        {
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating directory {path:?}"))?;
        } else {
            let parent_dir = path
                .parent()
                .with_context(|| format!("no parent directory for {path:?}"))?;
            std::fs::create_dir_all(parent_dir)
                .with_context(|| format!("creating parent directory {parent_dir:?}"))?;
            let mut file = smol::fs::File::create(&path)
                .await
                .with_context(|| format!("creating file {path:?}"))?;
            futures::io::copy(entry_reader, &mut file)
                .await
                .with_context(|| format!("extracting into file {path:?}"))?;
        }

        reader = item.skip().await.context("reading next zip entry")?;
    }

    Ok(())
}

#[cfg(unix)]
pub async fn extract_zip<R: AsyncRead + Unpin>(destination: &Path, reader: R) -> Result<()> {
    // Unix needs file permissions copied when extracting.
    // This is only possible to do when a reader impls `AsyncSeek` and `seek::ZipFileReader` is used.
    // `stream::ZipFileReader` also has the `unix_permissions` method, but it will always return `Some(0)`.
    //
    // A typical `reader` comes from a streaming network response, so cannot be sought right away,
    // and reading the entire archive into the memory seems wasteful.
    //
    // So, save the stream into a temporary file first and then get it read with a seeking reader.
    let mut file = async_fs::File::from(tempfile::tempfile().context("creating a temporary file")?);
    futures::io::copy(&mut BufReader::new(reader), &mut file)
        .await
        .context("saving archive contents into the temporary file")?;
    extract_seekable_zip(destination, file).await
}

#[cfg(unix)]
pub async fn extract_seekable_zip<R: AsyncRead + AsyncSeek + Unpin>(
    destination: &Path,
    reader: R,
) -> Result<()> {
    let mut reader = read::seek::ZipFileReader::new(BufReader::new(reader))
        .await
        .context("reading the zip archive")?;
    let destination = &destination
        .canonicalize()
        .unwrap_or_else(|_| destination.to_path_buf());
    for (i, entry) in reader.file().entries().to_vec().into_iter().enumerate() {
        let filename = entry
            .filename()
            .as_str()
            .context("reading zip entry file name")?;

        if !archive_path_is_normal(filename) {
            continue;
        }

        let path = destination.join(filename);

        if entry
            .dir()
            .with_context(|| format!("reading zip entry metadata for path {path:?}"))?
        {
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating directory {path:?}"))?;
        } else {
            let parent_dir = path
                .parent()
                .with_context(|| format!("no parent directory for {path:?}"))?;
            std::fs::create_dir_all(parent_dir)
                .with_context(|| format!("creating parent directory {parent_dir:?}"))?;
            let mut file = smol::fs::File::create(&path)
                .await
                .with_context(|| format!("creating file {path:?}"))?;
            let mut entry_reader = reader
                .reader_with_entry(i)
                .await
                .with_context(|| format!("reading entry for path {path:?}"))?;
            futures::io::copy(&mut entry_reader, &mut file)
                .await
                .with_context(|| format!("extracting into file {path:?}"))?;

            if let Some(perms) = entry.unix_permissions()
                && perms != 0o000
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = std::fs::Permissions::from_mode(u32::from(perms));
                file.set_permissions(permissions)
                    .await
                    .with_context(|| format!("setting permissions for file {path:?}"))?;
            }
        }
    }

    Ok(())
}

/// OHOS tar extraction with the sandbox link fallback.
///
/// The OHOS app sandbox denies symlink(2) and hard_link(2), so async-tar's
/// default unpack aborts as soon as a tar contains a link entry. This
/// extractor recovers denied link entries and materializes them as real
/// copies, so tarballs that legitimately contain links (node distribution,
/// LSP packages, ...) still extract successfully. Shared by every in-app tar
/// extraction path (github downloads, managed Node.js, ...).
#[cfg(target_env = "ohos")]
pub async fn unpack_tar_ohos<R>(
    archive: async_tar::Archive<R>,
    destination_path: &Path,
    url: &str,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    log::info!("unpack_tar_ohos: extracting {url} into {destination_path:?}");
    let mut entries = archive
        .entries()
        .with_context(|| format!("opening archive from {url}"))?;

    // Link entries whose creation the sandbox denied. They are materialized
    // as real copies after the rest of the archive has been unpacked, because
    // a link's target may appear later in the tar.
    let mut links = Vec::new();
    // Directories are deferred to the end, mirroring Archive::unpack, so that
    // directory permissions do not interfere with descendant extraction.
    let mut directories = Vec::new();

    while let Some(entry) = entries.next().await {
        let mut entry = entry.with_context(|| format!("iterating archive from {url}"))?;
        let entry_type = entry.header().entry_type();

        if entry_type.is_dir() {
            directories.push(entry);
            continue;
        }

        // Let async-tar unpack this entry. On OHOS the only expected failure
        // is a link whose creation the sandbox denied; intercept it here and
        // materialize it afterwards.
        match entry.unpack_in(destination_path).await {
            Ok(_) => {}
            Err(err) if entry_type.is_symlink() || entry_type.is_hard_link() => {
                // async-tar yields async_std paths here; convert them to std
                // paths for the materialization helpers below.
                let link_path =
                    PathBuf::from(entry.path().context("reading link path")?.as_os_str());
                let link_target = entry
                    .link_name()
                    .context("reading link target")?
                    .map(|target| PathBuf::from(target.as_os_str()));
                // Link entries carry no payload; consume any remaining bytes
                // so the entry stream advances to the next header.
                entry
                    .read_to_end(&mut Vec::new())
                    .await
                    .context("skipping link payload")?;
                log::debug!(
                    "unpack_tar_ohos: denied link {link_path:?} -> {link_target:?}: {err}"
                );
                links.push((entry_type, link_path, link_target));
                continue;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("extracting {url} to {destination_path:?}"));
            }
        }
    }

    for mut directory in directories {
        directory
            .unpack_in(destination_path)
            .await
            .with_context(|| format!("extracting {url} to {destination_path:?}"))?;
    }

    if !links.is_empty() {
        log::info!(
            "unpack_tar_ohos: materializing {} link(s) from {url}",
            links.len()
        );
    }
    for (entry_type, link_path, link_target) in links {
        materialize_link(destination_path, entry_type, &link_path, link_target.as_deref())
            .await
            .with_context(|| {
                format!("materializing link {link_path:?} while extracting {url}")
            })?;
    }

    Ok(())
}

#[cfg(target_env = "ohos")]
async fn materialize_link(
    destination_path: &Path,
    entry_type: async_tar::EntryType,
    link_path: &Path,
    link_target: Option<&Path>,
) -> Result<()> {
    let Some(link_target) = link_target else {
        log::warn!("materialize_link: link {link_path:?} has no target, skipping");
        return Ok(());
    };

    // Build the link destination from the entry path, dropping any `..`
    // components the same way async-tar's unpack_in does.
    let mut dest = destination_path.to_path_buf();
    for part in link_path.components() {
        match part {
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                log::warn!(
                    "materialize_link: skipping link {link_path:?} escaping the extraction root"
                );
                return Ok(());
            }
            std::path::Component::Normal(part) => dest.push(part),
        }
    }
    if dest == destination_path {
        log::warn!("materialize_link: skipping link {link_path:?} with empty destination");
        return Ok(());
    }

    // Symlink targets are relative to the link's parent directory, while
    // hard-link targets are relative to the archive root.
    let target_path = if entry_type.is_hard_link() {
        destination_path.join(link_target)
    } else {
        dest.parent().unwrap_or(destination_path).join(link_target)
    };

    let root_canon = match async_fs::canonicalize(destination_path).await {
        Ok(path) => path,
        Err(err) => {
            log::warn!("materialize_link: cannot canonicalize {destination_path:?}: {err}");
            return Ok(());
        }
    };
    let target_canon = match async_fs::canonicalize(&target_path).await {
        Ok(path) => path,
        // A dangling link is harmless: the original archive would have left a
        // symlink whose target does not exist either.
        Err(err) => {
            log::warn!(
                "materialize_link: link target {target_path:?} does not exist ({err}), skipping {link_path:?}"
            );
            return Ok(());
        }
    };
    if !target_canon.starts_with(&root_canon) {
        log::warn!(
            "materialize_link: link target {target_path:?} escapes the extraction root, skipping {link_path:?}"
        );
        return Ok(());
    }

    copy_recursively(&target_canon, &dest).await
}

#[cfg(target_env = "ohos")]
async fn copy_recursively(src: &Path, dst: &Path) -> Result<()> {
    // Iterative traversal: a recursive async fn would need boxing, and an
    // explicit stack is just as clear.
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        let metadata = async_fs::metadata(&src)
            .await
            .with_context(|| format!("reading metadata of {src:?}"))?;
        if metadata.is_dir() {
            async_fs::create_dir_all(&dst)
                .await
                .with_context(|| format!("creating directory {dst:?}"))?;
            let mut entries = async_fs::read_dir(&src)
                .await
                .with_context(|| format!("reading directory {src:?}"))?;
            while let Some(entry) = entries.next().await {
                let entry = entry.with_context(|| format!("reading entry in {src:?}"))?;
                stack.push((entry.path(), dst.join(entry.file_name())));
            }
        } else {
            if let Some(parent) = dst.parent() {
                async_fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("creating parent directory {parent:?}"))?;
            }
            async_fs::copy(&src, &dst)
                .await
                .with_context(|| format!("copying {src:?} to {dst:?}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use async_zip::ZipEntryBuilder;
    use async_zip::base::write::ZipFileWriter;
    use futures::{AsyncSeek, AsyncWriteExt};
    use smol::io::Cursor;
    use tempfile::TempDir;

    use super::*;

    #[allow(unused_variables)]
    async fn compress_zip(src_dir: &Path, dst: &Path, keep_file_permissions: bool) -> Result<()> {
        let mut out = smol::fs::File::create(dst).await?;
        let mut writer = ZipFileWriter::new(&mut out);

        for entry in walkdir::WalkDir::new(src_dir) {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                continue;
            }

            let relative_path = path.strip_prefix(src_dir)?;
            let data = smol::fs::read(&path).await?;

            let filename = relative_path.display().to_string();

            #[cfg(unix)]
            {
                let mut builder =
                    ZipEntryBuilder::new(filename.into(), async_zip::Compression::Deflate);
                use std::os::unix::fs::PermissionsExt;
                let metadata = std::fs::metadata(path)?;
                let perms = keep_file_permissions.then(|| metadata.permissions().mode() as u16);
                builder = builder.unix_permissions(perms.unwrap_or_default());
                writer.write_entry_whole(builder, &data).await?;
            }
            #[cfg(not(unix))]
            {
                let builder =
                    ZipEntryBuilder::new(filename.into(), async_zip::Compression::Deflate);
                writer.write_entry_whole(builder, &data).await?;
            }
        }

        writer.close().await?;
        out.flush().await?;
        out.sync_all().await?;

        Ok(())
    }

    #[track_caller]
    fn assert_file_content(path: &Path, content: &str) {
        assert!(path.exists(), "file not found: {:?}", path);
        let actual = std::fs::read_to_string(path).unwrap();
        assert_eq!(actual, content);
    }

    #[track_caller]
    fn make_test_data() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path();

        std::fs::write(dst.join("test"), "Hello world.").unwrap();
        std::fs::create_dir_all(dst.join("foo/bar")).unwrap();
        std::fs::write(dst.join("foo/bar.txt"), "Foo bar.").unwrap();
        std::fs::write(dst.join("foo/dar.md"), "Bar dar.").unwrap();
        std::fs::write(dst.join("foo/bar/dar你好.txt"), "你好世界").unwrap();

        dir
    }

    async fn read_archive(path: &Path) -> impl AsyncRead + AsyncSeek + Unpin {
        let data = smol::fs::read(&path).await.unwrap();
        Cursor::new(data)
    }

    #[test]
    fn test_extract_zip() {
        let test_dir = make_test_data();
        let zip_file = test_dir.path().join("test.zip");

        smol::block_on(async {
            compress_zip(test_dir.path(), &zip_file, true)
                .await
                .unwrap();
            let reader = read_archive(&zip_file).await;

            let dir = tempfile::tempdir().unwrap();
            let dst = dir.path();
            extract_zip(dst, reader).await.unwrap();

            assert_file_content(&dst.join("test"), "Hello world.");
            assert_file_content(&dst.join("foo/bar.txt"), "Foo bar.");
            assert_file_content(&dst.join("foo/dar.md"), "Bar dar.");
            assert_file_content(&dst.join("foo/bar/dar你好.txt"), "你好世界");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_zip_preserves_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        smol::block_on(async {
            let test_dir = tempfile::tempdir().unwrap();
            let executable_path = test_dir.path().join("my_script");

            // Create an executable file
            std::fs::write(&executable_path, "#!/bin/bash\necho 'Hello'").unwrap();
            let mut perms = std::fs::metadata(&executable_path).unwrap().permissions();
            perms.set_mode(0o755); // rwxr-xr-x
            std::fs::set_permissions(&executable_path, perms).unwrap();

            // Create zip
            let zip_file = test_dir.path().join("test.zip");
            compress_zip(test_dir.path(), &zip_file, true)
                .await
                .unwrap();

            // Extract to new location
            let extract_dir = tempfile::tempdir().unwrap();
            let reader = read_archive(&zip_file).await;
            extract_zip(extract_dir.path(), reader).await.unwrap();

            // Check permissions are preserved
            let extracted_path = extract_dir.path().join("my_script");
            assert!(extracted_path.exists());
            let extracted_perms = std::fs::metadata(&extracted_path).unwrap().permissions();
            assert_eq!(extracted_perms.mode() & 0o777, 0o755);
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_zip_sets_default_permissions() {
        use std::os::unix::fs::PermissionsExt;

        smol::block_on(async {
            let test_dir = tempfile::tempdir().unwrap();
            let file_path = test_dir.path().join("my_script");

            std::fs::write(&file_path, "#!/bin/bash\necho 'Hello'").unwrap();
            // The permissions will be shaped by the umask in the test environment
            let original_perms = std::fs::metadata(&file_path).unwrap().permissions();

            // Create zip
            let zip_file = test_dir.path().join("test.zip");
            compress_zip(test_dir.path(), &zip_file, false)
                .await
                .unwrap();

            // Extract to new location
            let extract_dir = tempfile::tempdir().unwrap();
            let reader = read_archive(&zip_file).await;
            extract_zip(extract_dir.path(), reader).await.unwrap();

            // Permissions were not stored, so will be whatever the umask generates
            // by default for new files. This should match what we saw when we previously wrote
            // the file.
            let extracted_path = extract_dir.path().join("my_script");
            assert!(extracted_path.exists());
            let extracted_perms = std::fs::metadata(&extracted_path).unwrap().permissions();
            assert_eq!(
                extracted_perms.mode(),
                original_perms.mode(),
                "Expected matching Unix file mode for unzipped file without keep_file_permissions"
            );
            assert_eq!(
                extracted_perms, original_perms,
                "Expected default set of permissions for unzipped file without keep_file_permissions"
            );
        });
    }

    #[test]
    fn test_archive_path_is_normal_rejects_traversal() {
        assert!(!archive_path_is_normal("../parent.txt"));
        assert!(!archive_path_is_normal("foo/../../grandparent.txt"));
        assert!(!archive_path_is_normal("/tmp/absolute.txt"));

        assert!(archive_path_is_normal("foo/bar.txt"));
        assert!(archive_path_is_normal("foo/bar/baz.txt"));
        assert!(archive_path_is_normal("./foo/bar.txt"));
        assert!(archive_path_is_normal("normal.txt"));
    }

    async fn build_zip_with_entries(entries: &[(&str, &[u8])]) -> Cursor<Vec<u8>> {
        let mut buf = Cursor::new(Vec::new());
        let mut writer = ZipFileWriter::new(&mut buf);
        for (name, data) in entries {
            let builder = ZipEntryBuilder::new((*name).into(), async_zip::Compression::Stored);
            writer.write_entry_whole(builder, data).await.unwrap();
        }
        writer.close().await.unwrap();
        buf.set_position(0);
        buf
    }

    #[test]
    fn test_extract_zip_skips_path_traversal_entries() {
        smol::block_on(async {
            let base_dir = tempfile::tempdir().unwrap();
            let extract_dir = base_dir.path().join("subdir");
            std::fs::create_dir_all(&extract_dir).unwrap();

            let absolute_target = base_dir.path().join("absolute.txt");
            let reader = build_zip_with_entries(&[
                ("normal.txt", b"normal file"),
                ("subdir/nested.txt", b"nested file"),
                ("../parent.txt", b"parent file"),
                ("foo/../../grandparent.txt", b"grandparent file"),
                (absolute_target.to_str().unwrap(), b"absolute file"),
            ])
            .await;

            extract_zip(&extract_dir, reader).await.unwrap();

            assert_file_content(&extract_dir.join("normal.txt"), "normal file");
            assert_file_content(&extract_dir.join("subdir/nested.txt"), "nested file");

            assert!(
                !base_dir.path().join("parent.txt").exists(),
                "parent traversal entry should have been skipped"
            );
            assert!(
                !base_dir.path().join("grandparent.txt").exists(),
                "nested traversal entry should have been skipped"
            );
            assert!(
                !absolute_target.exists(),
                "absolute path entry should have been skipped"
            );
        });
    }
}
