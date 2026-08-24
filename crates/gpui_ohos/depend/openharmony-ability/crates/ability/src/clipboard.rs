//! OHOS system clipboard capability via the NDK C API.
//!
//! Wraps `libpasteboard` (`OH_Pasteboard_*`) and `libudmf` (`OH_UdmfData_*`) so Rust can read and
//! write the system clipboard directly, without the ArkTS `@ohos.pasteboard` permission gate.
//! Writing needs no permission; reading mirrors what warp-ohos does (`OH_Pasteboard_GetData`) and
//! does not depend on `ohos.permission.READ_PASTEBOARD`.

use std::ffi::{c_char, c_int, c_void, CStr, CString};

/// libpasteboard.so (API 13+).
#[link(name = "pasteboard")]
unsafe extern "C" {
    fn OH_Pasteboard_Create() -> *mut c_void;
    fn OH_Pasteboard_Destroy(pasteboard: *mut c_void);
    fn OH_Pasteboard_GetData(pasteboard: *mut c_void, status: *mut c_int) -> *mut c_void;
    fn OH_Pasteboard_SetData(pasteboard: *mut c_void, data: *mut c_void) -> c_int;
}

/// libudmf.so (API 12+).
#[link(name = "udmf")]
unsafe extern "C" {
    fn OH_UdsPlainText_Create() -> *mut c_void;
    fn OH_UdsPlainText_Destroy(plain_text: *mut c_void);
    fn OH_UdsPlainText_GetContent(plain_text: *mut c_void) -> *const c_char;
    fn OH_UdsPlainText_SetContent(plain_text: *mut c_void, content: *const c_char) -> c_int;

    fn OH_UdmfRecord_Create() -> *mut c_void;
    fn OH_UdmfRecord_Destroy(record: *mut c_void);
    fn OH_UdmfRecord_AddPlainText(record: *mut c_void, plain_text: *mut c_void) -> c_int;

    fn OH_UdmfData_Create() -> *mut c_void;
    fn OH_UdmfData_Destroy(data: *mut c_void);
    fn OH_UdmfData_AddRecord(data: *mut c_void, record: *mut c_void) -> c_int;
    fn OH_UdmfData_GetPrimaryPlainText(data: *mut c_void, plain_text: *mut c_void) -> c_int;
}

/// Write `text` to the system clipboard. Returns `false` on any NDK failure.
pub fn write_text(text: &str) -> bool {
    unsafe {
        let pasteboard = OH_Pasteboard_Create();
        if pasteboard.is_null() {
            log::error!("clipboard::write_text: OH_Pasteboard_Create failed");
            return false;
        }
        let result = write_text_inner(pasteboard, text);
        OH_Pasteboard_Destroy(pasteboard);
        result
    }
}

unsafe fn write_text_inner(pasteboard: *mut c_void, text: &str) -> bool {
    let plain_text = OH_UdsPlainText_Create();
    if plain_text.is_null() {
        log::error!("clipboard::write_text: OH_UdsPlainText_Create failed");
        return false;
    }

    let mut ok = false;
    if let Ok(content) = CString::new(text) {
        ok = OH_UdsPlainText_SetContent(plain_text, content.as_ptr()) == 0;
    }
    if !ok {
        log::error!("clipboard::write_text: OH_UdsPlainText_SetContent failed");
        OH_UdsPlainText_Destroy(plain_text);
        return false;
    }

    let record = OH_UdmfRecord_Create();
    if record.is_null() {
        log::error!("clipboard::write_text: OH_UdmfRecord_Create failed");
        OH_UdsPlainText_Destroy(plain_text);
        return false;
    }
    if OH_UdmfRecord_AddPlainText(record, plain_text) != 0 {
        log::error!("clipboard::write_text: OH_UdmfRecord_AddPlainText failed");
        OH_UdmfRecord_Destroy(record);
        OH_UdsPlainText_Destroy(plain_text);
        return false;
    }

    let data = OH_UdmfData_Create();
    if data.is_null() {
        log::error!("clipboard::write_text: OH_UdmfData_Create failed");
        OH_UdmfRecord_Destroy(record);
        OH_UdsPlainText_Destroy(plain_text);
        return false;
    }
    if OH_UdmfData_AddRecord(data, record) != 0 {
        log::error!("clipboard::write_text: OH_UdmfData_AddRecord failed");
        OH_UdmfData_Destroy(data);
        OH_UdmfRecord_Destroy(record);
        OH_UdsPlainText_Destroy(plain_text);
        return false;
    }

    let result = OH_Pasteboard_SetData(pasteboard, data) == 0;
    if !result {
        log::error!("clipboard::write_text: OH_Pasteboard_SetData failed");
    }

    OH_UdmfData_Destroy(data);
    OH_UdmfRecord_Destroy(record);
    OH_UdsPlainText_Destroy(plain_text);
    result
}

/// Read plain text from the system clipboard. Returns `None` when read fails or no plain text is
/// present.
pub fn read_text() -> Option<String> {
    unsafe {
        let pasteboard = OH_Pasteboard_Create();
        if pasteboard.is_null() {
            log::error!("clipboard::read_text: OH_Pasteboard_Create failed");
            return None;
        }
        let result = read_text_inner(pasteboard);
        OH_Pasteboard_Destroy(pasteboard);
        result
    }
}

unsafe fn read_text_inner(pasteboard: *mut c_void) -> Option<String> {
    let mut status: c_int = 0;
    let data = OH_Pasteboard_GetData(pasteboard, &mut status);
    if data.is_null() || status != 0 {
        log::warn!("clipboard::read_text: OH_Pasteboard_GetData status={status}");
        if !data.is_null() {
            OH_UdmfData_Destroy(data);
        }
        return None;
    }

    let plain_text = OH_UdsPlainText_Create();
    if plain_text.is_null() {
        log::error!("clipboard::read_text: OH_UdsPlainText_Create failed");
        OH_UdmfData_Destroy(data);
        return None;
    }

    let text = if OH_UdmfData_GetPrimaryPlainText(data, plain_text) == 0 {
        let content = OH_UdsPlainText_GetContent(plain_text);
        if content.is_null() {
            None
        } else {
            Some(CStr::from_ptr(content).to_string_lossy().into_owned())
        }
    } else {
        log::warn!("clipboard::read_text: OH_UdmfData_GetPrimaryPlainText failed");
        None
    };

    OH_UdsPlainText_Destroy(plain_text);
    OH_UdmfData_Destroy(data);
    text
}
