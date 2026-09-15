/*
 * OpenHarmony TLS key shim (LD_PRELOAD).
 *
 * Why this exists
 * ---------------
 * On HarmonyOS the libc caps pthread keys at PTHREAD_KEYS_MAX (128) per
 * process. The aarch64-unknown-linux-ohos target specification does not enable
 * native thread-local storage (it sets `tls-model: emulated`, so
 * `target_thread_local` is false), which makes Rust's std implement
 * `thread_local!` on top of pthread keys -- one key per thread-local variable
 * (see library/std/src/sys/thread_local/os.rs and key/unix.rs in rust-src).
 *
 * `-Z tls-model=...` cannot help here: that flag only changes how native TLS
 * variables are code-generated, it does not change which std thread-local
 * implementation is used (that choice is baked into the precompiled std from
 * the target specification). Verified on device: local-dynamic, initial-exec
 * and local-exec all still import pthread_key_create and still abort.
 *
 * rustc, cargo, build scripts and every proc-macro dylib loaded into rustc all
 * accumulate keys in the same process, so a large workspace exhausts the 128
 * keys and std aborts with "fatal runtime error: out of TLS keys".
 *
 * What this shim does
 * -------------------
 * It interposes the four pthread TLS entry points and hands out a "virtual"
 * key space far larger than the real one:
 *   - keys >= VIRTUAL_KEY_BASE belong to the shim, keys below pass through;
 *   - a virtual key's value lives in a per-thread heap array which grows on
 *     demand and is anchored by a SINGLE real pthread key, so at most one of
 *     the real 128 keys is consumed for the whole process;
 *   - thread-exit destructors are emulated with musl's semantics (clear the
 *     slot before invoking the destructor, retry up to DESTRUCTOR_ITERATIONS
 *     times).
 * Verified on device: 200 and 2000 thread-locals run correctly, and 4 threads
 * x 200 Drop-typed thread-locals perform exactly 800 destructor calls.
 *
 * This shim only affects processes it is preloaded into (i.e. the build). It
 * does not change the compiled artifact.
 *
 * Build:  clang -shared -fPIC -O2 -o ohos-tls-shim.so ohos-tls-shim.c -ldl
 * Debug:  OHOS_TLS_SHIM_DEBUG=1 prints per-process key statistics to stderr.
 */

#define _GNU_SOURCE

#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* Keys at or above this value are virtual (owned by this shim). Real pthread
 * keys are small integers, so this range cannot collide with them. */
#define VIRTUAL_KEY_BASE 0x40000000u

/* Upper bound on the number of virtual keys, i.e. on thread_local variables
 * per process. Comfortably above anything rustc or the workspace needs. */
#define MAX_VIRTUAL_KEYS 65536u

/* Initial capacity of a thread's slot array; grows by doubling on demand. */
#define INITIAL_SLOT_CAPACITY 64u

/* musl retries the thread-specific destructor pass this many times. */
#define DESTRUCTOR_ITERATIONS 4

#define DEBUG_ENV_NAME "OHOS_TLS_SHIM_DEBUG"

/* Per-thread storage for virtual key values. Heap allocated so that it can
 * grow, and anchored by one real pthread key. */
typedef struct {
    void **slots;
    size_t capacity;
} TlsBlock;

static pthread_once_t g_init_once = PTHREAD_ONCE_INIT;

static pthread_key_t g_anchor_key;
static int (*g_real_key_create)(pthread_key_t *, void (*)(void *));
static void *(*g_real_key_get)(pthread_key_t);
static int (*g_real_key_set)(pthread_key_t, const void *);
static int (*g_real_key_delete)(pthread_key_t);

static void (*g_virtual_dtor[MAX_VIRTUAL_KEYS])(void *);
static unsigned g_virtual_key_count;
static int g_debug;

static void run_virtual_dtors(void *block_ptr);

/* Emitted before stdio is usable in some paths, so keep it to write(). */
static void log_line(const char *message) {
    size_t len = strlen(message);
    ssize_t ignored = write(STDERR_FILENO, message, len);
    (void)ignored;
}

static void fatal(const char *message) {
    log_line("[ohos-tls-shim] FATAL: ");
    log_line(message);
    log_line("\n");
    abort();
}

static void report_stats(void) {
    if (!g_debug) {
        return;
    }
    char buffer[128];
    int written = snprintf(buffer, sizeof(buffer),
                           "[ohos-tls-shim] pid=%d virtual_keys=%u\n",
                           (int)getpid(), g_virtual_key_count);
    if (written > 0) {
        ssize_t ignored = write(STDERR_FILENO, buffer, (size_t)written);
        (void)ignored;
    }
}

/* Runs once per process: resolves the real libc entry points and reserves the
 * single real anchor key whose destructor drives all virtual destructors. */
static void init_real(void) {
    g_debug = (getenv(DEBUG_ENV_NAME) != NULL);

    g_real_key_create = (int (*)(pthread_key_t *, void (*)(void *)))
        dlsym(RTLD_NEXT, "pthread_key_create");
    g_real_key_get = (void *(*)(pthread_key_t))
        dlsym(RTLD_NEXT, "pthread_getspecific");
    g_real_key_set = (int (*)(pthread_key_t, const void *))
        dlsym(RTLD_NEXT, "pthread_setspecific");
    g_real_key_delete = (int (*)(pthread_key_t))
        dlsym(RTLD_NEXT, "pthread_key_delete");

    if (g_real_key_create == NULL || g_real_key_get == NULL ||
        g_real_key_set == NULL || g_real_key_delete == NULL) {
        fatal("dlsym(RTLD_NEXT) on pthread entry points failed");
    }
    if (g_real_key_create(&g_anchor_key, run_virtual_dtors) != 0) {
        fatal("cannot reserve the anchor pthread key");
    }
    atexit(report_stats);
}

static void ensure_init(void) {
    pthread_once(&g_init_once, init_real);
}

/* Returns this thread's block, allocating it on first use when requested. */
static TlsBlock *block_get(int create) {
    TlsBlock *block = (TlsBlock *)g_real_key_get(g_anchor_key);
    if (block == NULL && create) {
        block = (TlsBlock *)calloc(1, sizeof(TlsBlock));
        if (block == NULL) {
            fatal("out of memory allocating a thread block");
        }
        if (g_real_key_set(g_anchor_key, block) != 0) {
            fatal("cannot store the thread block under the anchor key");
        }
    }
    return block;
}

static void slots_ensure(TlsBlock *block, unsigned index) {
    if (index < block->capacity) {
        return;
    }
    size_t capacity = block->capacity ? block->capacity : INITIAL_SLOT_CAPACITY;
    while (capacity <= index) {
        capacity *= 2;
    }
    void **slots = (void **)realloc(block->slots, capacity * sizeof(void *));
    if (slots == NULL) {
        fatal("out of memory growing the thread slot array");
    }
    memset(slots + block->capacity, 0,
           (capacity - block->capacity) * sizeof(void *));
    block->slots = slots;
    block->capacity = capacity;
}

static int is_virtual_key(pthread_key_t key) {
    return (unsigned)key >= VIRTUAL_KEY_BASE;
}

static unsigned virtual_index(pthread_key_t key) {
    return (unsigned)key - VIRTUAL_KEY_BASE;
}

void *pthread_getspecific(pthread_key_t key) {
    ensure_init();
    if (!is_virtual_key(key)) {
        return g_real_key_get(key);
    }
    TlsBlock *block = block_get(0);
    unsigned index = virtual_index(key);
    if (block == NULL || index >= block->capacity) {
        return NULL;
    }
    return block->slots[index];
}

int pthread_setspecific(pthread_key_t key, const void *value) {
    ensure_init();
    if (!is_virtual_key(key)) {
        return g_real_key_set(key, value);
    }
    TlsBlock *block = block_get(1);
    unsigned index = virtual_index(key);
    slots_ensure(block, index);
    block->slots[index] = (void *)value;
    return 0;
}

int pthread_key_create(pthread_key_t *key, void (*dtor)(void *)) {
    ensure_init();
    unsigned index = __atomic_fetch_add(&g_virtual_key_count, 1, __ATOMIC_RELAXED);
    if (index >= MAX_VIRTUAL_KEYS) {
        /* Exhausting the virtual space is a shim bug, not a caller error. */
        return EAGAIN;
    }
    __atomic_store_n(&g_virtual_dtor[index], dtor, __ATOMIC_RELEASE);
    *key = (pthread_key_t)(VIRTUAL_KEY_BASE + index);
    return 0;
}

int pthread_key_delete(pthread_key_t key) {
    ensure_init();
    if (!is_virtual_key(key)) {
        return g_real_key_delete(key);
    }
    unsigned index = virtual_index(key);
    if (index < MAX_VIRTUAL_KEYS) {
        __atomic_store_n(&g_virtual_dtor[index], (void (*)(void *))NULL,
                         __ATOMIC_RELEASE);
    }
    return 0;
}

/* Invoked by libc when a thread exits. Emulates musl's thread-specific
 * destructor pass over the virtual keys. */
static void run_virtual_dtors(void *block_ptr) {
    TlsBlock *block = (TlsBlock *)block_ptr;
    if (block == NULL) {
        return;
    }
    /* Keep the block reachable so destructors that touch other thread-locals
     * reuse it instead of allocating a second block. */
    g_real_key_set(g_anchor_key, block);

    unsigned count = __atomic_load_n(&g_virtual_key_count, __ATOMIC_ACQUIRE);
    for (int iteration = 0; iteration < DESTRUCTOR_ITERATIONS; iteration++) {
        int pending = 0;
        for (unsigned index = 0; index < count && index < block->capacity;
             index++) {
            void *value = block->slots[index];
            if (value == NULL) {
                continue;
            }
            void (*dtor)(void *) =
                __atomic_load_n(&g_virtual_dtor[index], __ATOMIC_ACQUIRE);
            /* The OS clears the slot before invoking the destructor. */
            block->slots[index] = NULL;
            if (dtor != NULL) {
                dtor(value);
            }
            if (block->slots[index] != NULL) {
                pending = 1;
            }
        }
        if (!pending) {
            break;
        }
    }

    g_real_key_set(g_anchor_key, NULL);
    free(block->slots);
    free(block);
}
