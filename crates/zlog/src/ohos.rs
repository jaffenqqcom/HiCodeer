//! OHOS 平台日志输出：把 Rust `log::xxx!()` 记录投递到 hilog 系统日志服务。
//!
//! 参考 HMOS移植向导 06-logging.md 与 warp 的 `warp_logging::ohos` 实现。
//! 核心思路：`submit_to_hilog(record)` 把 `log::Record` 的级别映射成 hilog
//! `LogLevel` 后经 `OH_LOG_Print` 投递；zlog 的 `Zlog::log()` 在 OHOS 下
//! 直接调用本模块，跳过文件/stdout sink。
//!
//! 注意：本模块内不使用 `log::info!()` 等宏（会递归回 `Zlog::log()`），
//! 启动早期验证一律用 `direct_hilog_info` 直连。

use std::ffi::CStr;
use std::ffi::CString;
use std::os::raw::c_char;

use log::Record;

// ── HiLog NDK FFI（与 sysroot/usr/include/hilog/log.h 对齐）─────────────────

/// log_type：第三方应用固定用 LOG_APP（枚举值 0）。
const LOG_APP: i32 = 0;

/// hilog LogLevel 枚举值：DEBUG=3 / INFO=4 / WARN=5 / ERROR=6。
const LOG_LEVEL_DEBUG: i32 = 3;
const LOG_LEVEL_INFO: i32 = 4;
const LOG_LEVEL_WARN: i32 = 5;
const LOG_LEVEL_ERROR: i32 = 6;

/// 应用服务域名，0x0001 起可自定义。
const HILOG_DOMAIN: u32 = 0x0001;
/// hilog tag（31 字节以内），用于在设备日志里按应用聚合。
const HILOG_TAG: &CStr = c"HiCodeer";
/// 消息格式串：整条消息作为 public 参数明文投递。
const HILOG_FORMAT: &CStr = c"%{public}s";

#[link(name = "hilog_ndk.z")]
unsafe extern "C" {
    fn OH_LOG_Print(
        log_type: i32,
        log_level: i32,
        domain: u32,
        tag: *const c_char,
        fmt: *const c_char,
        ...
    ) -> i32;
}

/// 把一条 `log::Record` 投递到 hilog。
///
/// 由 zlog 的 `Zlog::log()` 在 OHOS 分支调用；级别映射与 hilog 枚举逐一对齐。
pub fn submit_to_hilog(record: &Record) {
    let hi_log_level = ohos_log_level(record.level());
    let message = record.args().to_string();
    let msg_cstr = match CString::new(message) {
        Ok(cstr) => cstr,
        Err(_) => {
            // 消息含内部空字节（概率极低），此时无更合适的日志渠道，直接跳过该条。
            return;
        }
    };
    unsafe {
        OH_LOG_Print(
            LOG_APP,
            hi_log_level,
            HILOG_DOMAIN,
            HILOG_TAG.as_ptr(),
            HILOG_FORMAT.as_ptr(),
            msg_cstr.as_ptr(),
        );
    }
}

/// 把 `log::Level` 映射到 hilog `LogLevel`：error=6 / warn=5 / info=4 / debug|trace=3。
fn ohos_log_level(level: log::Level) -> i32 {
    match level {
        log::Level::Error => LOG_LEVEL_ERROR,
        log::Level::Warn => LOG_LEVEL_WARN,
        log::Level::Info => LOG_LEVEL_INFO,
        log::Level::Debug | log::Level::Trace => LOG_LEVEL_DEBUG,
    }
}

/// 绕过 log 宏直连 hilog，用于启动早期诊断（此时全局 logger 可能尚未注册）。
///
/// 与 `log::info!()` 配合可区分两类故障：
/// - `direct_hilog_info` 出现、`log::info!()` 不出现 → 重定向未生效
/// - `direct_hilog_info` 也不出现 → hilog NDK 链接或 `OH_LOG_Print` FFI 本身有问题
pub fn direct_hilog_info(tag: &str, message: &str) {
    let tag_cstr = CString::new(tag).unwrap_or_else(|_| CString::new("HiCodeer").unwrap());
    let msg_cstr = CString::new(message).unwrap_or_else(|_| CString::new("").unwrap());
    unsafe {
        OH_LOG_Print(
            LOG_APP,
            LOG_LEVEL_INFO,
            HILOG_DOMAIN,
            tag_cstr.as_ptr(),
            HILOG_FORMAT.as_ptr(),
            msg_cstr.as_ptr(),
        );
    }
}
