// Host-compatibility preload for the Node.js processes the daemon spawns.
//
// The daemon preloads this file via `NODE_OPTIONS=--require ...` on every child
// it spawns, whatever the child turns out to be.
//
// Keep this the ONLY preload file. Every host workaround belongs in here, in
// its own labelled section below -- do not add a second shim, and do not split
// these patches across files. One file means one `--require` to materialise,
// keep in sync on the device, and reason about when a Node process misbehaves;
// a second file makes all three worse for no gain. The file is named for what
// it is, a shim, rather than for the first bug it happened to fix.
//
// Each patch below is written to be a no-op wherever the host already behaves
// the way the tools expect; a patch only steps in on the departures described
// directly above it. Keep that property when adding one: a preload that fires
// changes the behaviour of every Node process in the session.
//
// The same preload also runs on a mainstream Linux host, where none of these
// departures exist and every one of these substitutions would be wrong. A patch
// therefore has to establish at run time that it is needed, by one of:
// - a condition only this host can meet -- the platform name as it was before
//   this file rewrote it, or a tool that is actually on PATH;
// - a probe of the capability in question, so an error a mainstream host can
//   also produce (a name that is taken, a tree that is read-only) is handed
//   back to the caller exactly as the real call reported it.
// Never substitute on the strength of this being the host under test.
'use strict';

// ---------------------------------------------------------------------------
// os.userInfo() when the running uid is absent from /etc/passwd
// ---------------------------------------------------------------------------
// An OHOS app or service runs under a uid that is absent from /etc/passwd, so
// libuv's uv_os_get_passwd() fails with ENOENT (`id <uid>` reports "bad uid")
// and os.userInfo() throws ERR_SYSTEM_ERROR. JS ACP agents call it while
// assembling their request context, so every prompt fails before it is sent.
//
// The real implementation is tried first, so this patch is a no-op wherever
// os.userInfo() already works.
//
// The synthesized record ignores `options.encoding`; no caller we care about
// asks for buffered fields, and returning strings keeps the shape useful.

const os = require('os');

const real_user_info = os.userInfo;
const fallback_name = process.env.HICODEERD_OSUSER || 'hicodeer';

function fallback_user_info() {
  return {
    uid: typeof process.getuid === 'function' ? process.getuid() : -1,
    gid: typeof process.getgid === 'function' ? process.getgid() : -1,
    username: fallback_name,
    homedir: safe_homedir(),
    shell: '/bin/sh',
  };
}

// os.homedir() falls back to the same failing account lookup when HOME is
// unset, so it needs the same treatment as the record it feeds.
function safe_homedir() {
  if (process.env.HOME) {
    return process.env.HOME;
  }
  try {
    return os.homedir();
  } catch (error) {
    return '/';
  }
}

os.userInfo = function userInfo(options) {
  try {
    return real_user_info.call(os, options);
  } catch (error) {
    if (error && error.code === 'ERR_SYSTEM_ERROR') {
      return fallback_user_info();
    }
    throw error;
  }
};

// ---------------------------------------------------------------------------
// process.platform is a name most Node tools do not know
// ---------------------------------------------------------------------------
// Node reports the platform here as 'openharmony'. Tools that switch on
// process.platform generally handle win32/darwin/linux and treat anything else
// as fatal: TypeScript's tsserver, for one, calls Debug.fail("unsupported
// platform 'openharmony'") while resolving its global typings cache and exits
// with code 1 the moment it starts, which takes the whole TS Server down with
// it.
//
// 'linux' is the closest familiar platform: the runtime is Linux-based, and
// every path a tool would pick for it is one that works here. The rewrite is
// guarded so the preload stays a no-op wherever a known platform is already
// reported, and best-effort because the property could in principle be sealed;
// a tool that still sees the old value will report its own error.

// Captured before the rewrite below, so a later patch can still tell which
// host it is on -- the rest of the file sees the rewritten value.
const real_platform = process.platform;

if (process.platform === 'openharmony') {
  try {
    Object.defineProperty(process, 'platform', { value: 'linux' });
  } catch (error) {
    // Left as reported; a caller that cares surfaces its own error.
  }
}

// ---------------------------------------------------------------------------
// symlink(2) is refused by the filesystem the install tree lives on
// ---------------------------------------------------------------------------
// npm materializes each entry in `node_modules/.bin` as a symlink pointing at
// a file inside its package, and the tree is written to a filesystem that
// refuses links outright -- every such call fails, and the install fails with
// it. The entries are not optional: the host application launches npm-hosted
// language servers as `node <entry>`, and `npm run` executes them.
//
// The real call is attempted first, so this patch is a no-op wherever links
// are allowed; it only steps in once the filesystem has refused. A refusal on
// its own is not enough to go on, though -- a mainstream host refuses links
// too, for a name that is taken or a tree it may not write to, and substituting
// there would quietly paper over a real error. So the directory is probed once
// to see whether it can hold a link at all, and the caller's error stands
// wherever it can.
//
// What it substitutes depends on where the link was going:
//
// - A bin entry becomes a JavaScript launcher that hands off to the package's
//   own file by absolute path. A plain copy will not do: these files load
//   their implementation relatively (`require('../lib/cli.js')`), and a copy
//   sitting in `.bin` would resolve that against the wrong directory. A
//   launcher also satisfies both of its consumers -- it reads as JavaScript
//   for `node <entry>`, and its `#!` line makes it executable for `npm run`.
// - Anything else becomes a copy of the target, a directory one recursively.
//
// Both are reported as success: from npm's side the entry is now there, which
// is all it asked for.

const fs = require('fs');
const node_path = require('path');

const real_symlink = fs.symlink;
const real_symlink_sync = fs.symlinkSync;

// Codes by which a filesystem reports that it will not make a link, as
// opposed to a caller error npm is meant to see (EEXIST, ENOENT, ...).
const refused_codes = new Set([
  'EPERM',
  'EACCES',
  'EOPNOTSUPP',
  'ENOTSUP',
  'ENOSYS',
  'EPROTO',
  'EMLINK',
]);

function link_refused(error) {
  return Boolean(error) && refused_codes.has(error.code);
}

// `true` when `to` is one of npm's bin entries: those sit in a `.bin`
// directory next to the packages they point into.
function is_bin_entry(to) {
  return node_path.basename(node_path.dirname(to)) === '.bin';
}

function starts_elf(file) {
  try {
    const handle = fs.openSync(file, 'r');
    const head = Buffer.alloc(4);
    fs.readSync(handle, head, 0, 4, 0);
    fs.closeSync(handle);
    return head[0] === 0x7f && head[1] === 0x45 && head[2] === 0x4c && head[3] === 0x46;
  } catch (error) {
    return false;
  }
}

// Writes `to` as a launcher for `from`, spawning a native entry directly and
// handing a scripted one to the same node this launcher runs under.
function write_launcher(from, to) {
  const quoted = JSON.stringify(from);
  const call = starts_elf(from)
    ? `spawnSync(${quoted}, process.argv.slice(2), { stdio: "inherit" })`
    : `spawnSync(process.execPath, [${quoted}, ...process.argv.slice(2)], { stdio: "inherit" })`;
  const source = [
    '#!/usr/bin/env node',
    'const { spawnSync } = require("node:child_process");',
    `const done = ${call};`,
    'process.exit(done.status === null ? 1 : done.status);',
    '',
  ].join('\n');
  fs.writeFileSync(to, source);
  fs.chmodSync(to, 0o775);
}

// Copies `from` to `to`, descending into directories.
function copy_target(from, to) {
  const stat = fs.statSync(from);
  if (stat.isDirectory()) {
    fs.mkdirSync(to, { recursive: true });
    for (const name of fs.readdirSync(from)) {
      copy_target(node_path.join(from, name), node_path.join(to, name));
    }
    return;
  }
  fs.copyFileSync(from, to);
  try {
    fs.chmodSync(to, stat.mode & 0o7777);
  } catch (error) {
    // Left with the copy's default mode; it is still readable.
  }
}

// Stands a real file in for the link `from -> to` that was refused.
function stand_in(from, to) {
  const source = node_path.resolve(node_path.dirname(to), from);
  if (is_bin_entry(to)) {
    write_launcher(source, to);
  } else {
    copy_target(source, to);
  }
}

// Whether `dir` can hold a link at all. Probed by making one there, under a
// name of our own, in a directory we have just shown we can write to: a refusal
// then means the filesystem, not the caller. Cached per directory, because an
// install makes thousands of these calls into the same few.
//
// Undecided counts as supported: when the probe cannot be made -- no such
// directory, or one we may not write to -- the caller's error is the only
// account of it there is, so it stands.
const link_probe = new Map();

function linkable(dir) {
  const known = link_probe.get(dir);
  if (known !== undefined) {
    return known;
  }
  const base = node_path.join(dir, `.shim-link-probe-${process.pid}`);
  const target = `${base}-target`;
  let usable = true;
  try {
    fs.writeFileSync(target, '');
    try {
      real_symlink_sync.call(fs, target, base, 'file');
    } catch (error) {
      usable = !link_refused(error);
    }
  } catch (error) {
    usable = true;
  } finally {
    for (const left of [base, target]) {
      try {
        fs.unlinkSync(left);
      } catch (error) {
        // Nothing of ours is left behind.
      }
    }
  }
  link_probe.set(dir, usable);
  return usable;
}

// `true` when a refused link in `dir` is this host's doing, and so worth
// substituting for.
function refusal_is_ours(error, path_) {
  return link_refused(error) && !linkable(node_path.dirname(path_));
}

fs.symlinkSync = function symlinkSync(target, path_, type) {
  try {
    return real_symlink_sync.call(fs, target, path_, type);
  } catch (error) {
    if (!refusal_is_ours(error, path_)) {
      throw error;
    }
    stand_in(target, path_);
  }
};

fs.symlink = function symlink(target, path_, type, callback) {
  let kind = type;
  let done = callback;
  if (typeof kind === 'function') {
    done = kind;
    kind = undefined;
  }
  const finish = typeof done === 'function' ? done : () => {};
  return real_symlink.call(fs, target, path_, kind, (error) => {
    if (!error) {
      finish(null);
      return;
    }
    if (!refusal_is_ours(error, path_)) {
      finish(error);
      return;
    }
    try {
      stand_in(target, path_);
      finish(null);
    } catch (stand_in_error) {
      // The original refusal is what the caller is equipped to reason about.
      finish(error);
    }
  });
};

// npm reads this one by destructuring `fs/promises` when its bin linker
// loads, so the replacement has to be in place before that -- which the
// preload guarantees, being run ahead of the main module.
if (fs.promises) {
  fs.promises.symlink = function symlink(target, path_, type) {
    return new Promise((resolve, reject) => {
      fs.symlink(target, path_, type, (error) => {
        if (error) {
          reject(error);
        } else {
          resolve();
        }
      });
    });
  };
}

// ---------------------------------------------------------------------------
// A native binary unpacked by an install is unsigned until the install ends
// ---------------------------------------------------------------------------
// Signing happens after the install exits (the daemon's own sweep covers the
// tree), but a lifecycle script runs during it: a package that ships a native
// binary typically builds or probes it in `postinstall`, and the kernel refuses
// to execute an unsigned ELF, so the script dies and takes the install with it.
//
// Forbidding those scripts keeps that from happening but drops the installs
// that do their real work in one -- a package whose own dependencies are
// installed by its root `postinstall` never finishes. This patch removes the
// need to choose: it signs a native binary at the moment a child process is
// about to run it, so a script can run whatever it just unpacked.
//
// The check runs before the spawn, which is what makes it workable: `spawn`
// must hand back a child synchronously for the caller to attach its handlers
// to, so a failure caught afterwards is too late to retry into. Each path is
// examined once -- signed, or left alone -- and the signing tool is exempt, so
// this cannot recurse into itself.
//
// None of it is installed unless this is a host that checks signatures and has
// the tool that writes them, both decided once at load: on a mainstream host a
// spawn is left entirely alone, and neither the platform name nor a missing
// tool is enough on its own to start rewriting child processes.

const child_process = require('child_process');

// The host's signing tool, and the ELF section its output carries. A file that
// already has the section is left exactly as it is.
const SIGN_TOOL = 'binary-sign-tool';
const SIGN_SECTION = '.codesign';

// Taken before the replacements below, so the signing call itself is not
// intercepted by them.
const orig_spawn = child_process.spawn;
const orig_spawn_sync = child_process.spawnSync;
const orig_exec_file = child_process.execFile;
const orig_exec_file_sync = child_process.execFileSync;

// Paths already dealt with, in either direction, so each is examined once.
const signed_paths = new Set();
const skipped_paths = new Set();

// One line per failure, which is the only thing worth writing down: a session
// that signed everything it needed leaves the section behind on each file, and
// that is a better record of it. Nothing normal is logged, so the file stays
// empty for as long as nothing goes wrong.
// Best-effort: a session without its temp directory simply goes untraced, and
// nothing here is worth failing a spawn over.
function trace(line) {
  const dir = process.env.TMPDIR;
  if (!host_needs_signing || !dir) {
    return;
  }
  try {
    fs.appendFileSync(
      node_path.join(dir, 'shim-trace.log'),
      `${new Date().toISOString()} pid=${process.pid} ${line}\n`,
    );
  } catch (error) {
    // Left untraced.
  }
}

// Where the session keeps its files, or `null` when the session names none.
// The signing sweep the daemon runs after an install covers this same tree, and
// anything outside it was installed by other means and is already signed. A
// bare `/tmp` resolves to a root everything else sits under, which would put
// the whole machine in scope -- such a session gets no signing at all.
function scope_root() {
  const tmp = process.env.TMPDIR;
  if (!tmp) {
    return null;
  }
  const root = node_path.resolve(tmp, '..');
  return root === node_path.sep ? null : root;
}

function within_scope(file) {
  const root = scope_root();
  return root !== null && (file === root || file.startsWith(root + node_path.sep));
}

// `true` when the ELF at `file` carries a section named `name`, read straight
// from its section header table.
function elf_has_section(file, name) {
  let handle;
  try {
    handle = fs.openSync(file, 'r');
    const head = Buffer.alloc(64);
    if (fs.readSync(handle, head, 0, 64, 0) !== 64) {
      return false;
    }
    if (head[0] !== 0x7f || head[1] !== 0x45 || head[2] !== 0x4c || head[3] !== 0x46) {
      return false;
    }
    const wide = head[4] === 2;
    // Offsets of e_shoff, e_shentsize, e_shnum, e_shstrndx, and of sh_offset
    // and sh_size within one section header, for each word size.
    const [shoff, shentsize, shnum, shstrndx] = wide
      ? [Number(head.readBigUInt64LE(0x28)), head.readUInt16LE(0x3a),
         head.readUInt16LE(0x3c), head.readUInt16LE(0x3e)]
      : [head.readUInt32LE(0x20), head.readUInt16LE(0x2e),
         head.readUInt16LE(0x30), head.readUInt16LE(0x32)];
    if (!shoff || !shentsize || !shnum || shstrndx >= shnum) {
      return false;
    }
    const table = Buffer.alloc(shentsize * shnum);
    if (fs.readSync(handle, table, 0, table.length, shoff) !== table.length) {
      return false;
    }
    const header = shentsize * shstrndx;
    const [stroff, strsize] = wide
      ? [Number(table.readBigUInt64LE(header + 0x18)),
         Number(table.readBigUInt64LE(header + 0x20))]
      : [table.readUInt32LE(header + 0x10), table.readUInt32LE(header + 0x14)];
    // A name table far larger than any real one means a misread header, in
    // which case the file is left for the sweep to deal with.
    if (!strsize || strsize > 1 << 20) {
      return false;
    }
    const names = Buffer.alloc(strsize);
    if (fs.readSync(handle, names, 0, strsize, stroff) !== strsize) {
      return false;
    }
    const wanted = Buffer.from(`${name}\0`);
    for (let index = 0; index < shnum; index += 1) {
      const at = table.readUInt32LE(shentsize * index);
      if (names.indexOf(wanted, at) === at) {
        return true;
      }
    }
    return false;
  } catch (error) {
    return false;
  } finally {
    if (handle !== undefined) {
      try {
        fs.closeSync(handle);
      } catch (error) {
        // Nothing left to close cleanly.
      }
    }
  }
}

// Signs `file` in place, the way the daemon's own sweep does: sign to a
// sibling, replace the original with it, restore the executable bits the
// signing step drops.
function sign_file(file) {
  const out = `${file}.signed`;
  try {
    fs.unlinkSync(out);
  } catch (error) {
    // Nothing of ours was left behind.
  }
  const done = orig_spawn_sync(
    SIGN_TOOL,
    ['sign', '-inFile', file, '-outFile', out, '-selfSign', '1'],
    { stdio: 'ignore' },
  );
  if (!done || done.status !== 0) {
    return false;
  }
  fs.renameSync(out, file);
  try {
    fs.chmodSync(file, 0o775);
  } catch (error) {
    // Left with the mode the replacement was written under.
  }
  return true;
}

// The absolute path a spawn would run, or `null` when it is not a file this
// could sign: a shell command, an empty name, or one not on PATH.
function resolve_program(file) {
  if (typeof file !== 'string' || file.length === 0) {
    return null;
  }
  if (file.includes('/')) {
    return node_path.resolve(file);
  }
  for (const dir of (process.env.PATH || '').split(':')) {
    if (!dir) {
      continue;
    }
    const candidate = node_path.join(dir, file);
    try {
      if (fs.statSync(candidate).isFile()) {
        return candidate;
      }
    } catch (error) {
      // Not this one.
    }
  }
  return null;
}

// Signs the program a spawn is about to run, when it is an unsigned native
// binary of this session's. Never throws: a spawn is never held up by this.
function sign_before_spawn(file, options) {
  try {
    if (options && options.shell) {
      return;
    }
    const program = resolve_program(file);
    if (!program) {
      return;
    }
    if (signed_paths.has(program) || skipped_paths.has(program)) {
      return;
    }
    if (node_path.basename(program) === SIGN_TOOL || !within_scope(program)) {
      skipped_paths.add(program);
      return;
    }
    if (!starts_elf(program)) {
      skipped_paths.add(program);
      return;
    }
    if (elf_has_section(program, SIGN_SECTION)) {
      signed_paths.add(program);
      return;
    }
    const ok = sign_file(program);
    (ok ? signed_paths : skipped_paths).add(program);
    if (!ok) {
      trace(`sign failed ${program}`);
    }
  } catch (error) {
    // Left as it was; an unsigned binary reports its own error.
  }
}

// Whether signing is wanted at all: the tool that writes signatures is one
// this process can actually reach. That tool ships with the SDK of the one
// platform whose kernel checks them, so its presence is what "this host needs
// signed binaries" looks like from here -- not the platform name, which is not
// reliable: builds of Node for this same host have been seen reporting both
// `openharmony` and `linux`. Absent the tool, no spawn is touched at all: on a
// mainstream host the replacement below is never installed, and a session that
// has lost the tool from its PATH fails this one check instead of failing
// every spawn.
const host_needs_signing = resolve_program(SIGN_TOOL) !== null;

if (host_needs_signing) {
  child_process.spawn = function spawn(file, args, options) {
    sign_before_spawn(file, options);
    return orig_spawn.call(child_process, file, args, options);
  };

  child_process.spawnSync = function spawnSync(file, args, options) {
    sign_before_spawn(file, options);
    return orig_spawn_sync.call(child_process, file, args, options);
  };

  child_process.execFile = function execFile(file, args, options, callback) {
    // Both the argument list and the options are optional, so a callback can
    // turn up in either slot; the shapes are passed on exactly as they arrived.
    let list = args;
    let given = options;
    let settled = callback;
    if (typeof list === 'function') {
      settled = list;
      list = undefined;
    }
    if (typeof given === 'function') {
      settled = given;
      given = undefined;
    }
    sign_before_spawn(file, given);
    return orig_exec_file.call(child_process, file, list, given, settled);
  };

  child_process.execFileSync = function execFileSync(file, args, options) {
    sign_before_spawn(file, options);
    return orig_exec_file_sync.call(child_process, file, args, options);
  };
}
