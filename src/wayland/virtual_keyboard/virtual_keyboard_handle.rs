use std::os::unix::io::OwnedFd;
use std::{
    fmt,
    sync::{Arc, Mutex},
};

use tracing::debug;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_v1::Error::NoKeymap;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_v1::{
    self, ZwpVirtualKeyboardV1,
};
use wayland_server::{Client, DataInit, DisplayHandle, Resource, protocol::wl_keyboard::KeymapFormat};
use xkbcommon::xkb;

use crate::backend::input::{InputTime, KeyState, Keycode};
use crate::input::keyboard::{KeyboardTarget, KeymapFile, KeysymHandle, ModifiersState, Xkb};
use crate::{
    input::{Seat, SeatHandler},
    utils::SERIAL_COUNTER,
    wayland::{Dispatch2, seat::WaylandFocus},
};

#[derive(Debug, Default)]
pub(crate) struct VirtualKeyboard {
    state: Option<VirtualKeyboardState>,
}

struct VirtualKeyboardState {
    keymap: KeymapFile,
    mods: ModifiersState,
    /// The virtual keyboard's own keymap and modifier state: keys are resolved
    /// against it, never against the seat's keymap.
    xkb: Mutex<Xkb>,
}

impl fmt::Debug for VirtualKeyboardState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualKeyboardState")
            .field("keymap", &self.keymap)
            .field("mods", &self.mods)
            .field("xkb", &self.xkb)
            .finish()
    }
}

// This is OK because all parts of `xkb` will remain on the
// same thread
unsafe impl Send for VirtualKeyboard {}

/// Handle to a virtual keyboard instance
#[derive(Debug, Clone, Default)]
pub(crate) struct VirtualKeyboardHandle {
    pub(crate) inner: Arc<Mutex<VirtualKeyboard>>,
}

/// User data of ZwpVirtualKeyboardV1 object
pub struct VirtualKeyboardUserData<D: SeatHandler> {
    pub(super) handle: VirtualKeyboardHandle,
    pub(crate) seat: Seat<D>,
}

impl<D: SeatHandler> fmt::Debug for VirtualKeyboardUserData<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualKeyboardUserData")
            .field("handle", &self.handle)
            .field("seat", &self.seat.arc)
            .finish()
    }
}

impl<D> Dispatch2<ZwpVirtualKeyboardV1, D> for VirtualKeyboardUserData<D>
where
    D: SeatHandler + 'static,
    <D as SeatHandler>::KeyboardFocus: WaylandFocus,
{
    fn request(
        &self,
        user_data: &mut D,
        _client: &Client,
        virtual_keyboard: &ZwpVirtualKeyboardV1,
        request: zwp_virtual_keyboard_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwp_virtual_keyboard_v1::Request::Keymap { format, fd, size } => {
                update_keymap(self, format, fd, size as usize);
            }
            zwp_virtual_keyboard_v1::Request::Key { time, key, state } => {
                // Ensure keymap was initialized.
                let mut virtual_data = self.handle.inner.lock().unwrap();
                let vk_state = match virtual_data.state.as_mut() {
                    Some(vk_state) => vk_state,
                    None => {
                        virtual_keyboard.post_error(NoKeymap, "`key` sent before keymap.");
                        return;
                    }
                };

                // Ensure virtual keyboard's keymap is active.
                let keyboard_handle = self.seat.get_keyboard().unwrap();
                let focus = {
                    let mut internal = keyboard_handle.arc.internal.lock().unwrap();
                    let focus = internal.focus.as_mut().map(|(focus, _)| focus);
                    keyboard_handle.send_keymap(user_data, &focus, &vk_state.keymap, vk_state.mods);
                    // Delivered with the seat keyboard's lock released: the focus may
                    // move the keyboard focus in response to the key.
                    internal.focus.as_ref().map(|(focus, _)| focus.clone())
                };

                // Deliver through the focus target so it reaches every kind of focus
                // the compositor knows, not only wl_surface ones. The key is resolved
                // against the virtual keyboard's keymap, which the focused client has
                // just been sent as well.
                //
                // This should be wl_keyboard::KeyState, but the protocol does not state
                // the parameter is an enum.
                let key_state = if state == 1 {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                if let Some(focus) = focus {
                    // Virtual keyboard keys are evdev codes; xkb keycodes are offset by 8.
                    let keycode = Keycode::new(key + 8);
                    let handle = KeysymHandle::new(&vk_state.xkb, keycode);
                    focus.key(
                        &self.seat,
                        user_data,
                        handle,
                        key_state,
                        SERIAL_COUNTER.next_serial(),
                        InputTime::from_millis(time),
                    );
                }
            }
            zwp_virtual_keyboard_v1::Request::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                // Ensure keymap was initialized.
                let mut virtual_data = self.handle.inner.lock().unwrap();
                let state = match virtual_data.state.as_mut() {
                    Some(state) => state,
                    None => {
                        virtual_keyboard.post_error(NoKeymap, "`modifiers` sent before keymap.");
                        return;
                    }
                };

                // Update virtual keyboard's modifier state.
                {
                    let mut xkb = state.xkb.lock().unwrap();
                    xkb.state_mut()
                        .update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                    state.mods.update_with(xkb.state_mut());
                }

                // Ensure virtual keyboard's keymap is active.
                let keyboard_handle = self.seat.get_keyboard().unwrap();
                let mut internal = keyboard_handle.arc.internal.lock().unwrap();
                let focus = internal.focus.as_mut().map(|(focus, _)| focus);
                let keymap_changed =
                    keyboard_handle.send_keymap(user_data, &focus, &state.keymap, state.mods);

                // Report modifiers change to all keyboards.
                if !keymap_changed {
                    if let Some(focus) = focus {
                        focus.modifiers(&self.seat, user_data, state.mods, SERIAL_COUNTER.next_serial());
                    }
                }
            }
            zwp_virtual_keyboard_v1::Request::Destroy => {
                // Nothing to do
            }
            _ => unreachable!(),
        }
    }
}

/// Handle the zwp_virtual_keyboard_v1::keymap request.
///
/// The `true` returns when keymap was properly loaded.
fn update_keymap<D>(data: &VirtualKeyboardUserData<D>, format: u32, fd: OwnedFd, size: usize)
where
    D: SeatHandler + 'static,
{
    // Only libxkbcommon compatible keymaps are supported.
    if format != KeymapFormat::XkbV1 as u32 {
        debug!("Unsupported keymap format: {format:?}");
        return;
    }

    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    // SAFETY: we can map the keymap into the memory.
    let new_keymap = match unsafe {
        xkb::Keymap::new_from_fd(
            &context,
            fd,
            size,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
    } {
        Ok(Some(new_keymap)) => new_keymap,
        Ok(None) => {
            debug!("Invalid libxkbcommon keymap");
            return;
        }
        Err(err) => {
            debug!("Could not map the keymap: {err:?}");
            return;
        }
    };

    // Store active virtual keyboard map.
    let mut inner = data.handle.inner.lock().unwrap();
    let mods = inner.state.take().map(|state| state.mods).unwrap_or_default();
    inner.state = Some(VirtualKeyboardState {
        mods,
        keymap: KeymapFile::new(&new_keymap),
        xkb: Mutex::new(Xkb::from_keymap(context, new_keymap)),
    });
}
