//! Host-key round-trip sanity: the public half parsed from an OpenSSH public
//! line must equal the public half derived from the matching OpenSSH private
//! file. Reads real ssh-keygen outputs from `ZCODERD_KEY_DIR`.

use russh::keys::ssh_key::{PrivateKey, PublicKey};

fn read_file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| panic!("open {path}: {err}"))
}

#[test]
fn mgmt_host_key_roundtrip() {
    let dir = std::env::var("ZCODERD_KEY_DIR").expect("set ZCODERD_KEY_DIR");
    let private_txt = read_file(&format!("{dir}/mgmt_host_key"));
    let public_txt = read_file(&format!("{dir}/mgmt_host_key.pub"));

    let private = PrivateKey::from_openssh(&private_txt)
        .unwrap_or_else(|err| panic!("parse private: {err}"));
    let from_pub_line = public_txt
        .lines()
        .next()
        .expect("pub line")
        .trim();
    let expected = PublicKey::from_openssh(from_pub_line)
        .unwrap_or_else(|err| panic!("parse public '{from_pub_line}': {err}"));

    let derived = private.public_key();
    assert_eq!(derived, &expected, "private-derived public != parsed public line");
}
