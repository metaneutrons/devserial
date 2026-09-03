// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Native platform integration.
//!
//! All foreign function calls in the crate live here, behind safe wrappers, so
//! that the rest of the code contains no `unsafe` and no `#[cfg(target_os)]`
//! branches for native menus. Every platform provides the same surface; on
//! platforms without native integration the wrappers are inert.

/// State of the focused widget, mirrored into the native edit menu.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
// The shape mirrors the seven independent menu items of the native Edit menu.
#[allow(clippy::struct_excessive_bools)]
pub struct EditState {
    /// A text input currently has keyboard focus.
    pub text_focused: bool,
    /// The focused text input has a selection.
    pub text_selection: bool,
    /// The focused text input is non-empty.
    pub text_not_empty: bool,
    /// The capture buffer contains at least one line.
    pub buffer_not_empty: bool,
    /// Lines are selected in the capture buffer.
    pub buffer_selection: bool,
    /// An undo step is available.
    pub can_undo: bool,
    /// A redo step is available.
    pub can_redo: bool,
}

/// Native menu items that can request an action once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuRequest {
    /// Export dialog requested.
    Export,
    /// Open-port dialog requested.
    OpenPort,
    /// Port settings dialog requested.
    PortSettings,
    /// Connect or disconnect toggle requested.
    ToggleConnect,
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{EditState, MenuRequest};

    // SAFETY CONTRACT for the whole block: these symbols are implemented in
    // `resources/macos.m`, compiled into the binary by `build.rs`. All of them
    // are self-contained AppKit helpers. They take either plain scalars or
    // pointer plus length pairs that stay valid for the duration of the call,
    // and none of them retains a pointer past its return. `..._get_clipboard_text`
    // hands out an allocation that must be released with
    // `..._free_clipboard_text`, which the wrapper below always does.
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn devserial_init_macos_app(version: *const std::ffi::c_char, data: *const u8, len: usize);
        fn devserial_get_clipboard_text() -> *const std::ffi::c_char;
        fn devserial_free_clipboard_text(ptr: *const std::ffi::c_char);
        fn devserial_check_export_requested() -> bool;
        fn devserial_check_open_port_requested() -> bool;
        fn devserial_check_port_settings_requested() -> bool;
        fn devserial_check_toggle_connect_requested() -> bool;
        fn devserial_update_edit_state(
            is_text_focused: bool,
            has_text_selection: bool,
            text_field_not_empty: bool,
            buffer_not_empty: bool,
            has_buffer_selection: bool,
            can_undo: bool,
            can_redo: bool,
        );
    }

    pub fn init_app(icon_png: &'static [u8], version: &str) {
        let Ok(version) = std::ffi::CString::new(version) else {
            return;
        };
        // SAFETY: see the block contract. `version` outlives the call and
        // `icon_png` is static.
        #[allow(unsafe_code)]
        unsafe {
            devserial_init_macos_app(version.as_ptr(), icon_png.as_ptr(), icon_png.len());
        }
    }

    pub fn clipboard_text() -> Option<String> {
        // SAFETY: see the block contract. The returned allocation is freed
        // before this function returns and never escapes as a pointer.
        #[allow(unsafe_code)]
        unsafe {
            let ptr = devserial_get_clipboard_text();
            if ptr.is_null() {
                return None;
            }
            let text = std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned();
            devserial_free_clipboard_text(ptr);
            Some(text)
        }
    }

    pub fn take_menu_request(request: MenuRequest) -> bool {
        // SAFETY: see the block contract. These are argument-free predicates
        // that clear a flag inside the helper.
        #[allow(unsafe_code)]
        unsafe {
            match request {
                MenuRequest::Export => devserial_check_export_requested(),
                MenuRequest::OpenPort => devserial_check_open_port_requested(),
                MenuRequest::PortSettings => devserial_check_port_settings_requested(),
                MenuRequest::ToggleConnect => devserial_check_toggle_connect_requested(),
            }
        }
    }

    pub fn update_edit_state(state: EditState) {
        // SAFETY: see the block contract. Scalar arguments only.
        #[allow(unsafe_code)]
        unsafe {
            devserial_update_edit_state(
                state.text_focused,
                state.text_selection,
                state.text_not_empty,
                state.buffer_not_empty,
                state.buffer_selection,
                state.can_undo,
                state.can_redo,
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod fallback {
    use super::{EditState, MenuRequest};

    pub const fn init_app(_icon_png: &'static [u8], _version: &str) {}

    pub const fn clipboard_text() -> Option<String> {
        None
    }

    pub const fn take_menu_request(_request: MenuRequest) -> bool {
        false
    }

    pub const fn update_edit_state(_state: EditState) {}
}

#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(target_os = "macos"))]
use fallback as imp;

/// Initialize native application integration (dock icon, menus, about panel).
///
/// Inert on platforms without native integration.
pub fn init_app() {
    imp::init_app(crate::assets::ICON_PNG, env!("CARGO_PKG_VERSION"));
}

/// Read the system clipboard through the platform API.
///
/// Returns `None` when the platform provides no native clipboard access; the
/// GUI framework's own clipboard is used in that case.
#[must_use]
pub fn clipboard_text() -> Option<String> {
    imp::clipboard_text()
}

/// Consume a pending native menu request.
///
/// Returns `true` exactly once per menu activation.
#[must_use]
pub fn take_menu_request(request: MenuRequest) -> bool {
    imp::take_menu_request(request)
}

/// Publish the current edit state so native menu items enable and disable.
pub fn update_edit_state(state: EditState) {
    imp::update_edit_state(state);
}

/// Whether this build has native menu integration.
#[must_use]
pub const fn has_native_menus() -> bool {
    cfg!(target_os = "macos")
}
