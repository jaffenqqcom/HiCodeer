//! End-to-end probe against a running zcoderd (P3 bring-up).
//!
//! Reads the client half of the management keys from `ZCODERD_KEY_DIR`
//! (mgmt-host.pub + mgmt-client-key), boots a `SshCommandExecutor`, runs
//! `/bin/echo probe-ok`, prints its stdout and exit status, then runs a
//! long-lived command that is killed via `signal` to exercise the session
//! table. Usage:
//!
//! ```text
//! ZCODERD_KEY_DIR=<client-half-dir> cargo run --example probe \
//!   --manifest-path .../cmd-agent/cmd-client/Cargo.toml
//! ```

use std::io::Read as _;

use cmd_client::{ExecSpec, RemoteCommandExecutor, SshCommandExecutor};
use smol::io::AsyncReadExt as _;

/// Env var pointing at the directory holding the client half of the management
/// keys.
const KEY_DIR_ENV: &str = "ZCODERD_KEY_DIR";
/// Management host public key file (client half, pair A).
const MGMT_HOST_PUB_FILE: &str = "mgmt-host.pub";
/// Management client private key file (client half, pair B).
const MGMT_CLIENT_KEY_FILE: &str = "mgmt-client-key";

fn read_file(path: &str) -> String {
    let mut contents = String::new();
    std::fs::File::open(path)
        .unwrap_or_else(|err| panic!("open {path}: {err}"))
        .read_to_string(&mut contents)
        .unwrap_or_else(|err| panic!("read {path}: {err}"));
    contents
}

fn main() {
    env_logger::init();
    let key_dir = std::env::var(KEY_DIR_ENV)
        .unwrap_or_else(|_| panic!("set {KEY_DIR_ENV} to the client-half key dir"));
    let host_pub = read_file(&format!("{key_dir}/{MGMT_HOST_PUB_FILE}"));
    let client_key = read_file(&format!("{key_dir}/{MGMT_CLIENT_KEY_FILE}"));

    let executor = SshCommandExecutor::new(client_key, host_pub).expect("executor");
    let executor = std::sync::Arc::new(executor);

    smol::block_on(async {
        // Quick command: /bin/echo probe-ok
        let mut spec = ExecSpec::new("/bin/echo");
        spec.args.push("probe-ok".to_string());
        spec.stdout_mode = cmd_client::FdMode::Piped;
        let mut child = executor.spawn(spec).expect("spawn echo");
        let mut out = String::new();
        if let Some(stdout) = child.stdout.as_mut() {
            let _ = stdout.read_to_string(&mut out).await;
        }
        let exit = executor.wait_exit_async(child.session_id).await;
        println!("echo stdout={out:?} exit={exit:?}");

        // Long-lived command that we kill by session (exercises the server-side
        // session table + process-group signal).
        let mut sleep_spec = ExecSpec::new("/bin/sh");
        sleep_spec.args.push("-c".to_string());
        sleep_spec.args.push("echo start && sleep 300".to_string());
        sleep_spec.stdout_mode = cmd_client::FdMode::Piped;
        let mut sleeper = executor.spawn(sleep_spec).expect("spawn sleep");
        // Give it a moment to emit "start", then signal it.
        let mut first = [0u8; 32];
        if let Some(stdout) = sleeper.stdout.as_mut() {
            let _ = stdout.read(&mut first).await;
        }
        println!("sleep first bytes={:?}", String::from_utf8_lossy(&first));
        executor
            .signal(sleeper.session_id, cmd_client::Signal::SigKill)
            .expect("signal");
        let exit = executor.wait_exit_async(sleeper.session_id).await;
        println!("sleep exit after signal={exit:?}");
    });
}
