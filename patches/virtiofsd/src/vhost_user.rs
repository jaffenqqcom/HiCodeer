// Copyright 2019 Intel Corporation. All Rights Reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

use std::convert::TryInto;
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::{convert, error, fmt, io, process};

use futures::executor::{ThreadPool, ThreadPoolBuilder};
use log::*;

use vhost::vhost_user::message::*;
use vhost::vhost_user::Backend;
use vhost_user_backend::bitmap::BitmapMmapRegion;
use vhost_user_backend::{VhostUserBackend, VringMutex, VringState, VringT};
use virtio_bindings::bindings::virtio_config::*;
use virtio_bindings::bindings::virtio_ring::{
    VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC,
};
use virtio_queue::{DescriptorChain, QueueOwnedT};
use vm_memory::{
    ByteValued, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryLoadGuard, GuestMemoryMmap, Le32,
};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::event::{
    new_event_consumer_and_notifier, EventConsumer, EventFlag, EventNotifier,
};

use crate::descriptor_utils::{Error as VufDescriptorError, Reader, Writer};
use crate::filesystem::{FileSystem, SerializableFileSystem};
use crate::server::Server;
use crate::util::other_io_error;
use crate::Error as VhostUserFsError;

type LoggedMemory = GuestMemoryMmap<BitmapMmapRegion>;
type LoggedMemoryAtomic = GuestMemoryAtomic<LoggedMemory>;

const QUEUE_SIZE: usize = 32768;
// The spec allows for multiple request queues. We currently only support one.
const REQUEST_QUEUES: u32 = 1;
// In addition to the request queue there is one high-prio queue.
// Since VIRTIO_FS_F_NOTIFICATION is not advertised we do not have a
// notification queue.
const NUM_QUEUES: usize = REQUEST_QUEUES as usize + 1;

// The guest queued an available buffer for the high priority queue.
const HIPRIO_QUEUE_EVENT: u16 = 0;
// The guest queued an available buffer for the request queue.
const REQ_QUEUE_EVENT: u16 = 1;

/// The maximum length of the tag being used.
pub const MAX_TAG_LEN: usize = 36;

type Result<T> = std::result::Result<T, Error>;

// The compiler warns that some wrapped values are never read, but they are in fact read by
// `<Error as fmt::Display>::fmt()` via the derived `Debug`.
#[allow(dead_code)]
#[derive(Debug)]
pub enum Error {
    /// Failed to create kill eventfd.
    CreateKillEventFd(io::Error),
    /// Failed to create thread pool.
    CreateThreadPool(io::Error),
    /// Failed to handle event other than input event.
    HandleEventNotEpollIn,
    /// Failed to handle unknown event.
    HandleEventUnknownEvent,
    /// Iterating through the queue failed.
    IterateQueue,
    /// No memory configured.
    NoMemoryConfigured,
    /// Processing queue failed.
    ProcessQueue(VhostUserFsError),
    /// Creating a queue reader failed.
    QueueReader(VufDescriptorError),
    /// Creating a queue writer failed.
    QueueWriter(VufDescriptorError),
    /// The unshare(CLONE_FS) call failed.
    UnshareCloneFs(io::Error),
    /// Invalid tag name
    InvalidTag,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use self::Error::UnshareCloneFs;
        match self {
            UnshareCloneFs(error) => {
                write!(
                    f,
                    "The unshare(CLONE_FS) syscall failed with '{error}'. \
                    If running in a container please check that the container \
                    runtime seccomp policy allows unshare."
                )
            }
            Self::InvalidTag => write!(
                f,
                "The tag may not be empty or longer than {MAX_TAG_LEN} bytes (encoded as UTF-8)."
            ),
            Self::QueueReader(e) => write!(f, "Failed to create a queue reader: {e}"),
            Self::QueueWriter(e) => write!(f, "Failed to create a queue writer: {e}"),
            Self::ProcessQueue(e) => write!(f, "Failed to handle an incoming request: {e}"),
            _ => write!(f, "{self:?}"),
        }
    }
}

impl error::Error for Error {}

impl convert::From<Error> for io::Error {
    fn from(e: Error) -> Self {
        other_io_error(e)
    }
}

struct VhostUserFsThread<F: FileSystem + Send + Sync + 'static> {
    mem: Option<LoggedMemoryAtomic>,
    server: Arc<Server<F>>,
    // handle request from backend to frontend
    vu_req: Option<Backend>,
    event_idx: bool,
    pool: Option<ThreadPool>,
}

impl<F: FileSystem + SerializableFileSystem + Send + Sync + 'static> VhostUserFsThread<F> {
    fn new(fs: F, thread_pool_size: usize) -> Result<Self> {
        let pool = if thread_pool_size > 0 {
            // Test that unshare(CLONE_FS) works, it will be called for each thread.
            // It's an unprivileged system call but some Docker/Moby versions are
            // known to reject it via seccomp when CAP_SYS_ADMIN is not given.
            //
            // Note that the program is single-threaded here so this syscall has no
            // visible effect and is safe to make.
            let ret = unsafe { libc::unshare(libc::CLONE_FS) };
            if ret == -1 {
                return Err(Error::UnshareCloneFs(std::io::Error::last_os_error()));
            }

            Some(
                ThreadPoolBuilder::new()
                    .after_start(|_| {
                        // unshare FS for xattr operation
                        let ret = unsafe { libc::unshare(libc::CLONE_FS) };
                        assert_eq!(ret, 0); // Should not fail
                    })
                    .pool_size(thread_pool_size)
                    .create()
                    .map_err(Error::CreateThreadPool)?,
            )
        } else {
            None
        };

        Ok(VhostUserFsThread {
            mem: None,
            server: Arc::new(Server::new(fs)),
            vu_req: None,
            event_idx: false,
            pool,
        })
    }

    fn return_descriptor(
        vring_state: &mut VringState<LoggedMemoryAtomic>,
        head_index: u16,
        event_idx: bool,
        len: usize,
    ) {
        let used_len: u32 = match len.try_into() {
            Ok(l) => l,
            Err(_) => panic!("Invalid used length, can't return used descritors to the ring"),
        };

        if vring_state.add_used(head_index, used_len).is_err() {
            warn!("Couldn't return used descriptors to the ring");
        }

        if event_idx {
            match vring_state.needs_notification() {
                Err(_) => {
                    warn!("Couldn't check if queue needs to be notified");
                    vring_state.signal_used_queue().unwrap();
                }
                Ok(needs_notification) => {
                    if needs_notification {
                        vring_state.signal_used_queue().unwrap();
                    }
                }
            }
        } else {
            vring_state.signal_used_queue().unwrap();
        }
    }

    fn process_message(
        server: &Server<F>,
        mem: &GuestMemoryLoadGuard<LoggedMemory>,
        chain: DescriptorChain<GuestMemoryLoadGuard<LoggedMemory>>,
        vu_req: Option<&mut Backend>,
    ) -> Result<usize> {
        let reader = Reader::new(mem, chain.clone()).map_err(Error::QueueReader)?;
        let writer = Writer::new(mem, chain).map_err(Error::QueueWriter)?;

        server
            .handle_message(reader, writer, vu_req)
            .map_err(Error::ProcessQueue)
    }

    fn process_queue_pool(&self, vring: VringMutex<LoggedMemoryAtomic>) -> Result<bool> {
        let mut used_any = false;
        let atomic_mem = match &self.mem {
            Some(m) => m,
            None => return Err(Error::NoMemoryConfigured),
        };

        while let Some(avail_desc) = vring
            .get_mut()
            .get_queue_mut()
            .iter(atomic_mem.memory())
            .map_err(|_| Error::IterateQueue)?
            .next()
        {
            used_any = true;

            // Prepare a set of objects that can be moved to the worker thread.
            let atomic_mem = atomic_mem.clone();
            let server = self.server.clone();
            let mut vu_req = self.vu_req.clone();
            let event_idx = self.event_idx;
            let worker_vring = vring.clone();
            let worker_desc = avail_desc.clone();

            self.pool.as_ref().unwrap().spawn_ok(async move {
                let mem = atomic_mem.memory();
                let head_index = worker_desc.head_index();

                let len = Self::process_message(&server, &mem, worker_desc, vu_req.as_mut())
                    .unwrap_or_else(|e| {
                        error!("{e}");
                        process::exit(1);
                    });

                Self::return_descriptor(&mut worker_vring.get_mut(), head_index, event_idx, len);
            });
        }

        Ok(used_any)
    }

    fn process_queue_serial(
        &self,
        vring_state: &mut VringState<LoggedMemoryAtomic>,
    ) -> Result<bool> {
        let mut used_any = false;
        let mem = match &self.mem {
            Some(m) => m.memory(),
            None => return Err(Error::NoMemoryConfigured),
        };
        let mut vu_req = self.vu_req.clone();

        let avail_chains: Vec<DescriptorChain<GuestMemoryLoadGuard<LoggedMemory>>> = vring_state
            .get_queue_mut()
            .iter(mem.clone())
            .map_err(|_| Error::IterateQueue)?
            .collect();

        for chain in avail_chains {
            used_any = true;

            let head_index = chain.head_index();

            let len = Self::process_message(&self.server, &mem, chain, vu_req.as_mut())
                .unwrap_or_else(|e| {
                    error!("{e}");
                    process::exit(1);
                });

            Self::return_descriptor(vring_state, head_index, self.event_idx, len);
        }

        Ok(used_any)
    }

    fn handle_event_pool(
        &self,
        device_event: u16,
        vrings: &[VringMutex<LoggedMemoryAtomic>],
    ) -> io::Result<()> {
        let idx = match device_event {
            HIPRIO_QUEUE_EVENT => {
                debug!("HIPRIO_QUEUE_EVENT");
                0
            }
            REQ_QUEUE_EVENT => {
                debug!("QUEUE_EVENT");
                1
            }
            _ => return Err(Error::HandleEventUnknownEvent.into()),
        };

        if self.event_idx {
            // vm-virtio's Queue implementation only checks avail_index
            // once, so to properly support EVENT_IDX we need to keep
            // calling process_queue() until it stops finding new
            // requests on the queue.
            loop {
                vrings[idx].disable_notification().unwrap();
                // we can't recover from an error here, so let's hope it's transient
                if let Err(e) = self.process_queue_pool(vrings[idx].clone()) {
                    error!("processing the vring {idx}: {e}");
                }
                if !vrings[idx].enable_notification().unwrap() {
                    break;
                }
            }
        } else {
            // Without EVENT_IDX, a single call is enough.
            self.process_queue_pool(vrings[idx].clone())?;
        }

        Ok(())
    }

    fn handle_event_serial(
        &self,
        device_event: u16,
        vrings: &[VringMutex<LoggedMemoryAtomic>],
    ) -> io::Result<()> {
        let mut vring_state = match device_event {
            HIPRIO_QUEUE_EVENT => {
                debug!("HIPRIO_QUEUE_EVENT");
                vrings[0].get_mut()
            }
            REQ_QUEUE_EVENT => {
                debug!("QUEUE_EVENT");
                vrings[1].get_mut()
            }
            _ => return Err(Error::HandleEventUnknownEvent.into()),
        };

        if self.event_idx {
            // vm-virtio's Queue implementation only checks avail_index
            // once, so to properly support EVENT_IDX we need to keep
            // calling process_queue() until it stops finding new
            // requests on the queue.
            loop {
                vring_state.disable_notification().unwrap();
                // we can't recover from an error here, so let's hope it's transient
                if let Err(e) = self.process_queue_serial(&mut vring_state) {
                    error!("processing the vring: {e}");
                }
                if !vring_state.enable_notification().unwrap() {
                    break;
                }
            }
        } else {
            // Without EVENT_IDX, a single call is enough.
            self.process_queue_serial(&mut vring_state)?;
        }

        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioFsConfig {
    tag: [u8; MAX_TAG_LEN],
    num_request_queues: Le32,
}

// vm-memory needs a Default implementation even though these values are never
// used anywhere...
impl Default for VirtioFsConfig {
    fn default() -> Self {
        Self {
            tag: [0; MAX_TAG_LEN],
            num_request_queues: Le32::default(),
        }
    }
}

unsafe impl ByteValued for VirtioFsConfig {}

struct PremigrationThread {
    handle: JoinHandle<()>,
    cancel: Arc<AtomicBool>,
}

/// A builder for configurable creation of [`VhostUserFsBackend`] objects.
#[derive(Debug, Default)]
pub struct VhostUserFsBackendBuilder {
    thread_pool_size: usize,
    tag: Option<String>,
}

impl VhostUserFsBackendBuilder {
    /// Adjust the size of the thread pool to use.
    ///
    /// A value of `0` disables the usage of a thread pool.
    pub fn set_thread_pool_size(mut self, size: usize) -> Self {
        self.thread_pool_size = size;
        self
    }

    /// Set the tag to use for the file system.
    ///
    /// The tag length must not exceed [`MAX_TAG_LEN`] bytes.
    pub fn set_tag(mut self, tag: Option<String>) -> Self {
        self.tag = tag;
        self
    }

    /// Build the [`VhostUserFsBackend`] object.
    pub fn build<F>(self, fs: F) -> Result<VhostUserFsBackend<F>>
    where
        F: FileSystem + SerializableFileSystem + Send + Sync + 'static,
    {
        let thread = RwLock::new(VhostUserFsThread::new(fs, self.thread_pool_size)?);
        Ok(VhostUserFsBackend {
            thread,
            premigration_thread: None.into(),
            migration_thread: None.into(),
            tag: self.tag,
        })
    }
}

pub struct VhostUserFsBackend<F: FileSystem + SerializableFileSystem + Send + Sync + 'static> {
    thread: RwLock<VhostUserFsThread<F>>,
    premigration_thread: Mutex<Option<PremigrationThread>>,
    migration_thread: Mutex<Option<JoinHandle<io::Result<()>>>>,
    tag: Option<String>,
}

impl<F: FileSystem + SerializableFileSystem + Send + Sync + 'static> VhostUserFsBackend<F> {
    /// Create a [`VhostUserFsBackend`] without a thread pool or a tag.
    ///
    /// For more configurable creation refer to
    /// [`VhostUserFsBackendBuilder`].
    pub fn new(fs: F) -> Result<Self> {
        VhostUserFsBackendBuilder::default().build(fs)
    }
}

impl<F: FileSystem + SerializableFileSystem + Send + Sync + 'static> VhostUserBackend
    for VhostUserFsBackend<F>
{
    type Bitmap = BitmapMmapRegion;
    type Vring = VringMutex<LoggedMemoryAtomic>;

    fn num_queues(&self) -> usize {
        NUM_QUEUES
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        (1 << VIRTIO_F_VERSION_1)
            | (1 << VIRTIO_RING_F_INDIRECT_DESC)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
            | VhostUserVirtioFeatures::LOG_ALL.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        let mut protocol_features = VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::BACKEND_REQ
            | VhostUserProtocolFeatures::BACKEND_SEND_FD
            | VhostUserProtocolFeatures::REPLY_ACK
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
            | VhostUserProtocolFeatures::LOG_SHMFD
            | VhostUserProtocolFeatures::DEVICE_STATE
            | VhostUserProtocolFeatures::RESET_DEVICE;

        if self.tag.is_some() {
            protocol_features |= VhostUserProtocolFeatures::CONFIG;
        }

        protocol_features
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        // virtio spec 1.2, 5.11.4:
        //   The tag is encoded in UTF-8 and padded with NUL bytes if shorter than
        //   the available space. This field is not NUL-terminated if the encoded
        //   bytes take up the entire field.
        // The length was already checked when parsing the arguments. Hence, we
        // only assert that everything looks sane and pad with NUL bytes to the
        // fixed length.
        let tag = self.tag.as_ref().expect("Did not expect read of config if tag is not set. We do not advertise F_CONFIG in that case!");
        assert!(tag.len() <= MAX_TAG_LEN, "too long tag length");
        assert!(!tag.is_empty(), "tag should not be empty");
        let mut fixed_len_tag = [0; MAX_TAG_LEN];
        fixed_len_tag[0..tag.len()].copy_from_slice(tag.as_bytes());

        let config = VirtioFsConfig {
            tag: fixed_len_tag,
            num_request_queues: Le32::from(REQUEST_QUEUES),
        };

        let offset = offset as usize;
        let size = size as usize;
        let mut result: Vec<_> = config
            .as_slice()
            .iter()
            .skip(offset)
            .take(size)
            .copied()
            .collect();
        // pad with 0s up to `size`
        result.resize(size, 0);
        result
    }

    fn acked_features(&self, features: u64) {
        if features & VhostUserVirtioFeatures::LOG_ALL.bits() != 0 {
            // F_LOG_ALL set: Prepare for migration (unless we're already doing that)
            let mut premigration_thread = self.premigration_thread.lock().unwrap();
            if premigration_thread.is_none() {
                let cancel = Arc::new(AtomicBool::new(false));
                let cloned_server = Arc::clone(&self.thread.read().unwrap().server);
                let cloned_cancel = Arc::clone(&cancel);
                let handle =
                    thread::spawn(move || cloned_server.prepare_serialization(cloned_cancel));
                *premigration_thread = Some(PremigrationThread { handle, cancel });
            }
        } else {
            // F_LOG_ALL cleared: Migration cancelled, if any was ongoing
            // (Note that this is our interpretation, and not said by the specification.  The back
            // end might clear this flag also on the source side once the VM has been stopped, even
            // before we receive SET_DEVICE_STATE_FD.  QEMU will clear F_LOG_ALL only when the VM
            // is running, i.e. when the source resumes after a cancelled migration, which is
            // exactly what we want, but it would be better if we had a more reliable way that is
            // backed up by the spec.  We could delay cancelling until we receive a guest request
            // while F_LOG_ALL is cleared, but that can take an indefinite amount of time.)
            if let Some(premigration_thread) = self.premigration_thread.lock().unwrap().take() {
                premigration_thread.cancel.store(true, Ordering::Relaxed);
                // Ignore the result, we are cancelling anyway
                let _ = premigration_thread.handle.join();
            }
        }
    }

    fn reset_device(&self) {
        // Clear our device state
        self.thread.write().unwrap().server.destroy();
    }

    fn set_event_idx(&self, enabled: bool) {
        self.thread.write().unwrap().event_idx = enabled;
    }

    fn update_memory(&self, mem: LoggedMemoryAtomic) -> io::Result<()> {
        self.thread.write().unwrap().mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &self,
        device_event: u16,
        evset: EventSet,
        vrings: &[VringMutex<LoggedMemoryAtomic>],
        _thread_id: usize,
    ) -> io::Result<()> {
        if evset != EventSet::IN {
            return Err(Error::HandleEventNotEpollIn.into());
        }

        let thread = self.thread.read().unwrap();

        if thread.pool.is_some() {
            thread.handle_event_pool(device_event, vrings)
        } else {
            thread.handle_event_serial(device_event, vrings)
        }
    }

    fn exit_event(&self, _thread_index: usize) -> Option<(EventConsumer, EventNotifier)> {
        Some(
            new_event_consumer_and_notifier(EventFlag::NONBLOCK)
                .expect("Failed to create exit notifier"),
        )
    }

    fn set_backend_req_fd(&self, vu_req: Backend) {
        self.thread.write().unwrap().vu_req = Some(vu_req);
    }

    fn set_device_state_fd(
        &self,
        direction: VhostTransferStateDirection,
        phase: VhostTransferStatePhase,
        file: File,
    ) -> io::Result<Option<File>> {
        // Our caller (vhost-user-backend crate) pretty much ignores error objects we return (only
        // cares whether we succeed or not), so log errors here
        if let Err(err) = self.do_set_device_state_fd(direction, phase, file) {
            error!("Failed to initiate state (de-)serialization: {err}");
            return Err(err);
        }
        Ok(None)
    }

    fn check_device_state(&self) -> io::Result<()> {
        // Our caller (vhost-user-backend crate) pretty much ignores error objects we return (only
        // cares whether we succeed or not), so log errors here
        if let Err(err) = self.do_check_device_state() {
            error!("Migration failed: {err}");
            return Err(err);
        }
        Ok(())
    }
}

impl<F: FileSystem + SerializableFileSystem + Send + Sync + 'static> VhostUserFsBackend<F> {
    fn do_set_device_state_fd(
        &self,
        direction: VhostTransferStateDirection,
        phase: VhostTransferStatePhase,
        file: File,
    ) -> io::Result<()> {
        if phase != VhostTransferStatePhase::STOPPED {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Transfer in phase {phase:?} is not supported"),
            ));
        }

        let server = Arc::clone(&self.thread.read().unwrap().server);
        let join_handle = match direction {
            VhostTransferStateDirection::SAVE => {
                // We should have a premigration thread that was started with `F_LOG_ALL`.  It
                // should already be finished, but you never know.
                let premigration_thread = self.premigration_thread.lock().unwrap().take();

                thread::spawn(move || {
                    if let Some(premigration_thread) = premigration_thread {
                        // Let’s hope it’s finished.  Otherwise, we block migration downtime for a
                        // bit longer, but there’s nothing we can do.
                        premigration_thread.handle.join().map_err(|_| {
                            other_io_error(
                                "Failed to finalize serialization preparation".to_string(),
                            )
                        })?;
                    } else {
                        // If we don’t have a premigration thread, that either means migration was
                        // cancelled at some point (i.e. F_LOG_ALL cleared; very unlikely and we
                        // consider sending SET_DEVICE_STATE_FD afterwards a protocol violation),
                        // or that there simply was no F_LOG_ALL at all.  QEMU doesn’t necessarily
                        // do memory logging when snapshotting, and in such cases we have no choice
                        // but to just run preserialization now.
                        warn!(
                            "Front-end did not announce migration to begin, so we failed to \
                            prepare for it; collecting data now.  If you are doing a snapshot, \
                            that is OK; otherwise, migration downtime may be prolonged."
                        );
                        server.prepare_serialization(Arc::new(AtomicBool::new(false)));
                    }

                    server
                        .serialize(file)
                        .map_err(|e| io::Error::new(e.kind(), format!("Failed to save state: {e}")))
                })
            }

            VhostTransferStateDirection::LOAD => {
                if let Some(premigration_thread) = self.premigration_thread.lock().unwrap().take() {
                    // Strange, but OK
                    premigration_thread.cancel.store(true, Ordering::Relaxed);
                    warn!("Cancelling serialization preparation because of incoming migration");
                    let _ = premigration_thread.handle.join();
                }

                thread::spawn(move || {
                    server
                        .deserialize_and_apply(file)
                        .map_err(|e| io::Error::new(e.kind(), format!("Failed to load state: {e}")))
                })
            }
        };

        *self.migration_thread.lock().unwrap() = Some(join_handle);

        Ok(())
    }

    fn do_check_device_state(&self) -> io::Result<()> {
        let Some(migration_thread) = self.migration_thread.lock().unwrap().take() else {
            // `check_device_state()` must follow a successful `set_device_state_fd()`, so this is
            // a protocol violation
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Front-end attempts to check migration state, but no migration has been done",
            ));
        };

        migration_thread
            .join()
            .map_err(|_| other_io_error("Failed to join the migration thread"))?
    }
}
