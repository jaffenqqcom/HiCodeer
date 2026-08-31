//! zcoder-side path -> QEMU mount-point lookup table.
//!
//! The table is keyed by the zcoder-side URI or path root: each entry maps one
//! zcoder root (e.g. the sandbox root or an opened-folder URI) to its mount
//! point inside the QEMU guest. Command arguments are rewritten by matching a
//! key as a path prefix (boundary-checked) and replacing it with the guest
//! mount point. The table is maintained exclusively by mount_folder and
//! unmount_folder so it always mirrors the live mounts.

use qemu_cmd_agent_protocol::messages::RootMap;

/// Ordered set of (host_root, guest_root) mappings. host_root is the
/// zcoder-side URI or path used as the lookup index.
#[derive(Debug, Default, Clone)]
pub struct PathMap {
    maps: Vec<RootMap>,
}

impl PathMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces one zcoder root -> guest mount-point mapping.
    /// Called only by mount_folder.
    pub fn add(&mut self, map: RootMap) {
        log::info!("[diag] path_map::add: {} -> {}", map.host_root, map.guest_root);
        self.maps.retain(|m| m.host_root != map.host_root);
        self.maps.push(map);
    }

    /// Number of registered mappings.
    pub fn len(&self) -> usize {
        self.maps.len()
    }

    /// Removes one mapping. Called only by unmount_folder.
    pub fn remove(&mut self, host_root: &str) {
        log::info!("[diag] path_map::remove: {host_root}");
        self.maps.retain(|m| m.host_root != host_root);
    }

    /// Rewrites `path` through the lookup table. A key match must end at a
    /// path boundary (`/` or end of string), so e.g. a `/sandbox` key does not
    /// rewrite `/sandboxx`. Unmatched paths pass through unchanged.
    ///
    /// `--flag=<path>` style arguments (e.g. `--cache=/data/...`) map only the
    /// value when it starts with `/`, mirroring the OpenEuler cmd-agentd fix
    /// for npm-style inline options.
    pub fn map_path(&self, path: &str) -> String {
        if path.starts_with('-') {
            if let Some((flag, value)) = path.split_once('=') {
                if !value.is_empty() && value.starts_with('/') {
                    let mapped = format!("{flag}={}", self.map_path_value(value));
                    log::debug!("[diag] path_map::map_path: inline {path} -> {mapped}");
                    return mapped;
                }
            }
            return path.to_string();
        }
        self.map_path_value(path)
    }

    /// Rewrites a bare path by indexing into the table: the first key that is
    /// a path-boundary prefix of `path` is replaced with its guest mount point.
    fn map_path_value(&self, path: &str) -> String {
        for m in &self.maps {
            let key = m.host_root.as_str();
            if let Some(rest) = path.strip_prefix(key) {
                // The key matches only at a path boundary (exact root or a `/`
                // continuation), so `/a/b` never rewrites `/a/bc`.
                if rest.is_empty() || rest.starts_with('/') {
                    let mapped = format!("{}{}", m.guest_root, rest);
                    log::debug!("[diag] path_map::map_path_value: {path} -> {mapped}");
                    return mapped;
                }
            }
        }
        path.to_string()
    }
}
