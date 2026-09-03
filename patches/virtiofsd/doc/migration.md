Migration with virtio-fs
========================

virtiofsd supports migration through [vhost-user’s device state
interface](https://qemu-project.gitlab.io/qemu/interop/vhost-user.html#migrating-back-end-state),
allowing it to place internal state into the vhost-user front-end’s (e.g.
QEMU’s) migration stream.  This allows it to transfer information about files
and directories the guest has open to the destination instance.  It is very
important to note however that virtiofsd never migrates any data, i.e. source
and destination are expected to export shared directories with matching
contents (e.g. by using the same directory on the same filesystem).

If you do not care about any of the details, feel free to skip ahead to the
[section explaining recommended configurations](#recommended-configurations).

Filesystem State Requirements
-----------------------------

As just mentioned, virtiofsd does not migrate any filesystem data, and provides
no facilities to do so.  The user is responsible for ensuring that the shared
directories used by the source and destination instances of virtiofsd have the
same content.  Specifically, they must have the same content during switch-over,
once execution is stopped on the source, until it is resumed on the destination.

One way to achieve this is to use the same directory on the same filesystem for
both instances, e.g. by using a shared network filesystem.  If that is not
possible, the contents of the shared directory must be copied (outside of QEMU)
from the source to the destination during the switch-over phase.  This may be
reasonably feasible for a read-only use case, where copying can take place long
in advance of the actual migration.

Snapshots
---------

Because virtiofsd embeds its state into the front-end’s migration stream, it is
possible to store this stream somewhere to restore it later, i.e. in a
snapshot.  From a technical perspective, this is perfectly fine, but it must
again be stressed that virtiofsd’s state includes absolutely no data;
therefore, some mechanism outside of virtio-fs/virtiofsd must be used to ensure
that when restoring such a snapshot, the shared directory is in exactly the
same state as it was when the snapshot was taken.

What Needs to Be Migrated Anyway?
---------------------------------

For every file or directory that is open in the guest, virtiofsd has a
corresponding file descriptor (FD) open in the shared directory.  The
destination instance must restore these FDs, so the source instance must provide
instructions on how to do so.

The same applies to files and directories the guest does not really have open,
but still has their directory entries cached; through FUSE, the guest kernel can
reference all such cached entries by associated integer IDs.  Therefore,
virtiofsd needs to have an internal map that can convert each ID into
something that strongly references its associated filesystem object;
specifically, either an `O_PATH` FD or a file handle, depending on the
`--inode-file-handles` setting.  These too need to be transferred in some manner
to the destination.

Migration Modes
---------------

There are two general ways virtiofsd’s internal state can be serialized and
migrated, [by path](#by-path---migration-modefind-paths) or [as file
handles](#as-file-handles---migration-modefile-handles).

### By Path (`--migration-mode=find-paths`)

For every filesystem object that must be transferred to the destination,
virtiofsd tries to find its path inside of the shared directory, and transmits
that to the destination, which then opens it.

Because paths can change, this mode can be quite brittle.  virtiofsd begins
collecting paths once migration starts (long before the switch-over phase), so
any changes to those paths afterwards can lead to various problems, especially
if those changes are done by third parties outside of the VM guest.

Some examples for such changes are:

#### Unlinking

Files can exist without paths, specifically when they’re opened but unlinked.
Consequently, such files (that may be open in the guest) cannot be migrated
using paths.  When migrating anyway, the file contents will be lost once the
source instance is quit.

Note that for files for which virtiofsd cannot find a path, migration will
produce an error.  The error response behavior is controlled via the destination
instance’s `--migration-on-error` switch; `abort` will abort migration (on the
destination) when any error occurs, allowing execution to be resumed on the
source side, with any FD still open.  `guest-error` will continue migration,
marking any file that could not be migrated as faulty, returning errors for any
guest accesses.

#### Renaming / Moving

When files or directories are renamed or moved by the migrating guest, virtiofsd
is naturally aware of this, and so can update the paths it holds internally.

This is not the case when paths are changed outside of virtiofsd, by third
parties.  In this case, virtiofsd will remain unaware and will send the outdated
path to the destination, which will not be able to resolve it (error behavior is
then controlled by the `--migration-on-error` switch, as described in the
[Unlinking](#unlinking) section).

In contrast to the *unlinking* case, it would at least theoretically be possible
to migrate these files using their new paths, if virtiofsd somehow could get
notified of the rename/move.  The `--migration-confirm-paths` option has it
double-check each collected path at switch-over time, and so may be able to
detect such moves and renames in many cases (but does so on the source side, so
still has a non-empty TOCTTOU window).

#### Replacing

In the *renaming / moving* case, the worst thing that can happen is that a file
the guest has open is no longer accessible after migration.  A much worse case
is when a file is replaced without it being noticed: In this case, the
destination will open the other file, but present it as the old one to the
guest, with no error indication at all.  That can lead to data corruption.

The migration destination cannot detect this case without performing specific
checks, because opening the path it has received from the source will succeed
(but yield the wrong file).  Such checks are:

* `--migration-verify-handles`: With this switch, source and destination
  generate a file handle for each transferred path.  A file handle is a piece
  of data that uniquely identifies a filesystem object (like a file or
  directory), and becomes invalid (“stale”) when that object is deleted; so we
  can use it to verify a file’s identity between source and destination.
  However, it only works when source and destination use the same shared
  directory on the same filesystem (e.g. a network filesystem).  Furthermore,
  any mismatches that are detected cannot be recovered from (i.e. we still don’t
  know the involved files’ true paths, so `--migration-on-error` will decide how
  to proceed).
* `--migration-confirm-paths`: This switch makes the source instance
  double-check all paths during switch-over, i.e. when both the source and
  destination instance are stopped.  While this can theoretically allow error
  recovery (by fetching an updated path from */proc/self/fd*), and does not
  require source and destination to use the same filesystem, it still leaves a
  small TOCTTOU window open (between checking and the destination instance
  opening the paths), and it requires doing potentially quite a bit of I/O
  (checking paths) during migration downtime, which is generally not desirable.

Both switches can also be used together, but they can only be used in
*find-paths* migration mode, not *file-handles* (because they simply are not
necessary in *file-handles* mode).  Check the [dedicated section for more
information on recommended configurations](#recommended-configurations).

#### Implementation Detail: Collecting Paths

There are two ways paths can be collected, either by [looking up FDs in
*/proc/self/fd*](#querying-procselffd), or by [recursing through the shared
directory](#recursing-through-shared-directory).  virtiofsd implements both of
these, but only uses the latter as a fall-back for when the former fails.

##### Querying /proc/self/fd

*/proc/self/fd* contains a symbolic link for each file descriptor opened by the
current process.  These aren’t really symbolic links, though: Opening them does
not resolve their link target, but directly opens (basically duplicates) the
corresponding file descriptor.

Still, these links can have valid targets: The kernel tries to keep track
internally what paths the underlying filesystem objects have, and provides this
information there.  Querying this is thus a much faster way to get a path for
our file descriptors than to recurse through the shared directory.

The downside is that there is no formal guarantee that this works.  It is
unclear under what circumstances this can break down; if it does, virtiofsd will
fall back to [recursing through the shared
directory](#recursing-through-shared-directory).

For what it’s worth, the only case we have seen where a file has a valid path,
but */proc/self/fd* cannot provide it, is to use its file handle to open the
file, when it has not yet been opened through its path.  For example:

1. Open file using path
2. Generate and store file handle
3. Unmount file system, then mount it again
4. Open stored file handle

Something like this can happen with virtiofsd only on the migration destination
instance after a *file-handles* migration; in other cases, virtiofsd will
generally open files by path first, giving the kernel a chance to make a note of
that path.

##### Recursing Through Shared Directory

We can also obtain files’ paths by recursing through the shared directory,
enumerating all paths therein, and associating them with the respective files
and directories.  Naturally, this is quite slow, especially the more files there
are in the shared directory, which is why virtiofsd will only fall back to this
implementation if it fails to query a path from */proc/self/fd*.

### As File Handles (`--migration-mode=file-handles`)

Every filesystem object that must be transferred to the destination is converted
to a file handle (a piece of data that uniquely identifies this object on a
given filesystem, and can be used to open it), which is sent to the destination.
Because there is a unique and permanent relationship between such an object and
its file handle, this migration mode is not susceptible to the problems “by
path” migration has, for example, a file handle even stays valid when a file has
a link count of 0 (i.e. is deleted, has no path anymore) but some process still
has it open (i.e. holds an FD).

However, because file handles are just some data that allows access to
everything on a filesystem without checking e.g. access rights along a file’s
path, opening them requires the *DAC_READ_SEARCH* capability, which grants the
ability to read any file, regardless of its access mode.  Generally, this
capability is only available to applications running as root.

Furthermore, because file handles are specific to a given filesystem instance,
when using them for virtio-fs migration, the source and destination instance
must use the same shared directory on the same filesystem, e.g. a shared network
filesystem.

Recommended Configurations
--------------------------

### General

Consider which **`--migration-on-error`** mode suits your needs:

* `abort`: When any error is encountered (e.g. destination cannot find a file
  that is open in the guest), abort migration altogether.  You can then
  generally resume execution on the source; the source virtiofsd instance will
  retain all open file descriptors until it is quit.
* `guest-error`: When encountering errors pertaining to a specific file or
  directory, do not abort migration, but instead mark that file or directory as
  invalid.  Any guest accesses to it will then result in guest-visible errors.

### Shared Filesystems

When source and destination instance use the same shared directory on the same
filesystem, using **`--migration-mode=file-handles`** is recommended.  This
requires the destination instance to have the `DAC_READ_SEARCH` capability.

If that capability cannot be provided, we recommend using
**`--migration-mode=find-paths`** together with
**`--migration-verify-handles`**.  Using **`--migration-confirm-paths`**
additionally is optional; it can better recover from unexpected path changes
than `verify-handles` alone, but will prolong migration downtime.

### Different Filesystem

If the source and destination shared directory are not the exact same directory
on the same filesystem, users must ensure their contents are equal at migration
switch-over.  For example, read-only configuration directories presented to the
guest via virtio-fs can just be copied over to the destination ahead of
migration.

For such cases, use **`--migration-mode=find-paths`**.

We also recommend the filesystem to be read-only, which can be reinforced with
virtiofsd’s **`--readonly`** switch.  If that is not possible, *take special
care* to ensure source and destination directory contents match at the
switch-over point in time!

If, during migration, it is possible for the shared directory contents to be
modified by a party other than the migrating virtiofsd instance, we strongly
recommend using **`--migration-confirm-paths`**.  Still, that is not a 100 %
safe solution.  So above all, for the case where source and destination instance
do not use the same shared directory on the same (shared) filesystem, we
strongly advise not to allow the shared directory to be modified at all during
migration.
