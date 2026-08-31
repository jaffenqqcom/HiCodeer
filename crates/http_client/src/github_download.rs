use std::{
    path::{Path, PathBuf},
    pin::Pin,
    task::Poll,
};

use anyhow::{Context, Result};
use async_compression::futures::bufread::{BzDecoder, GzipDecoder};
use futures::{AsyncRead, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt, io::BufReader};
#[cfg(target_env = "ohos")]
use futures::{AsyncReadExt, StreamExt};
use sha2::{Digest, Sha256};

use crate::{HttpClient, github::AssetKind};

fn sha256_matches(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected)
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct GithubBinaryMetadata {
    pub metadata_version: u64,
    pub digest: Option<String>,
}

impl GithubBinaryMetadata {
    pub async fn read_from_file(metadata_path: &Path) -> Result<GithubBinaryMetadata> {
        let metadata_content = async_fs::read_to_string(metadata_path)
            .await
            .with_context(|| format!("reading metadata file at {metadata_path:?}"))?;
        serde_json::from_str(&metadata_content)
            .with_context(|| format!("parsing metadata file at {metadata_path:?}"))
    }

    pub async fn write_to_file(&self, metadata_path: &Path) -> Result<()> {
        let metadata_content = serde_json::to_string(self)
            .with_context(|| format!("serializing metadata for {metadata_path:?}"))?;
        async_fs::write(metadata_path, metadata_content.as_bytes())
            .await
            .with_context(|| format!("writing metadata file at {metadata_path:?}"))?;
        Ok(())
    }
}

pub async fn download_server_binary(
    http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Result<(), anyhow::Error> {
    log::info!("downloading github artifact from {url}");
    let Some(destination_parent) = destination_path.parent() else {
        anyhow::bail!("destination path has no parent: {destination_path:?}");
    };

    let staging_path = staging_path(destination_parent, asset_kind)?;
    let mut response = http_client
        .get(url, Default::default(), true)
        .await
        .with_context(|| format!("downloading release from {url}"))?;
    let body = response.body_mut();

    if let Err(err) = extract_to_staging(body, digest, url, &staging_path, asset_kind).await {
        cleanup_staging_path(&staging_path, asset_kind).await;
        return Err(err);
    }

    if let Err(err) = finalize_download(&staging_path, destination_path).await {
        cleanup_staging_path(&staging_path, asset_kind).await;
        return Err(err);
    }

    Ok(())
}


pub async fn download_server_raw_binary(
    http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    binary_file_name: &str,
) -> Result<(), anyhow::Error> {
    log::info!("downloading raw binary from {url}");
    let Some(destination_parent) = destination_path.parent() else {
        anyhow::bail!("destination path has no parent: {destination_path:?}");
    };

    let staging_path = staging_dir_path(destination_parent)?;
    let result = async {
        let mut response = http_client
            .get(url, Default::default(), true)
            .await
            .with_context(|| format!("downloading release from {url}"))?;

        let binary_path = staging_path.join(binary_file_name);
        let mut writer = HashingWriter {
            writer: async_fs::File::create(&binary_path)
                .await
                .with_context(|| format!("creating a file {binary_path:?} for {url}"))?,
            hasher: Sha256::new(),
        };
        futures::io::copy(&mut BufReader::new(response.body_mut()), &mut writer)
            .await
            .with_context(|| format!("saving binary contents from {url}"))?;
        let asset_sha_256 = writer
            .finish()
            .await
            .with_context(|| format!("flushing binary contents for {url}"))?;

        if let Some(expected_sha_256) = digest {
            anyhow::ensure!(
                sha256_matches(&asset_sha_256, expected_sha_256),
                "{url} asset got SHA-256 mismatch. Expected: {expected_sha_256}, Got: {asset_sha_256}",
            );
        }

        util::fs::make_file_executable(&binary_path)
            .await
            .with_context(|| format!("marking {binary_path:?} as executable"))?;
        finalize_download(&staging_path, destination_path).await
    }
    .await;

    if let Err(err) = result {
        if let Err(err) = async_fs::remove_dir_all(&staging_path).await {
            log::warn!("failed to remove staging directory {staging_path:?}: {err:?}");
        }
        return Err(err);
    }

    Ok(())
}


async fn extract_to_staging(
    body: impl AsyncRead + Unpin,
    digest: Option<&str>,
    url: &str,
    staging_path: &Path,
    asset_kind: AssetKind,
) -> Result<()> {
    match digest {
        Some(expected_sha_256) => {
            let temp_asset_file = tempfile::NamedTempFile::new()
                .with_context(|| format!("creating a temporary file for {url}"))?;
            let (temp_asset_file, _temp_guard) = temp_asset_file.into_parts();
            let mut writer = HashingWriter {
                writer: async_fs::File::from(temp_asset_file),
                hasher: Sha256::new(),
            };
            futures::io::copy(&mut BufReader::new(body), &mut writer)
                .await
                .with_context(|| {
                    format!("saving archive contents into the temporary file for {url}")
                })?;
            let asset_sha_256 = format!("{:x}", writer.hasher.finalize());

            anyhow::ensure!(
                sha256_matches(&asset_sha_256, expected_sha_256),
                "{url} asset got SHA-256 mismatch. Expected: {expected_sha_256}, Got: {asset_sha_256}",
            );
            writer
                .writer
                .seek(std::io::SeekFrom::Start(0))
                .await
                .with_context(|| format!("seeking temporary file for {url}"))?;
            stream_file_archive(&mut writer.writer, url, staging_path, asset_kind)
                .await
                .with_context(|| {
                    format!("extracting downloaded asset for {url} into {staging_path:?}")
                })?;
        }
        None => {
            stream_response_archive(body, url, staging_path, asset_kind)
                .await
                .with_context(|| {
                    format!("extracting response for asset {url} into {staging_path:?}")
                })?;
        }
    }
    Ok(())
}

fn staging_dir_path(parent: &Path) -> Result<PathBuf> {
    let dir = tempfile::Builder::new()
        .prefix(".tmp-github-download-")
        .tempdir_in(parent)
        .with_context(|| format!("creating staging directory in {parent:?}"))?;
    Ok(dir.keep())
}

fn staging_path(parent: &Path, asset_kind: AssetKind) -> Result<PathBuf> {
    match asset_kind {
        AssetKind::TarGz | AssetKind::TarBz2 | AssetKind::Zip => staging_dir_path(parent),
        AssetKind::Gz => {
            let path = tempfile::Builder::new()
                .prefix(".tmp-github-download-")
                .tempfile_in(parent)
                .with_context(|| format!("creating staging file in {parent:?}"))?
                .into_temp_path()
                .keep()
                .with_context(|| format!("persisting staging file in {parent:?}"))?;
            Ok(path)
        }
    }
}

async fn cleanup_staging_path(staging_path: &Path, asset_kind: AssetKind) {
    match asset_kind {
        AssetKind::TarGz | AssetKind::TarBz2 | AssetKind::Zip => {
            if let Err(err) = async_fs::remove_dir_all(staging_path).await {
                log::warn!("failed to remove staging directory {staging_path:?}: {err:?}");
            }
        }
        AssetKind::Gz => {
            if let Err(err) = async_fs::remove_file(staging_path).await {
                log::warn!("failed to remove staging file {staging_path:?}: {err:?}");
            }
        }
    }
}

async fn finalize_download(staging_path: &Path, destination_path: &Path) -> Result<()> {
    _ = async_fs::remove_dir_all(destination_path).await;
    async_fs::rename(staging_path, destination_path)
        .await
        .with_context(|| format!("renaming {staging_path:?} to {destination_path:?}"))?;
    Ok(())
}

async fn stream_response_archive(
    response: impl AsyncRead + Unpin,
    url: &str,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Result<()> {
    match asset_kind {
        AssetKind::TarGz => extract_tar_gz(destination_path, url, response).await?,
        AssetKind::TarBz2 => extract_tar_bz2(destination_path, url, response).await?,
        AssetKind::Gz => extract_gz(destination_path, url, response).await?,
        AssetKind::Zip => {
            util::archive::extract_zip(destination_path, response).await?;
        }
    };
    Ok(())
}

async fn stream_file_archive(
    file_archive: impl AsyncRead + AsyncSeek + Unpin,
    url: &str,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Result<()> {
    match asset_kind {
        AssetKind::TarGz => extract_tar_gz(destination_path, url, file_archive).await?,
        AssetKind::TarBz2 => extract_tar_bz2(destination_path, url, file_archive).await?,
        AssetKind::Gz => extract_gz(destination_path, url, file_archive).await?,
        #[cfg(not(windows))]
        AssetKind::Zip => {
            util::archive::extract_seekable_zip(destination_path, file_archive).await?;
        }
        #[cfg(windows)]
        AssetKind::Zip => {
            util::archive::extract_zip(destination_path, file_archive).await?;
        }
    };
    Ok(())
}

async fn extract_tar_gz(
    destination_path: &Path,
    url: &str,
    from: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let decompressed_bytes = GzipDecoder::new(BufReader::new(from));
    unpack_tar_archive(destination_path, url, decompressed_bytes).await?;
    Ok(())
}

async fn extract_tar_bz2(
    destination_path: &Path,
    url: &str,
    from: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let decompressed_bytes = BzDecoder::new(BufReader::new(from));
    unpack_tar_archive(destination_path, url, decompressed_bytes).await?;
    Ok(())
}

async fn unpack_tar_archive(
    destination_path: &Path,
    url: &str,
    archive_bytes: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    // We don't need to set the modified time. It's irrelevant to downloaded
    // archive verification, and some filesystems return errors when asked to
    // apply it after extraction.
    let archive = async_tar::ArchiveBuilder::new(archive_bytes)
        .set_preserve_mtime(false)
        .build();

    // The OHOS app sandbox denies symlink(2) and hard_link(2), so async-tar's
    // default unpack aborts as soon as a tar contains a link entry. On OHOS,
    // recover those links in the unpack error path and materialize them as
    // real copies (see unpack_tar_archive_ohos).
    #[cfg(target_env = "ohos")]
    return unpack_tar_archive_ohos(archive, destination_path, url).await;

    #[cfg(not(target_env = "ohos"))]
    {
        archive
            .unpack(destination_path)
            .await
            .with_context(|| format!("extracting {url} to {destination_path:?}"))?;
        Ok(())
    }
}

#[cfg(target_env = "ohos")]
async fn unpack_tar_archive_ohos<R>(
    archive: async_tar::Archive<R>,
    destination_path: &Path,
    url: &str,
) -> Result<(), anyhow::Error>
where
    R: AsyncRead + Unpin,
{
    log::info!("unpack_tar_archive_ohos: extracting {url} into {destination_path:?}");
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
                    std::path::PathBuf::from(entry.path().context("reading link path")?.as_os_str());
                let link_target = entry
                    .link_name()
                    .context("reading link target")?
                    .map(|target| std::path::PathBuf::from(target.as_os_str()));
                // Link entries carry no payload; consume any remaining bytes
                // so the entry stream advances to the next header.
                entry
                    .read_to_end(&mut Vec::new())
                    .await
                    .context("skipping link payload")?;
                log::debug!(
                    "unpack_tar_archive_ohos: denied link {link_path:?} -> {link_target:?}: {err}"
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
            "unpack_tar_archive_ohos: materializing {} link(s) from {url}",
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

async fn extract_gz(
    destination_path: &Path,
    url: &str,
    from: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let mut decompressed_bytes = GzipDecoder::new(BufReader::new(from));
    let mut file = async_fs::File::create(&destination_path)
        .await
        .with_context(|| {
            format!("creating a file {destination_path:?} for a download from {url}")
        })?;
    futures::io::copy(&mut decompressed_bytes, &mut file)
        .await
        .with_context(|| format!("extracting {url} to {destination_path:?}"))?;
    Ok(())
}

struct HashingWriter<W: AsyncWrite + Unpin> {
    writer: W,
    hasher: Sha256,
}

impl<W: AsyncWrite + Unpin> HashingWriter<W> {
    /// Closes and drops the inner writer, returning the hex SHA-256 digest of
    /// everything written.
    ///
    /// Taking `self` by value guarantees the writer is dropped before this
    /// returns. For file writers this releases the OS handle, which Windows
    /// requires before an ancestor directory can be renamed or deleted; note
    /// that closing alone is not enough, as `async_fs::File` holds its handle
    /// until dropped.
    async fn finish(mut self) -> std::io::Result<String> {
        self.writer.close().await?;
        drop(self.writer);
        Ok(format!("{:x}", self.hasher.finalize()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for HashingWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::result::Result<usize, std::io::Error>> {
        match Pin::new(&mut self.writer).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.hasher.update(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::result::Result<(), std::io::Error>> {
        Pin::new(&mut self.writer).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AsyncBody, Response};
    use futures::future::BoxFuture;
    use http::HeaderValue;
    use url::Url;

    struct StaticResponseClient {
        body: Vec<u8>,
    }

    impl HttpClient for StaticResponseClient {
        fn send(
            &self,
            _req: http::Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let body = self.body.clone();
            Box::pin(async move {
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body))
                    .unwrap())
            })
        }

        fn user_agent(&self) -> Option<&HeaderValue> {
            None
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }
    }

    #[test]
    fn downloads_raw_binary_with_uppercase_digest_into_destination_dir() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().unwrap();
            let destination_path = temp_dir.path().join("v_1");
            let contents = b"#!/bin/sh\necho hello\n".to_vec();
            let expected_sha_256 = format!("{:X}", Sha256::digest(&contents));
            let client = StaticResponseClient { body: contents };

            download_server_raw_binary(
                &client,
                "https://example.com/agent-binary",
                Some(&expected_sha_256),
                &destination_path,
                "agent-binary",
            )
            .await
            .unwrap();

            let binary_path = destination_path.join("agent-binary");
            assert_eq!(
                std::fs::read(&binary_path).unwrap(),
                b"#!/bin/sh\necho hello\n"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&binary_path)
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o111, 0o111, "binary should be executable");
            }
        });
    }

    #[test]
    fn raw_binary_digest_mismatch_cleans_up_staging() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().unwrap();
            let destination_path = temp_dir.path().join("v_1");
            let client = StaticResponseClient {
                body: b"some binary".to_vec(),
            };

            let error = download_server_raw_binary(
                &client,
                "https://example.com/agent-binary",
                Some("0000000000000000000000000000000000000000000000000000000000000000"),
                &destination_path,
                "agent-binary",
            )
            .await
            .unwrap_err();

            assert!(error.to_string().contains("SHA-256 mismatch"));
            assert!(!destination_path.exists());
            let leftover_entries = std::fs::read_dir(temp_dir.path()).unwrap().count();
            assert_eq!(leftover_entries, 0, "staging directory should be removed");
        });
    }

    #[test]
    fn downloads_archive_with_uppercase_digest_and_extracts_contents() {
        futures::executor::block_on(async {
            let archive = vec![
                0x50, 0x4b, 0x03, 0x04, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x00,
                0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x05, 0x00,
                0x00, 0x00, 0x61, 0x67, 0x65, 0x6e, 0x74, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x50, 0x4b,
                0x01, 0x02, 0x14, 0x03, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x00,
                0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x05, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x01, 0x00, 0x00,
                0x00, 0x00, 0x61, 0x67, 0x65, 0x6e, 0x74, 0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00,
                0x00, 0x01, 0x00, 0x01, 0x00, 0x33, 0x00, 0x00, 0x00, 0x28, 0x00, 0x00, 0x00, 0x00,
                0x00,
            ];
            let expected_sha_256 = format!("{:X}", Sha256::digest(&archive));
            let client = StaticResponseClient { body: archive };
            let temp_dir = tempfile::tempdir().unwrap();
            let destination_path = temp_dir.path().join("v_1");

            download_server_binary(
                &client,
                "https://example.com/agent.zip",
                Some(&expected_sha_256),
                &destination_path,
                AssetKind::Zip,
            )
            .await
            .unwrap();

            assert_eq!(
                std::fs::read(destination_path.join("agent")).unwrap(),
                b"hello"
            );
        });
    }

    #[test]
    fn archive_digest_mismatch_prevents_extraction_and_cleans_up_staging() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().unwrap();
            let destination_path = temp_dir.path().join("v_1");
            let client = StaticResponseClient {
                body: b"not an archive".to_vec(),
            };

            let error = download_server_binary(
                &client,
                "https://example.com/agent.zip",
                Some("0000000000000000000000000000000000000000000000000000000000000000"),
                &destination_path,
                AssetKind::Zip,
            )
            .await
            .unwrap_err();

            assert!(error.to_string().contains("SHA-256 mismatch"));
            assert!(!destination_path.exists());
            let leftover_entries = std::fs::read_dir(temp_dir.path()).unwrap().count();
            assert_eq!(leftover_entries, 0, "staging directory should be removed");
        });
    }
}
