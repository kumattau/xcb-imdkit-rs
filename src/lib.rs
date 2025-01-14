/*!
Wrapper around [xcb-imdkit](https://github.com/fcitx/xcb-imdkit), providing an IME client.

[xcb-imdkit](https://github.com/fcitx/xcb-imdkit) provides a partial implementation of the [X11
Input Method Protocol](https://www.x.org/releases/current/doc/libX11/XIM/xim.html) using
[XCB](https://xcb.freedesktop.org/). This wrapper library provides the most essential functionality
of said library as simply as possible.

To get started quickly, consult the examples folder.
*/

#[macro_use]
extern crate lazy_static;

use std::os::raw::{c_char, c_void};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use xcb::x::Window;
use xcb::{Raw, Xid, XidNew};

use bitflags::bitflags;

use clib::*;

mod clib;

type LogFn = dyn for<'a> FnMut(&'a str) + Send;

lazy_static! {
    static ref LOGGER: Mutex<Option<Box<LogFn>>> = Mutex::default();
}

extern "C" {
    fn xcb_log_wrapper(msg: *const c_char, ...);
}

#[no_mangle]
fn rust_log(msg: *const c_char) {
    let msg = unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy();
    let msg = msg.trim();
    if let Some(logger) = LOGGER.lock().unwrap().as_mut() {
        logger(msg);
    }
}

extern "C" fn create_ic_callback(im: *mut xcb_xim_t, new_ic: xcb_xic_t, user_data: *mut c_void) {
    let ime = unsafe { ime_from_user_data(user_data) };
    ime.ic = Some(new_ic);
    unsafe {
        xcb_xim_set_ic_focus(im, new_ic);
    }
}

extern "C" fn open_callback(im: *mut xcb_xim_t, user_data: *mut c_void) {
    let ime = unsafe { ime_from_user_data(user_data) };
    let input_style = ime.input_style.bits();
    let spot = xcb_point_t {
        x: ime.pos_req.x,
        y: ime.pos_req.y,
    };
    let w = &mut ime.pos_req.win as *mut u32;
    unsafe {
        let nested = xcb_xim_create_nested_list(
            im,
            XCB_XIM_XNSpotLocation,
            &spot,
            std::ptr::null_mut::<c_void>(),
        );
        xcb_xim_create_ic(
            im,
            Some(create_ic_callback),
            user_data,
            XCB_XIM_XNInputStyle,
            &input_style,
            XCB_XIM_XNClientWindow,
            w,
            XCB_XIM_XNFocusWindow,
            w,
            XCB_XIM_XNPreeditAttributes,
            &nested,
            std::ptr::null_mut::<c_void>(),
        );
        free(nested.data as _);
    }
    ime.pos_cur = ime.pos_req;
}

unsafe fn xim_encoding_to_utf8(
    im: *mut xcb_xim_t,
    xim_str: *const c_char,
    length: usize,
) -> String {
    let mut buf: Vec<u8> = vec![];
    if xcb_xim_get_encoding(im) == _xcb_xim_encoding_t_XCB_XIM_UTF8_STRING {
        buf.extend(std::slice::from_raw_parts(
            xim_str as *const u8,
            length as usize,
        ));
    } else if xcb_xim_get_encoding(im) == _xcb_xim_encoding_t_XCB_XIM_COMPOUND_TEXT {
        let mut new_length = 0usize;
        let utf8 = xcb_compound_text_to_utf8(xim_str, length as usize, &mut new_length);
        if !utf8.is_null() {
            buf.extend(std::slice::from_raw_parts(utf8 as _, new_length));
            free(utf8 as _);
        }
    }
    String::from_utf8_unchecked(buf)
}

unsafe fn ime_from_user_data(user_data: *mut c_void) -> &'static mut ImeClient {
    &mut *(user_data as *mut ImeClient)
}

extern "C" fn disconnected_callback(
    _im: *mut xcb_xim_t,
    user_data: *mut c_void,
) {
    let ime = unsafe { ime_from_user_data(user_data) };
    ime.ic.take();
}

extern "C" fn commit_string_callback(
    im: *mut xcb_xim_t,
    _ic: xcb_xic_t,
    _flag: u32,
    input: *mut c_char,
    length: u32,
    _keysym: *mut u32,
    _n_keysym: usize,
    user_data: *mut c_void,
) {
    let input = unsafe { xim_encoding_to_utf8(im, input, length as usize) };
    let ime = unsafe { ime_from_user_data(user_data) };
    let win = unsafe { Window::new(ime.pos_req.win) };
    ime.callbacks.commit_string.as_mut().map(|f| f(win, &input));
}

extern "C" fn update_pos_callback(_im: *mut xcb_xim_t, ic: xcb_xic_t, user_data: *mut c_void) {
    let ime = unsafe { ime_from_user_data(user_data) };
    if ime.pos_update_queued {
        ime.pos_update_queued = false;
        ime.send_pos_update(ic);
    } else {
        ime.is_processing_pos_update = false;
    }
}

const XCB_KEY_PRESS: u8 = 2;
const XCB_KEY_RELEASE: u8 = 3;

extern "C" fn forward_event_callback(
    _im: *mut xcb_xim_t,
    _ic: xcb_xic_t,
    event: *mut xcb_key_press_event_t,
    user_data: *mut c_void,
) {
    let pressed = unsafe { ((*event).response_type & 0x7f) == XCB_KEY_PRESS };
    let ptr = event as *const xcb::ffi::xcb_generic_event_t;
    let event = unsafe {
        if pressed {
            xcb::Event::X(xcb::x::Event::KeyPress(xcb::x::KeyPressEvent::from_raw(
                ptr as _,
            )))
        } else {
            xcb::Event::X(xcb::x::Event::KeyRelease(
                xcb::x::KeyReleaseEvent::from_raw(ptr as _),
            ))
        }
    };
    let ime = unsafe { ime_from_user_data(user_data) };
    let win = unsafe { Window::new(ime.pos_req.win) };
    ime.callbacks.forward_event.as_mut().map(|f| f(win, &event));

    // xcb::KeyPressEvent has a Drop impl that will free `event`, but since we don't own it, we
    // have to prevent that from happening
    std::mem::forget(event);
}

extern "C" fn preedit_start_callback(_im: *mut xcb_xim_t, _ic: xcb_xic_t, user_data: *mut c_void) {
    let ime = unsafe { ime_from_user_data(user_data) };
    let win = unsafe { Window::new(ime.pos_req.win) };
    ime.callbacks.preedit_start.as_mut().map(|f| f(win));
}

extern "C" fn preedit_draw_callback(
    im: *mut xcb_xim_t,
    _ic: xcb_xic_t,
    frame: *mut xcb_im_preedit_draw_fr_t,
    user_data: *mut c_void,
) {
    let frame = unsafe { &*frame };
    let preedit_info = PreeditInfo { inner: frame, im };
    let ime = unsafe { ime_from_user_data(user_data) };
    let win = unsafe { Window::new(ime.pos_req.win) };
    ime.callbacks
        .preedit_draw
        .as_mut()
        .map(|f| f(win, preedit_info));
}

extern "C" fn preedit_done_callback(_im: *mut xcb_xim_t, _ic: xcb_xic_t, user_data: *mut c_void) {
    let ime = unsafe { ime_from_user_data(user_data) };
    let win = unsafe { Window::new(ime.pos_req.win) };
    ime.callbacks.preedit_done.as_mut().map(|f| f(win));
}

bitflags! {
    /// [`InputStyle`] determines how the IME should integrate into the application.
    pub struct InputStyle: u32 {
        /// By default let the IME handle all input composition internally and only process the
        /// final string after composition is finished using [`ImeClient::set_commit_string_cb`].
        const DEFAULT = 0;

        /// Enable calling of the preedit callbacks like the one set with
        /// [`ImeClient::set_preedit_draw_cb`]. This enables displaying the currently edited text
        /// inside the application and not only within the IME. The IME may stop displaying its
        /// cursor if this flag is set.
        const PREEDIT_CALLBACKS = _xcb_im_style_t_XCB_IM_PreeditCallbacks;
    }
}

bitflags! {
    /// [`InputFeedback`] is feedback information to the preedit text.
    /// Each element in [`PreeditInfo::feedback_array`] is a bitmask represented by a value of [`InputFeedback`].
    pub struct InputFeedback: u32 {
        /// By default the preedit text should be drawn in normal manner.
        const DEFAULT = 0;

        /// The preedit text should be drawn by swapping the foreground and background colors used to draw normal, unhighlighted characters.
        const REVERSE = xcb_im_feedback_t_XCB_XIM_REVERSE;

        /// The preedit text should be underlined.
        const UNDERLINE = xcb_im_feedback_t_XCB_XIM_UNDERLINE;

        /// The preedit text should be drawn in some unique manner that must be different from REVERSE and UNDERLINE.
        const HIGHLIGHT = xcb_im_feedback_t_XCB_XIM_HIGHLIGHT;

        /// The preedit text should be drawn in some unique manner that must be different from REVERSE and UNDERLINE.
        const PRIMARY = xcb_im_feedback_t_XCB_XIM_PRIMARY;

        /// The preedit text should be drawn in some unique manner that must be different from REVERSE and UNDERLINE.
        const SECONDARY = xcb_im_feedback_t_XCB_XIM_SECONDARY;

        /// The preedit text should be drawn in some unique manner that must be different from REVERSE and UNDERLINE.
        const TERTIARY = xcb_im_feedback_t_XCB_XIM_TERTIARY;

        /// The preedit text is preferably displayed in the primary draw direction from the caret position in the preedit area forward.
        const VISIBLE_TO_FORWARD = xcb_im_feedback_t_XCB_XIM_VISIBLE_TO_FORWARD;

        /// The preedit text is preferably displayed from the caret position in the preedit area backward, relative to the primary draw direction.
        const VISIBLE_TO_BACKWORD = xcb_im_feedback_t_XCB_XIM_VISIBLE_TO_BACKWORD;

        /// The preedit text is preferably displayed with the caret position in the preedit area centered.
        const VISIBLE_TO_CENTER = xcb_im_feedback_t_XCB_XIM_VISIBLE_TO_CENTER;
    }
}

type StringCB = dyn for<'a> FnMut(Window, &'a str);
type KeyPressCB = dyn for<'a> FnMut(Window, &'a xcb::Event);
type PreeditDrawCB = dyn for<'a> FnMut(Window, PreeditInfo<'a>);
type NotifyCB = dyn FnMut(Window);

#[derive(Default)]
struct Callbacks {
    commit_string: Option<Box<StringCB>>,
    forward_event: Option<Box<KeyPressCB>>,
    preedit_start: Option<Box<NotifyCB>>,
    preedit_draw: Option<Box<PreeditDrawCB>>,
    preedit_done: Option<Box<NotifyCB>>,
}

#[derive(Debug, Clone, Copy)]
struct ImePos {
    win: u32,
    x: i16,
    y: i16,
}

/// [`PreeditInfo`] provides information about the text that is currently being edited by the IME.
///
/// Additionally it provides information about how the text has been changed.
pub struct PreeditInfo<'a> {
    im: *mut xcb_xim_t,
    inner: &'a xcb_im_preedit_draw_fr_t,
}

impl<'a> PreeditInfo<'a> {
    /// Status bitmask.
    ///
    /// - `0x01`: no string
    /// - `0x02`: no feedback
    ///
    /// If no bits are set, [`text`] contains the current text of the IME.
    ///
    /// [`text`]: PreeditInfo::text
    pub fn status(&self) -> u32 {
        self.inner.status
    }

    /// Cursor offset within the currently edited text in characters.
    pub fn caret(&self) -> u32 {
        self.inner.caret
    }

    /// Starting change position.
    pub fn chg_first(&self) -> u32 {
        self.inner.chg_first
    }

    /// Length of the change counting characters.
    pub fn chg_length(&self) -> u32 {
        self.inner.chg_length
    }

    /// Current text in the IME.
    pub fn text(&self) -> String {
        unsafe {
            xim_encoding_to_utf8(
                self.im,
                self.inner.preedit_string as _,
                self.inner.length_of_preedit_string as usize,
            )
        }
    }

    /// Feedback information to each character of preedit text.
    /// Refer to [`InputFeedback`] for more details.
    pub fn feedback_array(&self) -> &[u32] {
        unsafe {
            std::slice::from_raw_parts(
                self.inner.feedback_array.items,
                self.inner.feedback_array.size as usize,
            )
        }
    }
}

impl<'a> std::fmt::Debug for PreeditInfo<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreeditInfo")
            .field("status", &self.status())
            .field("caret", &self.caret())
            .field("chg_first", &self.chg_first())
            .field("chg_length", &self.chg_length())
            .field("feedback_array", &self.feedback_array())
            .field("text", &self.text());
        Ok(())
    }
}

/// Input Method Editor (IME) client.
///
/// [`ImeClient`] represents one instance of an Input Method Editor client. It provides callbacks for
/// event handling as well as control over the position of the IME window. There should be only one
/// IME client per application and it is advised to create at most one instance.
pub struct ImeClient {
    conn: Option<Arc<xcb::Connection>>,
    im: *mut xcb_xim_t,
    ic: Option<xcb_xic_t>,
    callbacks: Callbacks,
    input_style: InputStyle,
    pos_cur: ImePos,
    pos_req: ImePos,
    is_processing_pos_update: bool,
    pos_update_queued: bool,
}

impl ImeClient {
    /// Set the global logger for xcb-imdkit.
    ///
    /// The callback will receive debug messages from the [C
    /// library](https://github.com/fcitx/xcb-imdkit) this crate is wrapping.
    pub fn set_logger<F>(f: F)
    where
        F: for<'a> FnMut(&'a str) + Send + 'static,
    {
        LOGGER.lock().unwrap().replace(Box::new(f));
    }

    /// Create a new [`ImeClient`].
    ///
    /// The first two arguments correspond to the result of [`xcb::Connection::connect`] with the
    /// connection wrapped into an [`Arc`] to ensure that the `Ime` does not outlive its
    /// connection.
    /// For documentation on `input_style` refer to [`InputStyle`].
    /// `im_name` can be used to specify a custom IME server to connect to using the syntax
    /// `@im=custom_server`.
    ///
    /// [`Arc`]: std::sync::Arc
    pub fn new(
        conn: Arc<xcb::Connection>,
        screen_id: i32,
        input_style: InputStyle,
        im_name: Option<&str>,
    ) -> Pin<Box<Self>> {
        let mut res = unsafe { Self::unsafe_new(&conn, screen_id, input_style, im_name) };
        res.conn = Some(conn);
        res
    }

    /// Create a new [`ImeClient`].
    ///
    /// This is the same as [`new`], except that the [`xcb::Connection`] is not wrapped
    /// into an [`Arc`].
    ///
    /// # Safety
    ///
    /// The caller is responsible to ensure that the [`ImeClient`] does not outlive the connection.
    ///
    /// [`Arc`]: std::sync::Arc
    /// [`new`]: ImeClient::new
    pub unsafe fn unsafe_new(
        conn: &xcb::Connection,
        screen_id: i32,
        input_style: InputStyle,
        im_name: Option<&str>,
    ) -> Pin<Box<Self>> {
        xcb_compound_text_init();
        let im = xcb_xim_create(
            conn.get_raw_conn() as _,
            screen_id,
            im_name.map_or(std::ptr::null(), |name| name.as_ptr() as _),
        );
        let mut res = Box::pin(Self {
            conn: None,
            im,
            ic: None,
            callbacks: Callbacks::default(),
            input_style,
            pos_cur: ImePos { win: 0, x: 0, y: 0 },
            pos_req: ImePos { win: 0, x: 0, y: 0 },
            is_processing_pos_update: false,
            pos_update_queued: false,
        });
        let callbacks = xcb_xim_im_callback {
            disconnected: Some(disconnected_callback),
            commit_string: Some(commit_string_callback),
            forward_event: Some(forward_event_callback),
            preedit_start: Some(preedit_start_callback),
            preedit_draw: Some(preedit_draw_callback),
            preedit_done: Some(preedit_done_callback),
            ..Default::default()
        };
        let data: *mut Self = res.as_mut().get_mut();
        xcb_xim_set_im_callback(im, &callbacks, data as _);
        xcb_xim_set_log_handler(im, Some(xcb_log_wrapper));
        xcb_xim_set_use_compound_text(im, true);
        xcb_xim_set_use_utf8_string(im, true);
        res
    }

    fn try_open_ic(&mut self) {
        if self.ic.is_some() {
            return;
        }
        let data: *mut ImeClient = self as _;
        unsafe { xcb_xim_open(self.im, Some(open_callback), true, data as _) };
    }

    /// Let the IME client process XCB's events.
    ///
    /// Return `true` if the IME client is handling the event and `false` if the event is ignored
    /// by the IME client and has to be handled separately.
    ///
    /// This method should be called on **any** event from the event queue and not just
    /// keypress/keyrelease events as it handles other events as well.
    ///
    /// Typically you will want to let the IME client handle all keypress/keyrelease events in your
    /// main loop. The IME client will then forward all key events that were not used for input
    /// composition to the callback set by [`set_forward_event_cb`]. Often those events include all
    /// keyrelease events as well as the events for `ESC`, `Enter` or key combinations such as
    /// `CTRL+C`.
    /// To obtain the text currently typed into the IME and the final string consult
    /// [`set_preedit_draw_cb`] and [`set_commit_string_cb`].
    ///
    /// [`set_forward_event_cb`]: ImeClient::set_forward_event_cb
    /// [`set_commit_string_cb`]: ImeClient::set_commit_string_cb
    /// [`set_preedit_draw_cb`]: ImeClient::set_preedit_draw_cb
    pub fn process_event(&mut self, event: &xcb::Event) -> bool {
        let raw = event.as_raw();
        if !unsafe { xcb_xim_filter_event(self.im, raw as _) } {
            let mask = unsafe { (*raw).response_type & !0x80 };
            if (mask == XCB_KEY_PRESS) || (mask == XCB_KEY_RELEASE) {
                match self.ic {
                    Some(ic) => {
                        unsafe {
                            xcb_xim_forward_event(self.im, ic, raw as _);
                        }
                        return true;
                    }
                    _ => {
                        self.try_open_ic();
                    }
                }
            }
        }
        false
    }

    /// Set the position at which to place the IME window.
    ///
    /// Set the position of the IME window relative to the window specified by `win`. Coordinates
    /// increase from the top left corner of the window.
    ///
    /// Return `true` if an update for the IME window position has been sent to the IME, `false` if
    /// the update has been queued. If there is still an update request queued and this method is
    /// called, the previously queued request is discarded in favor of the new one.
    pub fn update_pos(&mut self, win: Window, x: i16, y: i16) -> bool {
        self.pos_req = ImePos {
            win: win.resource_id(),
            x,
            y,
        };
        match self.ic {
            Some(ic) => {
                if self.is_processing_pos_update {
                    self.pos_update_queued = true;
                    return false;
                }
                self.send_pos_update(ic);
                true
            }
            _ => {
                self.try_open_ic();
                false
            }
        }
    }

    fn send_pos_update(&mut self, ic: xcb_xic_t) {
        self.is_processing_pos_update = true;
        let spot = xcb_point_t {
            x: self.pos_req.x,
            y: self.pos_req.y,
        };
        let nested = unsafe {
            xcb_xim_create_nested_list(
                self.im,
                XCB_XIM_XNSpotLocation,
                &spot,
                std::ptr::null_mut::<c_void>(),
            )
        };
        if self.pos_req.win != self.pos_cur.win {
            let w = &mut self.pos_req.win as *mut _;
            unsafe {
                xcb_xim_set_ic_values(
                    self.im,
                    ic,
                    Some(update_pos_callback),
                    self as *mut _ as _,
                    XCB_XIM_XNClientWindow,
                    w,
                    XCB_XIM_XNFocusWindow,
                    w,
                    XCB_XIM_XNPreeditAttributes,
                    &nested,
                    std::ptr::null_mut::<c_void>(),
                );
            }
        } else {
            unsafe {
                xcb_xim_set_ic_values(
                    self.im,
                    ic,
                    Some(update_pos_callback),
                    self as *mut _ as _,
                    XCB_XIM_XNPreeditAttributes,
                    &nested,
                    std::ptr::null_mut::<c_void>(),
                );
            }
        }
        unsafe { free(nested.data as _) };
        self.pos_cur = self.pos_req;
    }

    /// Set callback to be called once input composition is done.
    ///
    /// The window (set by [`update_pos`]) as well as the completed input are passed as arguments.
    ///
    /// [`update_pos`]: ImeClient::update_pos
    pub fn set_commit_string_cb<F>(&mut self, f: F)
    where
        F: for<'a> FnMut(Window, &'a str) + 'static,
    {
        self.callbacks.commit_string = Some(Box::new(f));
    }

    // Set callback for keypress/keyrelease events unhandled by the IME.
    //
    // The first argument passed is the window (set by [`update_pos`]), the second the key event.
    /// Often those events include all keyrelease events as well as the events for `ESC`, `Enter`
    /// or key combinations such as `CTRL+C`. Please note that [`xcb::KeyPressEvent`] ==
    /// [`xcb::KeyReleaseEvent`] (see [`xcb::ffi::xcb_key_release_event_t`]) and keyrelease events
    /// are also supplied.
    ///
    /// [`update_pos`]: ImeClient::update_pos
    pub fn set_forward_event_cb<F>(&mut self, f: F)
    where
        F: for<'a> FnMut(Window, &'a xcb::Event) + 'static,
    {
        self.callbacks.forward_event = Some(Box::new(f));
    }

    /// Callback called once the IME has been opened.
    ///
    /// The current window (set by [`update_pos`]) is supplied as argument.
    /// Calls callback only if [`InputStyle::PREEDIT_CALLBACKS`] is set.
    ///
    /// [`update_pos`]: ImeClient::update_pos
    pub fn set_preedit_start_cb<F>(&mut self, f: F)
    where
        F: FnMut(Window) + 'static,
    {
        self.callbacks.preedit_start = Some(Box::new(f));
    }

    /// Callback called whenever the text whitin the IME has changed.
    ///
    /// The current window (set by [`update_pos`]) is supplied as argument as well as
    /// [`PreeditInfo`], which contains, among other things, the current text of the IME.
    /// Calls callback only if [`InputStyle::PREEDIT_CALLBACKS`] is set.
    ///
    /// [`update_pos`]: ImeClient::update_pos
    pub fn set_preedit_draw_cb<F>(&mut self, f: F)
    where
        F: for<'a> FnMut(Window, PreeditInfo<'a>) + 'static,
    {
        self.callbacks.preedit_draw = Some(Box::new(f));
    }

    /// Callback called once the IME has been closed.
    ///
    /// The current window (set by [`update_pos`]) is supplied as argument.
    /// Calls callback only if [`InputStyle::PREEDIT_CALLBACKS`] is set.
    ///
    /// [`update_pos`]: ImeClient::update_pos
    pub fn set_preedit_done_cb<F>(&mut self, f: F)
    where
        F: FnMut(Window) + 'static,
    {
        self.callbacks.preedit_done = Some(Box::new(f));
    }
}

impl Drop for ImeClient {
    fn drop(&mut self) {
        unsafe {
            if let Some(ic) = self.ic {
                xcb_xim_destroy_ic(self.im, ic, None, std::ptr::null_mut());
            }
            xcb_xim_close(self.im);
            xcb_xim_destroy(self.im);
        }
    }
}
