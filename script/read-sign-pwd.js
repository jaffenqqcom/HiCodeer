#!/usr/bin/env node
"use strict";

// Reads the keystore password of a HAP project out of the encrypted value that
// DevEco Studio keeps in build-profile.json5.
//
// The scheme is not a secret: hvigor ships it in plain JavaScript at
// hvigor-ohos-plugin/src/utils/decipher-util.js. Only the random material under
// the keystore's sibling `material/` directory is per-keystore, and that
// directory travels with the keystore, so the password stays recoverable
// locally. Doing this at build time means swapping a keystore never requires
// syncing the password into this repository by hand.
//
// Usage: node read-sign-pwd.js <hap-project-dir>   (prints the password)

const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

// The fixed 16-byte component from hvigor's decipher-util.js. Useless on its
// own: the per-keystore randomness lives in material/.
const COMPONENT = new Int8Array([
  49, 243, 9, 115, 214, 175, 91, 184, 211, 190, 177, 88, 101, 131, 192, 119,
]);

const PROFILE_NAME = "build-profile.json5";

function fail(message) {
  process.stderr.write("read-sign-pwd: " + message + "\n");
  process.exit(1);
}

function readField(profileText, field, quoted) {
  const pattern = quoted
    ? new RegExp('"' + field + '"\\s*:\\s*"([^"]+)"')
    : new RegExp('"' + field + '"\\s*:\\s*"([0-9a-fA-F]+)"');
  const match = pattern.exec(profileText);
  if (!match) {
    fail(`no "${field}" found in ${PROFILE_NAME}`);
  }
  return match[1];
}

function readSingleFile(dir) {
  const names = fs.readdirSync(dir).filter((name) => name !== ".DS_Store");
  if (names.length !== 1) {
    fail(`expected exactly one file under ${dir}, found ${names.length}`);
  }
  return new Int8Array(fs.readFileSync(path.resolve(dir, names[0])));
}

function xorAll(parts) {
  const length = parts[0].byteLength;
  const out = new Int8Array(length);
  for (const part of parts) {
    if (part.byteLength !== length) {
      fail("key material component length mismatch");
    }
    for (let i = 0; i < length; i++) {
      out[i] ^= part[i];
    }
  }
  return Buffer.from(out);
}

function openGcm(key, data) {
  const declared =
    ((255 & data[0]) << 24) | ((255 & data[1]) << 16) | ((255 & data[2]) << 8) | (255 & data[3]);
  const ivLength = data.length - 4 - declared;
  const decipher = crypto.createDecipheriv("aes-128-gcm", key, data.slice(4, 4 + ivLength));
  decipher.setAuthTag(data.slice(data.length - 16));
  const head = decipher.update(data.subarray(4 + ivLength, data.length - 16));
  return Buffer.concat([head, decipher.final()]);
}

function unwrapDataKey(materialRoot) {
  const shardRoot = path.resolve(materialRoot, "fd");
  const shards = fs
    .readdirSync(shardRoot)
    .filter((name) => name !== ".DS_Store")
    .map((name) => readSingleFile(path.resolve(shardRoot, name)));
  const salt = readSingleFile(path.resolve(materialRoot, "ac"));
  const rootKey = new Int8Array(
    crypto.pbkdf2Sync(xorAll(shards.concat([COMPONENT])).toString(), salt, 10000, 16, "sha256"),
  );
  return openGcm(rootKey, readSingleFile(path.resolve(materialRoot, "ce")));
}

const projectDir = process.argv[2];
if (!projectDir) {
  fail("usage: node read-sign-pwd.js <hap-project-dir>");
}

const profilePath = path.resolve(projectDir, PROFILE_NAME);
let profileText;
try {
  profileText = fs.readFileSync(profilePath, "utf-8");
} catch (error) {
  fail(`cannot read ${profilePath}: ${error.message}`);
}

const encrypted = readField(profileText, "storePassword", false);
const storeFile = readField(profileText, "storeFile", true);
const materialRoot = path.resolve(path.dirname(path.resolve(projectDir, storeFile)), "material");
if (!fs.existsSync(materialRoot)) {
  fail(`no material directory at ${materialRoot}; cannot recover the password`);
}

const plain = openGcm(
  unwrapDataKey(materialRoot),
  new Int8Array(Buffer.from(encrypted, "hex")),
).toString("utf-8");
if (plain.length === 0) {
  fail("recovered an empty password");
}
process.stdout.write(plain + "\n");
