use std::{
    path::{Path, PathBuf},
    pin::Pin,
    task::Poll,
};

use anyhow::{Context, Result};
use async_compression::futures::bufread::{BzDecoder, GzipDecoder};
use futures::{
    AsyncRead, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt, StreamExt, io::BufReader,
};
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
    // [diag] record the extracted tree for sync cross-checking: the sync
    // engine watches destination_path and should mirror exactly these files.
    let (extracted_files, extracted_bytes) = count_files(&staging_path);
    log::info!(
        "[diag] github_download: extracted {url} -> staging {staging_path:?} ({extracted_files} files, {extracted_bytes} bytes)"
    );

    if let Err(err) = finalize_download(&staging_path, destination_path).await {
        cleanup_staging_path(&staging_path, asset_kind).await;
        return Err(err);
    }
    // [diag] record the final rename so the sync engine's watch can be compared.
    log::info!(
        "[diag] github_download: finalized {staging_path:?} -> {destination_path:?}"
    );

    Ok(())
}

/// Recursively counts files and total bytes under `path` (a file or directory),
/// for the sync cross-check log. Returns (file_count, total_bytes).
fn count_files(path: &Path) -> (usize, u64) {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return (0, 0),
    };
    if meta.is_file() {
        return (1, meta.len());
    }
    let mut count = 0usize;
    let mut bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let (c, b) = count_files(&entry.path());
            count += c;
            bytes += b;
        }
    }
    (count, bytes)
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
    // [diag] record the raw-binary finalize for sync cross-checking.
    log::info!(
        "[diag] github_download: raw binary finalized {staging_path:?} -> {destination_path:?}"
    );

    Ok(())
}


async fn extract_to_staging(
    body: impl AsyncRead + Unpin,
    digest: Option<&str>,
    url: &str,
    staging_path: &Path,
    asset_kind: AssetKind,
) -> Result<()> {
    // Buffer the archive into a temporary file before unpacking. The response
    // body is a single-use stream, so buffering is what allows a failed unpack
    // to be retried below without re-downloading.
    let temp_asset_file = tempfile::NamedTempFile::new()
        .with_context(|| format!("creating a temporary file for {url}"))?;
    let (temp_asset_file, _temp_guard) = temp_asset_file.into_parts();
    let mut writer = HashingWriter {
        writer: async_fs::File::from(temp_asset_file),
        hasher: Sha256::new(),
    };
    futures::io::copy(&mut BufReader::new(body), &mut writer)
        .await
        .with_context(|| format!("saving archive contents into the temporary file for {url}"))?;
    let asset_sha_256 = format!("{:x}", writer.hasher.finalize());

    if let Some(expected_sha_256) = digest {
        anyhow::ensure!(
            sha256_matches(&asset_sha_256, expected_sha_256),
            "{url} asset got SHA-256 mismatch. Expected: {expected_sha_256}, Got: {asset_sha_256}",
        );
    }
    writer
        .writer
        .seek(std::io::SeekFrom::Start(0))
        .await
        .with_context(|| format!("seeking temporary file for {url}"))?;

    if let Err(first_err) =
        stream_file_archive(&mut writer.writer, url, staging_path, asset_kind).await
    {
        // The OHOS sandbox forbids third-party apps from creating symlinks
        // (symlink() -> EACCES), so archives that contain symlinks (e.g.
        // vscode-eslint) fail the standard unpack with Permission denied. Retry
        // once, materializing symlink/hardlink entries as real copies of their
        // targets; any other kind of failure is returned as-is.
        #[cfg(target_env = "ohos")]
        if is_permission_denied(&first_err) {
            log::warn!(
                "github_download: standard unpack of {url} failed with permission denied \
                 ({first_err:#}); retrying with link-copy unpacker"
            );
            cleanup_staging_path(staging_path, asset_kind).await;
            async_fs::create_dir_all(staging_path)
                .await
                .with_context(|| format!("recreating staging directory {staging_path:?}"))?;
            writer
                .writer
                .seek(std::io::SeekFrom::Start(0))
                .await
                .with_context(|| format!("seeking temporary file for {url} for retry"))?;
            return stream_file_archive_with_link_copy(
                &mut writer.writer,
                url,
                staging_path,
                asset_kind,
            )
            .await
            .with_context(|| {
                format!("extracting downloaded asset for {url} into {staging_path:?}")
            });
        }
        return Err(first_err);
    }
    Ok(())
}

/// True when any cause in the error chain is an io::Error with kind
/// PermissionDenied — the errno the OHOS sandbox reports for symlink().
#[cfg(target_env = "ohos")]
fn is_permission_denied(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

/// Unpacks a tar archive entry by entry, replacing symlink/hardlink entries
/// with recursive copies of their link targets. Used on OHOS wherever the
/// standard unpack would hit Permission denied, because the sandbox forbids
/// creating symlinks. The result is content-equivalent to the archive.
///
/// Also used by the Node.js runtime download: the official tarballs ship
/// `bin/npm` and friends as symlinks, so they cannot be unpacked as-is here.
#[cfg(target_env = "ohos")]
pub async fn unpack_tar_archive_with_link_copy(
    destination_path: &Path,
    url: &str,
    archive_bytes: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let archive = async_tar::ArchiveBuilder::new(archive_bytes)
        .set_preserve_mtime(false)
        .build();
    let mut entries = archive.entries()?;

    // Deferred link entries: (archive-relative dst, link name, is_hard_link).
    let mut deferred: Vec<(PathBuf, Option<PathBuf>, bool)> = Vec::new();
    while let Some(entry) = entries.next().await {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            // async_tar's entry paths are async_std types; normalize to
            // std::path::PathBuf for the deferred list.
            let dst = PathBuf::from(entry.path()?.as_os_str());
            let link = entry.link_name()?.map(|name| PathBuf::from(name.as_os_str()));
            deferred.push((dst, link, kind.is_hard_link()));
        } else {
            entry.unpack_in(destination_path).await?;
        }
    }

    for (dst_rel, link, is_hard_link) in deferred {
        let Some(link) = link else {
            continue;
        };
        let dst_full = destination_path.join(&dst_rel);
        // tar semantics: hard-link names are relative to the archive root,
        // symlink names are relative to the directory containing the link.
        let src = if is_hard_link {
            destination_path.join(&link)
        } else {
            dst_full.parent().unwrap_or(destination_path).join(&link)
        };
        if src.exists() {
            copy_path(&src, &dst_full).await?;
        } else {
            log::warn!(
                "github_download: link target {src:?} for {dst_rel:?} missing in {url}; skipped"
            );
        }
    }
    Ok(())
}

/// Recursively copies a file or directory from `src` to `dst`, replacing a
/// symlink that the OHOS sandbox forbids creating with a real copy of its
/// target. Existing destination files are overwritten.
///
/// Implemented as an iterative DFS so a deep tree cannot overflow the async
/// recursion size limit (a recursive async fn would need boxing).
#[cfg(target_env = "ohos")]
async fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        if src.is_dir() {
            async_fs::create_dir_all(&dst).await?;
            let mut entries = async_fs::read_dir(&src).await?;
            while let Some(entry) = entries.next().await {
                let entry = entry?;
                // async_fs entry paths are async_std types; normalize to std.
                let child_src = PathBuf::from(entry.path().as_os_str());
                let child_dst = dst.join(entry.file_name());
                stack.push((child_src, child_dst));
            }
        } else {
            if let Some(parent) = dst.parent() {
                async_fs::create_dir_all(parent).await?;
            }
            async_fs::copy(&src, &dst).await?;
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

/// Streams an archive with the link-copy unpacker (OHOS fallback). Only tar
/// archives can contain symlinks; gz is a single decompressed file and zip has
/// its own link handling, so those fall back to the standard unpacker.
#[cfg(target_env = "ohos")]
async fn stream_file_archive_with_link_copy(
    file_archive: impl AsyncRead + AsyncSeek + Unpin,
    url: &str,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Result<()> {
    match asset_kind {
        AssetKind::TarGz => {
            extract_tar_gz_with_link_copy(destination_path, url, file_archive).await?
        }
        AssetKind::TarBz2 => {
            extract_tar_bz2_with_link_copy(destination_path, url, file_archive).await?
        }
        _ => stream_file_archive(file_archive, url, destination_path, asset_kind).await?,
    }
    Ok(())
}

/// OHOS link-copy variant of `extract_tar_gz`.
#[cfg(target_env = "ohos")]
async fn extract_tar_gz_with_link_copy(
    destination_path: &Path,
    url: &str,
    from: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let decompressed_bytes = GzipDecoder::new(BufReader::new(from));
    unpack_tar_archive_with_link_copy(destination_path, url, decompressed_bytes).await?;
    Ok(())
}

/// OHOS link-copy variant of `extract_tar_bz2`.
#[cfg(target_env = "ohos")]
async fn extract_tar_bz2_with_link_copy(
    destination_path: &Path,
    url: &str,
    from: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let decompressed_bytes = BzDecoder::new(BufReader::new(from));
    unpack_tar_archive_with_link_copy(destination_path, url, decompressed_bytes).await?;
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
    archive
        .unpack(&destination_path)
        .await
        .with_context(|| format!("extracting {url} to {destination_path:?}"))?;
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
